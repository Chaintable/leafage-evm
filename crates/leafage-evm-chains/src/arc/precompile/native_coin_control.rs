// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Native Coin Control Precompile
//!
//! This precompile implements native coin control operations including
//! blocklisting and unblocklisting addresses from receiving native coin transfers.

use super::helpers::{
    abi_decode_raw_with_zero6_validation, check_delegatecall, check_gas_remaining,
    check_staticcall, emit_event, new_reverted_with_early_penalty, read, write,
    PrecompileErrorOrRevert, ERR_EXECUTION_REVERTED, LOG_BASE_COST, LOG_TOPIC_COST,
    NATIVE_FIAT_TOKEN_ADDRESS, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, PRECOMPILE_SLOAD_GAS_COST,
    PRECOMPILE_SSTORE_GAS_COST,
};
use super::macros::precompile;
use crate::arc::{ArcHardfork, ArcHardforkFlags};
use alloy::primitives::{address, Address, StorageKey, U256};
use alloy::sol_types::{sol, SolCall, SolValue};
use alloy_evm::EvmInternals;
use revm::interpreter::Gas;
use revm::precompile::PrecompileOutput;

// Native coin control precompile address
pub const NATIVE_COIN_CONTROL_ADDRESS: Address =
    address!("0x1800000000000000000000000000000000000001");

// Allowed caller form NativeFiatToken
const ALLOWED_CALLER_ADDRESS: Address = NATIVE_FIAT_TOKEN_ADDRESS;

// Storage key for allowed caller (deprecated since Zero5)
const ALLOWED_CALLER_STORAGE_KEY: StorageKey = StorageKey::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
]);

// Gas costs
const BLOCKLISTED_EVENT_GAS_COST: u64 = LOG_BASE_COST + 2 * LOG_TOPIC_COST; // 2 topics
const UNBLOCKLISTED_EVENT_GAS_COST: u64 = LOG_BASE_COST + 2 * LOG_TOPIC_COST; // 2 topics

// Total gas costs for each operation

// - Reading allowed caller (2100 gas)
// - Writing blocklist storage (2900 gas)
// - Emitting event (1125 gas)
// Total: 6125 gas
const BLOCKLIST_GAS_COST: u64 =
    PRECOMPILE_SLOAD_GAS_COST + PRECOMPILE_SSTORE_GAS_COST + BLOCKLISTED_EVENT_GAS_COST;

// - Reading blocklist storage (2100 gas)
// Total: 2100 gas
pub const IS_BLOCKLISTED_GAS_COST: u64 = PRECOMPILE_SLOAD_GAS_COST;

// - Reading allowed caller (2100 gas)
// - Writing blocklist storage (2900 gas)
// - Emitting event (1125 gas)
// Total: 6125 gas
const UNBLOCKLIST_GAS_COST: u64 =
    PRECOMPILE_SLOAD_GAS_COST + PRECOMPILE_SSTORE_GAS_COST + UNBLOCKLISTED_EVENT_GAS_COST;

// Storage values
pub const BLOCKLISTED_STATUS: U256 = U256::from_limbs([1, 0, 0, 0]); // 0x01
pub const UNBLOCKLISTED_STATUS: U256 = U256::ZERO; // 0x00

// Error messages
const ERR_CANNOT_BLOCKLIST: &str = "Not enabled for blocklisting";
const ERR_CANNOT_UNBLOCKLIST: &str = "Not enabled for unblocklisting";

sol! {
    /// Native Coin Control precompile interface
    interface INativeCoinControl {
        /// Add an address to the blocklist
        function blocklist(address account) external returns (bool success);

        /// Check if an address is blocklisted
        function isBlocklisted(address account) external view returns (bool _isBlocklisted);

        /// Remove an address from the blocklist
        function unBlocklist(address account) external returns (bool success);
    }

    /// Events
    #[derive(Debug)]
    event Blocklisted(address indexed account);

    #[derive(Debug)]
    event UnBlocklisted(address indexed account);
}

