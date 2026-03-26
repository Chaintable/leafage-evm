//! EVM storage abstraction layer for Tempo precompile contracts (leafage-evm adaptation).
//!
//! Provides:
//! - [`PrecompileStorageProvider`] trait -- low-level storage operations
//! - [`LeafageStorageProvider`] -- production implementation backed by alloy-evm 0.29.2 EvmInternals
//! - [`StorageCtx`] -- thread-local accessor using `scoped-tls`
//! - [`CheckpointGuard`] -- RAII guard for atomic state mutation batching
//!
//! ## API adaptation notes (alloy-evm 0.29.2)
//!
//! - `chain_id` passed explicitly to `LeafageStorageProvider::new` for convenience
//! - Gas accounting uses hardcoded `TempoGasCosts` constants (matching GasParams overrides)
//! - `with_account_info` uses `load_account_code` + `JournaledAccountTr::account()` for info access
//! - Checkpoint operations delegate to `EvmInternals` (alloy-evm 0.29.2)

use alloy::primitives::{keccak256, Address, Log, LogData, B256, U256};
use alloy_evm::EvmInternals;
use revm::{
    interpreter::gas::{
        COLD_ACCOUNT_ACCESS_COST_ADDITIONAL, COLD_SLOAD_COST, KECCAK256, KECCAK256WORD, LOG,
        LOGDATA, LOGTOPIC, WARM_STORAGE_READ_COST, WARM_SSTORE_RESET,
    },
    state::Bytecode,
};

/// COLD_SLOAD_COST - WARM_STORAGE_READ_COST (removed in revm 36, was 2000)
const COLD_SLOAD_COST_ADDITIONAL: u64 = COLD_SLOAD_COST - WARM_STORAGE_READ_COST;
use scoped_tls::scoped_thread_local;
use std::cell::RefCell;

use super::error::{Result, TempoPrecompileError};
use crate::tempo::gas_params::TempoGasCosts;
use crate::tempo::hardfork::TempoHardfork;

/// Re-export of `revm::context_interface::journaled_state::JournalCheckpoint`.
///
/// alloy-evm 0.29.2's `EvmInternals` exposes `checkpoint()`, `checkpoint_commit()`,
/// and `checkpoint_revert()` which delegate to the underlying journal. We use the real
/// revm `JournalCheckpoint` type directly.
pub use revm::context_interface::journaled_state::JournalCheckpoint;

// ---------------------------------------------------------------------------
// PrecompileStorageProvider trait
// ---------------------------------------------------------------------------

/// Low-level storage provider for interacting with the EVM.
///
/// Mirrors the Tempo `PrecompileStorageProvider` trait with identical method
/// signatures so that downstream precompile code can be ported with minimal changes.
pub trait PrecompileStorageProvider {
    /// Returns the chain ID.
    fn chain_id(&self) -> u64;

    /// Returns the current block timestamp.
    fn timestamp(&self) -> U256;

    /// Returns the current block beneficiary (coinbase).
    fn beneficiary(&self) -> Address;

    /// Returns the current block number.
    fn block_number(&self) -> u64;

    /// Sets the bytecode at the given address.
    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()>;

