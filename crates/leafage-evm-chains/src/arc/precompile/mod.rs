// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

mod helpers;
mod macros;
mod native_coin_authority;
mod native_coin_control;
mod pq;
mod system_accounting;

use crate::arc::{ArcHardfork, ArcHardforkFlags};
use alloy::primitives::Address;
use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::precompile::PrecompileId;

use native_coin_authority::NATIVE_COIN_AUTHORITY_ADDRESS;
use native_coin_control::NATIVE_COIN_CONTROL_ADDRESS;
use pq::PQ_ADDRESS;
use system_accounting::SYSTEM_ACCOUNTING_ADDRESS;

/// Adds the Arc v0.7.3 current-mainnet precompiles to the Osaka standard set.
pub(crate) fn extend_arc_precompiles(
    precompiles: &mut PrecompilesMap,
    hardfork_flags: ArcHardforkFlags,
) {
    precompiles.ensure_dynamic_precompiles();
    precompiles.set_precompile_lookup(move |address: &Address| match *address {
        NATIVE_COIN_AUTHORITY_ADDRESS => Some(DynPrecompile::new_stateful(
            PrecompileId::Custom("NATIVE_COIN_AUTHORITY".into()),
            move |input| native_coin_authority::run_native_coin_authority(input, hardfork_flags),
        )),
        NATIVE_COIN_CONTROL_ADDRESS => Some(DynPrecompile::new_stateful(
            PrecompileId::Custom("NATIVE_COIN_CONTROL".into()),
            move |input| native_coin_control::run_native_coin_control(input, hardfork_flags),
        )),
        SYSTEM_ACCOUNTING_ADDRESS => Some(DynPrecompile::new_stateful(
            PrecompileId::Custom("SYSTEM_ACCOUNTING".into()),
            move |input| system_accounting::run_system_accounting(input, hardfork_flags),
        )),
        PQ_ADDRESS if hardfork_flags.is_active(ArcHardfork::Zero6) => Some(
            DynPrecompile::new_stateful(PrecompileId::Custom("PQ".into()), move |input| {
                pq::run_pq(input, hardfork_flags)
            }),
        ),
        _ => None,
    });
}

#[cfg(test)]
mod tests;
