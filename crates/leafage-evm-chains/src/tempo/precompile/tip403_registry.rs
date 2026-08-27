//! TIP-403 transfer policy registry precompile.
//!
//! Manages whitelist, blacklist, and compound transfer policies that TIP-20
//! tokens reference to gate sender/recipient authorization.
//!
//! Ported from `tempo/crates/precompiles/src/tip403_registry/`.
//!
//! ## Storage layout
//!
//! | Slot | Field             | Type                                        |
//! |------|-------------------|---------------------------------------------|
//! |  0   | policy_id_counter | u64                                         |
//! |  1   | policy_records    | Mapping<u64, PolicyRecord>                  |
//! |  2   | policy_set        | Mapping<u64, Mapping<Address, bool>>        |
//! |  3   | receive_policies  | Mapping<Address, ReceivePolicy> (T6+)      |
//! |  4   | token_transfer_policies | Mapping<Address, TokenTransferPolicy> (T9+) |

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::super::address::TempoAddressExt;
use super::error::{Result, TempoPrecompileError};
use super::storage::StorageOps;
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType};
use super::tip20::TIP20Token;
use super::tip20_factory::TIP20Factory;
use super::{
    ACCOUNT_KEYCHAIN_ADDRESS, ADDRESS_REGISTRY_ADDRESS, CURRENT_COMMITTEE_ADDRESS,
    NONCE_PRECOMPILE_ADDRESS, Precompile, RECEIVE_POLICY_GUARD_ADDRESS, SIGNATURE_VERIFIER_ADDRESS,
    STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS, TIP20_CHANNEL_RESERVE_ADDRESS,
    TIP20_FACTORY_ADDRESS, TIP403_REGISTRY_ADDRESS, VALIDATOR_CONFIG_ADDRESS,
    VALIDATOR_CONFIG_V2_ADDRESS, dispatch_call, input_cost, mutate, mutate_void, unknown_selector,
    view,
};

// ===========================================================================
// Constants
// ===========================================================================

/// Built-in policy ID that always rejects authorization.
pub const REJECT_ALL_POLICY_ID: u64 = 0;

/// Built-in policy ID that always allows authorization.
pub const ALLOW_ALL_POLICY_ID: u64 = 1;

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ITIP403Registry {
        enum PolicyType {
            WHITELIST,
            BLACKLIST,
            COMPOUND,
        }

        enum BlockedReason {
            NONE,
            TOKEN_FILTER,
            RECEIVE_POLICY,
        }

        function policyIdCounter() external view returns (uint64);
        function policyExists(uint64 policyId) external view returns (bool);
        function policyData(uint64 policyId) external view returns (PolicyType policyType, address admin);
        function isAuthorized(uint64 policyId, address user) external view returns (bool);
        function isAuthorizedSender(uint64 policyId, address user) external view returns (bool);
        function isAuthorizedRecipient(uint64 policyId, address user) external view returns (bool);
        function isAuthorizedMintRecipient(uint64 policyId, address user) external view returns (bool);
        function compoundPolicyData(uint64 policyId) external view returns (uint64 senderPolicyId, uint64 recipientPolicyId, uint64 mintRecipientPolicyId);
        function receivePolicy(address account) external view returns (bool hasReceivePolicy, uint64 senderPolicyId, PolicyType senderPolicyType, uint64 tokenFilterId, PolicyType tokenFilterType, address recoveryAuthority);
        function validateReceivePolicy(address token, address sender, address receiver) external view returns (bool authorized, BlockedReason blockedReason);
        function tokenTransferPolicyId(address token) external view returns (bool isSet, uint64 policyId);

        function createPolicy(address admin, PolicyType policyType) external returns (uint64);
        function createPolicyWithAccounts(address admin, PolicyType policyType, address[] calldata accounts) external returns (uint64);
        function setPolicyAdmin(uint64 policyId, address admin) external;
        function modifyPolicyWhitelist(uint64 policyId, address account, bool allowed) external;
        function modifyPolicyBlacklist(uint64 policyId, address account, bool restricted) external;
        function createCompoundPolicy(uint64 senderPolicyId, uint64 recipientPolicyId, uint64 mintRecipientPolicyId) external returns (uint64);
        function setReceivePolicy(uint64 senderPolicyId, uint64 tokenFilterId, address recoveryAuthority) external;
        function migrateTransferPolicyIds(address[] calldata tokens) external returns (uint256 migrated);

        event PolicyAdminUpdated(uint64 indexed policyId, address indexed updater, address indexed admin);
        event PolicyCreated(uint64 indexed policyId, address indexed updater, PolicyType policyType);
        event WhitelistUpdated(uint64 indexed policyId, address indexed updater, address indexed account, bool allowed);
        event BlacklistUpdated(uint64 indexed policyId, address indexed updater, address indexed account, bool restricted);
        event CompoundPolicyCreated(uint64 indexed policyId, address indexed creator, uint64 senderPolicyId, uint64 recipientPolicyId, uint64 mintRecipientPolicyId);
        event ReceivePolicyUpdated(address indexed account, uint64 senderPolicyId, uint64 tokenFilterId, address recoveryAuthority);

        error Unauthorized();
        error PolicyNotFound();
        error PolicyNotSimple();
        error InvalidPolicyType();
        error IncompatiblePolicyType();
        error VirtualAddressNotAllowed();
        error InvalidReceivePolicyType();
        error InvalidRecoveryAuthority();
    }
}

// ===========================================================================
// Error helpers
// ===========================================================================

fn err_unauthorized() -> TempoPrecompileError {
    TempoPrecompileError::Revert(ITIP403Registry::Unauthorized {}.abi_encode().into())
}

fn err_policy_not_found() -> TempoPrecompileError {
    TempoPrecompileError::Revert(ITIP403Registry::PolicyNotFound {}.abi_encode().into())
}

fn err_policy_not_simple() -> TempoPrecompileError {
    TempoPrecompileError::Revert(ITIP403Registry::PolicyNotSimple {}.abi_encode().into())
}

fn err_invalid_policy_type() -> TempoPrecompileError {
    TempoPrecompileError::Revert(ITIP403Registry::InvalidPolicyType {}.abi_encode().into())
}

fn err_incompatible_policy_type() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        ITIP403Registry::IncompatiblePolicyType {}
            .abi_encode()
            .into(),
    )
}

fn err_virtual_address_not_allowed() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        ITIP403Registry::VirtualAddressNotAllowed {}
            .abi_encode()
            .into(),
    )
}

fn err_invalid_receive_policy_type() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        ITIP403Registry::InvalidReceivePolicyType {}
            .abi_encode()
            .into(),
    )
}

fn err_invalid_recovery_authority() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        ITIP403Registry::InvalidRecoveryAuthority {}
            .abi_encode()
            .into(),
    )
}

// ===========================================================================
// Authorization role
// ===========================================================================