    /// Executes a closure with access to the account info for the given address.
    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&revm::state::AccountInfo),
    ) -> Result<()>;

    /// Performs an SLOAD operation (persistent storage read).
    fn sload(&mut self, address: Address, key: U256) -> Result<U256>;

    /// Performs a TLOAD operation (transient storage read).
    fn tload(&mut self, address: Address, key: U256) -> Result<U256>;

    /// Performs an SSTORE operation (persistent storage write).
    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()>;

    /// Performs a TSTORE operation (transient storage write).
    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()>;

    /// Emits an event from the given contract address.
    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()>;

    /// Deducts gas from the remaining gas and returns an error if insufficient.
    fn deduct_gas(&mut self, gas: u64) -> Result<()>;

    /// Add refund to the refund gas counter.
    fn refund_gas(&mut self, gas: i64);

    /// Returns the gas used so far.
    fn gas_used(&self) -> u64;

    /// Returns the gas refunded so far.
    fn gas_refunded(&self) -> i64;

    /// Returns the currently active hardfork.
    fn spec(&self) -> TempoHardfork;

    /// Returns whether the current call context is static.
    fn is_static(&self) -> bool;

    /// Creates a new journal checkpoint.
    fn checkpoint(&mut self) -> JournalCheckpoint;

    /// Commits all state changes since the given checkpoint.
    fn checkpoint_commit(&mut self, checkpoint: JournalCheckpoint);

    /// Reverts all state changes back to the given checkpoint.
    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint);

    /// Computes keccak256 and charges the appropriate gas.
    fn keccak256(&mut self, data: &[u8]) -> Result<B256> {
        let num_words =
            u64::try_from(data.len().div_ceil(32)).map_err(|_| TempoPrecompileError::OutOfGas)?;
        let price = KECCAK256WORD
            .checked_mul(num_words)
            .and_then(|w: u64| w.checked_add(KECCAK256))
            .ok_or(TempoPrecompileError::OutOfGas)?;
        self.deduct_gas(price)?;
        Ok(keccak256(data))
    }

    /// Recovers the signer address from an ECDSA signature and charges ecrecover gas.
    ///
    /// As per [TIP-1004], only `v` values of `27` or `28` are accepted (no `0`/`1` normalization).
    /// Returns `Ok(None)` on invalid signatures; callers map to domain-specific errors.
    ///
    /// [TIP-1004]: <https://github.com/tempoxyz/tempo/blob/main/tips/tip-1004.md#signature-validation>
    fn recover_signer(&mut self, digest: B256, v: u8, r: B256, s: B256) -> Result<Option<Address>> {
        use super::ECRECOVER_GAS;
        self.deduct_gas(ECRECOVER_GAS)?;

        if v != 27 && v != 28 {
            return Ok(None);
        }

        let recid = secp256k1::ecdsa::RecoveryId::try_from((v as i32) - 27)
            .map_err(|_| TempoPrecompileError::Fatal("invalid recovery id".to_string()))?;
        let mut sig_bytes = [0u8; 64];
        sig_bytes[..32].copy_from_slice(r.as_slice());
        sig_bytes[32..].copy_from_slice(s.as_slice());
        let sig = match secp256k1::ecdsa::RecoverableSignature::from_compact(&sig_bytes, recid) {
            Ok(sig) => sig,
            Err(_) => return Ok(None),
        };
        let msg = secp256k1::Message::from_digest(*digest);
        let pubkey = match secp256k1::SECP256K1.recover_ecdsa(&msg, &sig) {
            Ok(pk) => pk,
            Err(_) => return Ok(None),
        };
        let hash = keccak256(&pubkey.serialize_uncompressed()[1..]);
        let recovered = Address::from_slice(&hash[12..]);

        if recovered.is_zero() {
            Ok(None)
        } else {
            Ok(Some(recovered))
        }
    }
}

/// Storage operations for a given (contract) address.
///
/// Abstracts over persistent storage (SLOAD/SSTORE) and transient storage (TLOAD/TSTORE).
pub trait StorageOps {
    /// Stores a value at the provided slot.
    fn store(&mut self, slot: U256, value: U256) -> Result<()>;
    /// Loads a value from the provided slot.
    fn load(&self, slot: U256) -> Result<U256>;
}

/// Trait providing access to a contract's address and storage context.
///
/// Automatically implemented by individual precompile contract types.
pub trait ContractStorage {
    /// Contract address.
    fn address(&self) -> Address;

    /// Contract storage accessor.
    fn storage(&self) -> &StorageCtx;

    /// Contract storage mutable accessor.
    fn storage_mut(&mut self) -> &mut StorageCtx;

    /// Returns true if the contract has been initialized (has bytecode deployed).
    fn is_initialized(&self) -> Result<bool> {
        self.storage()
            .with_account_info(self.address(), |info| Ok(!info.is_empty_code_hash()))
    }
}

