//! Validator Config V2 precompile -- upgraded consensus validator registry with lifecycle
//! tracking, ed25519 ownership proof, and migration support from V1.
//!
//! Ported from `tempo/crates/precompiles/src/validator_config_v2/`.
//!
//! ## Key differences from V1
//!
//! - Append-only validator records with height-based lifecycle (addedAtHeight, deactivatedAtHeight)
//! - Ed25519 signature verification for add/rotate operations
//! - Active-indices vec for O(active_count) enumeration via swap-and-pop
//! - IP uniqueness enforcement (ingress hashing)
//! - V1 migration support
//!
//! ## Storage layout
//!
//! | Slot | Field                             | Type                     |
//! |------|-----------------------------------|--------------------------|
//! |  0   | config                            | Config (packed)          |
//! |  1   | validators                        | Vec<ValidatorRecord>     |
//! |  2   | address_to_index                  | Mapping<Address, u64>    |
//! |  3   | pubkey_to_index                   | Mapping<B256, u64>       |
//! |  4   | next_network_identity_rotation_epoch | u64                  |
//! |  5   | active_ingress_ips                | Mapping<B256, bool>      |
//! |  6   | active_indices                    | Vec<u64>                 |
//!
//! ## Signature verification
//!
//! Ed25519 signature verification is **stubbed** in leafage-evm. The original Tempo node uses
//! `commonware-cryptography` for ed25519 verification, which we do not depend on. Since leafage
//! is a read-only node and signature verification only gates mutate operations, this stub
//! does not affect view call correctness.

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_types::{
    Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType, VecHandler,
};
use super::validator_config::ValidatorConfig;
use super::{
    fill_precompile_output, input_cost, mutate, mutate_void, view, Precompile,
    VALIDATOR_CONFIG_V2_ADDRESS,
};

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    interface IValidatorConfigV2 {
        function owner() external view returns (address);
        function getActiveValidators() external view returns (Validator[] memory);
        function getInitializedAtHeight() external view returns (uint64);
        function validatorCount() external view returns (uint64);
        function validatorByIndex(uint64 index) external view returns (Validator memory);
        function validatorByAddress(address validatorAddress) external view returns (Validator memory);
        function validatorByPublicKey(bytes32 publicKey) external view returns (Validator memory);
        function getNextNetworkIdentityRotationEpoch() external view returns (uint64);
        function isInitialized() external view returns (bool);

        function addValidator(
            address validatorAddress,
            bytes32 publicKey,
            string memory ingress,
            string memory egress,
            address feeRecipient,
            bytes memory signature
        ) external returns (uint64);
        function deactivateValidator(uint64 idx) external;
        function rotateValidator(
            uint64 idx,
            bytes32 publicKey,
            string memory ingress,
            string memory egress,
            bytes memory signature
        ) external;
        function setFeeRecipient(uint64 idx, address feeRecipient) external;
        function setIpAddresses(uint64 idx, string memory ingress, string memory egress) external;
        function transferValidatorOwnership(uint64 idx, address newAddress) external;
        function transferOwnership(address newOwner) external;
        function setNetworkIdentityRotationEpoch(uint64 epoch) external;
        function migrateValidator(uint64 idx) external;
        function initializeIfMigrated() external;

        struct Validator {
            bytes32 publicKey;
            address validatorAddress;
            string ingress;
            string egress;
            address feeRecipient;
            uint64 index;
            uint64 addedAtHeight;
            uint64 deactivatedAtHeight;
        }

        event ValidatorAdded(uint64 index, address validatorAddress, bytes32 publicKey, string ingress, string egress, address feeRecipient);
        event ValidatorDeactivated(uint64 index, address validatorAddress);
        event ValidatorRotated(uint64 index, uint64 deactivatedIndex, address validatorAddress, bytes32 oldPublicKey, bytes32 newPublicKey, string ingress, string egress, address caller);
        event FeeRecipientUpdated(uint64 index, address feeRecipient, address caller);
        event IpAddressesUpdated(uint64 index, string ingress, string egress, address caller);
        event ValidatorOwnershipTransferred(uint64 index, address oldAddress, address newAddress, address caller);
        event OwnershipTransferred(address oldOwner, address newOwner);
        event NetworkIdentityRotationEpochSet(uint64 previousEpoch, uint64 nextEpoch);
        event ValidatorMigrated(uint64 index, address validatorAddress, bytes32 publicKey);
        event SkippedValidatorMigration(uint64 index, address validatorAddress, bytes32 publicKey);
        event Initialized(uint64 height);

        error NotInitialized();
        error AlreadyInitialized();
        error Unauthorized();
        error ValidatorNotFound();
        error ValidatorAlreadyDeactivated();
        error InvalidPublicKey();
        error PublicKeyAlreadyExists();
        error InvalidValidatorAddress();
        error AddressAlreadyHasValidator();
        error NotIpPort(string value, string reason);
        error NotIp(string value, string reason);
        error IngressAlreadyExists(string ingress);
        error InvalidSignature();
        error InvalidSignatureFormat();
        error InvalidOwner();
        error InvalidMigrationIndex();
        error MigrationNotComplete();
        error EmptyV1ValidatorSet();
    }
}

// ===========================================================================
// Error helpers
// ===========================================================================

fn err_not_initialized() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IValidatorConfigV2::NotInitialized {}.abi_encode().into())
}

