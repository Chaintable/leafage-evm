//! Validator Config (V1) precompile -- manages the on-chain consensus validator set.
//!
//! Ported from `tempo/crates/precompiles/src/validator_config/`.
//!
//! ## Storage layout
//!
//! | Slot | Field             | Type                           |
//! |------|-------------------|--------------------------------|
//! |  0   | owner             | Address                        |
//! |  1   | validators_array  | Vec<Address>                   |
//! |  2   | validators        | Mapping<Address, Validator>    |
//! |  3   | next_dkg_ceremony | u64                            |

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::storage::StorageOps;
use super::storage_types::{
    Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType, VecHandler,
};
use super::{dispatch_call,
    fill_precompile_output, input_cost, mutate_void, view, Precompile, VALIDATOR_CONFIG_ADDRESS,
};

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    interface IValidatorConfig {
        function owner() external view returns (address);
        function getValidators() external view returns (Validator[] memory);
        function getNextFullDkgCeremony() external view returns (uint64);
        function validatorsArray(uint256 index) external view returns (address);
        function validators(address validator) external view returns (Validator memory);
        function validatorCount() external view returns (uint64);

        function addValidator(
            address newValidatorAddress,
            bytes32 publicKey,
            bool active,
            string memory inboundAddress,
            string memory outboundAddress
        ) external;
        function updateValidator(
            address newValidatorAddress,
            bytes32 publicKey,
            string memory inboundAddress,
            string memory outboundAddress
        ) external;
        function changeValidatorStatus(address validator, bool active) external;
        function changeValidatorStatusByIndex(uint256 index, bool active) external;
        function changeOwner(address newOwner) external;
        function setNextFullDkgCeremony(uint64 epoch) external;

        struct Validator {
            bytes32 publicKey;
            bool active;
            uint64 index;
            address validatorAddress;
            string inboundAddress;
            string outboundAddress;
        }

        error Unauthorized();
        error ValidatorAlreadyExists();
        error ValidatorNotFound();
        error InvalidPublicKey();
        error NotHostPort(string field, string value, string reason);
        error NotIpPort(string field, string value, string reason);
    }
}

// ===========================================================================
// IP validation (inlined from tempo/ip_validation.rs)
// ===========================================================================