// ---------------------------------------------------------------------------
// LeafageStorageProvider (adapted from EvmPrecompileStorageProvider)
// ---------------------------------------------------------------------------

/// Production [`PrecompileStorageProvider`] backed by alloy-evm 0.29.2's `EvmInternals`.
///
/// Adapted from Tempo's `EvmPrecompileStorageProvider` with these key differences:
/// - `chain_id` is passed explicitly for convenience
/// - Gas accounting uses hardcoded `TempoGasCosts` constants (matching GasParams overrides in TempoEvm)
/// - `with_account_info` uses `load_account_code` + `JournaledAccountTr::account().info`
/// - Checkpoint operations delegate to `EvmInternals` (available since alloy-evm 0.29.2)
pub struct LeafageStorageProvider<'a> {
    internals: EvmInternals<'a>,
    gas_remaining: u64,
    gas_refunded: i64,
    gas_limit: u64,
    chain_id: u64,
    spec: TempoHardfork,
    is_static: bool,
}

impl<'a> LeafageStorageProvider<'a> {
    /// Creates a new storage provider.
    ///
    /// # Arguments
    /// - `internals` -- alloy-evm EvmInternals (journal + block_env)
    /// - `gas_limit` -- maximum gas for this precompile execution
    /// - `chain_id` -- chain ID (passed explicitly since EvmInternals 0.25.2 lacks chain_id())
    /// - `is_static` -- whether this is a STATICCALL context
    pub fn new(
        internals: EvmInternals<'a>,
        gas_limit: u64,
        chain_id: u64,
        is_static: bool,
    ) -> Self {
        Self {
            internals,
            gas_remaining: gas_limit,
            gas_refunded: 0,
            gas_limit,
            chain_id,
            spec: TempoHardfork::default(),
            is_static,
        }
    }

    /// Creates a new storage provider with maximum gas limit and non-static context.
    pub fn new_max_gas(internals: EvmInternals<'a>, chain_id: u64) -> Self {
        Self::new(internals, u64::MAX, chain_id, false)
    }
}

