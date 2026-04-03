//! 2D nonce management precompile and expiring nonce replay protection.
//!
//! Enables concurrent transaction execution as part of Tempo Transactions.
//!
//! Ported from `tempo/crates/precompiles/src/nonce/`.
//!
//! ## Storage layout
//!
//! | Slot | Field                   | Type                                     |
//! |------|-------------------------|------------------------------------------|
//! |  0   | nonces                  | Mapping<Address, Mapping<U256, u64>>     |
//! |  1   | expiring_nonce_seen     | Mapping<B256, u64>                       |
//! |  2   | expiring_nonce_ring     | Mapping<u32, B256>                       |
//! |  3   | expiring_nonce_ring_ptr | u32                                      |

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{Handler, Mapping, Slot};
use super::{dispatch_call, input_cost, view, Precompile, NONCE_PRECOMPILE_ADDRESS};

// ===========================================================================
// Constants
// ===========================================================================

/// Capacity of the expiring nonce seen set (supports 10k TPS for 30 seconds).
pub const EXPIRING_NONCE_SET_CAPACITY: u32 = 300_000;

/// Maximum allowed skew for expiring nonce transactions (30 seconds).
pub const EXPIRING_NONCE_MAX_EXPIRY_SECS: u64 = 30;

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    interface INonce {
        function getNonce(address account, uint256 nonceKey) external view returns (uint64);

        event NonceIncremented(address indexed account, uint256 indexed nonceKey, uint64 newNonce);

        error ProtocolNonceNotSupported();
        error InvalidNonceKey();
        error NonceOverflow();
        error InvalidExpiringNonceExpiry();
        error ExpiringNonceReplay();
        error ExpiringNonceSetFull();
    }
}

// ===========================================================================
// NonceManager struct (manual macro expansion)
// ===========================================================================

/// NonceManager precompile -- manages 2D nonces for concurrent tx execution.
pub struct NonceManager {
    // Slot 0: nonces -- mapping(address => mapping(uint256 => uint64))
    pub nonces: Mapping<Address, Mapping<U256, u64>>,
    // Slot 1: expiring_nonce_seen -- mapping(bytes32 => uint64)
    pub expiring_nonce_seen: Mapping<B256, u64>,
    // Slot 2: expiring_nonce_ring -- mapping(uint32 => bytes32)
    // Note: u32 key stored via U256 since StorageKey is not impl'd for u32
    pub expiring_nonce_ring: Mapping<U256, B256>,
    // Slot 3: expiring_nonce_ring_ptr
    pub expiring_nonce_ring_ptr: Slot<u32>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl NonceManager {
    pub fn new() -> Self {
        let address = NONCE_PRECOMPILE_ADDRESS;
        Self {
            nonces: Mapping::new(U256::from(0), address),
            expiring_nonce_seen: Mapping::new(U256::from(1), address),
            expiring_nonce_ring: Mapping::new(U256::from(2), address),
            expiring_nonce_ring_ptr: Slot::new(U256::from(3), address),
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

    /// Initializes the nonce manager precompile storage layout.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    /// Returns the current nonce for `account` at the given `nonceKey`.
    ///
    /// Protocol nonce (key 0) is stored in account state, not in this precompile.
    pub fn get_nonce(&self, call: INonce::getNonceCall) -> Result<u64> {
        if call.nonceKey.is_zero() {
            return Err(TempoPrecompileError::Revert(
                INonce::ProtocolNonceNotSupported {}.abi_encode().into(),
            ));
        }
        self.nonces[call.account][call.nonceKey].read()
    }

    /// Increments the 2D nonce for `account` at `nonce_key` and returns the new value.
    ///
    /// Key 0 is reserved for the protocol nonce.
    pub fn increment_nonce(&mut self, account: Address, nonce_key: U256) -> Result<u64> {
        if nonce_key.is_zero() {
            return Err(TempoPrecompileError::Revert(
                INonce::InvalidNonceKey {}.abi_encode().into(),
            ));
        }

        let current = self.nonces[account][nonce_key].read()?;

        let new_nonce = current.checked_add(1).ok_or_else(|| {
            TempoPrecompileError::Revert(INonce::NonceOverflow {}.abi_encode().into())
        })?;

        self.nonces[account][nonce_key].write(new_nonce)?;

        self.emit_event(INonce::NonceIncremented {
            account,
            nonceKey: nonce_key,
            newNonce: new_nonce,
        })?;

        Ok(new_nonce)
    }

    /// Checks if a hash has been seen and is still valid (not expired).
    pub fn is_expiring_nonce_seen(&self, hash: B256, now: u64) -> Result<bool> {
        let expiry = self.expiring_nonce_seen[hash].read()?;
        Ok(expiry != 0 && expiry > now)
    }

    /// Validates and records an expiring nonce transaction.
    ///
    /// Uses a circular buffer that overwrites expired entries as the pointer advances.
    pub fn check_and_mark_expiring_nonce(
        &mut self,
        expiring_nonce_hash: B256,
        valid_before: u64,
    ) -> Result<()> {
        let now: u64 = self.storage.timestamp().saturating_to();

        // 1. Validate expiry window: must be in (now, now + max_skew]
        if valid_before <= now || valid_before > now.saturating_add(EXPIRING_NONCE_MAX_EXPIRY_SECS)
        {
            return Err(TempoPrecompileError::Revert(
                INonce::InvalidExpiringNonceExpiry {}.abi_encode().into(),
            ));
        }

        // 2. Replay check: reject if hash is already seen and not expired
        let seen_expiry = self.expiring_nonce_seen[expiring_nonce_hash].read()?;
        if seen_expiry != 0 && seen_expiry > now {
            return Err(TempoPrecompileError::Revert(
                INonce::ExpiringNonceReplay {}.abi_encode().into(),
            ));
        }

        // 3. Get current pointer and use directly as index
        let ptr = self.expiring_nonce_ring_ptr.read()?;
        let idx = U256::from(ptr);
        let old_hash = self.expiring_nonce_ring[idx].read()?;

        // 4. If there's an existing entry, check if it's expired (can be evicted)
        if old_hash != B256::ZERO {
            let old_expiry = self.expiring_nonce_seen[old_hash].read()?;
            if old_expiry != 0 && old_expiry > now {
                return Err(TempoPrecompileError::Revert(
                    INonce::ExpiringNonceSetFull {}.abi_encode().into(),
                ));
            }
            // Clear the old entry from seen set
            self.expiring_nonce_seen[old_hash].write(0)?;
        }

        // 5. Insert new entry
        self.expiring_nonce_ring[U256::from(ptr)].write(expiring_nonce_hash)?;
        self.expiring_nonce_seen[expiring_nonce_hash].write(valid_before)?;

        // 6. Advance pointer (wraps at CAPACITY, not u32::MAX)
        let next = if ptr + 1 >= EXPIRING_NONCE_SET_CAPACITY {
            0
        } else {
            ptr + 1
        };
        self.expiring_nonce_ring_ptr.write(next)?;

        Ok(())
    }
}

impl ContractStorage for NonceManager {
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

impl Precompile for NonceManager {
    fn call(&mut self, calldata: &[u8], _msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            INonce::INonceCalls::abi_decode,
            |call| match call {
                INonce::INonceCalls::getNonce(call) => view(call, |c| self.get_nonce(c)),
            },
        )
    }
}