/// Authorization role for policy checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRole {
    /// Check both sender AND recipient (symmetric).
    Transfer,
    /// Check sender authorization only (T2+).
    Sender,
    /// Check recipient authorization only (T2+).
    Recipient,
    /// Check mint recipient authorization only (T2+).
    MintRecipient,
}

impl AuthRole {
    #[inline]
    fn transfer_or(t2_variant: Self) -> Self {
        // leafage always runs latest spec (T2+), so always return the T2 variant
        if StorageCtx::default().spec().is_t2() {
            t2_variant
        } else {
            Self::Transfer
        }
    }

    /// Hardfork-aware: always returns `Transfer`.
    pub fn transfer() -> Self {
        Self::Transfer
    }

    /// Hardfork-aware: returns `Sender` for T2+, `Transfer` for pre-T2.
    pub fn sender() -> Self {
        Self::transfer_or(Self::Sender)
    }

    /// Hardfork-aware: returns `Recipient` for T2+, `Transfer` for pre-T2.
    pub fn recipient() -> Self {
        Self::transfer_or(Self::Recipient)
    }

    /// Hardfork-aware: returns `MintRecipient` for T2+, `Transfer` for pre-T2.
    pub fn mint_recipient() -> Self {
        Self::transfer_or(Self::MintRecipient)
    }
}

// ===========================================================================
// PolicyData storage type
// ===========================================================================

/// Base policy metadata. Packed into a single storage slot.
#[derive(Debug, Clone)]
pub struct PolicyData {
    /// Discriminant of the PolicyType enum (u8).
    pub policy_type: u8,
    /// Address authorized to modify this policy.
    pub admin: Address,
}

impl Default for PolicyData {
    fn default() -> Self {
        Self {
            policy_type: 0,
            admin: Address::ZERO,
        }
    }
}

impl StorableType for PolicyData {
    // u8(1) + Address(20) = 21 bytes, fits in one slot
    const LAYOUT: Layout = Layout::Bytes(21);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for PolicyData {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let word = storage.load(slot)?;
        let bytes = word.to_be_bytes::<32>();
        // Packed right-aligned:
        //   byte 31: policy_type (u8, offset 0)
        //   bytes 11..31: admin (Address, offset 1)
        let policy_type = bytes[31];
        let admin = Address::from_slice(&bytes[11..31]);

        Ok(Self { policy_type, admin })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[31] = self.policy_type;
        bytes[11..31].copy_from_slice(self.admin.as_slice());
        storage.store(slot, U256::from_be_bytes(bytes))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)
    }
}

impl PolicyData {
    /// Decodes the raw `policy_type` u8 to a `PolicyType` enum.
    ///
    /// Pre-T2: COMPOUND (2) is rejected (it did not exist yet); unknown values
    ///         produce `UnderOverflow` to match the original writer panic behavior.
    /// T2+: all three known variants are accepted; unknown values produce
    ///       `InvalidPolicyType`.
    fn policy_type(&self) -> Result<ITIP403Registry::PolicyType> {
        let is_t2 = StorageCtx::default().spec().is_t2();

        // try_into uses the sol!-generated TryFrom<u8> impl
        let ty: core::result::Result<ITIP403Registry::PolicyType, _> = self.policy_type.try_into();

        match ty {
            Ok(t) if is_t2 || t != ITIP403Registry::PolicyType::COMPOUND => Ok(t),
            _ => Err(if is_t2 {
                err_invalid_policy_type()
            } else {
                TempoPrecompileError::under_overflow()
            }),
        }
    }

    /// Returns `true` if the policy type is simple (WHITELIST or BLACKLIST).
    pub fn is_simple(&self) -> bool {
        self.policy_type == ITIP403Registry::PolicyType::WHITELIST as u8
            || self.policy_type == ITIP403Registry::PolicyType::BLACKLIST as u8
    }

    /// Returns `true` if the policy type is compound.
    pub fn is_compound(&self) -> bool {
        self.policy_type == ITIP403Registry::PolicyType::COMPOUND as u8
    }

    /// Returns `true` if the policy data is the default (uninitialized) value.
    fn is_default(&self) -> bool {
        self.policy_type == 0 && self.admin == Address::ZERO
    }
}

// ===========================================================================
// CompoundPolicyData storage type
// ===========================================================================

/// Data for compound policies (TIP-1015).
#[derive(Debug, Clone, Default)]
pub struct CompoundPolicyData {
    pub sender_policy_id: u64,
    pub recipient_policy_id: u64,
    pub mint_recipient_policy_id: u64,
}

impl StorableType for CompoundPolicyData {
    // 3 x u64 = 24 bytes, fits in one slot
    const LAYOUT: Layout = Layout::Bytes(24);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for CompoundPolicyData {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let word = storage.load(slot)?;
        let bytes = word.to_be_bytes::<32>();
        // Packed right-aligned:
        //   bytes 24..32: sender_policy_id (u64, offset 0)
        //   bytes 16..24: recipient_policy_id (u64, offset 8)
        //   bytes 8..16: mint_recipient_policy_id (u64, offset 16)
        let sender_policy_id = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
        let recipient_policy_id = u64::from_be_bytes(bytes[16..24].try_into().unwrap());
        let mint_recipient_policy_id = u64::from_be_bytes(bytes[8..16].try_into().unwrap());

        Ok(Self {
            sender_policy_id,
            recipient_policy_id,
            mint_recipient_policy_id,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&self.sender_policy_id.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.recipient_policy_id.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.mint_recipient_policy_id.to_be_bytes());
        storage.store(slot, U256::from_be_bytes(bytes))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)
    }
}

// ===========================================================================
// PolicyRecord storage type
// ===========================================================================

/// Policy record containing base data and optional compound data.
#[derive(Debug, Clone)]
pub struct PolicyRecord {
    pub base: PolicyData,
    pub compound: CompoundPolicyData,
}

impl Default for PolicyRecord {
    fn default() -> Self {
        Self {
            base: PolicyData::default(),
            compound: CompoundPolicyData::default(),
        }
    }
}

impl StorableType for PolicyRecord {
    // PolicyData (1 slot) + CompoundPolicyData (1 slot) = 2 slots
    const LAYOUT: Layout = Layout::Slots(2);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for PolicyRecord {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let base = PolicyData::load(storage, slot, LayoutCtx::FULL)?;
        let compound = CompoundPolicyData::load(storage, slot + U256::from(1), LayoutCtx::FULL)?;
        Ok(Self { base, compound })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        self.base.store(storage, slot, LayoutCtx::FULL)?;
        self.compound
            .store(storage, slot + U256::from(1), LayoutCtx::FULL)
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        PolicyData::delete(storage, slot, LayoutCtx::FULL)?;
        CompoundPolicyData::delete(storage, slot + U256::from(1), LayoutCtx::FULL)
    }
}

/// Packed T9 TIP-1092 token-to-policy binding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenTransferPolicy {
    policy_id: u64,
    is_set: bool,
}

