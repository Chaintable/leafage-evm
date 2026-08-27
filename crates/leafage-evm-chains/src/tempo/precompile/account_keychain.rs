//! Account Keychain precompile -- manages session keys and per-token spending limits.
//!
//! Ported from `tempo/crates/precompiles/src/account_keychain/`.
//!
//! ## Storage layout
//!
//! | Slot | Field            | Type                                        |
//! |------|------------------|---------------------------------------------|
//! |  0   | keys             | Mapping<Address, Mapping<Address, AuthorizedKey>> |
//! |  1   | spending_limits  | Mapping<B256, Mapping<Address, U256>>        |
//! |  2   | key_scopes       | Mapping (T3+)                                |
//! |  3   | witnesses        | Mapping (T5+)                                |
//! | 2/3, 3/4, 4/5 | transaction_key / tx_origin | transient by fork       |
//!
//! ## Signature verification
//!
//! Methods that only read keychain state (getKey, getRemainingLimit, etc.) are fully ported.
//! `validate_keychain_authorization` checks key existence, revocation, expiry, and signature
//! type matching -- all state reads, no actual cryptographic verification.
//!
//! P256/WebAuthn signature verification is **not needed** in precompile dispatch. The actual
//! cryptographic verification happens in the handler layer's `verify_signature` during tx
//! validation, which is not triggered by eth_call. The precompile only stores/reads key
//! metadata (including signature type for type-matching checks).

use std::collections::HashSet;

use alloy::primitives::{keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy::sol_types::{SolCall, SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::StorageOps;
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{
    packing, Handler, Layout, LayoutCtx, Mapping, Set as StorageSet, SetHandler, Slot, Storable,
    StorableType, StorageKey,
};
use super::tip20::ITIP20;
use super::tip20_factory::TIP20Factory;
use super::super::address::TempoAddressExt;
use super::{
    dispatch_call, input_cost, mutate_void, unknown_selector, view, Precompile,
    ACCOUNT_KEYCHAIN_ADDRESS,
};

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IAccountKeychain {
        enum SignatureType {
            Secp256k1,
            P256,
            WebAuthn,
        }

        struct LegacyTokenLimit {
            address token;
            uint256 amount;
        }

        struct TokenLimit {
            address token;
            uint256 amount;
            uint64 period;
        }

        /// Selector-level recipient rule (TIP-1011, T3+).
        struct SelectorRule {
            bytes4 selector;
            address[] recipients;
        }

        /// Per-target call scope (TIP-1011, T3+).
        struct CallScope {
            address target;
            SelectorRule[] selectorRules;
        }

        struct KeyRestrictions {
            uint64 expiry;
            bool enforceLimits;
            TokenLimit[] limits;
            bool allowAnyCalls;
            CallScope[] allowedCalls;
        }

        struct KeyInfo {
            SignatureType signatureType;
            address keyId;
            uint64 expiry;
            bool enforceLimits;
            bool isRevoked;
        }

        event KeyAuthorized(address indexed account, address indexed publicKey, uint8 signatureType, uint64 expiry);
        event KeyRevoked(address indexed account, address indexed publicKey);
        event SpendingLimitUpdated(address indexed account, address indexed publicKey, address indexed token, uint256 newLimit);
        event KeyAuthorizationWitness(address indexed account, bytes32 indexed witness);
        event KeyAuthorizationWitnessBurned(address indexed account, bytes32 indexed witness);
        event AdminKeyAuthorized(address indexed account, address indexed publicKey);

        function authorizeKey(
            address keyId,
            SignatureType signatureType,
            uint64 expiry,
            bool enforceLimits,
            LegacyTokenLimit[] calldata limits
        ) external;

        function authorizeKey(
            address keyId,
            SignatureType signatureType,
            KeyRestrictions calldata config
        ) external;

        function authorizeKey(
            address keyId,
            SignatureType signatureType,
            KeyRestrictions calldata config,
            bytes32 witness
        ) external;

        function authorizeAdminKey(
            address keyId,
            SignatureType signatureType,
            bytes32 witness
        ) external;

        function burnKeyAuthorizationWitness(bytes32 witness) external;

        function revokeKey(address keyId) external;

        function updateSpendingLimit(
            address keyId,
            address token,
            uint256 newLimit
        ) external;

        function getKey(address account, address keyId) external view returns (KeyInfo memory);

        /// (TIP-1011, T3+) Set or replace allowed calls for one or more key+target pairs.
        function setAllowedCalls(
            address keyId,
            CallScope[] calldata scopes
        ) external;

        /// (TIP-1011, T3+) Remove any configured call scope for a key+target pair.
        function removeAllowedCalls(address keyId, address target) external;

        /// (TIP-1011, T3+) Returns whether the key is call-scoped and the configured scopes.
        ///
        /// `isScoped = false` means unrestricted. `isScoped = true && scopes.length == 0`
        /// means scoped deny-all. Missing, revoked, or expired keys report scoped deny-all
        /// so this getter never exposes stale persisted scope state.
        function getAllowedCalls(
            address account,
            address keyId
        ) external view returns (bool isScoped, CallScope[] memory scopes);

        function getRemainingLimit(
            address account,
            address keyId,
            address token
        ) external view returns (uint256);

        function getTransactionKey() external view returns (address);

        function isKeyAuthorizationWitnessBurned(
            address account,
            bytes32 witness
        ) external view returns (bool);

        function isAdminKey(address account, address keyId) external view returns (bool);

        error UnauthorizedCaller();
        error KeyAlreadyExists();
        error KeyNotFound();
        error KeyExpired();
        error SpendingLimitExceeded();
        error InvalidSignatureType();
        error ZeroPublicKey();
        error ExpiryInPast();
        error KeyAlreadyRevoked();
        error SignatureTypeMismatch(uint8 expected, uint8 actual);
        /// (TIP-1011, T3+) Raised by setCallScopes / validate_call_scopes.
        error InvalidCallScope();
        /// (T3+) Spending limit value exceeds the TIP-20 `u128` supply range.
        error InvalidSpendingLimit();
        error KeyAuthorizationWitnessAlreadyBurned();
        error InvalidKeyId();
        error LegacyAuthorizeKeySelectorChanged(bytes4 newSelector);
    }
}

// ===========================================================================
// Error helpers
// ===========================================================================

fn err_unauthorized_caller() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::UnauthorizedCaller {}.abi_encode().into())
}

fn err_key_already_exists() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::KeyAlreadyExists {}.abi_encode().into())
}

fn err_key_not_found() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::KeyNotFound {}.abi_encode().into())
}

fn err_key_expired() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::KeyExpired {}.abi_encode().into())
}

fn err_spending_limit_exceeded() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IAccountKeychain::SpendingLimitExceeded {}
            .abi_encode()
            .into(),
    )
}

fn err_invalid_signature_type() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IAccountKeychain::InvalidSignatureType {}
            .abi_encode()
            .into(),
    )
}

fn err_zero_public_key() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::ZeroPublicKey {}.abi_encode().into())
}

fn err_expiry_in_past() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::ExpiryInPast {}.abi_encode().into())
}

fn err_key_already_revoked() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::KeyAlreadyRevoked {}.abi_encode().into())
}

#[allow(dead_code)]
fn err_signature_type_mismatch(expected: u8, actual: u8) -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IAccountKeychain::SignatureTypeMismatch { expected, actual }
            .abi_encode()
            .into(),
    )
}

fn err_invalid_call_scope() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::InvalidCallScope {}.abi_encode().into())
}

fn err_invalid_spending_limit() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IAccountKeychain::InvalidSpendingLimit {}.abi_encode().into(),
    )
}

fn err_key_authorization_witness_already_burned() -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IAccountKeychain::KeyAuthorizationWitnessAlreadyBurned {}
            .abi_encode()
            .into(),
    )
}

fn err_invalid_key_id() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IAccountKeychain::InvalidKeyId {}.abi_encode().into())
}

fn err_legacy_authorize_key_selector_changed(selector: [u8; 4]) -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IAccountKeychain::LegacyAuthorizeKeySelectorChanged {
            newSelector: FixedBytes::new(selector),
        }
        .abi_encode()
        .into(),
    )
}

// ===========================================================================
// TIP-1011 — constrained TIP-20 selectors for recipient-restricted rules
// ===========================================================================

const TIP20_TRANSFER_SELECTOR: [u8; 4] = ITIP20::transferCall::SELECTOR;
const TIP20_APPROVE_SELECTOR: [u8; 4] = ITIP20::approveCall::SELECTOR;
const TIP20_TRANSFER_WITH_MEMO_SELECTOR: [u8; 4] = ITIP20::transferWithMemoCall::SELECTOR;

/// Returns true if `selector` is one of TIP-20's recipient-bearing selectors
/// (`transfer`, `approve`, `transferWithMemo`). Mirrors writer
/// `account_keychain/mod.rs::is_constrained_tip20_selector`.
#[inline]
fn is_constrained_tip20_selector(selector: [u8; 4]) -> bool {
    matches!(
        selector,
        TIP20_TRANSFER_SELECTOR | TIP20_APPROVE_SELECTOR | TIP20_TRANSFER_WITH_MEMO_SELECTOR
    )
}

// ===========================================================================
// AuthorizedKey storage type
// ===========================================================================

/// Key information stored in the precompile.
///
/// Storage layout (packed into a single slot, right-aligned):
/// - byte 0: signature_type (u8)
/// - bytes 1-8: expiry (u64)
/// - byte 9: enforce_limits (bool)
/// - byte 10: is_revoked (bool)
/// - byte 11: is_admin (bool, T6+)
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthorizedKey {
    pub signature_type: u8,
    pub expiry: u64,
    pub enforce_limits: bool,
    pub is_revoked: bool,
    pub is_admin: bool,
}