fn err_already_initialized() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::AlreadyInitialized {}.abi_encode().into(),
    )
}

fn err_unauthorized() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IValidatorConfigV2::Unauthorized {}.abi_encode().into())
}

fn err_validator_not_found() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::ValidatorNotFound {}.abi_encode().into(),
    )
}

fn err_validator_already_deactivated() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::ValidatorAlreadyDeactivated {}
            .abi_encode()
            .into(),
    )
}

fn err_invalid_public_key() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IValidatorConfigV2::InvalidPublicKey {}.abi_encode().into())
}

fn err_public_key_already_exists() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::PublicKeyAlreadyExists {}
            .abi_encode()
            .into(),
    )
}

fn err_invalid_validator_address() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::InvalidValidatorAddress {}
            .abi_encode()
            .into(),
    )
}

fn err_address_already_has_validator() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::AddressAlreadyHasValidator {}
            .abi_encode()
            .into(),
    )
}

fn err_not_ip_port(value: String, reason: String) -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::NotIpPort { value, reason }
            .abi_encode()
            .into(),
    )
}

fn err_not_ip(value: String, reason: String) -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::NotIp { value, reason }
            .abi_encode()
            .into(),
    )
}

fn err_ingress_already_exists(ingress: String) -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::IngressAlreadyExists { ingress }
            .abi_encode()
            .into(),
    )
}

#[allow(dead_code)]
fn err_invalid_signature() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IValidatorConfigV2::InvalidSignature {}.abi_encode().into())
}

fn err_invalid_owner() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IValidatorConfigV2::InvalidOwner {}.abi_encode().into())
}

fn err_invalid_migration_index() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::InvalidMigrationIndex {}
            .abi_encode()
            .into(),
    )
}

fn err_migration_not_complete() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::MigrationNotComplete {}
            .abi_encode()
            .into(),
    )
}

fn err_empty_v1_validator_set() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IValidatorConfigV2::EmptyV1ValidatorSet {}
            .abi_encode()
            .into(),
    )
}

// ===========================================================================
// IP validation
// ===========================================================================

