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
//! |  2   | transaction_key  | Address (transient)                          |
//! |  3   | tx_origin        | Address (transient)                          |
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
    Handler, Layout, LayoutCtx, Mapping, Set as StorageSet, SetHandler, Slot, Storable,
    StorableType, StorageKey,
};
use super::tip20::ITIP20;
use super::tip20_factory::TIP20Factory;
use super::{
    dispatch_call, input_cost, mutate_void, view, Precompile,
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

        struct TokenLimit {
            address token;
            uint256 amount;
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

        function authorizeKey(
            address keyId,
            SignatureType signatureType,
            uint64 expiry,
            bool enforceLimits,
            TokenLimit[] calldata limits
        ) external;

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
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthorizedKey {
    pub signature_type: u8,
    pub expiry: u64,
    pub enforce_limits: bool,
    pub is_revoked: bool,
}

impl StorableType for AuthorizedKey {
    // u8(1) + u64(8) + bool(1) + bool(1) = 11 bytes, fits in one slot
    const LAYOUT: Layout = Layout::Bytes(11);
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

        Ok(Self {
            signature_type,
            expiry,
            enforce_limits,
            is_revoked,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[31] = self.signature_type;
        bytes[23..31].copy_from_slice(&self.expiry.to_be_bytes());
        bytes[22] = if self.enforce_limits { 1 } else { 0 };
        bytes[21] = if self.is_revoked { 1 } else { 0 };
        storage.store(slot, U256::from_be_bytes(bytes))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)
    }
}

// ===========================================================================
// AccountKeychain struct
// ===========================================================================

/// Account Keychain precompile for managing authorized keys (session keys, spending limits).
pub struct AccountKeychain {
    // Slot 0: keys[account][keyId] -> AuthorizedKey
    pub(crate) keys: Mapping<Address, Mapping<Address, AuthorizedKey>>,
    // Slot 1: spending_limits[hash(account,keyId)][token] -> amount
    pub(crate) spending_limits: Mapping<B256, Mapping<Address, U256>>,
    // Slot 2: transaction_key (transient)
    pub(crate) transaction_key: Slot<Address>,
    // Slot 3: tx_origin (transient)
    pub(crate) tx_origin: Slot<Address>,
    // Slot 4 (T3+, TIP-1011): key_scopes[hash(account, keyId)] -> KeyScope
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

    pub address: Address,
    pub storage: StorageCtx,
}

impl AccountKeychain {
    pub fn new() -> Self {
        let address = ACCOUNT_KEYCHAIN_ADDRESS;
        Self {
            keys: Mapping::new(U256::from(0), address),
            spending_limits: Mapping::new(U256::from(1), address),
            transaction_key: Slot::new(U256::from(2), address),
            tx_origin: Slot::new(U256::from(3), address),
            call_scope_base: U256::from(4),
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

    /// Ensures admin operations are authorized for this caller.
    ///
    /// Rules:
    /// - transaction must be signed by the main key (`transaction_key == Address::ZERO`)
    /// - T2+: caller must match tx.origin (prevents confused-deputy self-administration)
    fn ensure_admin_caller(&self, msg_sender: Address) -> Result<()> {
        if !self.transaction_key.t_read()?.is_zero() {
            return Err(err_unauthorized_caller());
        }

        if self.storage.spec().is_t2() {
            let tx_origin = self.tx_origin.t_read()?;
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
        call: IAccountKeychain::authorizeKeyCall,
    ) -> Result<()> {
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
        };

        self.keys[msg_sender][call.keyId].write(new_key)?;

        // Set initial spending limits (only if enforce_limits is true)
        if call.enforceLimits {
            let limit_key = Self::spending_limit_key(msg_sender, call.keyId);
            for limit in call.limits {
                self.spending_limits[limit_key][limit.token].write(limit.amount)?;
            }
        }

        self.emit_event(IAccountKeychain::KeyAuthorized {
            account: msg_sender,
            publicKey: call.keyId,
            signatureType: signature_type,
            expiry: call.expiry,
        })
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

        // If this key had unlimited spending, enable limits now
        if !key.enforce_limits {
            key.enforce_limits = true;
            self.keys[msg_sender][call.keyId].write(key)?;
        }

        let limit_key = Self::spending_limit_key(msg_sender, call.keyId);
        self.spending_limits[limit_key][call.token].write(call.newLimit)?;

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
        self.spending_limits[limit_key][call.token].read()
    }

    /// Returns the access key used to authorize the current transaction.
    pub fn get_transaction_key(
        &self,
        _call: IAccountKeychain::getTransactionKeyCall,
        _msg_sender: Address,
    ) -> Result<Address> {
        self.transaction_key.t_read()
    }

    /// Internal: Set the transaction key (called during transaction validation).
    pub fn set_transaction_key(&mut self, key_id: Address) -> Result<()> {
        self.transaction_key.t_write(key_id)
    }

    /// Sets the transaction origin for the current transaction.
    pub fn set_tx_origin(&mut self, origin: Address) -> Result<()> {
        self.tx_origin.t_write(origin)
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
        let remaining = self.spending_limits[limit_key][token].read()?;

        if amount > remaining {
            return Err(err_spending_limit_exceeded());
        }

        self.spending_limits[limit_key][token].write(remaining - amount)
    }

    /// Refund spending limit after a fee refund.
    pub fn refund_spending_limit(
        &mut self,
        account: Address,
        token: Address,
        amount: U256,
    ) -> Result<()> {
        let transaction_key = self.transaction_key.t_read()?;

        if transaction_key == Address::ZERO {
            return Ok(());
        }

        let tx_origin = self.tx_origin.t_read()?;
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
        let remaining = self.spending_limits[limit_key][token].read()?;
        let new_remaining = remaining.saturating_add(amount);
        self.spending_limits[limit_key][token].write(new_remaining)
    }

    /// Authorize a token transfer with access key spending limits.
    pub fn authorize_transfer(
        &mut self,
        account: Address,
        token: Address,
        amount: U256,
    ) -> Result<()> {
        let transaction_key = self.transaction_key.t_read()?;

        if transaction_key == Address::ZERO {
            return Ok(());
        }

        let tx_origin = self.tx_origin.t_read()?;
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
        let transaction_key = self.transaction_key.t_read()?;

        if transaction_key == Address::ZERO {
            return Ok(());
        }

        let tx_origin = self.tx_origin.t_read()?;
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
        self.load_active_key(msg_sender, call.keyId)?;
        if current_timestamp >= self.keys[msg_sender][call.keyId].read()?.expiry {
            return Err(err_key_expired());
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
    /// **NOTE**: This commit lands the T3 path that defers the TIP20 lookup to
    /// the writer's stateful `TIP20Factory::is_tip20` (a storage probe).
    /// FU-5 (next commit) flips to the T4 stateless `Address::is_tip20` format
    /// check when running on T4+.
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
            let v = TIP20Factory::new().is_tip20(target)?;
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

        dispatch_call(
            calldata,
            IAccountKeychain::IAccountKeychainCalls::abi_decode,
            |call| match call {
                IAccountKeychain::IAccountKeychainCalls::authorizeKey(call) => {
                    mutate_void(call, msg_sender, |sender, c| self.authorize_key(sender, c))
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
    use alloy::primitives::address;

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
    fn key_scope_base_matches_keccak_of_left_padded_key_and_slot4() {
        let kc = AccountKeychain::new();
        let key_hash = B256::repeat_byte(0xAB);

        let computed = kc.key_scope_base(key_hash);

        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(key_hash.as_slice());
        buf[32..].copy_from_slice(&U256::from(4u8).to_be_bytes::<32>());
        let expected = U256::from_be_bytes(keccak256(buf).0);

        assert_eq!(computed, expected, "key_scope_base = keccak(key || slot4)");
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
}