impl PrecompileStorageProvider for LeafageStorageProvider<'_> {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.internals.block_timestamp()
    }

    fn beneficiary(&self) -> Address {
        use revm::context::Block;
        self.internals.block_env().beneficiary()
    }

    fn block_number(&self) -> u64 {
        use revm::context::Block;
        self.internals.block_env().number().to::<u64>()
    }

    #[inline]
    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        // Gas: CODE_DEPOSIT_PER_BYTE * code_len
        let deposit_cost = TempoGasCosts::CODE_DEPOSIT_PER_BYTE
            .checked_mul(code.len() as u64)
            .ok_or(TempoPrecompileError::OutOfGas)?;
        self.deduct_gas(deposit_cost)?;

        self.internals.set_code(address, code);
        Ok(())
    }

    #[inline]
    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&revm::state::AccountInfo),
    ) -> Result<()> {
        // alloy-evm 0.29.2 load_account_code returns StateLoad<Box<dyn JournaledAccountTr>>.
        // Extract info and cold flag, then drop the borrow before calling deduct_gas.
        let (info, is_cold) = {
            let result = self.internals.load_account_code(address)?;
            (result.data.account().info.clone(), result.is_cold)
        };

        self.deduct_gas(WARM_STORAGE_READ_COST)?;

        if is_cold {
            self.deduct_gas(COLD_ACCOUNT_ACCESS_COST_ADDITIONAL)?;
        }

        f(&info);
        Ok(())
    }

    #[inline]
    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        let result = self.internals.sload(address, key)?;

        // Gas: WARM_STORAGE_READ_COST + cold storage additional cost if cold
        self.deduct_gas(WARM_STORAGE_READ_COST)?;

        if result.is_cold {
            self.deduct_gas(COLD_SLOAD_COST_ADDITIONAL)?;
        }

        Ok(result.data)
    }

    #[inline]
    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.deduct_gas(WARM_STORAGE_READ_COST)?;
        Ok(self.internals.tload(address, key))
    }

    #[inline]
    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        let result = self.internals.sstore(address, key, value)?;

        // Static gas
        self.deduct_gas(WARM_STORAGE_READ_COST)?;

        // Dynamic gas: simplified from Tempo's GasParams.sstore_dynamic_gas
        // For the Tempo chain, SSTORE_SET is 250k (vs Ethereum 20k).
        // We use the standard EIP-2200 gas schedule with Tempo's overridden constants.
        let sstore_data = &result.data;
        let dynamic_gas = if sstore_data.is_new_eq_present() {
            // No-op store: 0 additional gas
            0
        } else if sstore_data.is_original_eq_present() {
            if sstore_data.original_value.is_zero() {
                // 0 -> non-zero: SSTORE_SET cost (Tempo: 250k)
                TempoGasCosts::SSTORE_SET
            } else {
                // non-zero -> different non-zero: WARM_SSTORE_RESET
                WARM_SSTORE_RESET
            }
        } else {
            // Dirty slot: 0 additional gas
            0
        };

        // Cold storage additional cost
        let cold_gas = if result.is_cold {
            COLD_SLOAD_COST_ADDITIONAL
        } else {
            0
        };

        self.deduct_gas(dynamic_gas.saturating_add(cold_gas))?;

        // Refund gas (EIP-3529 style)
        let refund = sstore_refund(sstore_data);
        self.refund_gas(refund);

        Ok(())
    }

    #[inline]
    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        self.deduct_gas(WARM_STORAGE_READ_COST)?;
        self.internals.tstore(address, key, value);
        Ok(())
    }

    #[inline]
    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        let gas = LOG
            .saturating_add(LOGTOPIC.saturating_mul(event.topics().len() as u64))
            .saturating_add(LOGDATA.saturating_mul(event.data.len() as u64));
        self.deduct_gas(gas)?;

        self.internals.log(Log {
            address,
            data: event,
        });

        Ok(())
    }

    #[inline]
    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        self.gas_remaining = self
            .gas_remaining
            .checked_sub(gas)
            .ok_or(TempoPrecompileError::OutOfGas)?;
        Ok(())
    }

    #[inline]
    fn refund_gas(&mut self, gas: i64) {
        self.gas_refunded = self.gas_refunded.saturating_add(gas);
    }

    #[inline]
    fn gas_used(&self) -> u64 {
        self.gas_limit - self.gas_remaining
    }

    #[inline]
    fn gas_refunded(&self) -> i64 {
        self.gas_refunded
    }

    #[inline]
    fn spec(&self) -> TempoHardfork {
        self.spec
    }

    #[inline]
    fn is_static(&self) -> bool {
        self.is_static
    }

    #[inline]
    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.internals.checkpoint()
    }

    #[inline]
    fn checkpoint_commit(&mut self, _checkpoint: JournalCheckpoint) {
        self.internals.checkpoint_commit()
    }

    #[inline]
    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.internals.checkpoint_revert(checkpoint)
    }
}

/// Computes sstore gas refund following EIP-3529 rules.
///
/// Simplified from Tempo's `GasParams::sstore_refund` -- uses standard Ethereum
/// constants since Tempo only overrides SSTORE_SET.
#[inline]
fn sstore_refund(result: &revm::interpreter::SStoreResult) -> i64 {
    use revm::interpreter::gas::SSTORE_RESET;

    if result.is_new_eq_present() {
        return 0;
    }

    let mut refund: i64 = 0;

    if result.is_original_eq_present() {
        // Clean slot transition
    } else {
        // Dirty slot: refund for restoring to original
        if !result.original_value.is_zero() {
            if result.present_value.is_zero() {
                // Was cleared, now being set again -- remove the set refund
                refund -= SSTORE_RESET as i64;
            } else if result.new_value.is_zero() {
                // Being cleared -- add clear refund
                refund += SSTORE_RESET as i64;
            }
        }
        if result.original_value == result.new_value {
            // Restoring to original value
            if result.original_value.is_zero() {
                // Was 0 -> X -> 0: refund SSTORE_SET - WARM_STORAGE_READ_COST
                refund += (TempoGasCosts::SSTORE_SET - WARM_STORAGE_READ_COST) as i64;
            } else {
                // Was X -> Y -> X: refund SSTORE_RESET - WARM_STORAGE_READ_COST
                refund += (SSTORE_RESET - WARM_STORAGE_READ_COST) as i64;
            }
        }
    }

    refund
}