/// Checks if the caller is authorized to call mutative native coin control functions
fn is_authorized(
    internals: &mut EvmInternals,
    caller: Address,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<bool, PrecompileErrorOrRevert> {
    // Get allowed caller
    let allowed_caller_output = read(
        internals,
        NATIVE_COIN_CONTROL_ADDRESS,
        ALLOWED_CALLER_STORAGE_KEY,
        gas_counter,
        hardfork_flags,
    )?;

    // Compare caller to allowed_caller_output
    let caller_word = U256::from_be_slice(caller.as_ref());
    let allowed_caller_word = U256::from_be_slice(&allowed_caller_output);
    Ok(caller_word == allowed_caller_word)
}

/// Computes the storage slot for a mapping key of type address
///
/// Delegates to the execution-config canonical implementation.
pub fn compute_is_blocklisted_storage_slot(key: Address) -> StorageKey {
    StorageKey::from(crate::arc::native::blocklist_storage_slot(key).to_be_bytes::<32>())
}

precompile!(run_native_coin_control, precompile_input, hardfork_flags; {
    INativeCoinControl::blocklistCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;

            // Check if static call is attempting to modify state
            check_staticcall(
                &precompile_input,
                &mut gas_counter,
            )?;

            // Decode arguments passed to blocklist function
            let args = abi_decode_raw_with_zero6_validation::<INativeCoinControl::blocklistCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            if hardfork_flags.is_active(ArcHardfork::Zero8) {
                let success_gas_floor = if hardfork_flags.is_active(ArcHardfork::Zero5) {
                    if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_BLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                    BLOCKLIST_GAS_COST - PRECOMPILE_SLOAD_GAS_COST
                } else {
                    if !is_authorized(
                        &mut precompile_input.internals,
                        precompile_input.caller,
                        &mut gas_counter,
                        hardfork_flags,
                    )? {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_BLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                    BLOCKLIST_GAS_COST
                };

                check_delegatecall(
                    NATIVE_COIN_CONTROL_ADDRESS,
                    &precompile_input,
                    &gas_counter,
                    hardfork_flags,
                )?;
                check_gas_remaining(&gas_counter, success_gas_floor)?;
            } else if hardfork_flags.is_active(ArcHardfork::Zero5) {
                if hardfork_flags.is_active(ArcHardfork::Zero6) {
                    // Auth first so the Zero6 early-revert penalty is reachable
                    // regardless of remaining gas; otherwise the success-path
                    // floor below OOGs callers in the 200..4024 gas window.
                    if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_BLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                    check_gas_remaining(
                        &gas_counter,
                        BLOCKLIST_GAS_COST - PRECOMPILE_SLOAD_GAS_COST,
                    )?;
                } else {
                    // Zero5-only: keep the original order to preserve consensus
                    // on networks already past the Zero5 activation block.
                    check_gas_remaining(
                        &gas_counter,
                        BLOCKLIST_GAS_COST - PRECOMPILE_SLOAD_GAS_COST,
                    )?;
                    if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_BLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                }
            } else {
                // Early return if not enough gas
                check_gas_remaining(&gas_counter, BLOCKLIST_GAS_COST)?;

                // Check authorization
                if !(is_authorized(
                    &mut precompile_input.internals,
                    precompile_input.caller,
                    &mut gas_counter,
                    hardfork_flags,
                )?) {
                    return Err(new_reverted_with_early_penalty(gas_counter, ERR_CANNOT_BLOCKLIST, hardfork_flags));
                }
            }

            if !hardfork_flags.is_active(ArcHardfork::Zero8) {
                check_delegatecall(
                    NATIVE_COIN_CONTROL_ADDRESS,
                    &precompile_input,
                    &gas_counter,
                    hardfork_flags,
                )?;
            }

            // Add to blocklist
            let storage_slot = compute_is_blocklisted_storage_slot(args.account);
            write(
                &mut precompile_input.internals,
                NATIVE_COIN_CONTROL_ADDRESS,
                storage_slot,
                &BLOCKLISTED_STATUS.to_be_bytes_vec(),
                &mut gas_counter,
                hardfork_flags,
            )?;

            // Emit event
            emit_event(
                &mut precompile_input.internals,
                NATIVE_COIN_CONTROL_ADDRESS,
                &Blocklisted {
                    account: args.account,
                },
                &mut gas_counter,
            )?;

            let output = true.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },

    INativeCoinControl::isBlocklistedCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;

            // Decode arguments passed to isBlocklisted function
            let args =
                abi_decode_raw_with_zero6_validation::<INativeCoinControl::isBlocklistedCall>(
                    input,
                    hardfork_flags,
                )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            // Early return if not enough gas
            check_gas_remaining(&gas_counter, IS_BLOCKLISTED_GAS_COST)?;

            // Check if address is blocklisted
            let storage_slot = compute_is_blocklisted_storage_slot(args.account);
            let storage_output = read(
                &mut precompile_input.internals,
                NATIVE_COIN_CONTROL_ADDRESS,
                storage_slot,
                &mut gas_counter,
                hardfork_flags,
            )?;

            let status = U256::from_be_slice(&storage_output);
            // Pessimistically assume blocklisted unless strictly matching unblocklisted status
            let is_blocked = status != UNBLOCKLISTED_STATUS;

            let output = is_blocked.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },

    INativeCoinControl::unBlocklistCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;

            // Check if static call is attempting to modify state
            check_staticcall(
                &precompile_input,
                &mut gas_counter,
            )?;

            // Decode arguments passed to unBlocklist function
            let args = abi_decode_raw_with_zero6_validation::<INativeCoinControl::unBlocklistCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            if hardfork_flags.is_active(ArcHardfork::Zero8) {
                let success_gas_floor = if hardfork_flags.is_active(ArcHardfork::Zero5) {
                    if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_UNBLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                    UNBLOCKLIST_GAS_COST - PRECOMPILE_SLOAD_GAS_COST
                } else {
                    if !is_authorized(
                        &mut precompile_input.internals,
                        precompile_input.caller,
                        &mut gas_counter,
                        hardfork_flags,
                    )? {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_UNBLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                    UNBLOCKLIST_GAS_COST
                };

                check_delegatecall(
                    NATIVE_COIN_CONTROL_ADDRESS,
                    &precompile_input,
                    &gas_counter,
                    hardfork_flags,
                )?;
                check_gas_remaining(&gas_counter, success_gas_floor)?;
            } else if hardfork_flags.is_active(ArcHardfork::Zero5) {
                if hardfork_flags.is_active(ArcHardfork::Zero6) {
                    // Auth first so the Zero6 early-revert penalty is reachable
                    // regardless of remaining gas; otherwise the success-path
                    // floor below OOGs callers in the 200..4024 gas window.
                    if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_UNBLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                    check_gas_remaining(
                        &gas_counter,
                        UNBLOCKLIST_GAS_COST - PRECOMPILE_SLOAD_GAS_COST,
                    )?;
                } else {
                    // Zero5-only: keep the original order to preserve consensus
                    // on networks already past the Zero5 activation block.
                    check_gas_remaining(
                        &gas_counter,
                        UNBLOCKLIST_GAS_COST - PRECOMPILE_SLOAD_GAS_COST,
                    )?;
                    if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                        return Err(new_reverted_with_early_penalty(
                            gas_counter,
                            ERR_CANNOT_UNBLOCKLIST,
                            hardfork_flags,
                        ));
                    }
                }
            } else {
                // Early return if not enough gas
                check_gas_remaining(&gas_counter, UNBLOCKLIST_GAS_COST)?;

                // Check authorization
                if !(is_authorized(
                    &mut precompile_input.internals,
                    precompile_input.caller,
                    &mut gas_counter,
                    hardfork_flags,
                )?) {
                    return Err(new_reverted_with_early_penalty(gas_counter, ERR_CANNOT_UNBLOCKLIST, hardfork_flags));
                }
            }

            if !hardfork_flags.is_active(ArcHardfork::Zero8) {
                check_delegatecall(
                    NATIVE_COIN_CONTROL_ADDRESS,
                    &precompile_input,
                    &gas_counter,
                    hardfork_flags,
                )?;
            }

            // Remove from blocklist
            let storage_slot = compute_is_blocklisted_storage_slot(args.account);
            write(
                &mut precompile_input.internals,
                NATIVE_COIN_CONTROL_ADDRESS,
                storage_slot,
                &UNBLOCKLISTED_STATUS.to_be_bytes_vec(),
                &mut gas_counter,
                hardfork_flags,
            )?;

            // Emit event
            emit_event(
                &mut precompile_input.internals,
                NATIVE_COIN_CONTROL_ADDRESS,
                &UnBlocklisted {
                    account: args.account,
                },
                &mut gas_counter,
            )?;

            let output = true.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },
});