impl StorableType for TokenTransferPolicy {
    const LAYOUT: Layout = Layout::Bytes(9);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for TokenTransferPolicy {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let bytes = storage.load(slot)?.to_be_bytes::<32>();
        Ok(Self {
            policy_id: u64::from_be_bytes(bytes[24..32].try_into().unwrap()),
            is_set: bytes[23] != 0,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&self.policy_id.to_be_bytes());
        bytes[23] = u8::from(self.is_set);
        storage.store(slot, U256::from_be_bytes(bytes))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)
    }
}

/// Compact recovery-authority representation used by T6 receive policies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecoveryMode {
    #[default]
    Originator,
    Receiver,
    ThirdParty,
}

impl RecoveryMode {
    fn encode(authority: Address, account: Address) -> (Self, Address) {
        if authority.is_zero() {
            (Self::Originator, Address::ZERO)
        } else if authority == account {
            (Self::Receiver, Address::ZERO)
        } else {
            (Self::ThirdParty, authority)
        }
    }

    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Originator),
            1 => Ok(Self::Receiver),
            2 => Ok(Self::ThirdParty),
            _ => Err(err_invalid_receive_policy_type()),
        }
    }
}

/// Slot 3 mapping value: packed config followed by an optional third-party address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReceivePolicy {
    pub has_receive_policy: bool,
    pub sender_policy_id: u64,
    pub sender_policy_type: u8,
    pub token_filter_id: u64,
    pub token_filter_type: u8,
    pub recovery_mode: RecoveryMode,
    pub recovery_address: Address,
}