/// Validates that `input` is of the form `<ip>:<port>`.
fn ensure_address_is_ip_port(input: &str) -> std::result::Result<(), String> {
    input
        .parse::<std::net::SocketAddr>()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ===========================================================================
// Validator storage type
// ===========================================================================

/// On-chain record for a single consensus validator.
///
/// Storage layout (4 slots total, matching `#[derive(Storable)]`):
///   - slot+0: public_key (B256, 32 bytes)
///   - slot+1: active (bool, offset 0) + index (u64, offset 1) + validator_address (Address, offset 9) -- packed
///   - slot+2: inbound_address (String, dynamic)
///   - slot+3: outbound_address (String, dynamic)
///
/// NOTE: The Tempo `#[derive(Storable)]` packs bool(1) + u64(8) + Address(20) = 29 bytes into one slot.
/// We reproduce the same layout.
#[derive(Debug, Clone)]
pub(crate) struct Validator {
    public_key: B256,
    active: bool,
    index: u64,
    validator_address: Address,
    inbound_address: String,
    outbound_address: String,
}

impl Default for Validator {
    fn default() -> Self {
        Self {
            public_key: B256::ZERO,
            active: false,
            index: 0,
            validator_address: Address::ZERO,
            inbound_address: String::new(),
            outbound_address: String::new(),
        }
    }
}

impl StorableType for Validator {
    // B256 (1 slot) + packed(bool+u64+Address) (1 slot) + String (1 slot) + String (1 slot) = 4 slots
    const LAYOUT: Layout = Layout::Slots(4);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for Validator {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        // Slot+0: public_key (B256)
        let word0 = storage.load(slot)?;
        let public_key = B256::from(word0.to_be_bytes::<32>());

        // Slot+1: packed (active: bool @ offset 0, index: u64 @ offset 1, validator_address: Address @ offset 9)
        // In Solidity packed storage, smaller types are packed right-to-left in a slot.
        // The Tempo #[derive(Storable)] packs sequentially from byte offset 0:
        //   active: 1 byte at offset 0 (byte 31 in big-endian word)
        //   index: 8 bytes at offset 1 (bytes 23-30)
        //   validator_address: 20 bytes at offset 9 (bytes 3-22)
        let word1 = storage.load(slot + U256::from(1))?;
        let bytes1 = word1.to_be_bytes::<32>();
        // active at rightmost byte (offset 0 in packed = byte 31)
        let active = bytes1[31] != 0;
        // index at offset 1 (bytes 23..31 in big-endian)
        let index = u64::from_be_bytes(bytes1[23..31].try_into().unwrap());
        // validator_address at offset 9 (bytes 3..23 in big-endian)
        let validator_address = Address::from_slice(&bytes1[3..23]);

        // Slot+2: inbound_address (String)
        let inbound_address = String::load(storage, slot + U256::from(2), LayoutCtx::FULL)?;

        // Slot+3: outbound_address (String)
        let outbound_address = String::load(storage, slot + U256::from(3), LayoutCtx::FULL)?;

        Ok(Self {
            public_key,
            active,
            index,
            validator_address,
            inbound_address,
            outbound_address,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        // Slot+0: public_key
        storage.store(slot, U256::from_be_bytes(self.public_key.0))?;

        // Slot+1: packed
        let mut bytes1 = [0u8; 32];
        // active at byte 31
        bytes1[31] = if self.active { 1 } else { 0 };
        // index at bytes 23..31
        bytes1[23..31].copy_from_slice(&self.index.to_be_bytes());
        // validator_address at bytes 3..23
        bytes1[3..23].copy_from_slice(self.validator_address.as_slice());
        storage.store(slot + U256::from(1), U256::from_be_bytes(bytes1))?;

        // Slot+2: inbound_address
        self.inbound_address
            .store(storage, slot + U256::from(2), LayoutCtx::FULL)?;

        // Slot+3: outbound_address
        self.outbound_address
            .store(storage, slot + U256::from(3), LayoutCtx::FULL)?;

        Ok(())
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)?;
        storage.store(slot + U256::from(1), U256::ZERO)?;
        String::delete(storage, slot + U256::from(2), LayoutCtx::FULL)?;
        String::delete(storage, slot + U256::from(3), LayoutCtx::FULL)?;
        Ok(())
    }
}

// ===========================================================================
// ValidatorConfig struct (manual macro expansion)
// ===========================================================================

/// Validator Config precompile for managing consensus validators.
pub struct ValidatorConfig {
    // Slot 0: owner
    pub owner: Slot<Address>,
    // Slot 1: validators_array (Vec<Address>)
    pub validators_array: VecHandler<Address>,
    // Slot 2: validators (Mapping<Address, Validator>)
    pub(crate) validators: Mapping<Address, Validator>,
    // Slot 3: next_dkg_ceremony
    pub next_dkg_ceremony: Slot<u64>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl ValidatorConfig {
    pub fn new() -> Self {
        let address = VALIDATOR_CONFIG_ADDRESS;
        Self {
            owner: Slot::new(U256::from(0), address),
            validators_array: VecHandler::new(U256::from(1), address),
            validators: Mapping::new(U256::from(2), address),
            next_dkg_ceremony: Slot::new(U256::from(3), address),
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

    /// Initializes the validator config precompile with an owner.
    pub fn initialize(&mut self, owner: Address) -> Result<()> {
        self.__initialize()?;
        self.owner.write(owner)
    }

    /// Returns the current contract owner address.
    pub fn owner(&self) -> Result<Address> {
        self.owner.read()
    }

    /// Returns `Ok(())` if `caller` is the owner, otherwise reverts.
    pub fn check_owner(&self, caller: Address) -> Result<()> {
        if self.owner()? != caller {
            return Err(TempoPrecompileError::Revert(
                IValidatorConfig::Unauthorized {}.abi_encode().into(),
            ));
        }
        Ok(())
    }

    /// Transfers contract ownership.
    pub fn change_owner(
        &mut self,
        sender: Address,
        call: IValidatorConfig::changeOwnerCall,
    ) -> Result<()> {
        self.check_owner(sender)?;
        self.owner.write(call.newOwner)
    }

    /// Returns the total number of registered validators.
    pub fn validator_count(&self) -> Result<u64> {
        self.validators_array.len().map(|c| c as u64)
    }

    /// Returns the validator address stored at `index` in the ordered array.
    pub fn validators_array(&self, index: u64) -> Result<Address> {
        match self.validators_array.at(index as usize)? {
            Some(elem) => elem.read(),
            None => Err(TempoPrecompileError::Revert(
                // Encode as Panic(0x32) for array out of bounds
                Bytes::from_static(&[
                    0x4e, 0x48, 0x7b, 0x71, // Panic selector
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32,
                ]),
            )),
        }
    }

    /// Returns the full Validator record for the given address.
    pub fn validators(
        &self,
        validator: Address,
    ) -> Result<IValidatorConfig::Validator> {
        let v = self.validators[validator].read()?;
        Ok(IValidatorConfig::Validator {
            publicKey: v.public_key,
            active: v.active,
            index: v.index,
            validatorAddress: v.validator_address,
            inboundAddress: v.inbound_address,
            outboundAddress: v.outbound_address,
        })
    }

    /// Check if a validator exists by checking if their publicKey is non-zero.
    fn validator_exists(&self, validator: Address) -> Result<bool> {
        let v = self.validators[validator].read()?;
        Ok(!v.public_key.is_zero())
    }

    /// Returns all registered validators in index order.
    pub fn get_validators(&self) -> Result<Vec<IValidatorConfig::Validator>> {
        let count = self.validators_array.len()?;
        let mut validators = Vec::with_capacity(count);

        for i in 0..count {
            let validator_address = self.validators_array[i].read()?;
            let v = self.validators[validator_address].read()?;

            validators.push(IValidatorConfig::Validator {
                publicKey: v.public_key,
                active: v.active,
                index: v.index,
                validatorAddress: validator_address,
                inboundAddress: v.inbound_address,
                outboundAddress: v.outbound_address,
            });
        }

        Ok(validators)
    }

    /// Registers a new validator. Owner-only.
    pub fn add_validator(
        &mut self,
        sender: Address,
        call: IValidatorConfig::addValidatorCall,
    ) -> Result<()> {
        if call.publicKey.is_zero() {
            return Err(TempoPrecompileError::Revert(
                IValidatorConfig::InvalidPublicKey {}.abi_encode().into(),
            ));
        }

        self.check_owner(sender)?;

        if self.validator_exists(call.newValidatorAddress)? {
            return Err(TempoPrecompileError::Revert(
                IValidatorConfig::ValidatorAlreadyExists {}
                    .abi_encode()
                    .into(),
            ));
        }

        // Validate addresses (leafage always runs latest spec, use Display formatting)
        ensure_address_is_ip_port(&call.inboundAddress).map_err(|err| {
            TempoPrecompileError::Revert(
                IValidatorConfig::NotHostPort {
                    field: "inboundAddress".to_string(),
                    value: call.inboundAddress.clone(),
                    reason: err,
                }
                .abi_encode()
                .into(),
            )
        })?;
        ensure_address_is_ip_port(&call.outboundAddress).map_err(|err| {
            TempoPrecompileError::Revert(
                IValidatorConfig::NotIpPort {
                    field: "outboundAddress".to_string(),
                    value: call.outboundAddress.clone(),
                    reason: err,
                }
                .abi_encode()
                .into(),
            )
        })?;

        let count = self.validator_count()?;
        let validator = Validator {
            public_key: call.publicKey,
            active: call.active,
            index: count,
            validator_address: call.newValidatorAddress,
            inbound_address: call.inboundAddress,
            outbound_address: call.outboundAddress,
        };
        self.validators[call.newValidatorAddress].write(validator)?;

        self.validators_array.push(call.newValidatorAddress)
    }

    /// Updates validator information and optionally rotates to a new address.
    pub fn update_validator(
        &mut self,
        sender: Address,
        call: IValidatorConfig::updateValidatorCall,
    ) -> Result<()> {
        if call.publicKey.is_zero() {
            return Err(TempoPrecompileError::Revert(
                IValidatorConfig::InvalidPublicKey {}.abi_encode().into(),
            ));
        }

        if !self.validator_exists(sender)? {
            return Err(TempoPrecompileError::Revert(
                IValidatorConfig::ValidatorNotFound {}.abi_encode().into(),
            ));
        }

        let old_validator = self.validators[sender].read()?;

        if call.newValidatorAddress != sender {
            if self.validator_exists(call.newValidatorAddress)? {
                return Err(TempoPrecompileError::Revert(
                    IValidatorConfig::ValidatorAlreadyExists {}
                        .abi_encode()
                        .into(),
                ));
            }

            self.validators_array[old_validator.index as usize]
                .write(call.newValidatorAddress)?;
            self.validators[sender].delete()?;
        }

        ensure_address_is_ip_port(&call.inboundAddress).map_err(|err| {
            TempoPrecompileError::Revert(
                IValidatorConfig::NotHostPort {
                    field: "inboundAddress".to_string(),
                    value: call.inboundAddress.clone(),
                    reason: err,
                }
                .abi_encode()
                .into(),
            )
        })?;
        ensure_address_is_ip_port(&call.outboundAddress).map_err(|err| {
            TempoPrecompileError::Revert(
                IValidatorConfig::NotIpPort {
                    field: "outboundAddress".to_string(),
                    value: call.outboundAddress.clone(),
                    reason: err,
                }
                .abi_encode()
                .into(),
            )
        })?;

        let updated_validator = Validator {
            public_key: call.publicKey,
            active: old_validator.active,
            index: old_validator.index,
            validator_address: call.newValidatorAddress,
            inbound_address: call.inboundAddress,
            outbound_address: call.outboundAddress,
        };

        self.validators[call.newValidatorAddress].write(updated_validator)
    }

    /// Sets a validator's active flag by address. Owner-only.
    pub fn change_validator_status(
        &mut self,
        sender: Address,
        call: IValidatorConfig::changeValidatorStatusCall,
    ) -> Result<()> {
        self.check_owner(sender)?;

        if !self.validator_exists(call.validator)? {
            return Err(TempoPrecompileError::Revert(
                IValidatorConfig::ValidatorNotFound {}.abi_encode().into(),
            ));
        }

        let mut validator = self.validators[call.validator].read()?;
        validator.active = call.active;
        self.validators[call.validator].write(validator)
    }

    /// Sets a validator's active flag by array index. Owner-only, T1+.
    pub fn change_validator_status_by_index(
        &mut self,
        sender: Address,
        call: IValidatorConfig::changeValidatorStatusByIndexCall,
    ) -> Result<()> {
        self.check_owner(sender)?;

        let index: usize = call.index.try_into().map_err(|_| {
            TempoPrecompileError::Revert(
                IValidatorConfig::ValidatorNotFound {}.abi_encode().into(),
            )
        })?;
        let validator_address = match self.validators_array.at(index)? {
            Some(elem) => elem.read()?,
            None => {
                return Err(TempoPrecompileError::Revert(
                    IValidatorConfig::ValidatorNotFound {}.abi_encode().into(),
                ))
            }
        };

        let mut validator = self.validators[validator_address].read()?;
        validator.active = call.active;
        self.validators[validator_address].write(validator)
    }

    /// Returns the epoch at which a fresh DKG ceremony will be triggered.
    pub fn get_next_full_dkg_ceremony(&self) -> Result<u64> {
        self.next_dkg_ceremony.read()
    }

    /// Sets the epoch at which a fresh DKG ceremony will be triggered. Owner-only.
    pub fn set_next_full_dkg_ceremony(
        &mut self,
        sender: Address,
        call: IValidatorConfig::setNextFullDkgCeremonyCall,
    ) -> Result<()> {
        self.check_owner(sender)?;
        self.next_dkg_ceremony.write(call.epoch)
    }
}

impl ContractStorage for ValidatorConfig {
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

/// Dispatches calldata, handling selector validation and ABI decode errors.

impl Precompile for ValidatorConfig {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            IValidatorConfig::IValidatorConfigCalls::abi_decode,
            |call| match call {
                // View functions
                IValidatorConfig::IValidatorConfigCalls::owner(call) => {
                    view(call, |_| self.owner())
                }
                IValidatorConfig::IValidatorConfigCalls::getValidators(call) => {
                    view(call, |_| self.get_validators())
                }
                IValidatorConfig::IValidatorConfigCalls::getNextFullDkgCeremony(call) => {
                    view(call, |_| self.get_next_full_dkg_ceremony())
                }
                IValidatorConfig::IValidatorConfigCalls::validatorsArray(call) => {
                    view(call, |c| {
                        let index = u64::try_from(c.index).map_err(|_| {
                            TempoPrecompileError::Revert(
                                Bytes::from_static(&[
                                    0x4e, 0x48, 0x7b, 0x71, // Panic selector
                                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32,
                                ]),
                            )
                        })?;
                        self.validators_array(index)
                    })
                }
                IValidatorConfig::IValidatorConfigCalls::validators(call) => {
                    view(call, |c| self.validators(c.validator))
                }
                IValidatorConfig::IValidatorConfigCalls::validatorCount(call) => {
                    view(call, |_| self.validator_count())
                }

                // Mutate functions
                IValidatorConfig::IValidatorConfigCalls::addValidator(call) => {
                    mutate_void(call, msg_sender, |s, c| self.add_validator(s, c))
                }
                IValidatorConfig::IValidatorConfigCalls::updateValidator(call) => {
                    mutate_void(call, msg_sender, |s, c| self.update_validator(s, c))
                }
                IValidatorConfig::IValidatorConfigCalls::changeValidatorStatus(call) => {
                    mutate_void(call, msg_sender, |s, c| {
                        self.change_validator_status(s, c)
                    })
                }
                IValidatorConfig::IValidatorConfigCalls::changeValidatorStatusByIndex(call) => {
                    // Leafage always runs latest spec, so this is always available
                    mutate_void(call, msg_sender, |s, c| {
                        self.change_validator_status_by_index(s, c)
                    })
                }
                IValidatorConfig::IValidatorConfigCalls::changeOwner(call) => {
                    mutate_void(call, msg_sender, |s, c| self.change_owner(s, c))
                }
                IValidatorConfig::IValidatorConfigCalls::setNextFullDkgCeremony(call) => {
                    mutate_void(call, msg_sender, |s, c| {
                        self.set_next_full_dkg_ceremony(s, c)
                    })
                }
            },
        )
    }
}
