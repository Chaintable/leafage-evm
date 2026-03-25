//! Tempo precompile implementations for leafage-evm.
//!
//! This module provides the storage infrastructure, error handling, and
//! trait definitions required by all 9 Tempo precompile contracts.
//!
//! ## Architecture
//!
//! - [`error`] -- `TempoPrecompileError`, `Result<T>`, `IntoPrecompileResult`
//! - [`storage`] -- `PrecompileStorageProvider` trait, `LeafageStorageProvider`, `StorageCtx`
//! - [`storage_types`] -- `Slot`, `Mapping`, packing helpers, primitive type encoders

pub mod error;
pub mod storage;
pub mod storage_types;

pub use error::{IntoPrecompileResult, Result, TempoPrecompileError};
pub use storage::{
    CheckpointGuard, ContractStorage, LeafageStorageProvider, PrecompileStorageProvider,
    StorageCtx, StorageOps,
};
pub use storage_types::{
    packing, packing::FieldLocation, BytesLikeHandler, FromWord, Handler, Layout, LayoutCtx,
    Mapping, Packable, Slot, Storable, StorableType, StorageKey,
};

use alloy::primitives::{address, Address, Bytes};
use alloy::sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileOutput, PrecompileResult};

// ===========================================================================
// Address constants (from tempo-contracts)
// ===========================================================================

pub const TIP_FEE_MANAGER_ADDRESS: Address =
    address!("0xfeec000000000000000000000000000000000000");
pub const PATH_USD_ADDRESS: Address = address!("0x20C0000000000000000000000000000000000000");
pub const DEFAULT_FEE_TOKEN: Address = PATH_USD_ADDRESS;
pub const TIP403_REGISTRY_ADDRESS: Address =
    address!("0x403C000000000000000000000000000000000000");
pub const TIP20_FACTORY_ADDRESS: Address =
    address!("0x20FC000000000000000000000000000000000000");
pub const STABLECOIN_DEX_ADDRESS: Address =
    address!("0xdec0000000000000000000000000000000000000");
pub const NONCE_PRECOMPILE_ADDRESS: Address =
    address!("0x4E4F4E4345000000000000000000000000000000");
pub const VALIDATOR_CONFIG_ADDRESS: Address =
    address!("0xCCCCCCCC00000000000000000000000000000000");
pub const ACCOUNT_KEYCHAIN_ADDRESS: Address =
    address!("0xAAAAAAAA00000000000000000000000000000000");
pub const VALIDATOR_CONFIG_V2_ADDRESS: Address =
    address!("0xCCCCCCCC00000000000000000000000000000001");

// ===========================================================================
// Gas constants
// ===========================================================================

/// Input per word cost. Covers ABI decoding and cloning of input into call data.
pub const INPUT_PER_WORD_COST: u64 = 6;

/// Gas cost for `ecrecover` signature verification.
pub const ECRECOVER_GAS: u64 = 3_000;

/// Returns the gas cost for decoding calldata of the given length, rounded up to word boundaries.
#[inline]
pub fn input_cost(calldata_len: usize) -> u64 {
    calldata_len
        .div_ceil(32)
        .saturating_mul(INPUT_PER_WORD_COST as usize) as u64
}

// ===========================================================================
// Precompile trait
// ===========================================================================

/// Trait implemented by all Tempo precompile contract types.
///
/// Precompiles must provide a dispatcher that decodes the 4-byte function selector
/// from calldata, ABI-decodes the arguments, and routes to the corresponding method.
pub trait Precompile {
    /// Dispatches an EVM call to this precompile.
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult;
}

// ===========================================================================
// Solidity error types
// ===========================================================================

alloy::sol! {
    error DelegateCallNotAllowed();
    error StaticCallNotAllowed();
}

// ===========================================================================
// tempo_precompile! macro
// ===========================================================================