impl StorableType for ReceivePolicy {
    const LAYOUT: Layout = Layout::Slots(2);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for ReceivePolicy {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let bytes = storage.load(slot)?.to_be_bytes::<32>();
        let recovery_word = storage.load(slot + U256::ONE)?.to_be_bytes::<32>();
        Ok(Self {
            has_receive_policy: bytes[31] != 0,
            sender_policy_id: u64::from_be_bytes(bytes[23..31].try_into().unwrap()),
            sender_policy_type: bytes[22],
            token_filter_id: u64::from_be_bytes(bytes[14..22].try_into().unwrap()),
            token_filter_type: bytes[13],
            recovery_mode: RecoveryMode::decode(bytes[12])?,
            recovery_address: Address::from_slice(&recovery_word[12..32]),
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[31] = u8::from(self.has_receive_policy);
        bytes[23..31].copy_from_slice(&self.sender_policy_id.to_be_bytes());
        bytes[22] = self.sender_policy_type;
        bytes[14..22].copy_from_slice(&self.token_filter_id.to_be_bytes());
        bytes[13] = self.token_filter_type;
        bytes[12] = self.recovery_mode as u8;
        storage.store(slot, U256::from_be_bytes(bytes))?;
        let mut recovery_word = [0u8; 32];
        recovery_word[12..32].copy_from_slice(self.recovery_address.as_slice());
        storage.store(slot + U256::ONE, U256::from_be_bytes(recovery_word))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)?;
        storage.store(slot + U256::ONE, U256::ZERO)
    }
}

fn is_reserved_recovery_authority(address: Address) -> bool {
    let bytes = address.as_slice();
    let ethereum_precompile =
        bytes[..19].iter().all(|byte| *byte == 0) && (1..=17).contains(&bytes[19]);
    ethereum_precompile
        || address.is_tip20()
        || matches!(
            address,
            TIP_FEE_MANAGER_ADDRESS
                | TIP403_REGISTRY_ADDRESS
                | TIP20_FACTORY_ADDRESS
                | STABLECOIN_DEX_ADDRESS
                | TIP20_CHANNEL_RESERVE_ADDRESS
                | NONCE_PRECOMPILE_ADDRESS
                | VALIDATOR_CONFIG_ADDRESS
                | ACCOUNT_KEYCHAIN_ADDRESS
                | VALIDATOR_CONFIG_V2_ADDRESS
                | SIGNATURE_VERIFIER_ADDRESS
                | ADDRESS_REGISTRY_ADDRESS
                | RECEIVE_POLICY_GUARD_ADDRESS
                | CURRENT_COMMITTEE_ADDRESS
        )
}

// ===========================================================================
// TIP403Registry struct
// ===========================================================================

/// TIP-403 transfer policy registry precompile.
pub struct TIP403Registry {
    // Slot 0: policy_id_counter
    pub(crate) policy_id_counter: Slot<u64>,
    // Slot 1: policy_records
    pub(crate) policy_records: Mapping<u64, PolicyRecord>,
    // Slot 2: policy_set
    pub(crate) policy_set: Mapping<u64, Mapping<Address, bool>>,
    // Slot 3 (T6+): per-account receive policy.
    pub(crate) receive_policies: Mapping<Address, ReceivePolicy>,
    // Slot 4 (T9+): token-to-transfer-policy binding.
    pub(crate) token_transfer_policies: Mapping<Address, TokenTransferPolicy>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl TIP403Registry {
    pub fn new() -> Self {
        let address = TIP403_REGISTRY_ADDRESS;
        Self {
            policy_id_counter: Slot::new(U256::from(0), address),
            policy_records: Mapping::new(U256::from(1), address),
            policy_set: Mapping::new(U256::from(2), address),
            receive_policies: Mapping::new(U256::from(3), address),
            token_transfer_policies: Mapping::new(U256::from(4), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }

    /// Initializes the TIP-403 registry precompile.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    /// Returns the next policy ID to be assigned (always >= 2).
    pub fn policy_id_counter(&self) -> Result<u64> {
        self.policy_id_counter.read().map(|counter| counter.max(2))
    }

    /// Returns `true` if the given policy ID exists.
    pub fn policy_exists(&self, call: ITIP403Registry::policyExistsCall) -> Result<bool> {
        if self.builtin_authorization(call.policyId).is_some() {
            return Ok(true);
        }
        let counter = self.policy_id_counter()?;
        Ok(call.policyId < counter)
    }

    /// Returns registry binding state and the effective policy for a deployed TIP-20 token.
    pub fn token_transfer_policy_id(
        &self,
        call: ITIP403Registry::tokenTransferPolicyIdCall,
    ) -> Result<ITIP403Registry::tokenTransferPolicyIdReturn> {
        if !TIP20Factory::new().is_tip20(call.token)? {
            return Err(TempoPrecompileError::Revert(
                super::tip20::ITIP20::InvalidToken {}.abi_encode().into(),
            ));
        }

        let registered = self.registered_token_transfer_policy_id(call.token)?;
        let policy_id = match registered {
            Some(policy_id) => policy_id,
            None => TIP20Token::from_address(call.token)?.legacy_transfer_policy_id()?,
        };
        Ok(ITIP403Registry::tokenTransferPolicyIdReturn {
            isSet: registered.is_some(),
            policyId: policy_id,
        })
    }

    /// Returns a registry-owned policy ID without validating the token address.
    pub(crate) fn registered_token_transfer_policy_id(
        &self,
        token: Address,
    ) -> Result<Option<u64>> {
        let binding = self.token_transfer_policies[token].read()?;
        Ok(binding.is_set.then_some(binding.policy_id))
    }

    /// Writes the active registry binding for a TIP-20 token.
    pub(crate) fn set_token_transfer_policy(
        &mut self,
        token: Address,
        policy_id: u64,
    ) -> Result<()> {
        self.token_transfer_policies[token].write(TokenTransferPolicy {
            policy_id,
            is_set: true,
        })
    }

    /// Migrates valid unbound TIP-20 tokens from token-local policy storage.
    pub fn migrate_transfer_policy_ids(
        &mut self,
        call: ITIP403Registry::migrateTransferPolicyIdsCall,
    ) -> Result<U256> {
        let factory = TIP20Factory::new();
        let mut migrated = U256::ZERO;
        for token in call.tokens {
            if !factory.is_tip20(token)?
                || self.registered_token_transfer_policy_id(token)?.is_some()
            {
                continue;
            }

            let mut token_contract = TIP20Token::from_address(token)?;
            let policy_id = token_contract.legacy_transfer_policy_id()?;
            self.set_token_transfer_policy(token, policy_id)?;
            token_contract.delete_legacy_transfer_policy_id()?;
            migrated += U256::ONE;
        }
        Ok(migrated)
    }

    /// Returns the type and admin of a policy. Reverts if the policy does not exist or has an
    /// invalid type.
    pub fn policy_data(
        &self,
        call: ITIP403Registry::policyDataCall,
    ) -> Result<ITIP403Registry::policyDataReturn> {
        if self.storage.spec().is_t2() {
            // Built-in policies are virtual (not stored), and match the `PolicyType`:
            //  - 0: REJECT_ALL_POLICY_ID -> WHITELIST
            //  - 1: ALLOW_ALL_POLICY_ID  -> BLACKLIST
            if self.builtin_authorization(call.policyId).is_some() {
                let policy_type: ITIP403Registry::PolicyType = (call.policyId as u8)
                    .try_into()
                    .map_err(|_| err_invalid_policy_type())?;
                return Ok(ITIP403Registry::policyDataReturn {
                    policyType: policy_type,
                    admin: Address::ZERO,
                });
            }
        } else {
            // Pre-T2: check existence before reading
            if !self.policy_exists(ITIP403Registry::policyExistsCall {
                policyId: call.policyId,
            })? {
                return Err(err_policy_not_found());
            }
        }

        // Get policy data and verify that the policy id exists (T2+)
        let data = self.get_policy_data(call.policyId)?;

        Ok(ITIP403Registry::policyDataReturn {
            policyType: data.policy_type()?,
            admin: data.admin,
        })
    }

    /// Returns the sub-policy IDs of a compound policy (TIP-1015).
    pub fn compound_policy_data(
        &self,
        call: ITIP403Registry::compoundPolicyDataCall,
    ) -> Result<ITIP403Registry::compoundPolicyDataReturn> {
        let data = self.get_policy_data(call.policyId)?;

        if !data.is_compound() {
            let err = if self.policy_exists(ITIP403Registry::policyExistsCall {
                policyId: call.policyId,
            })? {
                err_incompatible_policy_type()
            } else {
                err_policy_not_found()
            };
            return Err(err);
        }

        let record = self.policy_records[call.policyId].read()?;
        Ok(ITIP403Registry::compoundPolicyDataReturn {
            senderPolicyId: record.compound.sender_policy_id,
            recipientPolicyId: record.compound.recipient_policy_id,
            mintRecipientPolicyId: record.compound.mint_recipient_policy_id,
        })
    }

    /// Creates a new simple (whitelist or blacklist) policy and returns its ID.
    pub fn create_policy(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::createPolicyCall,
    ) -> Result<u64> {
        let policy_type = ensure_is_simple(&call.policyType)?;
        let new_policy_id = self.policy_id_counter()?;

        self.policy_id_counter.write(
            new_policy_id
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;

        self.policy_records[new_policy_id].write(PolicyRecord {
            base: PolicyData {
                policy_type,
                admin: call.admin,
            },
            compound: CompoundPolicyData::default(),
        })?;

        self.emit_event(ITIP403Registry::PolicyCreated {
            policyId: new_policy_id,
            updater: msg_sender,
            policyType: policy_type
                .try_into()
                .unwrap_or(ITIP403Registry::PolicyType::WHITELIST),
        })?;

        self.emit_event(ITIP403Registry::PolicyAdminUpdated {
            policyId: new_policy_id,
            updater: msg_sender,
            admin: call.admin,
        })?;

        Ok(new_policy_id)
    }

    /// Creates a simple policy and pre-populates it with accounts.
    pub fn create_policy_with_accounts(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::createPolicyWithAccountsCall,
    ) -> Result<u64> {
        let admin = call.admin;
        let policy_type = ensure_is_simple(&call.policyType)?;
        let new_policy_id = self.policy_id_counter()?;

        self.policy_id_counter.write(
            new_policy_id
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;

        self.set_policy_data(new_policy_id, PolicyData { policy_type, admin })?;

        for account in call.accounts.iter() {
            self.set_policy_set(new_policy_id, *account, true)?;

            match call.policyType {
                ITIP403Registry::PolicyType::WHITELIST => {
                    self.emit_event(ITIP403Registry::WhitelistUpdated {
                        policyId: new_policy_id,
                        updater: msg_sender,
                        account: *account,
                        allowed: true,
                    })?;
                }
                ITIP403Registry::PolicyType::BLACKLIST => {
                    self.emit_event(ITIP403Registry::BlacklistUpdated {
                        policyId: new_policy_id,
                        updater: msg_sender,
                        account: *account,
                        restricted: true,
                    })?;
                }
                _ => {
                    return Err(err_incompatible_policy_type());
                }
            }
        }

        self.emit_event(ITIP403Registry::PolicyCreated {
            policyId: new_policy_id,
            updater: msg_sender,
            policyType: policy_type
                .try_into()
                .unwrap_or(ITIP403Registry::PolicyType::WHITELIST),
        })?;

        self.emit_event(ITIP403Registry::PolicyAdminUpdated {
            policyId: new_policy_id,
            updater: msg_sender,
            admin,
        })?;

        Ok(new_policy_id)
    }

    /// Transfers admin control of a policy. Only callable by the current admin.
    pub fn set_policy_admin(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::setPolicyAdminCall,
    ) -> Result<()> {
        let data = self.get_policy_data(call.policyId)?;

        if data.admin != msg_sender {
            return Err(err_unauthorized());
        }

        self.set_policy_data(
            call.policyId,
            PolicyData {
                admin: call.admin,
                ..data
            },
        )?;

        self.emit_event(ITIP403Registry::PolicyAdminUpdated {
            policyId: call.policyId,
            updater: msg_sender,
            admin: call.admin,
        })
    }

    /// Adds or removes an account from a whitelist policy.
    pub fn modify_policy_whitelist(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::modifyPolicyWhitelistCall,
    ) -> Result<()> {
        let data = self.get_policy_data(call.policyId)?;

        if data.admin != msg_sender {
            return Err(err_unauthorized());
        }

        if !matches!(data.policy_type()?, ITIP403Registry::PolicyType::WHITELIST) {
            return Err(err_incompatible_policy_type());
        }

        self.set_policy_set(call.policyId, call.account, call.allowed)?;

        self.emit_event(ITIP403Registry::WhitelistUpdated {
            policyId: call.policyId,
            updater: msg_sender,
            account: call.account,
            allowed: call.allowed,
        })
    }

    /// Adds or removes an account from a blacklist policy.
    pub fn modify_policy_blacklist(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::modifyPolicyBlacklistCall,
    ) -> Result<()> {
        let data = self.get_policy_data(call.policyId)?;

        if data.admin != msg_sender {
            return Err(err_unauthorized());
        }

        if !matches!(data.policy_type()?, ITIP403Registry::PolicyType::BLACKLIST) {
            return Err(err_incompatible_policy_type());
        }

        self.set_policy_set(call.policyId, call.account, call.restricted)?;

        self.emit_event(ITIP403Registry::BlacklistUpdated {
            policyId: call.policyId,
            updater: msg_sender,
            account: call.account,
            restricted: call.restricted,
        })
    }

    /// Creates a new compound policy referencing three simple sub-policies (TIP-1015).
    pub fn create_compound_policy(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::createCompoundPolicyCall,
    ) -> Result<u64> {
        self.validate_simple_policy(call.senderPolicyId)?;
        self.validate_simple_policy(call.recipientPolicyId)?;
        self.validate_simple_policy(call.mintRecipientPolicyId)?;

        let new_policy_id = self.policy_id_counter()?;

        self.policy_id_counter.write(
            new_policy_id
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;

        self.policy_records[new_policy_id].write(PolicyRecord {
            base: PolicyData {
                policy_type: ITIP403Registry::PolicyType::COMPOUND as u8,
                admin: Address::ZERO,
            },
            compound: CompoundPolicyData {
                sender_policy_id: call.senderPolicyId,
                recipient_policy_id: call.recipientPolicyId,
                mint_recipient_policy_id: call.mintRecipientPolicyId,
            },
        })?;

        self.emit_event(ITIP403Registry::CompoundPolicyCreated {
            policyId: new_policy_id,
            creator: msg_sender,
            senderPolicyId: call.senderPolicyId,
            recipientPolicyId: call.recipientPolicyId,
            mintRecipientPolicyId: call.mintRecipientPolicyId,
        })?;

        Ok(new_policy_id)
    }

    /// Returns an account's T6 receive-policy configuration.
    pub fn receive_policy(&self, account: Address) -> Result<ITIP403Registry::receivePolicyReturn> {
        let policy = self.receive_policies[account].read()?;
        let sender_policy_type = policy
            .sender_policy_type
            .try_into()
            .map_err(|_| err_invalid_receive_policy_type())?;
        let token_filter_type = policy
            .token_filter_type
            .try_into()
            .map_err(|_| err_invalid_receive_policy_type())?;
        Ok(ITIP403Registry::receivePolicyReturn {
            hasReceivePolicy: policy.has_receive_policy,
            senderPolicyId: policy.sender_policy_id,
            senderPolicyType: sender_policy_type,
            tokenFilterId: policy.token_filter_id,
            tokenFilterType: token_filter_type,
            recoveryAuthority: self.receive_policy_recovery(account, &policy),
        })
    }

    /// Returns the blocking reason for an inbound token transfer, if any.
    pub fn validate_receive_policy(
        &self,
        token: Address,
        sender: Address,
        receiver: Address,
    ) -> Result<Option<ITIP403Registry::BlockedReason>> {
        Ok(self
            .check_receive_policy(token, sender, receiver)?
            .map(|(reason, _)| reason))
    }

    pub(crate) fn check_receive_policy(
        &self,
        token: Address,
        sender: Address,
        receiver: Address,
    ) -> Result<Option<(ITIP403Registry::BlockedReason, Address)>> {
        let policy = self.receive_policies[receiver].read()?;
        if !policy.has_receive_policy {
            return Ok(None);
        }

        if !self.is_authorized_simple(policy.token_filter_id, token)? {
            return Ok(Some((
                ITIP403Registry::BlockedReason::TOKEN_FILTER,
                self.receive_policy_recovery(receiver, &policy),
            )));
        }
        if !self.is_authorized_simple(policy.sender_policy_id, sender)? {
            return Ok(Some((
                ITIP403Registry::BlockedReason::RECEIVE_POLICY,
                self.receive_policy_recovery(receiver, &policy),
            )));
        }
        Ok(None)
    }

    fn receive_policy_recovery(&self, account: Address, policy: &ReceivePolicy) -> Address {
        match policy.recovery_mode {
            RecoveryMode::Originator => Address::ZERO,
            RecoveryMode::Receiver => account,
            RecoveryMode::ThirdParty => policy.recovery_address,
        }
    }

    /// Configures the caller's T6 receive policy.
    pub fn set_receive_policy(
        &mut self,
        msg_sender: Address,
        call: ITIP403Registry::setReceivePolicyCall,
    ) -> Result<()> {
        if msg_sender.is_virtual() {
            return Err(err_virtual_address_not_allowed());
        }
        if call.recoveryAuthority.is_virtual()
            || is_reserved_recovery_authority(call.recoveryAuthority)
        {
            return Err(err_invalid_recovery_authority());
        }

        let sender_policy_type = self.validate_receive_policy_id(call.senderPolicyId)?;
        let token_filter_type = self.validate_receive_policy_id(call.tokenFilterId)?;
        let (recovery_mode, recovery_address) =
            RecoveryMode::encode(call.recoveryAuthority, msg_sender);
        self.receive_policies[msg_sender].write(ReceivePolicy {
            has_receive_policy: true,
            sender_policy_id: call.senderPolicyId,
            sender_policy_type,
            token_filter_id: call.tokenFilterId,
            token_filter_type,
            recovery_mode,
            recovery_address,
        })?;

        self.emit_event(ITIP403Registry::ReceivePolicyUpdated {
            account: msg_sender,
            senderPolicyId: call.senderPolicyId,
            tokenFilterId: call.tokenFilterId,
            recoveryAuthority: call.recoveryAuthority,
        })
    }

    fn validate_receive_policy_id(&self, policy_id: u64) -> Result<u8> {
        if self.builtin_authorization(policy_id).is_some() {
            return Ok(policy_id as u8);
        }
        if policy_id >= self.policy_id_counter()? {
            return Err(err_policy_not_found());
        }
        let data = self.get_policy_data(policy_id)?;
        if !data.is_simple() {
            return Err(err_invalid_receive_policy_type());
        }
        Ok(data.policy_type)
    }

    /// Core role-based authorization check (TIP-1015).
    pub fn is_authorized_as(&self, policy_id: u64, user: Address, role: AuthRole) -> Result<bool> {
        if let Some(auth) = self.builtin_authorization(policy_id) {
            return Ok(auth);
        }

        let data = self.get_policy_data(policy_id)?;

        if data.is_compound() {
            let record = self.policy_records[policy_id].read()?;
            let compound = record.compound;
            return match role {
                AuthRole::Sender => self.is_authorized_simple(compound.sender_policy_id, user),
                AuthRole::Recipient => {
                    self.is_authorized_simple(compound.recipient_policy_id, user)
                }
                AuthRole::MintRecipient => {
                    self.is_authorized_simple(compound.mint_recipient_policy_id, user)
                }
                AuthRole::Transfer => {
                    // T2+: short-circuit if sender fails
                    let sender_auth = self.is_authorized_simple(compound.sender_policy_id, user)?;
                    if self.storage.spec().is_t2() && !sender_auth {
                        return Ok(false);
                    }
                    let recipient_auth =
                        self.is_authorized_simple(compound.recipient_policy_id, user)?;
                    Ok(sender_auth && recipient_auth)
                }
            };
        }

        self.is_simple(policy_id, user, &data)
    }

    /// Returns authorization result for built-in policies.
    #[inline]
    fn builtin_authorization(&self, policy_id: u64) -> Option<bool> {
        match policy_id {
            ALLOW_ALL_POLICY_ID => Some(true),
            REJECT_ALL_POLICY_ID => Some(false),
            _ => None,
        }
    }

    /// Authorization for simple (non-compound) policies only.
    fn is_authorized_simple(&self, policy_id: u64, user: Address) -> Result<bool> {
        if let Some(auth) = self.builtin_authorization(policy_id) {
            return Ok(auth);
        }
        let data = self.get_policy_data(policy_id)?;
        self.is_simple(policy_id, user, &data)
    }

    /// Authorization check for simple (non-compound) policies.
    fn is_simple(&self, policy_id: u64, user: Address, data: &PolicyData) -> Result<bool> {
        // Read policy_set BEFORE checking policy type to match original gas consumption
        let is_in_set = self.policy_set[policy_id][user].read()?;

        match data.policy_type()? {
            ITIP403Registry::PolicyType::WHITELIST => Ok(is_in_set),
            ITIP403Registry::PolicyType::BLACKLIST => Ok(!is_in_set),
            ITIP403Registry::PolicyType::COMPOUND => Err(err_incompatible_policy_type()),
            _ => unreachable!(),
        }
    }

    /// Validates that a policy ID references an existing simple policy.
    fn validate_simple_policy(&self, policy_id: u64) -> Result<()> {
        if self.builtin_authorization(policy_id).is_some() {
            return Ok(());
        }

        if policy_id >= self.policy_id_counter()? {
            return Err(err_policy_not_found());
        }

        let data = self.get_policy_data(policy_id)?;
        if !data.is_simple() {
            return Err(err_policy_not_simple());
        }

        Ok(())
    }

    // -- Internal helper functions --

    /// Returns policy data for the given policy ID.
    fn get_policy_data(&self, policy_id: u64) -> Result<PolicyData> {
        // Read only the base slot (PolicyData), not the full PolicyRecord
        // (which includes CompoundPolicyData in a second slot). The compound
        // data is only needed for compound policy dispatch, not for the base
        // data check here. Writer reads .base only (handler.rs:638).
        use crate::tempo::precompile::storage_types::Slot;
        let base_slot = self.policy_records[policy_id].slot();
        let data: PolicyData = Slot::new(base_slot, self.address).read()?;

        // T2+: verify that the policy id exists
        if self.storage.spec().is_t2()
            && data.is_default()
            && policy_id >= self.policy_id_counter()?
        {
            return Err(err_policy_not_found());
        }

        Ok(data)
    }

    fn set_policy_data(&mut self, policy_id: u64, data: PolicyData) -> Result<()> {
        // Read existing record to preserve compound data
        let mut record = self.policy_records[policy_id].read()?;
        record.base = data;
        self.policy_records[policy_id].write(record)
    }

    fn set_policy_set(&mut self, policy_id: u64, account: Address, value: bool) -> Result<()> {
        self.policy_set[policy_id][account].write(value)
    }
}

impl ContractStorage for TIP403Registry {
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
// PolicyType validation
// ===========================================================================

/// Validates that a PolicyType is simple and returns its u8 discriminant.
///
/// Pre-T2: Converts COMPOUND (and any unknown variant) to 255 to match original
///          ABI decoding behavior (legacy bug-compatible).
/// T2+: Only allows WHITELIST and BLACKLIST.
fn ensure_is_simple(policy_type: &ITIP403Registry::PolicyType) -> Result<u8> {
    match policy_type {
        ITIP403Registry::PolicyType::WHITELIST | ITIP403Registry::PolicyType::BLACKLIST => {
            Ok(*policy_type as u8)
        }
        _ => {
            if StorageCtx::default().spec().is_t2() {
                Err(err_incompatible_policy_type())
            } else {
                // Pre-T2: store as 255 (legacy __Invalid discriminant)
                Ok(255u8)
            }
        }
    }
}

/// Returns `true` if the error indicates a failed policy lookup.
#[allow(dead_code)]
pub fn is_policy_lookup_error(e: &TempoPrecompileError) -> bool {
    if StorageCtx::default().spec().is_t2() {
        // T2+: typed TIP403 errors
        *e == err_invalid_policy_type() || *e == err_policy_not_found()
    } else {
        // Pre-T2: legacy Panic(UnderOverflow) sentinel
        *e == TempoPrecompileError::under_overflow()
    }
}

// ===========================================================================
// Dispatch
// ===========================================================================

impl Precompile for TIP403Registry {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        let selector = calldata
            .get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .unwrap_or_default();
        dispatch_call(
            calldata,
            |data| {
                ITIP403Registry::ITIP403RegistryCalls::abi_decode_with_config(
                    data,
                    crate::tempo::precompile::abi_decoder_config(),
                )
            },
            |call| match call {
                ITIP403Registry::ITIP403RegistryCalls::policyIdCounter(call) => {
                    view(call, |_| self.policy_id_counter())
                }
                ITIP403Registry::ITIP403RegistryCalls::policyExists(call) => {
                    view(call, |c| self.policy_exists(c))
                }
                ITIP403Registry::ITIP403RegistryCalls::tokenTransferPolicyId(call) => {
                    if !self.storage.spec().is_t9() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    view(call, |c| self.token_transfer_policy_id(c))
                }
                ITIP403Registry::ITIP403RegistryCalls::policyData(call) => {
                    view(call, |c| self.policy_data(c))
                }
                ITIP403Registry::ITIP403RegistryCalls::isAuthorized(call) => view(call, |c| {
                    self.is_authorized_as(c.policyId, c.user, AuthRole::Transfer)
                }),
                // TIP-1015: T2+ only (leafage always runs T2+)
                ITIP403Registry::ITIP403RegistryCalls::isAuthorizedSender(call) => {
                    view(call, |c| {
                        self.is_authorized_as(c.policyId, c.user, AuthRole::Sender)
                    })
                }
                ITIP403Registry::ITIP403RegistryCalls::isAuthorizedRecipient(call) => {
                    view(call, |c| {
                        self.is_authorized_as(c.policyId, c.user, AuthRole::Recipient)
                    })
                }
                ITIP403Registry::ITIP403RegistryCalls::isAuthorizedMintRecipient(call) => {
                    view(call, |c| {
                        self.is_authorized_as(c.policyId, c.user, AuthRole::MintRecipient)
                    })
                }
                ITIP403Registry::ITIP403RegistryCalls::compoundPolicyData(call) => {
                    view(call, |c| self.compound_policy_data(c))
                }
                ITIP403Registry::ITIP403RegistryCalls::receivePolicy(call) => {
                    if !self.storage.spec().is_t6() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    view(call, |c| self.receive_policy(c.account))
                }
                ITIP403Registry::ITIP403RegistryCalls::validateReceivePolicy(call) => {
                    if !self.storage.spec().is_t6() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    view(call, |c| {
                        let blocked = self
                            .validate_receive_policy(c.token, c.sender, c.receiver)?
                            .unwrap_or(ITIP403Registry::BlockedReason::NONE);
                        Ok(ITIP403Registry::validateReceivePolicyReturn {
                            authorized: blocked == ITIP403Registry::BlockedReason::NONE,
                            blockedReason: blocked,
                        })
                    })
                }
                ITIP403Registry::ITIP403RegistryCalls::createPolicy(call) => {
                    mutate(call, msg_sender, |s, c| self.create_policy(s, c))
                }
                ITIP403Registry::ITIP403RegistryCalls::createPolicyWithAccounts(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.create_policy_with_accounts(s, c)
                    })
                }
                ITIP403Registry::ITIP403RegistryCalls::setPolicyAdmin(call) => {
                    mutate_void(call, msg_sender, |s, c| self.set_policy_admin(s, c))
                }
                ITIP403Registry::ITIP403RegistryCalls::modifyPolicyWhitelist(call) => {
                    mutate_void(call, msg_sender, |s, c| self.modify_policy_whitelist(s, c))
                }
                ITIP403Registry::ITIP403RegistryCalls::modifyPolicyBlacklist(call) => {
                    mutate_void(call, msg_sender, |s, c| self.modify_policy_blacklist(s, c))
                }
                // TIP-1015: T2+ only (leafage always runs T2+)
                ITIP403Registry::ITIP403RegistryCalls::createCompoundPolicy(call) => {
                    mutate(call, msg_sender, |s, c| self.create_compound_policy(s, c))
                }
                ITIP403Registry::ITIP403RegistryCalls::setReceivePolicy(call) => {
                    if !self.storage.spec().is_t6() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    mutate_void(call, msg_sender, |s, c| self.set_receive_policy(s, c))
                }
                ITIP403Registry::ITIP403RegistryCalls::migrateTransferPolicyIds(call) => {
                    if !self.storage.spec().is_t9() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    mutate(call, msg_sender, |_, c| self.migrate_transfer_policy_ids(c))
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::PATH_USD_ADDRESS;
    use crate::tempo::precompile::storage_types::StorageKey;
    use crate::tempo::precompile::test_utils::TestStorageProvider;
    use alloy::sol_types::SolCall;

    fn initialize_test_token(token: Address, admin: Address) -> Result<()> {
        TIP20Token::from_address_unchecked(token).initialize(
            Address::ZERO,
            "Policy Token",
            "POL",
            "USD",
            PATH_USD_ADDRESS,
            admin,
        )
    }

    #[test]
    fn t9_token_transfer_policy_binding_packed_layout() {
        let token = Address::from_slice(&alloy::hex!("20c0000000000000000000000000000000000091"));
        let mut provider = TestStorageProvider::new(TempoHardfork::T9);

        StorageCtx::enter(&mut provider, || {
            let mut registry = TIP403Registry::new();
            registry.set_token_transfer_policy(token, 0x1122_3344_5566_7788)?;
            assert_eq!(
                registry.token_transfer_policies[token].read()?,
                TokenTransferPolicy {
                    policy_id: 0x1122_3344_5566_7788,
                    is_set: true,
                }
            );
            Result::<()>::Ok(())
        })
        .unwrap();

        let slot = token.mapping_slot(U256::from(4));
        assert_eq!(
            provider.storage(TIP403_REGISTRY_ADDRESS, slot),
            (U256::ONE << 64) | U256::from(0x1122_3344_5566_7788u64)
        );
    }

    #[test]
    fn t9_policy_fallback_explicit_zero_and_migration() {
        let admin = Address::repeat_byte(0x92);
        let token = Address::from_slice(&alloy::hex!("20c0000000000000000000000000000000000093"));
        let explicit_zero =
            Address::from_slice(&alloy::hex!("20c0000000000000000000000000000000000094"));
        let invalid = Address::repeat_byte(0x95);
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);

        StorageCtx::enter(&mut provider, || {
            initialize_test_token(token, admin)?;
            initialize_test_token(explicit_zero, admin)?;
            TIP20Token::from_address_unchecked(explicit_zero).change_transfer_policy_id(
                admin,
                super::super::tip20::ITIP20::changeTransferPolicyIdCall { newPolicyId: 0 },
            )?;
            Result::<()>::Ok(())
        })
        .unwrap();

        provider.set_spec(TempoHardfork::T9);
        StorageCtx::enter(&mut provider, || {
            let mut registry = TIP403Registry::new();
            assert_eq!(
                registry.token_transfer_policy_id(ITIP403Registry::tokenTransferPolicyIdCall {
                    token,
                })?,
                ITIP403Registry::tokenTransferPolicyIdReturn {
                    isSet: false,
                    policyId: ALLOW_ALL_POLICY_ID,
                }
            );
            assert_eq!(TIP20Token::from_address(token)?.transfer_policy_id()?, 1);

            assert_eq!(
                registry.token_transfer_policy_id(ITIP403Registry::tokenTransferPolicyIdCall {
                    token: explicit_zero,
                })?,
                ITIP403Registry::tokenTransferPolicyIdReturn {
                    isSet: false,
                    policyId: REJECT_ALL_POLICY_ID,
                }
            );
            registry.set_token_transfer_policy(explicit_zero, REJECT_ALL_POLICY_ID)?;
            assert_eq!(
                registry.token_transfer_policy_id(ITIP403Registry::tokenTransferPolicyIdCall {
                    token: explicit_zero,
                })?,
                ITIP403Registry::tokenTransferPolicyIdReturn {
                    isSet: true,
                    policyId: REJECT_ALL_POLICY_ID,
                }
            );

            assert_eq!(
                registry.migrate_transfer_policy_ids(
                    ITIP403Registry::migrateTransferPolicyIdsCall {
                        tokens: vec![invalid, token, token, explicit_zero],
                    }
                )?,
                U256::ONE
            );
            assert_eq!(
                registry.migrate_transfer_policy_ids(
                    ITIP403Registry::migrateTransferPolicyIdsCall {
                        tokens: vec![token, invalid],
                    }
                )?,
                U256::ZERO
            );
            assert_eq!(
                TIP20Token::from_address(token)?.legacy_transfer_policy_id()?,
                0
            );
            assert_eq!(
                TIP20Token::from_address(token)?.next_quote_token()?,
                PATH_USD_ADDRESS
            );
            assert_eq!(TIP20Token::from_address(token)?.transfer_policy_id()?, 1);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t9_new_token_and_admin_change_use_registry_binding() {
        let admin = Address::repeat_byte(0xa1);
        let stranger = Address::repeat_byte(0xa2);
        let token = Address::from_slice(&alloy::hex!("20c00000000000000000000000000000000000a3"));
        let mut provider = TestStorageProvider::new(TempoHardfork::T9);

        StorageCtx::enter(&mut provider, || {
            initialize_test_token(token, admin)?;
            let mut tip20 = TIP20Token::from_address(token)?;
            let registry = TIP403Registry::new();
            assert_eq!(tip20.legacy_transfer_policy_id()?, 0);
            assert_eq!(
                registry.registered_token_transfer_policy_id(token)?,
                Some(ALLOW_ALL_POLICY_ID)
            );

            let unauthorized = tip20.change_transfer_policy_id(
                stranger,
                super::super::tip20::ITIP20::changeTransferPolicyIdCall { newPolicyId: 0 },
            );
            assert!(unauthorized.is_err());
            assert_eq!(tip20.transfer_policy_id()?, ALLOW_ALL_POLICY_ID);

            tip20.change_transfer_policy_id(
                admin,
                super::super::tip20::ITIP20::changeTransferPolicyIdCall { newPolicyId: 0 },
            )?;
            assert_eq!(tip20.transfer_policy_id()?, REJECT_ALL_POLICY_ID);
            assert_eq!(tip20.legacy_transfer_policy_id()?, 0);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn registry_binding_only_becomes_effective_at_t9() {
        let admin = Address::repeat_byte(0xaa);
        let token = Address::from_slice(&alloy::hex!("20c00000000000000000000000000000000000ab"));
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);
        StorageCtx::enter(&mut provider, || {
            initialize_test_token(token, admin)?;
            TIP403Registry::new().set_token_transfer_policy(token, REJECT_ALL_POLICY_ID)?;
            assert_eq!(
                TIP20Token::from_address(token)?.transfer_policy_id()?,
                ALLOW_ALL_POLICY_ID
            );
            Result::<()>::Ok(())
        })
        .unwrap();

        provider.set_spec(TempoHardfork::T9);
        StorageCtx::enter(&mut provider, || {
            assert_eq!(
                TIP20Token::from_address(token)?.transfer_policy_id()?,
                REJECT_ALL_POLICY_ID
            );
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn token_transfer_policy_selectors_activate_at_t9() {
        let admin = Address::repeat_byte(0xb1);
        let token = Address::from_slice(&alloy::hex!("20c00000000000000000000000000000000000b2"));
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);
        StorageCtx::enter(&mut provider, || initialize_test_token(token, admin)).unwrap();

        let lookup = ITIP403Registry::tokenTransferPolicyIdCall { token };
        let migrate = ITIP403Registry::migrateTransferPolicyIdsCall {
            tokens: vec![token],
        };
        for calldata in [lookup.abi_encode(), migrate.abi_encode()] {
            let output = StorageCtx::enter(&mut provider, || {
                TIP403Registry::new().call(&calldata, Address::ZERO)
            })
            .unwrap();
            assert!(output.reverted);
        }

        provider.set_spec(TempoHardfork::T9);
        let lookup_output = StorageCtx::enter(&mut provider, || {
            TIP403Registry::new().call(&lookup.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(!lookup_output.reverted);
        assert_eq!(
            ITIP403Registry::tokenTransferPolicyIdCall::abi_decode_returns(&lookup_output.bytes)
                .unwrap(),
            ITIP403Registry::tokenTransferPolicyIdReturn {
                isSet: false,
                policyId: ALLOW_ALL_POLICY_ID,
            }
        );

        let migrate_output = StorageCtx::enter(&mut provider, || {
            TIP403Registry::new().call(&migrate.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(!migrate_output.reverted);
        assert_eq!(
            ITIP403Registry::migrateTransferPolicyIdsCall::abi_decode_returns(
                &migrate_output.bytes
            )
            .unwrap(),
            U256::ONE
        );
    }

    #[test]
    fn t6_receive_policy_checks_token_before_sender() {
        let receiver = Address::repeat_byte(0x61);
        let sender = Address::repeat_byte(0x62);
        let token = Address::repeat_byte(0x63);
        let recovery = Address::repeat_byte(0x64);
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);

        StorageCtx::enter(&mut provider, || {
            let mut registry = TIP403Registry::new();
            registry.set_receive_policy(
                receiver,
                ITIP403Registry::setReceivePolicyCall {
                    senderPolicyId: REJECT_ALL_POLICY_ID,
                    tokenFilterId: REJECT_ALL_POLICY_ID,
                    recoveryAuthority: recovery,
                },
            )?;

            let stored = registry.receive_policies[receiver].read()?;
            assert_eq!(stored.recovery_mode, RecoveryMode::ThirdParty);
            assert_eq!(stored.recovery_address, recovery);
            assert_eq!(
                registry.check_receive_policy(token, sender, receiver)?,
                Some((ITIP403Registry::BlockedReason::TOKEN_FILTER, recovery)),
            );

            registry.set_receive_policy(
                receiver,
                ITIP403Registry::setReceivePolicyCall {
                    senderPolicyId: REJECT_ALL_POLICY_ID,
                    tokenFilterId: ALLOW_ALL_POLICY_ID,
                    recoveryAuthority: Address::ZERO,
                },
            )?;
            assert_eq!(
                registry.check_receive_policy(token, sender, receiver)?,
                Some((
                    ITIP403Registry::BlockedReason::RECEIVE_POLICY,
                    Address::ZERO,
                )),
            );
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t6_receive_policy_rejects_reserved_recovery_authority() {
        let account = Address::repeat_byte(0x71);
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);
        let result = StorageCtx::enter(&mut provider, || {
            TIP403Registry::new().set_receive_policy(
                account,
                ITIP403Registry::setReceivePolicyCall {
                    senderPolicyId: ALLOW_ALL_POLICY_ID,
                    tokenFilterId: ALLOW_ALL_POLICY_ID,
                    recoveryAuthority: STABLECOIN_DEX_ADDRESS,
                },
            )
        });
        assert_eq!(result.unwrap_err(), err_invalid_recovery_authority());
    }

    #[test]
    fn receive_policy_selectors_are_gated_at_t6() {
        let call = ITIP403Registry::receivePolicyCall {
            account: Address::repeat_byte(0x81),
        };
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let output = StorageCtx::enter(&mut provider, || {
            TIP403Registry::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(output.reverted);
        assert_eq!(
            output.bytes.as_ref(),
            ITIP403Registry::receivePolicyCall::SELECTOR
        );
    }
}
