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

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::StorageOps;
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType};
use super::{
    dispatch_call, input_cost, mutate, mutate_void, view, Precompile, TIP403_REGISTRY_ADDRESS,
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

        function policyIdCounter() external view returns (uint64);
        function policyExists(uint64 policyId) external view returns (bool);
        function policyData(uint64 policyId) external view returns (PolicyType policyType, address admin);
        function isAuthorized(uint64 policyId, address user) external view returns (bool);
        function isAuthorizedSender(uint64 policyId, address user) external view returns (bool);
        function isAuthorizedRecipient(uint64 policyId, address user) external view returns (bool);
        function isAuthorizedMintRecipient(uint64 policyId, address user) external view returns (bool);
        function compoundPolicyData(uint64 policyId) external view returns (uint64 senderPolicyId, uint64 recipientPolicyId, uint64 mintRecipientPolicyId);

        function createPolicy(address admin, PolicyType policyType) external returns (uint64);
        function createPolicyWithAccounts(address admin, PolicyType policyType, address[] calldata accounts) external returns (uint64);
        function setPolicyAdmin(uint64 policyId, address admin) external;
        function modifyPolicyWhitelist(uint64 policyId, address account, bool allowed) external;
        function modifyPolicyBlacklist(uint64 policyId, address account, bool restricted) external;
        function createCompoundPolicy(uint64 senderPolicyId, uint64 recipientPolicyId, uint64 mintRecipientPolicyId) external returns (uint64);

        event PolicyAdminUpdated(uint64 indexed policyId, address indexed updater, address indexed admin);
        event PolicyCreated(uint64 indexed policyId, address indexed updater, PolicyType policyType);
        event WhitelistUpdated(uint64 indexed policyId, address indexed updater, address indexed account, bool allowed);
        event BlacklistUpdated(uint64 indexed policyId, address indexed updater, address indexed account, bool restricted);
        event CompoundPolicyCreated(uint64 indexed policyId, address indexed creator, uint64 senderPolicyId, uint64 recipientPolicyId, uint64 mintRecipientPolicyId);

        error Unauthorized();
        error PolicyNotFound();
        error PolicyNotSimple();
        error InvalidPolicyType();
        error IncompatiblePolicyType();
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

        dispatch_call(
            calldata,
            ITIP403Registry::ITIP403RegistryCalls::abi_decode,
            |call| match call {
                ITIP403Registry::ITIP403RegistryCalls::policyIdCounter(call) => {
                    view(call, |_| self.policy_id_counter())
                }
                ITIP403Registry::ITIP403RegistryCalls::policyExists(call) => {
                    view(call, |c| self.policy_exists(c))
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
            },
        )
    }
}