/// Creates a `DynPrecompile` wrapper for a Tempo precompile.
///
/// This macro:
/// 1. Rejects DELEGATECALL (not direct call)
/// 2. Sets up the `LeafageStorageProvider` with the precompile's gas budget
/// 3. Enters the `StorageCtx` scope
/// 4. Executes the precompile logic within that scope
///
/// Usage:
/// ```ignore
/// tempo_precompile!("TipFeeManager", chain_id, |input| { TipFeeManager::new() })
/// ```
#[macro_export]
macro_rules! tempo_precompile {
    ($id:expr, $chain_id:expr, |$input:ident| $impl:expr) => {{
        let chain_id = $chain_id;
        alloy_evm::precompiles::DynPrecompile::new_stateful(
            revm::precompile::PrecompileId::Custom($id.into()),
            move |$input| {
                if !$input.is_direct_call() {
                    return Ok(revm::precompile::PrecompileOutput::new_reverted(
                        0,
                        $crate::tempo::precompile::DelegateCallNotAllowed {}
                            .abi_encode()
                            .into(),
                    ));
                }
                let mut storage =
                    $crate::tempo::precompile::LeafageStorageProvider::new(
                        $input.internals,
                        $input.gas,
                        chain_id,
                        $input.is_static,
                    );
                $crate::tempo::precompile::StorageCtx::enter(&mut storage, || {
                    let result = $impl.call($input.data, $input.caller);
                    // Fill gas accounting from the storage context
                    result.map(|mut output| {
                        output.gas_used =
                            $crate::tempo::precompile::StorageCtx.gas_used();
                        if !output.reverted {
                            output.gas_refunded =
                                $crate::tempo::precompile::StorageCtx.gas_refunded();
                        }
                        output
                    })
                })
            },
        )
    }};
}

// Re-export for use in macro
pub use alloy::sol_types::SolError as _SolError;

// ===========================================================================
// Dispatch helpers
// ===========================================================================

/// Dispatches a parameterless view call, encoding the return via `T`.
#[inline]
pub fn metadata<T: SolCall>(f: impl FnOnce() -> Result<T::Return>) -> PrecompileResult {
    f().into_precompile_result(0, |ret| T::abi_encode_returns(&ret).into())
}

/// Dispatches a read-only call with decoded arguments, encoding the return via `T`.
#[inline]
pub fn view<T: SolCall>(call: T, f: impl FnOnce(T) -> Result<T::Return>) -> PrecompileResult {
    f(call).into_precompile_result(0, |ret| T::abi_encode_returns(&ret).into())
}

/// Dispatches a state-mutating call that returns ABI-encoded data.
///
/// Rejects static calls with [`StaticCallNotAllowed`].
#[inline]
pub fn mutate<T: SolCall>(
    call: T,
    sender: Address,
    f: impl FnOnce(Address, T) -> Result<T::Return>,
) -> PrecompileResult {
    if StorageCtx.is_static() {
        return Ok(PrecompileOutput::new_reverted(
            0,
            StaticCallNotAllowed {}.abi_encode().into(),
        ));
    }
    f(sender, call).into_precompile_result(0, |ret| T::abi_encode_returns(&ret).into())
}

/// Dispatches a state-mutating call that returns no data.
///
/// Rejects static calls with [`StaticCallNotAllowed`].
#[inline]
pub fn mutate_void<T: SolCall>(
    call: T,
    sender: Address,
    f: impl FnOnce(Address, T) -> Result<()>,
) -> PrecompileResult {
    if StorageCtx.is_static() {
        return Ok(PrecompileOutput::new_reverted(
            0,
            StaticCallNotAllowed {}.abi_encode().into(),
        ));
    }
    f(sender, call).into_precompile_result(0, |()| Bytes::new())
}

/// Fills gas accounting fields on a [`PrecompileOutput`] from the storage context.
#[inline]
pub fn fill_precompile_output(
    mut output: PrecompileOutput,
    storage: &StorageCtx,
) -> PrecompileOutput {
    output.gas_used = storage.gas_used();
    if !output.reverted {
        output.gas_refunded = storage.gas_refunded();
    }
    output
}