impl StorableType for AuthorizedKey {
    const LAYOUT: Layout = Layout::Bytes(12);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for AuthorizedKey {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let word = storage.load(slot)?;
        let bytes = word.to_be_bytes::<32>();
        // Packed right-aligned (Tempo convention):
        //   byte 31: signature_type (u8, offset 0)
        //   bytes 23..31: expiry (u64, offset 1)
        //   byte 22: enforce_limits (bool, offset 9)
        //   byte 21: is_revoked (bool, offset 10)
        let signature_type = bytes[31];
        let expiry = u64::from_be_bytes(bytes[23..31].try_into().unwrap());
        let enforce_limits = bytes[22] != 0;
        let is_revoked = bytes[21] != 0;
        let is_admin = bytes[20] != 0;

        Ok(Self {
            signature_type,
            expiry,
            enforce_limits,
            is_revoked,
            is_admin,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[31] = self.signature_type;
        bytes[23..31].copy_from_slice(&self.expiry.to_be_bytes());
        bytes[22] = if self.enforce_limits { 1 } else { 0 };
        bytes[21] = if self.is_revoked { 1 } else { 0 };
        bytes[20] = if self.is_admin { 1 } else { 0 };
        storage.store(slot, U256::from_be_bytes(bytes))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)
    }
}

// ===========================================================================
// SpendingLimitState (T3+ periodic spending limit) — 2-slot layout
// ===========================================================================
//
// Mirrors writer `crates/precompiles/src/account_keychain/mod.rs:130-146`
// `#[derive(Storable)]` for the `SpendingLimitState` struct. Pre-T3 stored
// only `remaining` (slot+0 U256). T3+ extends with three packed fields in
// slot+1:
//
//   slot+0:  remaining (U256, full slot)
//   slot+1:  packed { max u128 @ bytes 0..16, period u64 @ bytes 16..24,
//                     period_end u64 @ bytes 24..32 }
//
// Pre-T3 data (slot+1 == 0) decodes as max=0, period=0, period_end=0 —
// matches writer's non-periodic semantic.

/// Per-token spending limit row for an access key (T3+: full 4 fields).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpendingLimitState {
    /// Remaining amount available to spend within the current window.
    pub remaining: U256,
    /// Configured cap, capped to TIP-20's `u128` supply range on T3+.
    pub max: u128,
    /// Period length in seconds. `0` means non-periodic / one-shot.
    pub period: u64,
    /// End timestamp of the current rolling window.
    pub period_end: u64,
}

impl SpendingLimitState {
    /// Computes the next `period_end` after `current_timestamp` has crossed
    /// the existing window. Mirrors writer
    /// `account_keychain/mod.rs:151-160 compute_next_period_end`.
    ///
    /// **Caller MUST ensure** `self.period != 0` (non-periodic limits don't
    /// rollover). Saturating arithmetic on extreme timestamps avoids overflow.
    pub fn compute_next_period_end(&self, current_timestamp: u64) -> u64 {
        debug_assert!(self.period != 0, "period rollover requires non-zero period");
        let elapsed = current_timestamp.saturating_sub(self.period_end);
        let periods_elapsed = (elapsed / self.period).saturating_add(1);
        let advance = self.period.saturating_mul(periods_elapsed);
        self.period_end.saturating_add(advance)
    }
}

const SPENDING_LIMIT_MAX_OFFSET: usize = 0;
const SPENDING_LIMIT_PERIOD_OFFSET: usize = 16;
const SPENDING_LIMIT_PERIOD_END_OFFSET: usize = 24;
const SPENDING_LIMIT_MAX_BYTES: usize = 16;
const SPENDING_LIMIT_PERIOD_BYTES: usize = 8;
const SPENDING_LIMIT_PERIOD_END_BYTES: usize = 8;

impl StorableType for SpendingLimitState {
    const LAYOUT: Layout = Layout::Slots(2);
    type Handler = SpendingLimitStateHandler;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        SpendingLimitStateHandler::new(slot, address)
    }
}

impl Storable for SpendingLimitState {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let remaining = storage.load(slot)?;
        let packed = storage.load(slot + U256::ONE)?;
        let max = packing::extract_from_word::<u128>(
            packed,
            SPENDING_LIMIT_MAX_OFFSET,
            SPENDING_LIMIT_MAX_BYTES,
        )?;
        let period = packing::extract_from_word::<u64>(
            packed,
            SPENDING_LIMIT_PERIOD_OFFSET,
            SPENDING_LIMIT_PERIOD_BYTES,
        )?;
        let period_end = packing::extract_from_word::<u64>(
            packed,
            SPENDING_LIMIT_PERIOD_END_OFFSET,
            SPENDING_LIMIT_PERIOD_END_BYTES,
        )?;
        Ok(Self {
            remaining,
            max,
            period,
            period_end,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, self.remaining)?;
        let mut packed = U256::ZERO;
        packed = packing::insert_into_word(
            packed,
            &self.max,
            SPENDING_LIMIT_MAX_OFFSET,
            SPENDING_LIMIT_MAX_BYTES,
        )?;
        packed = packing::insert_into_word(
            packed,
            &self.period,
            SPENDING_LIMIT_PERIOD_OFFSET,
            SPENDING_LIMIT_PERIOD_BYTES,
        )?;
        packed = packing::insert_into_word(
            packed,
            &self.period_end,
            SPENDING_LIMIT_PERIOD_END_OFFSET,
            SPENDING_LIMIT_PERIOD_END_BYTES,
        )?;
        storage.store(slot + U256::ONE, packed)
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)?;
        storage.store(slot + U256::ONE, U256::ZERO)
    }
}

/// Field-level handler for [`SpendingLimitState`]. Each field is exposed as a
/// `Slot<T>` with the right packed offset so callers can read or write one
/// field without touching the others (mirrors writer's auto-derived
/// `SpendingLimitStateHandler`).
pub struct SpendingLimitStateHandler {
    /// `remaining` at slot+0 (full U256 slot).
    pub remaining: Slot<U256>,
    /// `max` at slot+1, packed offset 0 (16 bytes).
    pub max: Slot<u128>,
    /// `period` at slot+1, packed offset 16 (8 bytes).
    pub period: Slot<u64>,
    /// `period_end` at slot+1, packed offset 24 (8 bytes).
    pub period_end: Slot<u64>,
    base_slot: U256,
    address: Address,
}

impl SpendingLimitStateHandler {
    fn new(base_slot: U256, address: Address) -> Self {
        let packed_slot = base_slot + U256::ONE;
        Self {
            remaining: Slot::new(base_slot, address),
            max: Slot::new_with_ctx(
                packed_slot,
                LayoutCtx::packed(SPENDING_LIMIT_MAX_OFFSET),
                address,
            ),
            period: Slot::new_with_ctx(
                packed_slot,
                LayoutCtx::packed(SPENDING_LIMIT_PERIOD_OFFSET),
                address,
            ),
            period_end: Slot::new_with_ctx(
                packed_slot,
                LayoutCtx::packed(SPENDING_LIMIT_PERIOD_END_OFFSET),
                address,
            ),
            base_slot,
            address,
        }
    }
}

impl Handler<SpendingLimitState> for SpendingLimitStateHandler {
    fn read(&self) -> Result<SpendingLimitState> {
        Ok(SpendingLimitState {
            remaining: self.remaining.read()?,
            max: self.max.read()?,
            period: self.period.read()?,
            period_end: self.period_end.read()?,
        })
    }

    fn write(&mut self, value: SpendingLimitState) -> Result<()> {
        // Write the packed slot in one shot to match writer's auto-derive
        // (which avoids 3 RMW SSTOREs for the packed fields).
        self.remaining.write(value.remaining)?;
        let mut packed = U256::ZERO;
        packed = packing::insert_into_word(
            packed,
            &value.max,
            SPENDING_LIMIT_MAX_OFFSET,
            SPENDING_LIMIT_MAX_BYTES,
        )?;
        packed = packing::insert_into_word(
            packed,
            &value.period,
            SPENDING_LIMIT_PERIOD_OFFSET,
            SPENDING_LIMIT_PERIOD_BYTES,
        )?;
        packed = packing::insert_into_word(
            packed,
            &value.period_end,
            SPENDING_LIMIT_PERIOD_END_OFFSET,
            SPENDING_LIMIT_PERIOD_END_BYTES,
        )?;
        let mut packed_slot = Slot::<U256>::new(self.base_slot + U256::ONE, self.address);
        packed_slot.write(packed)
    }

    fn delete(&mut self) -> Result<()> {
        let mut slot0 = Slot::<U256>::new(self.base_slot, self.address);
        slot0.write(U256::ZERO)?;
        let mut slot1 = Slot::<U256>::new(self.base_slot + U256::ONE, self.address);
        slot1.write(U256::ZERO)
    }

    fn t_read(&self) -> Result<SpendingLimitState> {
        Err(TempoPrecompileError::Fatal(
            "SpendingLimitState does not support transient storage".into(),
        ))
    }

    fn t_write(&mut self, _value: SpendingLimitState) -> Result<()> {
        Err(TempoPrecompileError::Fatal(
            "SpendingLimitState does not support transient storage".into(),
        ))
    }

    fn t_delete(&mut self) -> Result<()> {
        Err(TempoPrecompileError::Fatal(
            "SpendingLimitState does not support transient storage".into(),
        ))
    }
}

// ===========================================================================
// AccountKeychain struct
// ===========================================================================

/// Account Keychain precompile for managing authorized keys (session keys, spending limits).
pub struct AccountKeychain {
    // Slot 0: keys[account][keyId] -> AuthorizedKey
    pub(crate) keys: Mapping<Address, Mapping<Address, AuthorizedKey>>,
    // Slot 1: spending_limits[hash(account,keyId)][token] -> SpendingLimitState
    // (pre-T3: only `.remaining` is meaningful; T3+ adds max/period/period_end
    //  packed in slot+1)
    pub(crate) spending_limits: Mapping<B256, Mapping<Address, SpendingLimitState>>,
    // Slot 2 (T3+, TIP-1011): key_scopes[hash(account, keyId)] -> KeyScope
    //
    // Per-entry layout (mirrors writer's `#[derive(Storable)]` for KeyScope;
    // 4 reserved slots starting at `keccak256(key . slot_be_32)`):
    //     +0: is_scoped (bool)
    //     +1, +2: targets Set<Address> (vec length at +1, positions Mapping at +2)
    //     +3: target_scopes Mapping<Address, TargetScope> base slot
    //   target_scopes[t] (3 slots): selectors Set<bytes4> at +0,+1 ; selector_scopes Mapping at +2
    //     selector_scopes[s] (2 slots): recipients Set<Address> at +0,+1
    //
    // Reads and writes are wired in via helper functions (`key_scope_base`,
    // `target_scope_base`, `selector_scope_base`) that compute the nested
    // base slot and pass it to a fresh `SetHandler`/`Slot`/`Mapping` instance.
    pub(crate) call_scope_base: U256,
    // Slot 3 (T5+, TIP-1053): burned witnesses by account.
    pub(crate) key_authorization_witnesses: Mapping<Address, Mapping<B256, bool>>,