/// Validates that `input` is of the form `<ip>:<port>`.
fn ensure_address_is_ip_port(input: &str) -> std::result::Result<(), String> {
    input
        .parse::<std::net::SocketAddr>()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Validates that `input` is a bare IP address (no port).
fn ensure_address_is_ip(input: &str) -> std::result::Result<(), String> {
    input
        .parse::<std::net::IpAddr>()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ===========================================================================
// Config storage type
// ===========================================================================

/// Contract-level configuration: ownership, initialization state, and migration bookkeeping.
///
/// Storage layout (packed into 2 slots):
///   - slot+0: owner (Address, 20 bytes)
///   - slot+1: is_init (bool, 1 byte @ offset 0) + init_at_height (u64, 8 bytes @ offset 1) +
///             migration_skipped_count (u8, 1 byte @ offset 9) + v1_validator_count (u8, 1 byte @ offset 10)
#[derive(Debug, Clone)]
struct Config {
    owner: Address,
    is_init: bool,
    init_at_height: u64,
    migration_skipped_count: u8,
    v1_validator_count: u8,
}

impl Config {
    fn new(owner: Address, is_init: bool, init_at_height: u64) -> Self {
        Self {
            owner,
            is_init,
            init_at_height,
            migration_skipped_count: 0,
            v1_validator_count: 0,
        }
    }

    fn require_init(self) -> Result<Self> {
        if !self.is_init {
            return Err(err_not_initialized());
        }
        Ok(self)
    }

    fn require_not_init(self) -> Result<Self> {
        if self.is_init {
            return Err(err_already_initialized());
        }
        Ok(self)
    }

    fn require_owner(self, caller: Address) -> Result<Self> {
        if self.owner != caller {
            return Err(err_unauthorized());
        }
        Ok(self)
    }

    fn require_owner_or_validator(self, caller: Address, validator: Address) -> Result<Self> {
        if caller != validator && self.owner != caller {
            return Err(err_unauthorized());
        }
        Ok(self)
    }
}

impl StorableType for Config {
    // Address (1 slot) + packed(bool+u64+u8+u8) (1 slot) = 2 slots
    const LAYOUT: Layout = Layout::Slots(2);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for Config {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        // Slot+0: owner (Address, low 20 bytes of big-endian word)
        let word0 = storage.load(slot)?;
        let bytes0 = word0.to_be_bytes::<32>();
        let owner = Address::from_slice(&bytes0[12..32]);

        // Slot+1: packed fields
        let word1 = storage.load(slot + U256::from(1))?;
        let bytes1 = word1.to_be_bytes::<32>();
        let is_init = bytes1[31] != 0;
        let init_at_height = u64::from_be_bytes(bytes1[23..31].try_into().unwrap());
        let migration_skipped_count = bytes1[22];
        let v1_validator_count = bytes1[21];

        Ok(Self {
            owner,
            is_init,
            init_at_height,
            migration_skipped_count,
            v1_validator_count,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        // Slot+0: owner
        let mut bytes0 = [0u8; 32];
        bytes0[12..32].copy_from_slice(self.owner.as_slice());
        storage.store(slot, U256::from_be_bytes(bytes0))?;

        // Slot+1: packed
        let mut bytes1 = [0u8; 32];
        bytes1[31] = if self.is_init { 1 } else { 0 };
        bytes1[23..31].copy_from_slice(&self.init_at_height.to_be_bytes());
        bytes1[22] = self.migration_skipped_count;
        bytes1[21] = self.v1_validator_count;
        storage.store(slot + U256::from(1), U256::from_be_bytes(bytes1))?;

        Ok(())
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)?;
        storage.store(slot + U256::from(1), U256::ZERO)?;
        Ok(())
    }
}

// ===========================================================================
// ValidatorRecord storage type
// ===========================================================================

/// A single entry in the `validators` vector.
///
/// Storage layout (Storable, 7 slots):
///   - slot+0: public_key (B256)
///   - slot+1: validator_address (Address, 20 bytes @ offset 0)
///   - slot+2: ingress (String, dynamic)
///   - slot+3: egress (String, dynamic)
///   - slot+4: fee_recipient (Address, 20 bytes @ offset 0)
///   - slot+5: packed(index: u64 @ 0, active_idx: u64 @ 8, added_at_height: u64 @ 16, deactivated_at_height: u64 @ 24)
///             Packed as 4 x u64 = 32 bytes in one slot
#[derive(Debug, Clone)]
struct ValidatorRecord {
    public_key: B256,
    validator_address: Address,
    ingress: String,
    egress: String,
    fee_recipient: Address,
    index: u64,
    active_idx: u64,
    added_at_height: u64,
    deactivated_at_height: u64,
}

impl Default for ValidatorRecord {
    fn default() -> Self {
        Self {
            public_key: B256::ZERO,
            validator_address: Address::ZERO,
            ingress: String::new(),
            egress: String::new(),
            fee_recipient: Address::ZERO,
            index: 0,
            active_idx: 0,
            added_at_height: 0,
            deactivated_at_height: 0,
        }
    }
}

impl StorableType for ValidatorRecord {
    // B256(1) + Address(1) + String(1) + String(1) + Address(1) + packed_u64x4(1) = 6 slots
    const LAYOUT: Layout = Layout::Slots(6);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for ValidatorRecord {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        // Slot+0: public_key
        let word0 = storage.load(slot)?;
        let public_key = B256::from(word0.to_be_bytes::<32>());

        // Slot+1: validator_address (Address, low 20 bytes)
        let word1 = storage.load(slot + U256::from(1))?;
        let bytes1 = word1.to_be_bytes::<32>();
        let validator_address = Address::from_slice(&bytes1[12..32]);

        // Slot+2: ingress (String)
        let ingress = String::load(storage, slot + U256::from(2), LayoutCtx::FULL)?;

        // Slot+3: egress (String)
        let egress = String::load(storage, slot + U256::from(3), LayoutCtx::FULL)?;

        // Slot+4: fee_recipient (Address, low 20 bytes)
        let word4 = storage.load(slot + U256::from(4))?;
        let bytes4 = word4.to_be_bytes::<32>();
        let fee_recipient = Address::from_slice(&bytes4[12..32]);

        // Slot+5: packed u64x4 (index, active_idx, added_at_height, deactivated_at_height)
        // Packed right-to-left (Tempo #[derive(Storable)] packing):
        //   index: 8 bytes at offset 0 (byte 24..32)
        //   active_idx: 8 bytes at offset 8 (byte 16..24)
        //   added_at_height: 8 bytes at offset 16 (byte 8..16)
        //   deactivated_at_height: 8 bytes at offset 24 (byte 0..8)
        let word5 = storage.load(slot + U256::from(5))?;
        let bytes5 = word5.to_be_bytes::<32>();
        let index = u64::from_be_bytes(bytes5[24..32].try_into().unwrap());
        let active_idx = u64::from_be_bytes(bytes5[16..24].try_into().unwrap());
        let added_at_height = u64::from_be_bytes(bytes5[8..16].try_into().unwrap());
        let deactivated_at_height = u64::from_be_bytes(bytes5[0..8].try_into().unwrap());

        Ok(Self {
            public_key,
            validator_address,
            ingress,
            egress,
            fee_recipient,
            index,
            active_idx,
            added_at_height,
            deactivated_at_height,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        // Slot+0: public_key
        storage.store(slot, U256::from_be_bytes(self.public_key.0))?;

        // Slot+1: validator_address
        let mut bytes1 = [0u8; 32];
        bytes1[12..32].copy_from_slice(self.validator_address.as_slice());
        storage.store(slot + U256::from(1), U256::from_be_bytes(bytes1))?;

        // Slot+2: ingress
        self.ingress
            .store(storage, slot + U256::from(2), LayoutCtx::FULL)?;

        // Slot+3: egress
        self.egress
            .store(storage, slot + U256::from(3), LayoutCtx::FULL)?;

        // Slot+4: fee_recipient
        let mut bytes4 = [0u8; 32];
        bytes4[12..32].copy_from_slice(self.fee_recipient.as_slice());
        storage.store(slot + U256::from(4), U256::from_be_bytes(bytes4))?;

        // Slot+5: packed u64x4
        let mut bytes5 = [0u8; 32];
        bytes5[24..32].copy_from_slice(&self.index.to_be_bytes());
        bytes5[16..24].copy_from_slice(&self.active_idx.to_be_bytes());
        bytes5[8..16].copy_from_slice(&self.added_at_height.to_be_bytes());
        bytes5[0..8].copy_from_slice(&self.deactivated_at_height.to_be_bytes());
        storage.store(slot + U256::from(5), U256::from_be_bytes(bytes5))?;

        Ok(())
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        for i in 0..6 {
            storage.store(slot + U256::from(i), U256::ZERO)?;
        }
        String::delete(storage, slot + U256::from(2), LayoutCtx::FULL)?;
        String::delete(storage, slot + U256::from(3), LayoutCtx::FULL)?;
        Ok(())
    }
}

// ===========================================================================
// ValidatorConfigV2 struct
// ===========================================================================

/// Validator Config V2 precompile.
pub struct ValidatorConfigV2 {
    // Slot 0: config
    config: Slot<Config>,
    // Slot 1: validators (Vec<ValidatorRecord>)
    validators: VecHandler<ValidatorRecord>,
    // Slot 2: address_to_index
    address_to_index: Mapping<Address, u64>,
    // Slot 3: pubkey_to_index
    pubkey_to_index: Mapping<B256, u64>,
    // Slot 4: next_network_identity_rotation_epoch
    next_network_identity_rotation_epoch: Slot<u64>,
    // Slot 5: active_ingress_ips
    active_ingress_ips: Mapping<B256, bool>,
    // Slot 6: active_indices
    active_indices: VecHandler<u64>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl ValidatorConfigV2 {
    pub fn new() -> Self {
        let address = VALIDATOR_CONFIG_V2_ADDRESS;
        Self {
            config: Slot::new(U256::from(0), address),
            validators: VecHandler::new(U256::from(1), address),
            address_to_index: Mapping::new(U256::from(2), address),
            pubkey_to_index: Mapping::new(U256::from(3), address),
            next_network_identity_rotation_epoch: Slot::new(U256::from(4), address),
            active_ingress_ips: Mapping::new(U256::from(5), address),
            active_indices: VecHandler::new(U256::from(6), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage
            .emit_event(self.address, event.into_log_data())
    }

    /// Initializes the validator config V2 precompile.
    pub fn initialize(&mut self, owner: Address) -> Result<()> {
        self.__initialize()?;
        let config = Config::new(owner, true, self.storage.block_number());
        self.config.write(config)
    }

    // =========================================================================
    // View methods
    // =========================================================================

    /// Returns the current owner.
    pub fn owner(&self) -> Result<Address> {
        let config: Config = self.config.read()?;
        Ok(config.owner)
    }

    /// Returns the block height at which the contract was initialized.
    pub fn get_initialized_at_height(&self) -> Result<u64> {
        let config: Config = self.config.read()?;
        Ok(config.init_at_height)
    }

    /// Returns whether V2 has been initialized.
    pub fn is_initialized(&self) -> Result<bool> {
        let config: Config = self.config.read()?;
        Ok(config.is_init)
    }

    /// Returns the total number of validators ever added.
    pub fn validator_count(&self) -> Result<u64> {
        Ok(self.validators.len()? as u64)
    }

    fn get_active_validator(&self, idx: u64) -> Result<ValidatorRecord> {
        if idx >= self.validators.len()? as u64 {
            return Err(err_validator_not_found());
        }
        let v = self.validators[idx as usize].read()?;
        if v.deactivated_at_height != 0 {
            return Err(err_validator_already_deactivated());
        }
        Ok(v)
    }

    fn read_validator_at(&self, index: u64) -> Result<IValidatorConfigV2::Validator> {
        let v = self.validators[index as usize].read()?;
        Ok(IValidatorConfigV2::Validator {
            publicKey: v.public_key,
            validatorAddress: v.validator_address,
            ingress: v.ingress,
            egress: v.egress,
            feeRecipient: v.fee_recipient,
            index: v.index,
            addedAtHeight: v.added_at_height,
            deactivatedAtHeight: v.deactivated_at_height,
        })
    }

    /// Returns the validator at the given global index.
    pub fn validator_by_index(&self, index: u64) -> Result<IValidatorConfigV2::Validator> {
        if index >= self.validator_count()? {
            return Err(err_validator_not_found());
        }
        self.read_validator_at(index)
    }

    /// Looks up a validator by its address.
    pub fn validator_by_address(&self, addr: Address) -> Result<IValidatorConfigV2::Validator> {
        let idx1 = self.address_to_index[addr].read()?;
        if idx1 == 0 {
            return Err(err_validator_not_found());
        }
        self.read_validator_at(idx1 - 1)
    }

    /// Looks up a validator by its Ed25519 public key.
    pub fn validator_by_public_key(&self, pubkey: B256) -> Result<IValidatorConfigV2::Validator> {
        let idx1 = self.pubkey_to_index[pubkey].read()?;
        if idx1 == 0 {
            return Err(err_validator_not_found());
        }
        self.read_validator_at(idx1 - 1)
    }

    /// Returns all validators ever added.
    pub fn get_validators(&self) -> Result<Vec<IValidatorConfigV2::Validator>> {
        let count = self.validator_count()?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            out.push(self.read_validator_at(i)?);
        }
        Ok(out)
    }

    /// Returns only active validators.
    pub fn get_active_validators(&self) -> Result<Vec<IValidatorConfigV2::Validator>> {
        let count = self.active_indices.len()?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let global_idx1 = self.active_indices[i].read()?;
            out.push(self.read_validator_at(global_idx1 - 1)?);
        }
        Ok(out)
    }

    /// Returns the next network identity rotation epoch.
    pub fn get_next_network_identity_rotation_epoch(&self) -> Result<u64> {
        self.next_network_identity_rotation_epoch.read()
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    fn validate_endpoints(ingress: &str, egress: &str) -> Result<()> {
        ensure_address_is_ip_port(ingress).map_err(|err| {
            err_not_ip_port(ingress.to_string(), err)
        })?;
        ensure_address_is_ip(egress).map_err(|err| {
            err_not_ip(egress.to_string(), err)
        })
    }

    /// Computes the keccak256 hash of the ingress IP:port for uniqueness checking.
    fn ingress_key(ingress: &str) -> Result<B256> {
        let addr = ingress
            .parse::<std::net::SocketAddr>()
            .map_err(|e| err_not_ip_port(ingress.to_string(), e.to_string()))?;

        let mut data = Vec::new();
        match addr {
            std::net::SocketAddr::V4(v4) => {
                data.extend_from_slice(&v4.ip().octets());
                data.extend_from_slice(&v4.port().to_be_bytes());
            }
            std::net::SocketAddr::V6(v6) => {
                data.extend_from_slice(&v6.ip().octets());
                data.extend_from_slice(&v6.scope_id().to_be_bytes());
                data.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
        Ok(keccak256(&data))
    }

    fn require_unique_ingress(&self, ingress: &str) -> Result<B256> {
        let ingress_hash = Self::ingress_key(ingress)?;
        if self.active_ingress_ips[ingress_hash].read()? {
            return Err(err_ingress_already_exists(ingress.to_string()));
        }
        Ok(ingress_hash)
    }

    fn update_ingress_ip_tracking(&mut self, old_ingress: &str, new_ingress: &str) -> Result<()> {
        let old_hash = Self::ingress_key(old_ingress)?;
        let new_hash = Self::ingress_key(new_ingress)?;

        if old_hash != new_hash {
            if self.active_ingress_ips[new_hash].read()? {
                return Err(err_ingress_already_exists(new_ingress.to_string()));
            }
            self.active_ingress_ips[old_hash].delete()?;
            self.active_ingress_ips[new_hash].write(true)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append_validator(
        &mut self,
        addr: Address,
        pubkey: B256,
        ingress: String,
        egress: String,
        fee_recipient: Address,
        added_at_height: u64,
        deactivated_at_height: u64,
    ) -> Result<u64> {
        let count = self.validator_count()?;
        let mut active_idx = 0u64;

        if deactivated_at_height == 0 {
            self.active_indices.push(count + 1)?; // 1-indexed
            active_idx = self.active_indices.len()? as u64; // 1-indexed
        }

        let v = ValidatorRecord {
            public_key: pubkey,
            validator_address: addr,
            ingress,
            egress,
            fee_recipient,
            index: count,
            active_idx,
            added_at_height,
            deactivated_at_height,
        };

        self.validators.push(v)?;
        self.pubkey_to_index[pubkey].write(count + 1)?;
        self.address_to_index[addr].write(count + 1)?;

        Ok(count)
    }

    fn require_new_address(&self, addr: Address) -> Result<()> {
        if addr.is_zero() {
            return Err(err_invalid_validator_address());
        }
        let idx1 = self.address_to_index[addr].read()?;
        if idx1 != 0 {
            let deact = self.validators[(idx1 - 1) as usize].read()?;
            if deact.deactivated_at_height == 0 {
                return Err(err_address_already_has_validator());
            }
        }
        Ok(())
    }

    fn require_new_pubkey(&self, pubkey: B256) -> Result<()> {
        if pubkey.is_zero() {
            return Err(err_invalid_public_key());
        }
        if self.pubkey_to_index[pubkey].read()? != 0 {
            return Err(err_public_key_already_exists());
        }
        Ok(())
    }

    /// Verifies a validator signature for add or rotate operations.
    ///
    /// **STUBBED**: leafage-evm does not include ed25519 verification. The original Tempo
    /// node uses `commonware-cryptography::ed25519` for this. Since leafage is a read-only
    /// node and signature verification only gates mutate operations (which are only exercised
    /// during simulateTransactions), we accept all signatures.
    fn verify_validator_signature(
        &self,
        _pubkey: &B256,
        _signature: &[u8],
        _validator_address: Address,
        _ingress: &str,
        _egress: &str,
        _is_add: bool,
        _fee_recipient: Option<Address>,
    ) -> Result<()> {
        // Stubbed: always succeeds in leafage-evm
        Ok(())
    }

    // =========================================================================
    // Mutating methods
    // =========================================================================

    /// Adds a new validator.
    pub fn add_validator(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::addValidatorCall,
    ) -> Result<u64> {
        self.config.read()?.require_init()?.require_owner(sender)?;
        self.require_new_pubkey(call.publicKey)?;
        self.require_new_address(call.validatorAddress)?;
        Self::validate_endpoints(&call.ingress, &call.egress)?;
        let ingress_hash = self.require_unique_ingress(&call.ingress)?;

        self.verify_validator_signature(
            &call.publicKey,
            &call.signature,
            call.validatorAddress,
            &call.ingress,
            &call.egress,
            true,
            Some(call.feeRecipient),
        )?;

        let block_height = self.storage.block_number();
        self.active_ingress_ips[ingress_hash].write(true)?;

        let index = self.append_validator(
            call.validatorAddress,
            call.publicKey,
            call.ingress.clone(),
            call.egress.clone(),
            call.feeRecipient,
            block_height,
            0,
        )?;

        self.emit_event(IValidatorConfigV2::ValidatorAdded {
            index,
            validatorAddress: call.validatorAddress,
            publicKey: call.publicKey,
            ingress: call.ingress,
            egress: call.egress,
            feeRecipient: call.feeRecipient,
        })?;

        Ok(index)
    }

    /// Deactivates a validator by setting its deactivatedAtHeight.
    pub fn deactivate_validator(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::deactivateValidatorCall,
    ) -> Result<()> {
        let v = self.get_active_validator(call.idx)?;
        self.config
            .read()?
            .require_owner_or_validator(sender, v.validator_address)?;

        self.active_ingress_ips[Self::ingress_key(&v.ingress)?].delete()?;

        let block_height = self.storage.block_number();

        // Write deactivated_at_height on the record
        let mut record = self.validators[call.idx as usize].read()?;
        record.deactivated_at_height = block_height;
        record.active_idx = 0;
        self.validators[call.idx as usize].write(record)?;

        // Swap-and-pop for active_indices
        let active_index = (v.active_idx - 1) as usize;
        let last_pos = self.active_indices.len()? - 1;

        if active_index != last_pos {
            let moved_val = self.active_indices[last_pos].read()?;
            self.active_indices[active_index].write(moved_val)?;
            // Update the moved validator's active_idx backpointer
            let mut moved_record = self.validators[(moved_val - 1) as usize].read()?;
            moved_record.active_idx = (active_index + 1) as u64;
            self.validators[(moved_val - 1) as usize].write(moved_record)?;
        }
        self.active_indices.pop()?;

        self.emit_event(IValidatorConfigV2::ValidatorDeactivated {
            index: call.idx,
            validatorAddress: v.validator_address,
        })
    }

    /// Rotates a validator to a new identity.
    pub fn rotate_validator(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::rotateValidatorCall,
    ) -> Result<()> {
        let v = self.get_active_validator(call.idx)?;
        self.config
            .read()?
            .require_init()?
            .require_owner_or_validator(sender, v.validator_address)?;
        self.require_new_pubkey(call.publicKey)?;
        Self::validate_endpoints(&call.ingress, &call.egress)?;
        self.require_unique_ingress(&call.ingress)?;

        self.verify_validator_signature(
            &call.publicKey,
            &call.signature,
            v.validator_address,
            &call.ingress,
            &call.egress,
            false,
            None,
        )?;

        let block_height = self.storage.block_number();
        self.update_ingress_ip_tracking(&v.ingress, &call.ingress)?;

        // Append deactivated snapshot
        let appended_idx = self.validators.len()? as u64;
        let snapshot = ValidatorRecord {
            public_key: v.public_key,
            validator_address: v.validator_address,
            ingress: v.ingress.clone(),
            egress: v.egress.clone(),
            fee_recipient: v.fee_recipient,
            index: appended_idx,
            active_idx: 0,
            added_at_height: v.added_at_height,
            deactivated_at_height: block_height,
        };
        self.validators.push(snapshot)?;

        // Update pubkey_to_index: old pubkey -> appended_idx + 1
        self.pubkey_to_index[v.public_key].write(appended_idx + 1)?;

        // Modify in-place at the original index
        let mut updated = self.validators[call.idx as usize].read()?;
        updated.public_key = call.publicKey;
        updated.ingress = call.ingress.clone();
        updated.egress = call.egress.clone();
        updated.added_at_height = block_height;
        self.validators[call.idx as usize].write(updated)?;

        // Set pubkey_to_index for new pubkey
        self.pubkey_to_index[call.publicKey].write(call.idx + 1)?;

        self.emit_event(IValidatorConfigV2::ValidatorRotated {
            index: call.idx,
            deactivatedIndex: appended_idx,
            validatorAddress: v.validator_address,
            oldPublicKey: v.public_key,
            newPublicKey: call.publicKey,
            ingress: call.ingress,
            egress: call.egress,
            caller: sender,
        })
    }

    /// Sets the fee recipient for a validator.
    pub fn set_fee_recipient(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::setFeeRecipientCall,
    ) -> Result<()> {
        let v = self.get_active_validator(call.idx)?;
        self.config
            .read()?
            .require_init()?
            .require_owner_or_validator(sender, v.validator_address)?;

        let mut record = self.validators[call.idx as usize].read()?;
        record.fee_recipient = call.feeRecipient;
        self.validators[call.idx as usize].write(record)?;

        self.emit_event(IValidatorConfigV2::FeeRecipientUpdated {
            index: call.idx,
            feeRecipient: call.feeRecipient,
            caller: sender,
        })
    }

    /// Updates a validator's IP addresses.
    pub fn set_ip_addresses(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::setIpAddressesCall,
    ) -> Result<()> {
        let v = self.get_active_validator(call.idx)?;
        self.config
            .read()?
            .require_init()?
            .require_owner_or_validator(sender, v.validator_address)?;

        Self::validate_endpoints(&call.ingress, &call.egress)?;
        self.update_ingress_ip_tracking(&v.ingress, &call.ingress)?;

        let mut record = self.validators[call.idx as usize].read()?;
        record.ingress = call.ingress.clone();
        record.egress = call.egress.clone();
        self.validators[call.idx as usize].write(record)?;

        self.emit_event(IValidatorConfigV2::IpAddressesUpdated {
            index: call.idx,
            ingress: call.ingress,
            egress: call.egress,
            caller: sender,
        })
    }

    /// Transfers a validator entry to a new address.
    pub fn transfer_validator_ownership(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::transferValidatorOwnershipCall,
    ) -> Result<()> {
        let v = self.get_active_validator(call.idx)?;
        self.config
            .read()?
            .require_init()?
            .require_owner_or_validator(sender, v.validator_address)?;
        self.require_new_address(call.newAddress)?;

        let old_address = v.validator_address;
        let mut record = self.validators[call.idx as usize].read()?;
        record.validator_address = call.newAddress;
        self.validators[call.idx as usize].write(record)?;

        self.address_to_index[old_address].delete()?;
        self.address_to_index[call.newAddress].write(call.idx + 1)?;

        self.emit_event(IValidatorConfigV2::ValidatorOwnershipTransferred {
            index: call.idx,
            oldAddress: old_address,
            newAddress: call.newAddress,
            caller: sender,
        })
    }

    /// Transfers contract ownership.
    pub fn transfer_ownership(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::transferOwnershipCall,
    ) -> Result<()> {
        if call.newOwner.is_zero() {
            return Err(err_invalid_owner());
        }
        let mut config = self.config.read()?.require_init()?.require_owner(sender)?;
        let old_owner = config.owner;
        config.owner = call.newOwner;
        self.config.write(config)?;

        self.emit_event(IValidatorConfigV2::OwnershipTransferred {
            oldOwner: old_owner,
            newOwner: call.newOwner,
        })
    }

    /// Sets the network identity rotation epoch.
    pub fn set_network_identity_rotation_epoch(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::setNetworkIdentityRotationEpochCall,
    ) -> Result<()> {
        self.config.read()?.require_init()?.require_owner(sender)?;
        let previous_epoch = self.next_network_identity_rotation_epoch.read()?;
        self.next_network_identity_rotation_epoch.write(call.epoch)?;
        self.emit_event(IValidatorConfigV2::NetworkIdentityRotationEpochSet {
            previousEpoch: previous_epoch,
            nextEpoch: call.epoch,
        })
    }

    // =========================================================================
    // Migration
    // =========================================================================

    fn require_migration_owner(&mut self, caller: Address) -> Result<Config> {
        let mut config = self.config.read()?.require_not_init()?;

        if config.owner.is_zero() {
            let v1 = ValidatorConfig::new();
            config.owner = v1.owner()?;
            let v1_count = v1.validator_count()?;
            if v1_count == 0 {
                return Err(err_empty_v1_validator_set());
            }
            config.v1_validator_count = v1_count as u8;
            self.config.write(Config {
                owner: config.owner,
                is_init: false,
                init_at_height: 0,
                migration_skipped_count: 0,
                v1_validator_count: config.v1_validator_count,
            })?;
        }

        config.require_owner(caller)
    }

    /// Migrates a single validator from V1 to V2.
    pub fn migrate_validator(
        &mut self,
        sender: Address,
        call: IValidatorConfigV2::migrateValidatorCall,
    ) -> Result<()> {
        let config = self.require_migration_owner(sender)?;
        let block_height = self.storage.block_number();

        let v1 = ValidatorConfig::new();
        let v1_count = u64::from(config.v1_validator_count);
        let migrated = self.validator_count()?;
        let skipped = config.migration_skipped_count;

        let total_processed = migrated + u64::from(skipped);
        if total_processed >= v1_count || call.idx != v1_count - total_processed - 1 {
            return Err(err_invalid_migration_index());
        }

        let v1_val = v1.validators(v1.validators_array(call.idx)?)?;

        // Closure-like skip helper
        let skip = |s: &mut Self| -> Result<()> {
            s.emit_event(IValidatorConfigV2::SkippedValidatorMigration {
                index: call.idx,
                validatorAddress: v1_val.validatorAddress,
                publicKey: v1_val.publicKey,
            })?;
            let mut cfg: Config = s.config.read()?;
            cfg.migration_skipped_count = skipped.saturating_add(1);
            s.config.write(cfg)
        };

        // Skip if public key is zero (invalid)
        if v1_val.publicKey.is_zero() {
            return skip(self);
        }

        // Skip if egress decoding fails (convert outboundAddress to bare IP)
        let egress = match v1_val.outboundAddress.parse::<std::net::SocketAddr>() {
            Ok(sa) => sa.ip().to_string(),
            Err(_) => return skip(self),
        };

        // Skip if public key is a duplicate
        if self.pubkey_to_index[v1_val.publicKey].read()? != 0 {
            return skip(self);
        }

        // Skip if address is a duplicate of an active validator
        let addr_idx = self.address_to_index[v1_val.validatorAddress].read()?;
        if addr_idx != 0 {
            let deact = self.validators[(addr_idx - 1) as usize].read()?;
            if deact.deactivated_at_height == 0 {
                return Err(err_address_already_has_validator());
            }
        }

        let now_active = v1_val.active;
        let ingress_hash = Self::ingress_key(&v1_val.inboundAddress)?;

        // Skip if ingress IP is a duplicate for active validators
        if now_active && self.active_ingress_ips[ingress_hash].read()? {
            return skip(self);
        }

        let migrated_idx = self.append_validator(
            v1_val.validatorAddress,
            v1_val.publicKey,
            v1_val.inboundAddress,
            egress,
            Address::ZERO,
            block_height,
            if now_active { 0 } else { block_height },
        )?;

        if now_active {
            self.active_ingress_ips[ingress_hash].write(true)?;
        }

        self.emit_event(IValidatorConfigV2::ValidatorMigrated {
            index: migrated_idx,
            validatorAddress: v1_val.validatorAddress,
            publicKey: v1_val.publicKey,
        })
    }

    /// Finalizes V1 -> V2 migration.
    pub fn initialize_if_migrated(&mut self, sender: Address) -> Result<()> {
        let mut config = self.require_migration_owner(sender)?;

        if config.v1_validator_count == 0
            || self.validator_count()? + u64::from(config.migration_skipped_count)
                < u64::from(config.v1_validator_count)
        {
            return Err(err_migration_not_complete());
        }

        let v1 = ValidatorConfig::new();
        let v1_next_dkg = v1.get_next_full_dkg_ceremony()?;
        self.next_network_identity_rotation_epoch.write(v1_next_dkg)?;

        let height = self.storage.block_number();
        config.init_at_height = height;
        config.is_init = true;
        self.config.write(config)?;

        self.emit_event(IValidatorConfigV2::Initialized { height })
    }
}

impl ContractStorage for ValidatorConfigV2 {
    #[inline]
    fn address(&self) -> Address {
        self.address
    }

    #[inline]
    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

// ===========================================================================
// Dispatch
// ===========================================================================

fn dispatch_call<T>(
    calldata: &[u8],
    decode: impl FnOnce(&[u8]) -> core::result::Result<T, alloy::sol_types::Error>,
    f: impl FnOnce(T) -> PrecompileResult,
) -> PrecompileResult {
    let storage = StorageCtx::default();

    if calldata.len() < 4 {
        return Ok(fill_precompile_output(
            PrecompileOutput::new_reverted(0, Bytes::new()),
            &storage,
        ));
    }

    let result = decode(calldata);

    match result {
        Ok(call) => f(call).map(|res| fill_precompile_output(res, &storage)),
        Err(alloy::sol_types::Error::UnknownSelector { selector, .. }) => {
            unknown_selector(*selector, storage.gas_used())
                .map(|res| fill_precompile_output(res, &storage))
        }
        Err(_) => Ok(fill_precompile_output(
            PrecompileOutput::new_reverted(0, Bytes::new()),
            &storage,
        )),
    }
}

fn unknown_selector(selector: [u8; 4], gas: u64) -> PrecompileResult {
    TempoPrecompileError::UnknownFunctionSelector(selector).into_precompile_result(gas)
}

impl Precompile for ValidatorConfigV2 {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            IValidatorConfigV2::IValidatorConfigV2Calls::abi_decode,
            |call| match call {
                // View functions
                IValidatorConfigV2::IValidatorConfigV2Calls::owner(call) => {
                    view(call, |_| self.owner())
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::getActiveValidators(call) => {
                    view(call, |_| self.get_active_validators())
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::getInitializedAtHeight(call) => {
                    view(call, |_| self.get_initialized_at_height())
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::validatorCount(call) => {
                    view(call, |_| self.validator_count())
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::validatorByIndex(call) => {
                    view(call, |c| self.validator_by_index(c.index))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::validatorByAddress(call) => {
                    view(call, |c| self.validator_by_address(c.validatorAddress))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::validatorByPublicKey(call) => {
                    view(call, |c| self.validator_by_public_key(c.publicKey))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::getNextNetworkIdentityRotationEpoch(call) => {
                    view(call, |_| self.get_next_network_identity_rotation_epoch())
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::isInitialized(call) => {
                    view(call, |_| self.is_initialized())
                }

                // Mutate functions
                IValidatorConfigV2::IValidatorConfigV2Calls::addValidator(call) => {
                    mutate(call, msg_sender, |s, c| self.add_validator(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::deactivateValidator(call) => {
                    mutate_void(call, msg_sender, |s, c| self.deactivate_validator(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::rotateValidator(call) => {
                    mutate_void(call, msg_sender, |s, c| self.rotate_validator(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::setFeeRecipient(call) => {
                    mutate_void(call, msg_sender, |s, c| self.set_fee_recipient(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::setIpAddresses(call) => {
                    mutate_void(call, msg_sender, |s, c| self.set_ip_addresses(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::transferValidatorOwnership(call) => {
                    mutate_void(call, msg_sender, |s, c| {
                        self.transfer_validator_ownership(s, c)
                    })
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::transferOwnership(call) => {
                    mutate_void(call, msg_sender, |s, c| self.transfer_ownership(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::setNetworkIdentityRotationEpoch(call) => {
                    mutate_void(call, msg_sender, |s, c| {
                        self.set_network_identity_rotation_epoch(s, c)
                    })
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::migrateValidator(call) => {
                    mutate_void(call, msg_sender, |s, c| self.migrate_validator(s, c))
                }
                IValidatorConfigV2::IValidatorConfigV2Calls::initializeIfMigrated(call) => {
                    mutate_void(call, msg_sender, |s, _| self.initialize_if_migrated(s))
                }
            },
        )
    }
}