// ---------------------------------------------------------------------------
// StorageCtx (thread-local accessor)
// ---------------------------------------------------------------------------

scoped_thread_local!(static STORAGE: RefCell<&mut dyn PrecompileStorageProvider>);

/// Thread-local storage accessor that delegates to the current `PrecompileStorageProvider`.
///
/// This is the primary interface used by precompile storage types (`Slot`, `Mapping`, etc.).
/// It must be used within a `StorageCtx::enter` closure.
///
/// Read operations take `&self`, write operations take `&mut self`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StorageCtx;

impl StorageCtx {
    /// Enter storage context. All storage operations must happen within the closure.
    ///
    /// # Safety (logical)
    ///
    /// The caller must ensure that only one `enter` call is active at a time per thread.
    pub fn enter<S, R>(storage: &mut S, f: impl FnOnce() -> R) -> R
    where
        S: PrecompileStorageProvider,
    {
        // SAFETY: `scoped_tls` ensures the pointer is only accessible within the closure scope.
        let storage: &mut dyn PrecompileStorageProvider = storage;
        let storage_static: &mut (dyn PrecompileStorageProvider + 'static) =
            unsafe { std::mem::transmute(storage) };
        let cell = RefCell::new(storage_static);
        STORAGE.set(&cell, f)
    }

    /// Execute an infallible function with access to the current thread-local storage provider.
    ///
    /// # Panics
    /// Panics if no storage context is set.
    fn with_storage<F, R>(f: F) -> R
    where
        F: FnOnce(&mut dyn PrecompileStorageProvider) -> R,
    {
        assert!(
            STORAGE.is_set(),
            "No storage context. 'StorageCtx::enter' must be called first"
        );
        STORAGE.with(|cell| {
            let mut guard = cell.borrow_mut();
            f(&mut **guard)
        })
    }

    /// Execute a (fallible) function with access to the current thread-local storage provider.
    fn try_with_storage<F, R>(f: F) -> Result<R>
    where
        F: FnOnce(&mut dyn PrecompileStorageProvider) -> Result<R>,
    {
        if !STORAGE.is_set() {
            return Err(TempoPrecompileError::Fatal(
                "No storage context. 'StorageCtx::enter' must be called first".to_string(),
            ));
        }
        STORAGE.with(|cell| {
            let mut guard = cell.borrow_mut();
            f(&mut **guard)
        })
    }

    // -- PrecompileStorageProvider method delegations --

    /// Executes a closure with access to the account info, returning the closure's result.
    pub fn with_account_info<T>(
        &self,
        address: Address,
        mut f: impl FnMut(&revm::state::AccountInfo) -> Result<T>,
    ) -> Result<T> {
        let mut result: Option<Result<T>> = None;
        Self::try_with_storage(|s| {
            s.with_account_info(address, &mut |info| {
                result = Some(f(info));
            })
        })?;
        result.unwrap()
    }

    /// Returns the chain ID.
    pub fn chain_id(&self) -> u64 {
        Self::with_storage(|s| s.chain_id())
    }

    /// Returns the current block timestamp.
    pub fn timestamp(&self) -> U256 {
        Self::with_storage(|s| s.timestamp())
    }

    /// Returns the current block beneficiary (coinbase).
    pub fn beneficiary(&self) -> Address {
        Self::with_storage(|s| s.beneficiary())
    }

    /// Returns the current block number.
    pub fn block_number(&self) -> u64 {
        Self::with_storage(|s| s.block_number())
    }

    /// Sets the bytecode at the given address.
    pub fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        Self::try_with_storage(|s| s.set_code(address, code))
    }