    pub address: Address,
    pub storage: StorageCtx,
}

/// Returns `(transaction_key, tx_origin)` transient slots for the active layout.
pub const fn transient_slots(spec: crate::tempo::hardfork::TempoHardfork) -> (u64, u64) {
    if spec.is_t5() {
        (4, 5)
    } else if spec.is_t3() {
        (3, 4)
    } else {
        (2, 3)
    }
}

impl AccountKeychain {
    pub fn new() -> Self {
        let address = ACCOUNT_KEYCHAIN_ADDRESS;
        Self {
            keys: Mapping::new(U256::from(0), address),
            spending_limits: Mapping::new(U256::from(1), address),
            call_scope_base: U256::from(2),
            key_authorization_witnesses: Mapping::new(U256::from(3), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    fn transaction_key(&self) -> Slot<Address> {
        let (slot, _) = transient_slots(self.storage.spec());
        Slot::new(U256::from(slot), self.address)
    }

    fn tx_origin(&self) -> Slot<Address> {
        let (_, slot) = transient_slots(self.storage.spec());
        Slot::new(U256::from(slot), self.address)
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }

    /// Initializes the account keychain precompile.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    /// Computes the hash key for spending limits mapping from account and keyId.
    pub fn spending_limit_key(account: Address, key_id: Address) -> B256 {
        let mut data = [0u8; 40];
        data[..20].copy_from_slice(account.as_slice());
        data[20..].copy_from_slice(key_id.as_slice());
        keccak256(data)
    }

    /// (T3+) Cap a spending-limit input to TIP-20's `u128` supply range.
    /// Mirrors writer `account_keychain/mod.rs:175-182`.
    #[inline]
    fn t3_spending_limit_cap(limit: U256) -> Result<u128> {
        if limit > U256::from(u128::MAX) {
            return Err(err_invalid_spending_limit());
        }
        Ok(limit.to::<u128>())
    }

    /// Ensures admin operations are authorized for this caller.
    ///
    /// Rules:
    /// - transaction must be signed by the root key, or on T6+ an active admin access key
    /// - T2+: caller must match tx.origin (prevents confused-deputy self-administration)
    fn ensure_admin_caller(&self, msg_sender: Address) -> Result<()> {
        let transaction_key = self.transaction_key().t_read()?;
        if !transaction_key.is_zero()
            && (!self.storage.spec().is_t6()
                || !self.is_admin_key(msg_sender, transaction_key)?)
        {
            return Err(err_unauthorized_caller());
        }

        if self.storage.spec().is_t2() {
            let tx_origin = self.tx_origin().t_read()?;
            if tx_origin.is_zero() || tx_origin != msg_sender {
                return Err(err_unauthorized_caller());
            }
        }

        Ok(())
    }

    /// Registers a new access key with signature type, expiry, and optional per-token spending limits.
    pub fn authorize_key(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::authorizeKey_0Call,
    ) -> Result<()> {
        if self.storage.spec().is_t3() {
            return Err(err_legacy_authorize_key_selector_changed(
                IAccountKeychain::authorizeKey_1Call::SELECTOR,
            ));
        }
        self.ensure_admin_caller(msg_sender)?;

        if call.keyId == Address::ZERO {
            return Err(err_zero_public_key());
        }

        // T0+: Expiry must be in the future
        // leafage always runs latest spec, so this is always enforced
        let current_timestamp: u64 = self.storage.timestamp().to::<u64>();
        if call.expiry <= current_timestamp {
            return Err(err_expiry_in_past());
        }

        // Check if key already exists (expiry > 0 means key exists)
        let existing_key = self.keys[msg_sender][call.keyId].read()?;
        if existing_key.expiry > 0 {
            return Err(err_key_already_exists());
        }

        // Check if previously revoked -- prevents replay attacks
        if existing_key.is_revoked {
            return Err(err_key_already_revoked());
        }

        // Convert SignatureType enum to u8
        let signature_type = match call.signatureType {
            IAccountKeychain::SignatureType::Secp256k1 => 0u8,
            IAccountKeychain::SignatureType::P256 => 1u8,
            IAccountKeychain::SignatureType::WebAuthn => 2u8,
            _ => return Err(err_invalid_signature_type()),
        };

        let new_key = AuthorizedKey {
            signature_type,
            expiry: call.expiry,
            enforce_limits: call.enforceLimits,
            is_revoked: false,
            is_admin: false,
        };

        self.keys[msg_sender][call.keyId].write(new_key)?;

        // Set initial spending limits (only if enforce_limits is true).
        // T3+ also caps `max` to TIP-20's u128 supply range so the cap field
        // populates the new layout's slot+1 packed `max` slot. Pre-T3 leaves
        // max=0 (non-periodic legacy behaviour).
        if call.enforceLimits {
            let limit_key = Self::spending_limit_key(msg_sender, call.keyId);
            let is_t3 = self.storage.spec().is_t3();
            for limit in call.limits {
                let max = if is_t3 {
                    Self::t3_spending_limit_cap(limit.amount)?
                } else {
                    0
                };
                let state = SpendingLimitState {
                    remaining: limit.amount,
                    max,
                    period: 0,
                    period_end: 0,
                };
                self.spending_limits[limit_key][limit.token].write(state)?;
            }
        }

        self.emit_event(IAccountKeychain::KeyAuthorized {
            account: msg_sender,
            publicKey: call.keyId,
            signatureType: signature_type,
            expiry: call.expiry,
        })
    }

    /// T3+ key authorization entry point, optionally carrying a T5 witness.
    fn authorize_key_with_restrictions(
        &mut self,
        msg_sender: Address,
        key_id: Address,
        signature_type: IAccountKeychain::SignatureType,
        config: IAccountKeychain::KeyRestrictions,
        witness: Option<B256>,
    ) -> Result<()> {
        self.authorize_key_with_restrictions_internal(
            msg_sender,
            key_id,
            signature_type,
            config,
            witness,
            false,
        )
    }

    fn authorize_key_with_restrictions_internal(
        &mut self,
        msg_sender: Address,
        key_id: Address,
        signature_type: IAccountKeychain::SignatureType,
        config: IAccountKeychain::KeyRestrictions,
        witness: Option<B256>,
        is_admin: bool,
    ) -> Result<()> {
        self.ensure_admin_caller(msg_sender)?;

        if key_id.is_zero() {
            return Err(err_zero_public_key());
        }
        if is_admin && key_id == msg_sender {
            return Err(err_invalid_key_id());
        }

        let now = self.storage.timestamp().to::<u64>();
        if config.expiry <= now {
            return Err(err_expiry_in_past());
        }

        let existing_key = self.keys[msg_sender][key_id].read()?;
        if existing_key.expiry > 0 {
            return Err(err_key_already_exists());
        }
        if existing_key.is_revoked {
            return Err(err_key_already_revoked());
        }

        let signature_type = match signature_type {
            IAccountKeychain::SignatureType::Secp256k1 => 0u8,
            IAccountKeychain::SignatureType::P256 => 1u8,
            IAccountKeychain::SignatureType::WebAuthn => 2u8,
            _ => return Err(err_invalid_signature_type()),
        };

        if config.enforceLimits {
            let mut seen_tokens = HashSet::with_capacity(config.limits.len());
            for limit in &config.limits {
                if !seen_tokens.insert(limit.token) {
                    return Err(err_invalid_spending_limit());
                }
                Self::t3_spending_limit_cap(limit.amount)?;
            }
        }

        let allowed_calls = if config.allowAnyCalls {
            if self.storage.spec().is_t5() && !config.allowedCalls.is_empty() {
                return Err(err_invalid_call_scope());
            }
            None
        } else {
            self.validate_call_scopes(&config.allowedCalls)?;
            Some(config.allowedCalls.as_slice())
        };

        if let Some(witness) = witness {
            self.ensure_key_authorization_witness_not_burned(msg_sender, witness)?;
        }

        self.keys[msg_sender][key_id].write(AuthorizedKey {
            signature_type,
            expiry: config.expiry,
            enforce_limits: config.enforceLimits,
            is_revoked: false,
            is_admin,
        })?;

        if !is_admin && config.enforceLimits {
            let limit_key = Self::spending_limit_key(msg_sender, key_id);
            for limit in &config.limits {
                let period_end = if limit.period == 0 {
                    0
                } else {
                    now.saturating_add(limit.period)
                };
                self.spending_limits[limit_key][limit.token].write(SpendingLimitState {
                    remaining: limit.amount,
                    max: Self::t3_spending_limit_cap(limit.amount)?,
                    period: limit.period,
                    period_end,
                })?;
            }
        }

        if !is_admin {
            if let Some(scopes) = allowed_calls {
            let key_hash = Self::spending_limit_key(msg_sender, key_id);
            for scope in scopes {
                self.upsert_target_scope(key_hash, scope)?;
            }
            self.is_scoped_slot(key_hash).write(true)?;
            }
        }

        if let Some(witness) = witness {
            self.emit_event(IAccountKeychain::KeyAuthorizationWitness {
                account: msg_sender,
                witness,
            })?;
        }

        self.emit_event(IAccountKeychain::KeyAuthorized {
            account: msg_sender,
            publicKey: key_id,
            signatureType: signature_type,
            expiry: config.expiry,
        })
    }

    fn authorize_admin_key(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::authorizeAdminKeyCall,
    ) -> Result<()> {
        let key_id = call.keyId;
        self.authorize_key_with_restrictions_internal(
            msg_sender,
            key_id,
            call.signatureType,
            IAccountKeychain::KeyRestrictions {
                expiry: u64::MAX,
                enforceLimits: false,
                limits: Vec::new(),
                allowAnyCalls: true,
                allowedCalls: Vec::new(),
            },
            Some(call.witness),
            true,
        )?;
        self.emit_event(IAccountKeychain::AdminKeyAuthorized {
            account: msg_sender,
            publicKey: key_id,
        })
    }

    fn ensure_key_authorization_witness_not_burned(
        &self,
        account: Address,
        witness: B256,
    ) -> Result<()> {
        if self.key_authorization_witnesses[account][witness].read()? {
            return Err(err_key_authorization_witness_already_burned());
        }
        Ok(())
    }

    fn burn_key_authorization_witness(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::burnKeyAuthorizationWitnessCall,
    ) -> Result<()> {
        self.ensure_admin_caller(msg_sender)?;
        self.ensure_key_authorization_witness_not_burned(msg_sender, call.witness)?;
        self.key_authorization_witnesses[msg_sender][call.witness].write(true)?;
        self.emit_event(IAccountKeychain::KeyAuthorizationWitnessBurned {
            account: msg_sender,
            witness: call.witness,
        })
    }

    fn is_key_authorization_witness_burned(
        &self,
        call: IAccountKeychain::isKeyAuthorizationWitnessBurnedCall,
    ) -> Result<bool> {
        self.key_authorization_witnesses[call.account][call.witness].read()
    }

    /// Permanently revokes an access key.
    pub fn revoke_key(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::revokeKeyCall,
    ) -> Result<()> {
        self.ensure_admin_caller(msg_sender)?;

        let key = self.keys[msg_sender][call.keyId].read()?;
        if key.expiry == 0 {
            return Err(err_key_not_found());
        }

        let revoked_key = AuthorizedKey {
            is_revoked: true,
            ..Default::default()
        };
        self.keys[msg_sender][call.keyId].write(revoked_key)?;

        self.emit_event(IAccountKeychain::KeyRevoked {
            account: msg_sender,
            publicKey: call.keyId,
        })
    }

    /// Updates the spending limit for a key-token pair.
    pub fn update_spending_limit(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::updateSpendingLimitCall,
    ) -> Result<()> {
        self.ensure_admin_caller(msg_sender)?;

        let mut key = self.load_active_key(msg_sender, call.keyId)?;

        let current_timestamp: u64 = self.storage.timestamp().to::<u64>();
        if current_timestamp >= key.expiry {
            return Err(err_key_expired());
        }
        if key.is_admin {
            return Err(err_invalid_key_id());
        }

        // If this key had unlimited spending, enable limits now
        if !key.enforce_limits {
            key.enforce_limits = true;
            self.keys[msg_sender][call.keyId].write(key)?;
        }

        // Update the spending limit. T3+ updates both remaining + max while
        // preserving period + period_end (read-modify-write). Pre-T3 only
        // touches the `.remaining` sub-slot to match writer's storage diff.
        let limit_key = Self::spending_limit_key(msg_sender, call.keyId);
        if self.storage.spec().is_t3() {
            let max = Self::t3_spending_limit_cap(call.newLimit)?;
            let handler = &mut self.spending_limits[limit_key][call.token];
            let mut state = handler.read()?;
            state.remaining = call.newLimit;
            state.max = max;
            handler.write(state)?;
        } else {
            self.spending_limits[limit_key][call.token]
                .remaining
                .write(call.newLimit)?;
        }

        self.emit_event(IAccountKeychain::SpendingLimitUpdated {
            account: msg_sender,
            publicKey: call.keyId,
            token: call.token,
            newLimit: call.newLimit,
        })
    }

    /// Returns key info for the given account-key pair.
    pub fn get_key(&self, call: IAccountKeychain::getKeyCall) -> Result<IAccountKeychain::KeyInfo> {
        let key = self.keys[call.account][call.keyId].read()?;

        // Key doesn't exist if expiry == 0, or key has been revoked
        if key.expiry == 0 || key.is_revoked {
            return Ok(IAccountKeychain::KeyInfo {
                signatureType: IAccountKeychain::SignatureType::Secp256k1,
                keyId: Address::ZERO,
                expiry: 0,
                enforceLimits: false,
                isRevoked: key.is_revoked,
            });
        }

        let signature_type = match key.signature_type {
            0 => IAccountKeychain::SignatureType::Secp256k1,
            1 => IAccountKeychain::SignatureType::P256,
            2 => IAccountKeychain::SignatureType::WebAuthn,
            _ => IAccountKeychain::SignatureType::Secp256k1,
        };

        Ok(IAccountKeychain::KeyInfo {
            signatureType: signature_type,
            keyId: call.keyId,
            expiry: key.expiry,
            enforceLimits: key.enforce_limits,
            isRevoked: key.is_revoked,
        })
    }

    /// Returns the remaining spending limit for a key-token pair, or zero if the key
    /// doesn't exist or has been revoked (T2+).
    pub fn get_remaining_limit(
        &self,
        call: IAccountKeychain::getRemainingLimitCall,
    ) -> Result<U256> {
        // T2+: return zero if key doesn't exist or has been revoked
        if self.storage.spec().is_t2() {
            let key = self.keys[call.account][call.keyId].read()?;
            if key.expiry == 0 || key.is_revoked {
                return Ok(U256::ZERO);
            }
        }

        let limit_key = Self::spending_limit_key(call.account, call.keyId);
        self.spending_limits[limit_key][call.token].remaining.read()
    }

    /// Returns the access key used to authorize the current transaction.
    pub fn get_transaction_key(
        &self,
        _call: IAccountKeychain::getTransactionKeyCall,
        _msg_sender: Address,
    ) -> Result<Address> {
        self.transaction_key().t_read()
    }

    /// Internal: Set the transaction key (called during transaction validation).
    pub fn set_transaction_key(&mut self, key_id: Address) -> Result<()> {
        self.transaction_key().t_write(key_id)
    }

    /// Sets the transaction origin for the current transaction.
    pub fn set_tx_origin(&mut self, origin: Address) -> Result<()> {
        self.tx_origin().t_write(origin)
    }

    /// Load and validate a key exists and is not revoked.
    fn load_active_key(&self, account: Address, key_id: Address) -> Result<AuthorizedKey> {
        let key = self.keys[account][key_id].read()?;

        if key.is_revoked {
            return Err(err_key_already_revoked());
        }

        if key.expiry == 0 {
            return Err(err_key_not_found());
        }

        Ok(key)
    }

    /// Validate keychain authorization (existence, revocation, expiry, and optionally signature type).
    ///
    /// This is called by the transaction validation logic, not directly via ABI dispatch.
    pub fn validate_keychain_authorization(
        &self,
        account: Address,
        key_id: Address,
        current_timestamp: u64,
        expected_sig_type: Option<u8>,
    ) -> Result<()> {
        let key = self.load_active_key(account, key_id)?;

        if current_timestamp >= key.expiry {
            return Err(err_key_expired());
        }

        if let Some(sig_type) = expected_sig_type {
            if key.signature_type != sig_type {
                return Err(err_signature_type_mismatch(key.signature_type, sig_type));
            }
        }

        Ok(())
    }

    /// Returns true for the account's implicit root key or an active T6 admin access key.
    pub fn is_admin_key(&self, account: Address, key_id: Address) -> Result<bool> {
        if key_id == account {
            return Ok(true);
        }

        let key = match self.load_active_key(account, key_id) {
            Ok(key) => key,
            Err(error) if error.is_system_error() => return Err(error),
            Err(_) => return Ok(false),
        };
        if self.storage.timestamp().to::<u64>() >= key.expiry {
            return Ok(false);
        }
        Ok(key.is_admin)
    }

    /// Returns whether `key_id` is a present, unrevoked, unexpired access key for `account`.
    pub fn is_active_key(&self, account: Address, key_id: Address) -> Result<bool> {
        match self.validate_keychain_authorization(
            account,
            key_id,
            self.storage.timestamp().to::<u64>(),
            None,
        ) {
            Ok(()) => Ok(true),
            Err(error) if error.is_system_error() => Err(error),
            Err(_) => Ok(false),
        }
    }

    /// Deducts `amount` from the key's remaining spending limit for `token`.
    pub fn verify_and_update_spending(
        &mut self,
        account: Address,
        key_id: Address,
        token: Address,
        amount: U256,
    ) -> Result<()> {
        if key_id == Address::ZERO {
            return Ok(());
        }

        let key = self.load_active_key(account, key_id)?;

        if !key.enforce_limits {
            return Ok(());
        }

        let limit_key = Self::spending_limit_key(account, key_id);

        if !self.storage.spec().is_t3() {
            let remaining = self.spending_limits[limit_key][token].remaining.read()?;
            if amount > remaining {
                return Err(err_spending_limit_exceeded());
            }
            return self.spending_limits[limit_key][token]
                .remaining
                .write(remaining - amount);
        }

        // T3+ periodic reset: if the active window has rolled over, refill
        // `remaining` to `max` and advance `period_end` by full period(s)
        // before applying the spend. Mirrors writer L1117-1180 +
        // SpendingLimitState::compute_next_period_end L151-160. Both the
        // rolled-over window and the deducted `remaining` are committed in a
        // single SSTORE per slot (matches writer L1171-1173).
        let mut state = self.spending_limits[limit_key][token].read()?;
        let now: u64 = self.storage.timestamp().to::<u64>();
        let is_periodic = state.period != 0;

        if is_periodic && now >= state.period_end {
            state.period_end = state.compute_next_period_end(now);
            state.remaining = U256::from(state.max);
        }

        if amount > state.remaining {
            return Err(err_spending_limit_exceeded());
        }

        let new_remaining = state.remaining - amount;
        if is_periodic {
            state.remaining = new_remaining;
            self.spending_limits[limit_key][token].write(state)
        } else {
            self.spending_limits[limit_key][token]
                .remaining
                .write(new_remaining)
        }
    }

    /// Refund spending limit after a fee refund.
    pub fn refund_spending_limit(
        &mut self,
        account: Address,
        token: Address,
        amount: U256,
    ) -> Result<()> {
        let transaction_key = self.transaction_key().t_read()?;

        if transaction_key == Address::ZERO {
            return Ok(());
        }

        let tx_origin = self.tx_origin().t_read()?;
        if account != tx_origin {
            return Ok(());
        }

        let key = match self.load_active_key(account, transaction_key) {
            Ok(key) => key,
            Err(_) => return Ok(()),
        };

        if !key.enforce_limits {
            return Ok(());
        }

        let limit_key = Self::spending_limit_key(account, transaction_key);
        // T3+ clamps `remaining + amount` to the configured `max` so refunds
        // can't overflow the spending budget. Pre-T3 keeps the saturating-add
        // semantic. Mirrors writer L1199-1250: legacy pre-T3 rows persisted only
        // `remaining`, so they deserialize with `max == 0`; for those we must
        // skip the clamp (otherwise `.min(0)` would zero out `remaining`).
        let new_remaining = if self.storage.spec().is_t3() {
            let handler = &self.spending_limits[limit_key][token];
            let state = handler.read()?;
            let refunded = state.remaining.saturating_add(amount);
            if state.max == 0 {
                refunded
            } else {
                refunded.min(U256::from(state.max))
            }
        } else {
            let remaining = self.spending_limits[limit_key][token].remaining.read()?;
            remaining.saturating_add(amount)
        };
        self.spending_limits[limit_key][token]
            .remaining
            .write(new_remaining)
    }

    /// Authorize a token transfer with access key spending limits.
    pub fn authorize_transfer(
        &mut self,
        account: Address,
        token: Address,
        amount: U256,
    ) -> Result<()> {
        let transaction_key = self.transaction_key().t_read()?;

        if transaction_key == Address::ZERO {
            return Ok(());
        }

        let tx_origin = self.tx_origin().t_read()?;
        if account != tx_origin {
            return Ok(());
        }

        self.verify_and_update_spending(account, transaction_key, token, amount)
    }

    /// Authorize a token approval with access key spending limits.
    #[allow(dead_code)]
    pub fn authorize_approve(
        &mut self,
        account: Address,
        token: Address,
        old_approval: U256,
        new_approval: U256,
    ) -> Result<()> {
        let transaction_key = self.transaction_key().t_read()?;

        if transaction_key == Address::ZERO {
            return Ok(());
        }

        let tx_origin = self.tx_origin().t_read()?;
        if account != tx_origin {
            return Ok(());
        }

        let approval_increase = new_approval.saturating_sub(old_approval);
        if approval_increase.is_zero() {
            return Ok(());
        }

        self.verify_and_update_spending(account, transaction_key, token, approval_increase)
    }

    // -----------------------------------------------------------------------
    // CallScope (TIP-1011, T3+) — slot computation helpers
    // -----------------------------------------------------------------------
    //
    // Slot map (recap of the `call_scope_base` field doc):
    //
    //   key_scope_base(key_hash):
    //     +0  is_scoped              (bool)
    //     +1  targets vec length     | Set<Address>
    //     +2  targets positions base | Mapping<Address, u32>
    //     +3  target_scopes base     | Mapping<Address, TargetScope>
    //
    //   target_scope_base(key_hash, target):
    //     +0  selectors vec length          | Set<FixedBytes<4>>
    //     +1  selectors positions base      | Mapping<FixedBytes<4>, u32>
    //     +2  selector_scopes base          | Mapping<FixedBytes<4>, SelectorScope>
    //
    //   selector_scope_base(key_hash, target, selector):
    //     +0  recipients vec length         | Set<Address>
    //     +1  recipients positions base     | Mapping<Address, u32>

    #[inline]
    fn key_scope_base(&self, key_hash: B256) -> U256 {
        key_hash.mapping_slot(self.call_scope_base)
    }

    fn is_scoped_slot(&self, key_hash: B256) -> Slot<bool> {
        Slot::new(self.key_scope_base(key_hash), self.address)
    }

    fn targets_handler(&self, key_hash: B256) -> SetHandler<Address> {
        SetHandler::new(self.key_scope_base(key_hash) + U256::ONE, self.address)
    }

    #[inline]
    fn target_scope_base(&self, key_hash: B256, target: Address) -> U256 {
        let target_scopes_map_base = self.key_scope_base(key_hash) + U256::from(3u8);
        target.mapping_slot(target_scopes_map_base)
    }

    fn selectors_handler(&self, key_hash: B256, target: Address) -> SetHandler<FixedBytes<4>> {
        SetHandler::new(self.target_scope_base(key_hash, target), self.address)
    }

    #[inline]
    fn selector_scope_base(
        &self,
        key_hash: B256,
        target: Address,
        selector: FixedBytes<4>,
    ) -> U256 {
        let selector_scopes_map_base = self.target_scope_base(key_hash, target) + U256::from(2u8);
        selector.mapping_slot(selector_scopes_map_base)
    }

    fn recipients_handler(
        &self,
        key_hash: B256,
        target: Address,
        selector: FixedBytes<4>,
    ) -> SetHandler<Address> {
        SetHandler::new(
            self.selector_scope_base(key_hash, target, selector),
            self.address,
        )
    }

    // -----------------------------------------------------------------------
    // CallScope — public dispatch entries
    // -----------------------------------------------------------------------

    /// (T3+) Set or replace allowed calls for one or more (key, target) pairs.
    /// Mirrors writer `account_keychain/mod.rs:462-487 set_allowed_calls`.
    pub fn set_allowed_calls(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::setAllowedCallsCall,
    ) -> Result<()> {
        if !self.storage.spec().is_t3() {
            return Err(err_invalid_call_scope());
        }
        self.ensure_admin_caller(msg_sender)?;

        let current_timestamp: u64 = self.storage.timestamp().to::<u64>();
        let key = self.load_active_key(msg_sender, call.keyId)?;
        if current_timestamp >= key.expiry {
            return Err(err_key_expired());
        }
        if key.is_admin {
            return Err(err_invalid_key_id());
        }

        let key_hash = Self::spending_limit_key(msg_sender, call.keyId);
        let scopes = call.scopes;
        if scopes.is_empty() {
            return Err(err_invalid_call_scope());
        }

        self.validate_call_scopes(&scopes)?;

        for scope in &scopes {
            self.upsert_target_scope(key_hash, scope)?;
        }

        self.is_scoped_slot(key_hash).write(true)
    }

    /// (T3+) Remove any configured call scope for a (key, target) pair.
    /// Mirrors writer `account_keychain/mod.rs:489-509 remove_allowed_calls`.
    pub fn remove_allowed_calls(
        &mut self,
        msg_sender: Address,
        call: IAccountKeychain::removeAllowedCallsCall,
    ) -> Result<()> {
        self.ensure_admin_caller(msg_sender)?;

        let current_timestamp: u64 = self.storage.timestamp().to::<u64>();
        let key = self.load_active_key(msg_sender, call.keyId)?;
        if current_timestamp >= key.expiry {
            return Err(err_key_expired());
        }
        if key.is_admin {
            return Err(err_invalid_key_id());
        }

        let key_hash = Self::spending_limit_key(msg_sender, call.keyId);
        if !self.is_scoped_slot(key_hash).read()? {
            return Ok(());
        }

        self.remove_target_scope(key_hash, call.target)
    }

    /// (T3+) Returns whether the key is call-scoped + the configured scopes.
    /// Mirrors writer `account_keychain/mod.rs:516-584 get_allowed_calls`.
    pub fn get_allowed_calls(
        &self,
        call: IAccountKeychain::getAllowedCallsCall,
    ) -> Result<IAccountKeychain::getAllowedCallsReturn> {
        if call.keyId.is_zero() {
            return Ok(IAccountKeychain::getAllowedCallsReturn {
                isScoped: false,
                scopes: Vec::new(),
            });
        }

        let current_timestamp: u64 = self.storage.timestamp().to::<u64>();
        let key = self.keys[call.account][call.keyId].read()?;
        if key.expiry == 0 || key.is_revoked || current_timestamp >= key.expiry {
            return Ok(IAccountKeychain::getAllowedCallsReturn {
                isScoped: true,
                scopes: Vec::new(),
            });
        }

        let key_hash = Self::spending_limit_key(call.account, call.keyId);
        let is_scoped = self.is_scoped_slot(key_hash).read()?;

        if !is_scoped {
            return Ok(IAccountKeychain::getAllowedCallsReturn {
                isScoped: false,
                scopes: Vec::new(),
            });
        }

        let targets = self.targets_handler(key_hash).read()?;
        let mut scopes = Vec::new();
        for target in targets.into_inner() {
            let selectors = self.selectors_handler(key_hash, target).read()?;

            let scope = if selectors.as_slice().is_empty() {
                IAccountKeychain::CallScope {
                    target,
                    selectorRules: Vec::new(),
                }
            } else {
                let mut rules = Vec::new();
                for selector in selectors.into_inner() {
                    let recipients = self
                        .recipients_handler(key_hash, target, selector)
                        .read()?;
                    rules.push(IAccountKeychain::SelectorRule {
                        selector,
                        recipients: recipients.into_inner(),
                    });
                }
                IAccountKeychain::CallScope {
                    target,
                    selectorRules: rules,
                }
            };
            scopes.push(scope);
        }

        Ok(IAccountKeychain::getAllowedCallsReturn {
            isScoped: true,
            scopes,
        })
    }

    // -----------------------------------------------------------------------
    // CallScope — internal mutators / validators
    // -----------------------------------------------------------------------

    /// Creates or replaces one target scope, including all nested selector rules.
    /// Mirrors writer `account_keychain/mod.rs:826-869 upsert_target_scope`.
    fn upsert_target_scope(
        &mut self,
        key_hash: B256,
        scope: &IAccountKeychain::CallScope,
    ) -> Result<()> {
        let target = scope.target;

        // Pre-T4: validate per-scope inline (T4 short-circuits to format check
        // only, performed up front in `validate_call_scopes`). FU-5 wires the
        // hardfork-specific TIP20 lookup; for now the T3 path is delegated to
        // `validate_call_scope` which falls back to format checks.
        if !self.storage.spec().is_t4() {
            self.validate_call_scope(scope)?;
        }

        self.targets_handler(key_hash).insert(target)?;
        self.clear_target_selectors(key_hash, target)?;

        if scope.selectorRules.is_empty() {
            // Keeping the target while clearing nested selector rows
            // intentionally widens this target to allow-all selectors.
            return Ok(());
        }

        for rule in &scope.selectorRules {
            let selector = rule.selector;
            self.selectors_handler(key_hash, target).insert(selector)?;

            if rule.recipients.is_empty() {
                if !self.storage.spec().is_t4() {
                    // Pre-T4 storage-touch parity with writer.
                    self.recipients_handler(key_hash, target, selector).delete()?;
                }
            } else {
                self.recipients_handler(key_hash, target, selector)
                    .write(StorageSet::from(rule.recipients.clone()))?;
            }
        }

        Ok(())
    }

    /// Clears the selectors set (and any per-selector recipient rows) for one target.
    /// Mirrors writer `account_keychain/mod.rs:798-824 clear_target_selectors`.
    fn clear_target_selectors(&mut self, key_hash: B256, target: Address) -> Result<()> {
        let mut selectors = self.selectors_handler(key_hash, target);
        let snapshot = selectors.read()?;
        for selector in snapshot.into_inner() {
            self.recipients_handler(key_hash, target, selector).delete()?;
        }
        selectors.delete()
    }

    /// Removes one (key, target) pair from the scope tree.
    /// Mirrors writer `account_keychain/mod.rs:777-797 remove_target_scope`.
    fn remove_target_scope(&mut self, key_hash: B256, target: Address) -> Result<()> {
        self.clear_target_selectors(key_hash, target)?;
        self.targets_handler(key_hash).remove(&target)?;
        Ok(())
    }

    /// Validates a list of `CallScope`s before persistence. Rejects duplicate
    /// targets and (post-T4) runs per-scope validation up front. Mirrors writer
    /// `account_keychain/mod.rs:871-885 validate_call_scopes`.
    fn validate_call_scopes(&self, scopes: &[IAccountKeychain::CallScope]) -> Result<()> {
        let mut seen_targets = HashSet::new();
        for scope in scopes {
            if !seen_targets.insert(scope.target) {
                return Err(err_invalid_call_scope());
            }
            if self.storage.spec().is_t4() {
                self.validate_call_scope(scope)?;
            }
        }
        Ok(())
    }

    /// Validates a single `CallScope`: zero-target rejected, then per-selector
    /// rules. Mirrors writer `account_keychain/mod.rs:887-900 validate_call_scope`.
    fn validate_call_scope(&self, scope: &IAccountKeychain::CallScope) -> Result<()> {
        if scope.target.is_zero() {
            return Err(err_invalid_call_scope());
        }
        if !scope.selectorRules.is_empty() {
            self.validate_selector_rules(scope.target, &scope.selectorRules)?;
        }
        Ok(())
    }

    /// Validates per-selector recipient rules for one target. Rejects duplicate
    /// selectors, duplicate recipients, zero recipients, and recipient-bearing
    /// rules on non-TIP-20 targets. Mirrors writer
    /// `account_keychain/mod.rs:902-947 validate_selector_rules`.
    ///
    /// **Hardfork behaviour** (mirrors writer L913-919):
    /// - Pre-T4: stateful `TIP20Factory::is_tip20(target)` — probes storage to
    ///   confirm the target is a deployed TIP-20.
    /// - T4+: stateless `target.is_tip20()` — only checks the address prefix.
    fn validate_selector_rules(
        &self,
        target: Address,
        rules: &[IAccountKeychain::SelectorRule],
    ) -> Result<()> {
        let mut cached_is_tip20: Option<bool> = None;
        let mut is_tip20 = || -> Result<bool> {
            if let Some(v) = cached_is_tip20 {
                return Ok(v);
            }
            let v = if !self.storage.spec().is_t4() {
                TIP20Factory::new().is_tip20(target)?
            } else {
                target.is_tip20()
            };
            cached_is_tip20 = Some(v);
            Ok(v)
        };

        let mut selectors = HashSet::new();
        for rule in rules {
            if !selectors.insert(rule.selector) {
                return Err(err_invalid_call_scope());
            }

            if rule.recipients.is_empty() {
                continue;
            }

            if !is_constrained_tip20_selector(*rule.selector) || !is_tip20()? {
                return Err(err_invalid_call_scope());
            }

            let mut unique_recipients = HashSet::new();
            for recipient in &rule.recipients {
                if recipient.is_zero() || !unique_recipients.insert(*recipient) {
                    return Err(err_invalid_call_scope());
                }
            }
        }

        Ok(())
    }
}

impl ContractStorage for AccountKeychain {
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

impl Precompile for AccountKeychain {
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
                IAccountKeychain::IAccountKeychainCalls::abi_decode_with_config(
                    data,
                    crate::tempo::precompile::abi_decoder_config(),
                )
            },
            |call| match call {
                IAccountKeychain::IAccountKeychainCalls::authorizeKey_0(call) => {
                    mutate_void(call, msg_sender, |sender, c| self.authorize_key(sender, c))
                }
                IAccountKeychain::IAccountKeychainCalls::authorizeKey_1(call) => {
                    if !self.storage.spec().is_t3() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    mutate_void(call, msg_sender, |sender, c| {
                        self.authorize_key_with_restrictions(
                            sender,
                            c.keyId,
                            c.signatureType,
                            c.config,
                            None,
                        )
                    })
                }
                IAccountKeychain::IAccountKeychainCalls::authorizeKey_2(call) => {
                    if !self.storage.spec().is_t5() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    mutate_void(call, msg_sender, |sender, c| {
                        self.authorize_key_with_restrictions(
                            sender,
                            c.keyId,
                            c.signatureType,
                            c.config,
                            Some(c.witness),
                        )
                    })
                }
                IAccountKeychain::IAccountKeychainCalls::authorizeAdminKey(call) => {
                    if !self.storage.spec().is_t6() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    mutate_void(call, msg_sender, |sender, call| {
                        self.authorize_admin_key(sender, call)
                    })
                }
                IAccountKeychain::IAccountKeychainCalls::burnKeyAuthorizationWitness(call) => {
                    if !self.storage.spec().is_t5() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    mutate_void(call, msg_sender, |sender, c| {
                        self.burn_key_authorization_witness(sender, c)
                    })
                }
                IAccountKeychain::IAccountKeychainCalls::revokeKey(call) => {
                    mutate_void(call, msg_sender, |sender, c| self.revoke_key(sender, c))
                }
                IAccountKeychain::IAccountKeychainCalls::updateSpendingLimit(call) => {
                    mutate_void(call, msg_sender, |sender, c| {
                        self.update_spending_limit(sender, c)
                    })
                }
                IAccountKeychain::IAccountKeychainCalls::getKey(call) => {
                    view(call, |c| self.get_key(c))
                }
                IAccountKeychain::IAccountKeychainCalls::getRemainingLimit(call) => {
                    view(call, |c| self.get_remaining_limit(c))
                }
                IAccountKeychain::IAccountKeychainCalls::getTransactionKey(call) => {
                    view(call, |c| self.get_transaction_key(c, msg_sender))
                }
                IAccountKeychain::IAccountKeychainCalls::isKeyAuthorizationWitnessBurned(call) => {
                    if !self.storage.spec().is_t5() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    view(call, |c| self.is_key_authorization_witness_burned(c))
                }
                IAccountKeychain::IAccountKeychainCalls::isAdminKey(call) => {
                    if !self.storage.spec().is_t6() {
                        return unknown_selector(selector, self.storage.gas_used());
                    }
                    view(call, |call| self.is_admin_key(call.account, call.keyId))
                }
                IAccountKeychain::IAccountKeychainCalls::setAllowedCalls(call) => {
                    mutate_void(call, msg_sender, |sender, c| self.set_allowed_calls(sender, c))
                }
                IAccountKeychain::IAccountKeychainCalls::removeAllowedCalls(call) => {
                    mutate_void(call, msg_sender, |sender, c| {
                        self.remove_allowed_calls(sender, c)
                    })
                }
                IAccountKeychain::IAccountKeychainCalls::getAllowedCalls(call) => {
                    view(call, |c| self.get_allowed_calls(c))
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::storage::with_read_only_storage_ctx;
    use crate::tempo::precompile::test_utils::TestStorageProvider;
    use alloy::primitives::address;
    use alloy::sol_types::SolEvent;
    use revm::database::EmptyDB;

    fn unrestricted_restrictions() -> IAccountKeychain::KeyRestrictions {
        IAccountKeychain::KeyRestrictions {
            expiry: 100,
            enforceLimits: false,
            limits: Vec::new(),
            allowAnyCalls: true,
            allowedCalls: Vec::new(),
        }
    }

    fn tip20_addr() -> Address {
        // 12-byte TIP-20 prefix (0x20C00000_0000_0000_0000_0000) + 8 random tail bytes
        address!("0x20C000000000000000000000DEADBEEFDEADBEEF")
    }

    fn non_tip20_addr() -> Address {
        address!("0xDEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF")
    }

    fn one_transfer_rule_with_recipient() -> Vec<IAccountKeychain::SelectorRule> {
        vec![IAccountKeychain::SelectorRule {
            selector: FixedBytes::from(TIP20_TRANSFER_SELECTOR),
            recipients: vec![Address::repeat_byte(1)],
        }]
    }

    #[test]
    fn t6_root_authorizes_admin_key() {
        let account = Address::repeat_byte(0x11);
        let admin_key = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);

        StorageCtx::enter(&mut provider, || -> Result<()> {
            let mut keychain = AccountKeychain::new();
            keychain.set_tx_origin(account)?;
            keychain.authorize_admin_key(
                account,
                IAccountKeychain::authorizeAdminKeyCall {
                    keyId: admin_key,
                    signatureType: IAccountKeychain::SignatureType::P256,
                    witness: B256::repeat_byte(3),
                },
            )?;

            let key = keychain.keys[account][admin_key].read()?;
            assert_eq!(key.signature_type, 1);
            assert_eq!(key.expiry, u64::MAX);
            assert!(!key.enforce_limits);
            assert!(!key.is_revoked);
            assert!(key.is_admin);
            assert!(keychain.is_admin_key(account, account)?);
            assert!(keychain.is_admin_key(account, admin_key)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t6_admin_key_can_manage_children_but_normal_key_cannot() {
        let account = Address::repeat_byte(0x31);
        let admin_key = Address::repeat_byte(0x32);
        let normal_key = Address::repeat_byte(0x33);
        let child_key = Address::repeat_byte(0x34);
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);

        StorageCtx::enter(&mut provider, || -> Result<()> {
            let mut keychain = AccountKeychain::new();
            keychain.set_tx_origin(account)?;
            keychain.authorize_admin_key(
                account,
                IAccountKeychain::authorizeAdminKeyCall {
                    keyId: admin_key,
                    signatureType: IAccountKeychain::SignatureType::Secp256k1,
                    witness: B256::ZERO,
                },
            )?;
            keychain.authorize_key_with_restrictions(
                account,
                normal_key,
                IAccountKeychain::SignatureType::Secp256k1,
                unrestricted_restrictions(),
                None,
            )?;

            keychain.set_transaction_key(admin_key)?;
            keychain.authorize_key_with_restrictions(
                account,
                child_key,
                IAccountKeychain::SignatureType::WebAuthn,
                unrestricted_restrictions(),
                None,
            )?;
            assert!(keychain.keys[account][child_key].read()?.expiry > 0);

            keychain.set_transaction_key(normal_key)?;
            let error = keychain
                .revoke_key(
                    account,
                    IAccountKeychain::revokeKeyCall { keyId: child_key },
                )
                .unwrap_err();
            assert_eq!(error.selector(), IAccountKeychain::UnauthorizedCaller::SELECTOR);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t6_admin_keys_cannot_be_root_or_receive_restrictions() {
        let account = Address::repeat_byte(0x41);
        let admin_key = Address::repeat_byte(0x42);
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);

        StorageCtx::enter(&mut provider, || -> Result<()> {
            let mut keychain = AccountKeychain::new();
            keychain.set_tx_origin(account)?;
            let root_error = keychain
                .authorize_admin_key(
                    account,
                    IAccountKeychain::authorizeAdminKeyCall {
                        keyId: account,
                        signatureType: IAccountKeychain::SignatureType::Secp256k1,
                        witness: B256::ZERO,
                    },
                )
                .unwrap_err();
            assert_eq!(root_error.selector(), IAccountKeychain::InvalidKeyId::SELECTOR);

            keychain.authorize_admin_key(
                account,
                IAccountKeychain::authorizeAdminKeyCall {
                    keyId: admin_key,
                    signatureType: IAccountKeychain::SignatureType::Secp256k1,
                    witness: B256::ZERO,
                },
            )?;
            let restriction_error = keychain
                .update_spending_limit(
                    account,
                    IAccountKeychain::updateSpendingLimitCall {
                        keyId: admin_key,
                        token: tip20_addr(),
                        newLimit: U256::ONE,
                    },
                )
                .unwrap_err();
            assert_eq!(
                restriction_error.selector(),
                IAccountKeychain::InvalidKeyId::SELECTOR
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn admin_key_selectors_are_gated_at_t6() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let call = IAccountKeychain::isAdminKeyCall {
            account: Address::repeat_byte(1),
            keyId: Address::repeat_byte(1),
        };
        let output = StorageCtx::enter(&mut provider, || {
            AccountKeychain::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(output.reverted);
        assert_eq!(output.bytes.as_ref(), IAccountKeychain::isAdminKeyCall::SELECTOR);
    }

    #[test]
    fn constrained_tip20_selectors_match_writer() {
        assert!(is_constrained_tip20_selector(TIP20_TRANSFER_SELECTOR));
        assert!(is_constrained_tip20_selector(TIP20_APPROVE_SELECTOR));
        assert!(is_constrained_tip20_selector(TIP20_TRANSFER_WITH_MEMO_SELECTOR));
        assert!(!is_constrained_tip20_selector([0xab, 0xcd, 0xef, 0x01]));
        // transferFrom is intentionally NOT constrained (no recipient field).
        let transfer_from_sel = ITIP20::transferFromCall::SELECTOR;
        assert!(!is_constrained_tip20_selector(transfer_from_sel));
    }

    #[test]
    fn key_scope_base_matches_keccak_of_left_padded_key_and_slot2() {
        let kc = AccountKeychain::new();
        let key_hash = B256::repeat_byte(0xAB);

        let computed = kc.key_scope_base(key_hash);

        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(key_hash.as_slice());
        buf[32..].copy_from_slice(&U256::from(2u8).to_be_bytes::<32>());
        let expected = U256::from_be_bytes(keccak256(buf).0);

        assert_eq!(computed, expected, "key_scope_base = keccak(key || slot2)");
    }

    #[test]
    fn transient_slots_match_persistent_layout_changes() {
        assert_eq!(transient_slots(TempoHardfork::T2), (2, 3));
        assert_eq!(transient_slots(TempoHardfork::T3), (3, 4));
        assert_eq!(transient_slots(TempoHardfork::T4), (3, 4));
        assert_eq!(transient_slots(TempoHardfork::T5), (4, 5));
        assert_eq!(transient_slots(TempoHardfork::T10), (4, 5));
    }

    #[test]
    fn t5_witness_is_reusable_until_manually_burned() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let account = Address::repeat_byte(0x11);
        let first_key = Address::repeat_byte(0x21);
        let second_key = Address::repeat_byte(0x22);
        let witness = B256::ZERO;

        StorageCtx::enter(&mut provider, || {
            let mut keychain = AccountKeychain::new();
            keychain.set_tx_origin(account)?;
            keychain.authorize_key_with_restrictions(
                account,
                first_key,
                IAccountKeychain::SignatureType::Secp256k1,
                unrestricted_restrictions(),
                Some(witness),
            )?;
            assert!(!keychain.is_key_authorization_witness_burned(
                IAccountKeychain::isKeyAuthorizationWitnessBurnedCall { account, witness },
            )?);

            keychain.authorize_key_with_restrictions(
                account,
                second_key,
                IAccountKeychain::SignatureType::Secp256k1,
                unrestricted_restrictions(),
                Some(witness),
            )?;
            assert!(keychain.keys[account][second_key].read()?.expiry > 0);
            Result::<()>::Ok(())
        })
        .unwrap();

        let events = provider.events(ACCOUNT_KEYCHAIN_ADDRESS);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].topics()[0], IAccountKeychain::KeyAuthorizationWitness::SIGNATURE_HASH);
        assert_eq!(events[1].topics()[0], IAccountKeychain::KeyAuthorized::SIGNATURE_HASH);
    }

    #[test]
    fn t5_burned_witness_blocks_authorization_and_access_key_burn() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let account = Address::repeat_byte(0x31);
        let access_key = Address::repeat_byte(0x32);
        let witness = B256::repeat_byte(0x55);

        StorageCtx::enter(&mut provider, || {
            let mut keychain = AccountKeychain::new();
            keychain.set_tx_origin(account)?;
            keychain.burn_key_authorization_witness(
                account,
                IAccountKeychain::burnKeyAuthorizationWitnessCall { witness },
            )?;
            assert!(keychain.is_key_authorization_witness_burned(
                IAccountKeychain::isKeyAuthorizationWitnessBurnedCall { account, witness },
            )?);

            let error = keychain
                .authorize_key_with_restrictions(
                    account,
                    access_key,
                    IAccountKeychain::SignatureType::Secp256k1,
                    unrestricted_restrictions(),
                    Some(witness),
                )
                .unwrap_err();
            assert_eq!(error, err_key_authorization_witness_already_burned());

            keychain.set_transaction_key(access_key)?;
            let error = keychain
                .burn_key_authorization_witness(
                    account,
                    IAccountKeychain::burnKeyAuthorizationWitnessCall {
                        witness: B256::repeat_byte(0x56),
                    },
                )
                .unwrap_err();
            assert_eq!(error, err_unauthorized_caller());
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn contradictory_allow_any_scope_is_rejected_from_t5() {
        let account = Address::repeat_byte(0x41);
        let key_id = Address::repeat_byte(0x42);
        let mut restrictions = unrestricted_restrictions();
        restrictions.allowedCalls.push(IAccountKeychain::CallScope {
            target: Address::repeat_byte(0x43),
            selectorRules: Vec::new(),
        });

        for (spec, should_succeed) in [(TempoHardfork::T4, true), (TempoHardfork::T5, false)] {
            let mut provider = TestStorageProvider::new(spec);
            let result = StorageCtx::enter(&mut provider, || {
                let mut keychain = AccountKeychain::new();
                keychain.set_tx_origin(account)?;
                keychain.authorize_key_with_restrictions(
                    account,
                    key_id,
                    IAccountKeychain::SignatureType::Secp256k1,
                    restrictions.clone(),
                    None,
                )
            });
            assert_eq!(result.is_ok(), should_succeed, "unexpected {spec:?} result");
        }
    }

    #[test]
    fn t5_witness_selector_is_inactive_before_t5() {
        let call = IAccountKeychain::authorizeKey_2Call {
            keyId: Address::repeat_byte(0x51),
            signatureType: IAccountKeychain::SignatureType::Secp256k1,
            config: unrestricted_restrictions(),
            witness: B256::ZERO,
        };
        let mut provider = TestStorageProvider::new(TempoHardfork::T4);
        let output = StorageCtx::enter(&mut provider, || {
            AccountKeychain::new().call(&call.abi_encode(), Address::repeat_byte(0x50))
        })
        .unwrap();
        assert!(output.reverted);
        assert_eq!(output.bytes.as_ref(), IAccountKeychain::authorizeKey_2Call::SELECTOR);
    }

    #[test]
    fn t3_restrictions_seed_periodic_limit_layout() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        provider.set_timestamp(U256::from(10));
        let account = Address::repeat_byte(0x61);
        let key_id = Address::repeat_byte(0x62);
        let token = Address::repeat_byte(0x63);

        StorageCtx::enter(&mut provider, || {
            let mut keychain = AccountKeychain::new();
            keychain.set_tx_origin(account)?;
            keychain.authorize_key_with_restrictions(
                account,
                key_id,
                IAccountKeychain::SignatureType::Secp256k1,
                IAccountKeychain::KeyRestrictions {
                    expiry: 100,
                    enforceLimits: true,
                    limits: vec![IAccountKeychain::TokenLimit {
                        token,
                        amount: U256::from(500),
                        period: 30,
                    }],
                    allowAnyCalls: true,
                    allowedCalls: Vec::new(),
                },
                None,
            )?;
            let limit_key = AccountKeychain::spending_limit_key(account, key_id);
            assert_eq!(
                keychain.spending_limits[limit_key][token].read()?,
                SpendingLimitState {
                    remaining: U256::from(500),
                    max: 500,
                    period: 30,
                    period_end: 40,
                },
            );
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn target_scope_base_uses_left_padded_address_at_map_offset_3() {
        let kc = AccountKeychain::new();
        let key_hash = B256::repeat_byte(0xCD);
        let target = address!("0x20C0000000000000000000000000000000000042");

        let computed = kc.target_scope_base(key_hash, target);

        let target_scopes_map_base = kc.key_scope_base(key_hash) + U256::from(3u8);
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(target.as_slice());
        buf[32..].copy_from_slice(&target_scopes_map_base.to_be_bytes::<32>());
        let expected = U256::from_be_bytes(keccak256(buf).0);

        assert_eq!(computed, expected);
    }

    #[test]
    fn selector_scope_base_uses_left_padded_selector_at_map_offset_2() {
        let kc = AccountKeychain::new();
        let key_hash = B256::repeat_byte(0xEF);
        let target = address!("0x20C0000000000000000000000000000000000042");
        let selector = FixedBytes::<4>::from([0xde, 0xad, 0xbe, 0xef]);

        let computed = kc.selector_scope_base(key_hash, target, selector);

        let selector_scopes_map_base =
            kc.target_scope_base(key_hash, target) + U256::from(2u8);
        let mut buf = [0u8; 64];
        // FixedBytes<4> mapping_slot left-pads (matches storage_types FixedBytes<4>::as_storage_bytes).
        buf[28..32].copy_from_slice(&selector.0);
        buf[32..].copy_from_slice(&selector_scopes_map_base.to_be_bytes::<32>());
        let expected = U256::from_be_bytes(keccak256(buf).0);

        assert_eq!(computed, expected);
    }

    #[test]
    fn validate_selector_rules_t4_rejects_non_tip20_prefix_target() {
        // T4 stateless path: target without the TIP-20 prefix is rejected even
        // before any storage probe.
        let kc = AccountKeychain::new();
        let rules = one_transfer_rule_with_recipient();
        let result = with_read_only_storage_ctx(
            &EmptyDB::default(),
            TempoHardfork::T4,
            4217,
            || kc.validate_selector_rules(non_tip20_addr(), &rules),
        );
        assert!(matches!(result, Err(TempoPrecompileError::Revert(_))));
    }

    #[test]
    fn validate_selector_rules_t4_accepts_tip20_prefix_with_no_bytecode() {
        // T4 stateless: prefix alone is sufficient — EmptyDB has no deployed
        // TIP-20 token, but the format check still passes.
        let kc = AccountKeychain::new();
        let rules = one_transfer_rule_with_recipient();
        let result = with_read_only_storage_ctx(
            &EmptyDB::default(),
            TempoHardfork::T4,
            4217,
            || kc.validate_selector_rules(tip20_addr(), &rules),
        );
        assert!(result.is_ok(), "T4 prefix-only validate should pass");
    }

    #[test]
    fn validate_selector_rules_t3_rejects_tip20_prefix_without_bytecode() {
        // T3 stateful: prefix passes the format check but the storage probe
        // (`TIP20Factory::is_tip20`) sees no code at `tip20_addr` → rejected.
        let kc = AccountKeychain::new();
        let rules = one_transfer_rule_with_recipient();
        let result = with_read_only_storage_ctx(
            &EmptyDB::default(),
            TempoHardfork::T3,
            4217,
            || kc.validate_selector_rules(tip20_addr(), &rules),
        );
        assert!(matches!(result, Err(TempoPrecompileError::Revert(_))));
    }

    // -- SpendingLimitState round-trip (FU-3) -----------------------------------

    /// In-memory `StorageOps` for unit-testing the 2-slot pack layout.
    struct MockStorage(std::collections::HashMap<U256, U256>);
    impl MockStorage {
        fn new() -> Self {
            Self(std::collections::HashMap::new())
        }
    }
    impl StorageOps for MockStorage {
        fn load(&self, slot: U256) -> Result<U256> {
            Ok(self.0.get(&slot).copied().unwrap_or(U256::ZERO))
        }
        fn store(&mut self, slot: U256, value: U256) -> Result<()> {
            self.0.insert(slot, value);
            Ok(())
        }
    }

    #[test]
    fn spending_limit_state_storable_round_trip() {
        let state = SpendingLimitState {
            remaining: U256::from(0x1234_5678_u32),
            max: 0xABCD_EF01_2345_6789_u128,
            period: 60 * 60 * 24,         // 86400 seconds
            period_end: 1_777_298_400_u64, // T3 timestamp
        };

        let mut mock = MockStorage::new();
        state.store(&mut mock, U256::from(7u8), LayoutCtx::FULL).unwrap();
        let loaded = SpendingLimitState::load(&mock, U256::from(7u8), LayoutCtx::FULL).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn spending_limit_state_packed_byte_layout_matches_writer() {
        // Verify slot+1 packed byte positions: max @ bytes 0..16,
        // period @ bytes 16..24, period_end @ bytes 24..32.
        let state = SpendingLimitState {
            remaining: U256::from(42u8),
            max: 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10_u128,
            period: 0x1112_1314_1516_1718_u64,
            period_end: 0x2122_2324_2526_2728_u64,
        };

        let mut mock = MockStorage::new();
        state.store(&mut mock, U256::from(0u8), LayoutCtx::FULL).unwrap();

        // Slot 0: remaining
        assert_eq!(mock.load(U256::ZERO).unwrap(), U256::from(42u8));
        // Slot 1: bytes encoded little-end-first per Solidity packing (offset 0 = LSB).
        let packed = mock.load(U256::ONE).unwrap();
        // max fills bytes [0..16] (LSB), period bytes [16..24], period_end [24..32].
        let expected = (U256::from(state.max))
            | (U256::from(state.period) << (8 * SPENDING_LIMIT_PERIOD_OFFSET))
            | (U256::from(state.period_end) << (8 * SPENDING_LIMIT_PERIOD_END_OFFSET));
        assert_eq!(packed, expected);
    }

    #[test]
    fn compute_next_period_end_within_one_period_advances_once() {
        let state = SpendingLimitState {
            remaining: U256::ZERO,
            max: 1_000,
            period: 86_400,
            period_end: 1_777_298_400,
        };
        // current = period_end exactly → advances exactly one period
        assert_eq!(
            state.compute_next_period_end(1_777_298_400),
            1_777_298_400 + 86_400,
        );
        // current = period_end + 1s (just past) → also one period
        assert_eq!(
            state.compute_next_period_end(1_777_298_401),
            1_777_298_400 + 86_400,
        );
    }

    #[test]
    fn compute_next_period_end_skips_multiple_periods() {
        let state = SpendingLimitState {
            remaining: U256::ZERO,
            max: 1_000,
            period: 100,
            period_end: 1_000,
        };
        // 350 seconds past period_end → 3 full periods elapsed + 1 = 4 advances
        let now = 1_000 + 350;
        assert_eq!(state.compute_next_period_end(now), 1_000 + 100 * 4);
    }

    #[test]
    fn compute_next_period_end_saturating_on_extreme_inputs() {
        let state = SpendingLimitState {
            remaining: U256::ZERO,
            max: 0,
            period: u64::MAX / 2,
            period_end: u64::MAX - 10,
        };
        // current_timestamp > period_end + period_max → saturates to u64::MAX
        let now = u64::MAX;
        let next = state.compute_next_period_end(now);
        assert_eq!(next, u64::MAX);
    }

    #[test]
    fn spending_limit_state_pre_t3_data_decodes_as_non_periodic() {
        // Pre-T3 entries wrote only slot+0 (remaining). Reading the new 2-slot
        // layout against that legacy storage must produce
        // {remaining, max=0, period=0, period_end=0} so the non-periodic
        // semantic kicks in (matches writer).
        let mut mock = MockStorage::new();
        let legacy_remaining = U256::from(99u8);
        mock.store(U256::from(5u8), legacy_remaining).unwrap();
        // slot+1 left untouched (== 0)

        let loaded = SpendingLimitState::load(&mock, U256::from(5u8), LayoutCtx::FULL).unwrap();
        assert_eq!(loaded.remaining, legacy_remaining);
        assert_eq!(loaded.max, 0);
        assert_eq!(loaded.period, 0);
        assert_eq!(loaded.period_end, 0);
    }

    #[test]
    fn validate_selector_rules_skips_tip20_check_for_recipientless_rules() {
        // Both T3 and T4: a rule with empty recipients doesn't trigger the
        // TIP-20 probe (skipped before `is_tip20()`), so it passes regardless.
        let kc = AccountKeychain::new();
        let rules = vec![IAccountKeychain::SelectorRule {
            selector: FixedBytes::from(TIP20_TRANSFER_SELECTOR),
            recipients: Vec::new(),
        }];

        for hardfork in [TempoHardfork::T3, TempoHardfork::T4] {
            let result = with_read_only_storage_ctx(
                &EmptyDB::default(),
                hardfork,
                4217,
                || kc.validate_selector_rules(non_tip20_addr(), &rules),
            );
            assert!(result.is_ok(), "{:?} recipientless rule should pass", hardfork);
        }
    }
}
