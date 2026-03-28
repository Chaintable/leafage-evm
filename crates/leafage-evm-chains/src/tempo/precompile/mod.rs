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

pub mod account_keychain;
pub mod error;
pub mod fee_manager;
pub mod nonce;
pub mod stablecoin_dex;
pub mod storage;
pub mod storage_types;
pub mod tip20;
pub mod tip20_factory;
pub mod tip403_registry;
pub mod validator_config;
pub mod validator_config_v2;

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
use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
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

// ===========================================================================
// Precompile registration
// ===========================================================================

/// Registers all 9 Tempo precompiles into the given [`PrecompilesMap`].
///
/// Uses [`set_precompile_lookup`] to install a closure that matches addresses to
/// the appropriate Tempo precompile. TIP-20 tokens use prefix matching; all other
/// precompiles use exact address matching.
///
/// Each precompile is wrapped via the [`tempo_precompile!`] macro which handles
/// DELEGATECALL rejection, `LeafageStorageProvider` setup, and gas accounting.
pub fn extend_tempo_precompiles(precompiles: &mut PrecompilesMap, chain_id: u64) {
    precompiles.set_precompile_lookup(move |address: &Address| {
        if tip20::is_tip20_prefix(*address) {
            Some(create_tip20_precompile(*address, chain_id))
        } else if *address == TIP20_FACTORY_ADDRESS {
            Some(create_tip20_factory_precompile(chain_id))
        } else if *address == TIP403_REGISTRY_ADDRESS {
            Some(create_tip403_registry_precompile(chain_id))
        } else if *address == TIP_FEE_MANAGER_ADDRESS {
            Some(create_fee_manager_precompile(chain_id))
        } else if *address == STABLECOIN_DEX_ADDRESS {
            Some(create_stablecoin_dex_precompile(chain_id))
        } else if *address == NONCE_PRECOMPILE_ADDRESS {
            Some(create_nonce_precompile(chain_id))
        } else if *address == VALIDATOR_CONFIG_ADDRESS {
            Some(create_validator_config_precompile(chain_id))
        } else if *address == ACCOUNT_KEYCHAIN_ADDRESS {
            Some(create_account_keychain_precompile(chain_id))
        } else if *address == VALIDATOR_CONFIG_V2_ADDRESS {
            Some(create_validator_config_v2_precompile(chain_id))
        } else {
            None
        }
    });
}

fn create_tip20_precompile(address: Address, chain_id: u64) -> DynPrecompile {
    tempo_precompile!("TIP20", chain_id, |input| {
        tip20::TIP20Token::from_address_unchecked(address)
    })
}

fn create_tip20_factory_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("TIP20Factory", chain_id, |input| {
        tip20_factory::TIP20Factory::new()
    })
}

fn create_tip403_registry_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("TIP403Registry", chain_id, |input| {
        tip403_registry::TIP403Registry::new()
    })
}

fn create_fee_manager_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("TipFeeManager", chain_id, |input| {
        fee_manager::TipFeeManager::new()
    })
}

fn create_stablecoin_dex_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("StablecoinDEX", chain_id, |input| {
        stablecoin_dex::StablecoinDEX::new()
    })
}

fn create_nonce_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("NonceManager", chain_id, |input| {
        nonce::NonceManager::new()
    })
}

fn create_validator_config_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("ValidatorConfig", chain_id, |input| {
        validator_config::ValidatorConfig::new()
    })
}

fn create_account_keychain_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("AccountKeychain", chain_id, |input| {
        account_keychain::AccountKeychain::new()
    })
}

fn create_validator_config_v2_precompile(chain_id: u64) -> DynPrecompile {
    tempo_precompile!("ValidatorConfigV2", chain_id, |input| {
        validator_config_v2::ValidatorConfigV2::new()
    })
}

// ===========================================================================
// caller_gas_allowance — read TIP-20 balance for estimateGas gas cap
// ===========================================================================

/// Tempo gas price scaling factor (1e12).
/// Ported from Tempo writer: `tempo_primitives::transaction::TEMPO_GAS_PRICE_SCALING_FACTOR`.
const TEMPO_GAS_PRICE_SCALING_FACTOR: alloy::primitives::U256 =
    alloy::primitives::uint!(1_000_000_000_000_U256);

/// Computes the maximum gas the caller can afford, based on TIP-20 fee token balance.
///
/// Ported from Tempo writer: `crates/node/src/rpc/mod.rs::caller_gas_allowance`.
///
/// Returns `fee_token_balance * SCALING_FACTOR / gas_price`.
/// Returns `None` if gas_price is 0 or on any storage read error.
pub fn tempo_caller_gas_allowance<DB: revm::DatabaseRef>(
    db: &DB,
    caller: alloy::primitives::Address,
    gas_price: u128,
    timestamp: u64,
    chain_id: u64,
) -> Option<u64>
where
    DB::Error: core::fmt::Debug,
{
    use crate::tempo::hardfork::TempoHardfork;

    if gas_price == 0 {
        return None;
    }

    let spec = TempoHardfork::from_timestamp(timestamp);

    // Simplified fee token resolution:
    // 1. Read user_tokens[caller] from FeeManager
    // 2. If zero, fallback to DEFAULT_FEE_TOKEN (PATH_USD)
    //
    // Writer has 5-tier priority (tx.fee_token → setUserToken decode → user_tokens →
    // TIP20 inference → DEX inference → default). For estimateGas without tx context,
    // reading stored preference + default covers the common case.
    let fee_token = storage::with_read_only_storage_ctx(db, spec, chain_id, || {
        let user_token = fee_manager::TipFeeManager::new()
            .user_tokens[caller]
            .read()
            .ok()?;
        if user_token.is_zero() {
            Some(DEFAULT_FEE_TOKEN)
        } else {
            Some(user_token)
        }
    })?;

    // Read TIP-20 balance of fee token for caller.
    let balance = storage::with_read_only_storage_ctx(db, spec, chain_id, || {
        tip20::TIP20Token::from_address_unchecked(fee_token)
            .balances[caller]
            .read()
            .ok()
    })?;

    // caller_gas_allowance = balance * SCALING_FACTOR / gas_price
    Some(
        balance
            .saturating_mul(TEMPO_GAS_PRICE_SCALING_FACTOR)
            .checked_div(alloy::primitives::U256::from(gas_price))
            .unwrap_or_default()
            .saturating_to(),
    )
}