    /// Performs an SLOAD operation (persistent storage read).
    pub fn sload(&self, address: Address, key: U256) -> Result<U256> {
        Self::try_with_storage(|s| s.sload(address, key))
    }

    /// Performs a TLOAD operation (transient storage read).
    pub fn tload(&self, address: Address, key: U256) -> Result<U256> {
        Self::try_with_storage(|s| s.tload(address, key))
    }

    /// Performs an SSTORE operation (persistent storage write).
    pub fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        Self::try_with_storage(|s| s.sstore(address, key, value))
    }

    /// Performs a TSTORE operation (transient storage write).
    pub fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        Self::try_with_storage(|s| s.tstore(address, key, value))
    }

    /// Emits an event from the given contract address.
    pub fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        Self::try_with_storage(|s| s.emit_event(address, event))
    }

    /// Adds refund to the gas refund counter.
    pub fn refund_gas(&mut self, gas: i64) {
        Self::with_storage(|s| s.refund_gas(gas))
    }

    /// Returns the gas used so far.
    pub fn gas_used(&self) -> u64 {
        Self::with_storage(|s| s.gas_used())
    }

    /// Returns the gas refunded so far.
    pub fn gas_refunded(&self) -> i64 {
        Self::with_storage(|s| s.gas_refunded())
    }

    /// Returns the currently active hardfork.
    pub fn spec(&self) -> TempoHardfork {
        Self::with_storage(|s| s.spec())
    }

    /// Returns whether the current call context is static.
    pub fn is_static(&self) -> bool {
        Self::with_storage(|s| s.is_static())
    }

    /// Creates a journal checkpoint and returns a RAII guard.
    ///
    /// All state mutations after this call will be atomically reverted if the
    /// guard is dropped without calling [`CheckpointGuard::commit`].
    pub fn checkpoint(&mut self) -> CheckpointGuard {
        let checkpoint = Self::with_storage(|s| {
            if s.spec().is_t1c() {
                Some(s.checkpoint())
            } else {
                None
            }
        });

        CheckpointGuard { checkpoint }
    }

    /// Deducts gas from the remaining gas and returns an error if insufficient.
    pub fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        Self::try_with_storage(|s| s.deduct_gas(gas))
    }

    /// Computes keccak256 and charges the appropriate gas.
    pub fn keccak256(&self, data: &[u8]) -> Result<B256> {
        Self::try_with_storage(|s| s.keccak256(data))
    }

    /// Recovers the signer address from an ECDSA signature.
    ///
    /// Only accepts `v` values of `27` or `28` per TIP-1004.
    /// Returns `Ok(None)` on invalid signatures.
    pub fn recover_signer(&self, digest: B256, v: u8, r: B256, s: B256) -> Result<Option<Address>> {
        Self::try_with_storage(|provider| provider.recover_signer(digest, v, r, s))
    }
}

// ---------------------------------------------------------------------------
// CheckpointGuard
// ---------------------------------------------------------------------------

/// RAII guard for atomic state mutation batching.
///
/// On drop, automatically reverts all state changes made since the checkpoint
/// unless [`commit`](CheckpointGuard::commit) was called.
///
/// Only active on T1C+ hardforks; prior to that it is a no-op.
pub struct CheckpointGuard {
    checkpoint: Option<JournalCheckpoint>,
}

impl CheckpointGuard {
    /// Commits all state changes since the checkpoint.
    pub fn commit(mut self) {
        if let Some(cp) = self.checkpoint.take() {
            StorageCtx::with_storage(|s| s.checkpoint_commit(cp));
        }
    }
}

impl Drop for CheckpointGuard {
    fn drop(&mut self) {
        if let Some(cp) = self.checkpoint.take() {
            StorageCtx::with_storage(|s| s.checkpoint_revert(cp));
        }
    }
}
