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

//! Native Coin Authority Precompile
//!
//! This precompile implements native coin operations including mint, burn,
//! transfer, and total supply management.

use super::helpers::{
    abi_decode_raw_with_zero6_validation, balance_decr, balance_incr, check_delegatecall,
    check_gas_remaining, check_staticcall, emit_event, new_reverted_with_early_penalty, read,
    transfer, write, PrecompileErrorOrRevert, ERR_BLOCKED_ADDRESS, ERR_EXECUTION_REVERTED,
    LOG_BASE_COST, LOG_DATA_COST, LOG_TOPIC_COST, NATIVE_FIAT_TOKEN_ADDRESS,
    PRECOMPILE_EARLY_REVERT_GAS_PENALTY, PRECOMPILE_SLOAD_GAS_COST, PRECOMPILE_SSTORE_GAS_COST,
};
use super::macros::precompile;
use super::native_coin_control::NATIVE_COIN_CONTROL_ADDRESS;
use super::native_coin_control::{compute_is_blocklisted_storage_slot, UNBLOCKLISTED_STATUS};
use crate::arc::{ArcHardfork, ArcHardforkFlags};
use alloy::primitives::{address, Address, StorageKey, U256};
use alloy::sol_types::{sol, SolCall, SolValue};
use alloy_evm::EvmInternals;
use revm::interpreter::Gas;
use revm::precompile::PrecompileOutput;

// Native coin authority precompile address
pub const NATIVE_COIN_AUTHORITY_ADDRESS: Address =
    address!("0x1800000000000000000000000000000000000000");

use revm::handler::SYSTEM_ADDRESS;

// Allowed caller from NativeFiatToken
const ALLOWED_CALLER_ADDRESS: Address = NATIVE_FIAT_TOKEN_ADDRESS;

// Storage key for allowed caller (deprecated since Zero5)
const ALLOWED_CALLER_STORAGE_KEY: StorageKey = StorageKey::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
]);

// Storage key for total supply
const TOTAL_SUPPLY_STORAGE_KEY: StorageKey = StorageKey::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
]);

const MINT_EVENT_GAS_COST: u64 = LOG_BASE_COST + 2 * LOG_TOPIC_COST + 32 * LOG_DATA_COST; // 2 topics + 32 bytes of data
const BURN_EVENT_GAS_COST: u64 = LOG_BASE_COST + 2 * LOG_TOPIC_COST + 32 * LOG_DATA_COST; // 2 topics + 32 bytes of data
const TRANSFER_EVENT_GAS_COST: u64 = LOG_BASE_COST + 3 * LOG_TOPIC_COST + 32 * LOG_DATA_COST; // 3 topics + 32 bytes of data

// Total gas costs for each operation

// Mint (pre-Zero5): NativeCoinMinted event (2 topics)
// - Reading allowed caller (2100 gas)
// - Reading blocked to (2100 gas)
// - Reading total supply (2100 gas)
// - Reading account balance (2100 gas)
// - Writing total supply (2900 gas)
// - Writing account balance (2900 gas)
// - Emitting NativeCoinMinted event (1381 gas)
// Total: 15,581 gas
const MINT_GAS_COST: u64 =
    4 * PRECOMPILE_SLOAD_GAS_COST + 2 * PRECOMPILE_SSTORE_GAS_COST + MINT_EVENT_GAS_COST;

// Burn (pre-Zero5): NativeCoinBurned event (2 topics)
// - Reading allowed caller (2100 gas)
// - Reading blocked from (2100 gas)
// - Reading account balance (2100 gas)
// - Reading total supply (2100 gas)
// - Writing account balance (2900 gas)
// - Writing total supply (2900 gas)
// - Emitting NativeCoinBurned event (1381 gas)
// Total: 15,581 gas
const BURN_GAS_COST: u64 =
    4 * PRECOMPILE_SLOAD_GAS_COST + 2 * PRECOMPILE_SSTORE_GAS_COST + BURN_EVENT_GAS_COST;

// - Reading allowed caller (2100 gas) (removed since Zero5)
// - Reading blocked from (2100 gas)
// - Reading blocked to (2100 gas)
// - Reading account balance x2 (4200 gas)
// - Writing account balance x2 (5800 gas)
// - Emitting event (1756 gas)
// Total: 18056 gas
const TRANSFER_GAS_COST: u64 =
    5 * PRECOMPILE_SLOAD_GAS_COST + 2 * PRECOMPILE_SSTORE_GAS_COST + TRANSFER_EVENT_GAS_COST;
// The gas cost for a transfer operation when the amount is zero
// - Reading allowed caller (2100 gas) (removed since Zero5)
// - Reading blocked from (2100 gas)
// - Reading blocked to (2100 gas)
const TRANSFER_GAS_COST_WITH_ZERO_AMOUNT: u64 = 3 * PRECOMPILE_SLOAD_GAS_COST;

const TOTAL_SUPPLY_GAS_COST: u64 = PRECOMPILE_SLOAD_GAS_COST;

// Error messages
const ERR_CANNOT_MINT: &str = "Not enabled native coin minter";
const ERR_CANNOT_BURN: &str = "Not enabled native coin burner";
const ERR_CANNOT_TRANSFER: &str = "Not enabled for native coin transfers";
const ERR_OVERFLOW: &str = "Arithmetic overflow";
const ERR_ZERO_AMOUNT: &str = "Zero amount invalid";
use super::helpers::ERR_ZERO_ADDRESS;

sol! {
    /// Native Coin Authority precompile interface
    interface INativeCoinAuthority {
        /// Mint new coins to the specified address
        function mint(address to, uint256 amount) external returns (bool);

        /// Burn coins from the specified address
        function burn(address from, uint256 amount) external returns (bool);

        /// Transfer coins between addresses
        function transfer(address from, address to, uint256 amount) external returns (bool);

        /// Get the total supply of native coins
        function totalSupply() external view returns (uint256 supply);
    }

    /// Events
    #[derive(Debug)]
    event NativeCoinMinted(address indexed recipient, uint256 amount);

    #[derive(Debug)]
    event NativeCoinBurned(address indexed from, uint256 amount);

    #[derive(Debug)]
    event NativeCoinTransferred(address indexed from, address indexed to, uint256 amount);

    /// ERC-20 Transfer event (EIP-7708), used under Zero5 for native coin transfers
    #[derive(Debug)]
    event Transfer(address indexed from, address indexed to, uint256 value);
}

/// Checks if the caller is authorized to call mutative native coin authority functions
fn is_authorized(
    internals: &mut EvmInternals,
    caller: Address,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<bool, PrecompileErrorOrRevert> {
    // Get allowed caller
    let allowed_caller_output = read(
        internals,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        ALLOWED_CALLER_STORAGE_KEY,
        gas_counter,
        hardfork_flags,
    )?;

    // Compare caller to allowed_caller_output
    let caller_word = U256::from_be_slice(caller.as_ref());
    let allowed_caller_word = U256::from_be_slice(&allowed_caller_output);
    Ok(caller_word == allowed_caller_word)
}

fn is_blocklisted(
    internals: &mut EvmInternals,
    address: Address,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<bool, PrecompileErrorOrRevert> {
    // Get address storage slot for blocklist
    let storage_slot = compute_is_blocklisted_storage_slot(address);
    let storage_output = read(
        internals,
        NATIVE_COIN_CONTROL_ADDRESS,
        storage_slot,
        gas_counter,
        hardfork_flags,
    )?;

    Ok(!U256::from_be_slice(&storage_output).eq(&UNBLOCKLISTED_STATUS))
}

precompile!(run_native_coin_authority, precompile_input, hardfork_flags; {
    INativeCoinAuthority::mintCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;
            // Check if static call is attempting to modify state
            check_staticcall(
                &precompile_input,
                &mut gas_counter,
            )?;

            // Decode arguments passed to mint function
            let args = abi_decode_raw_with_zero6_validation::<INativeCoinAuthority::mintCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED)
                )?;

            if hardfork_flags.is_active(ArcHardfork::Zero5) {
                // Zero5+: Skip early gas check - warm/cold pricing makes upfront calculation unreliable
                // Check authorization
                if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                    return Err(new_reverted_with_early_penalty(
                        gas_counter,
                        ERR_CANNOT_MINT,
                        hardfork_flags,
                    ));
                }
            } else {
                // Early return if not enough gas
                check_gas_remaining(&gas_counter, MINT_GAS_COST)?;

                // Check authorization
                if !is_authorized(
                    &mut precompile_input.internals,
                    precompile_input.caller,
                    &mut gas_counter,
                    hardfork_flags,
                )? {
                    return Err(new_reverted_with_early_penalty(gas_counter, ERR_CANNOT_MINT, hardfork_flags));
                }
            }

            // Prevent delegate calls
            check_delegatecall(
                NATIVE_COIN_AUTHORITY_ADDRESS,
                &precompile_input,
                &gas_counter,
                hardfork_flags,
            )?;

            // Reject minting to zero address (Zero5+)
            if hardfork_flags.is_active(ArcHardfork::Zero5) && args.to == Address::ZERO {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_ZERO_ADDRESS, hardfork_flags));
            }

            // Check blocklist
            if is_blocklisted(&mut precompile_input.internals, args.to, &mut gas_counter, hardfork_flags)? {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_BLOCKED_ADDRESS, hardfork_flags));
            }

            // Validate amount
            if args.amount == U256::ZERO {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_ZERO_AMOUNT, hardfork_flags));
            }

            // Read current total supply
            let total_supply_output = read(
                &mut precompile_input.internals,
                NATIVE_COIN_AUTHORITY_ADDRESS,
                TOTAL_SUPPLY_STORAGE_KEY,
                &mut gas_counter,
                hardfork_flags,
            )?;
            let current_total_supply = U256::from_be_slice(&total_supply_output);

            // Check for overflow
            let new_total_supply = match current_total_supply.checked_add(args.amount) {
                Some(new_total_supply) => new_total_supply,
                None => return Err(new_reverted_with_early_penalty(gas_counter, ERR_OVERFLOW, hardfork_flags)),
            };

            // Write new total supply
            write(
                &mut precompile_input.internals,
                NATIVE_COIN_AUTHORITY_ADDRESS,
                TOTAL_SUPPLY_STORAGE_KEY,
                &new_total_supply.to_be_bytes_vec(),
                &mut gas_counter,
                hardfork_flags,
            )?;

            // Update account balance
            balance_incr(&mut precompile_input.internals, args.to, args.amount, &mut gas_counter, hardfork_flags)?;

            // Emit event: ERC-20 Transfer(0x0, to) under Zero5, NativeCoinMinted otherwise.
            // Address::ZERO as `from` follows the ERC-20 convention for minting. This is
            // intentionally allowed here even though CALL/CREATE value transfers reject
            // Address::ZERO (see check_blocklist_and_create_log in evm.rs).
            if hardfork_flags.is_active(ArcHardfork::Zero5) {
                emit_event(&mut precompile_input.internals, SYSTEM_ADDRESS, &Transfer {
                    from: Address::ZERO,
                    to: args.to,
                    value: args.amount,
                }, &mut gas_counter)?;
            } else {
                emit_event(&mut precompile_input.internals, NATIVE_COIN_AUTHORITY_ADDRESS, &NativeCoinMinted {
                    recipient: args.to,
                    amount: args.amount,
                }, &mut gas_counter)?;
            }

            let output = true.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },

    INativeCoinAuthority::burnCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;
            // Check if static call is attempting to modify state
            check_staticcall(
                &precompile_input,
                &mut gas_counter,
            )?;

            // Decode arguments passed to burn function
            let args = abi_decode_raw_with_zero6_validation::<INativeCoinAuthority::burnCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            // Check authorization
            if hardfork_flags.is_active(ArcHardfork::Zero5) {
                // Zero5+: Skip early gas check - warm/cold pricing makes upfront calculation unreliable
                if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                    return Err(new_reverted_with_early_penalty(
                        gas_counter,
                        ERR_CANNOT_BURN,
                        hardfork_flags,
                    ));
                }
            } else {
                // Early return if not enough gas
                check_gas_remaining(&gas_counter, BURN_GAS_COST)?;

                if !(is_authorized(&mut precompile_input.internals, precompile_input.caller, &mut gas_counter, hardfork_flags)?) {
                    return Err(new_reverted_with_early_penalty(gas_counter, ERR_CANNOT_BURN, hardfork_flags));
                }
            }

            // Prevent delegate calls
            check_delegatecall(
                NATIVE_COIN_AUTHORITY_ADDRESS,
                &precompile_input,
                &gas_counter,
                hardfork_flags,
            )?;

            // Reject burning from zero address (Zero5+)
            if hardfork_flags.is_active(ArcHardfork::Zero5) && args.from == Address::ZERO {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_ZERO_ADDRESS, hardfork_flags));
            }

            // Check blocklist
            if is_blocklisted(&mut precompile_input.internals, args.from, &mut gas_counter, hardfork_flags)? {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_BLOCKED_ADDRESS, hardfork_flags));
            }

            // Validate amount
            if args.amount == U256::ZERO {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_ZERO_AMOUNT, hardfork_flags));
            }

            // Check balance and burn tokens
            balance_decr(&mut precompile_input.internals, args.from, args.amount, &mut gas_counter, hardfork_flags)?;

            // Adjust total supply
            let total_supply_output = read(
                &mut precompile_input.internals,
                NATIVE_COIN_AUTHORITY_ADDRESS,
                TOTAL_SUPPLY_STORAGE_KEY,
                &mut gas_counter,
                hardfork_flags,
            )?;
            let current_total_supply = U256::from_be_slice(&total_supply_output);

            // Write new total supply
            // Underflow cannot happen due to the balance check
            write(
                &mut precompile_input.internals,
                NATIVE_COIN_AUTHORITY_ADDRESS,
                TOTAL_SUPPLY_STORAGE_KEY,
                &current_total_supply.saturating_sub(args.amount).to_be_bytes_vec(),
                &mut gas_counter,
                hardfork_flags,
            )?;

            // Emit event: ERC-20 Transfer(from, 0x0) under Zero5, NativeCoinBurned otherwise.
            // Address::ZERO as `to` follows the ERC-20 convention for burning. This is
            // intentionally allowed here even though CALL/CREATE value transfers reject
            // Address::ZERO (see check_blocklist_and_create_log in evm.rs).
            if hardfork_flags.is_active(ArcHardfork::Zero5) {
                emit_event(&mut precompile_input.internals, SYSTEM_ADDRESS, &Transfer {
                    from: args.from,
                    to: Address::ZERO,
                    value: args.amount,
                }, &mut gas_counter)?;
            } else {
                emit_event(&mut precompile_input.internals, NATIVE_COIN_AUTHORITY_ADDRESS, &NativeCoinBurned {
                    from: args.from,
                    amount: args.amount,
                }, &mut gas_counter)?;
            }

            // Return response
            let output = true.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },

    INativeCoinAuthority::transferCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;
            // Check if static call is attempting to modify state
            check_staticcall(
                &precompile_input,
                &mut gas_counter,
            )?;

            // Decode arguments passed to transfer function
            let args = abi_decode_raw_with_zero6_validation::<INativeCoinAuthority::transferCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            // Check authorization
            if hardfork_flags.is_active(ArcHardfork::Zero5) {
                // Zero5+: Skip early gas check - warm/cold pricing makes upfront calculation unreliable
                if precompile_input.caller != ALLOWED_CALLER_ADDRESS {
                    return Err(new_reverted_with_early_penalty(
                        gas_counter,
                        ERR_CANNOT_TRANSFER,
                        hardfork_flags,
                    ));
                }
            } else {
                // The gas cost is different if the amount is zero or not
                let expect_gas_cost = if args.amount != U256::ZERO {
                    TRANSFER_GAS_COST
                } else {
                    TRANSFER_GAS_COST_WITH_ZERO_AMOUNT
                };

                // Early return if not enough gas
                check_gas_remaining(&gas_counter, expect_gas_cost)?;

                if !(is_authorized(&mut precompile_input.internals, precompile_input.caller, &mut gas_counter, hardfork_flags)?) {
                    return Err(new_reverted_with_early_penalty(gas_counter, ERR_CANNOT_TRANSFER, hardfork_flags));
                }
            }

            // Prevent delegate calls
            check_delegatecall(
                NATIVE_COIN_AUTHORITY_ADDRESS,
                &precompile_input,
                &gas_counter,
                hardfork_flags,
            )?;

            // Reject transfers involving zero address (Zero5+)
            if hardfork_flags.is_active(ArcHardfork::Zero5)
                && (args.from == Address::ZERO || args.to == Address::ZERO)
            {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_ZERO_ADDRESS, hardfork_flags));
            }

            // Check blocklist
            if is_blocklisted(&mut precompile_input.internals, args.from, &mut gas_counter, hardfork_flags)? {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_BLOCKED_ADDRESS, hardfork_flags));
            }
            if is_blocklisted(&mut precompile_input.internals, args.to, &mut gas_counter, hardfork_flags)? {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_BLOCKED_ADDRESS, hardfork_flags));
            }

            // Zero amount transfers are allowed, but do not emit an event
            if args.amount != U256::ZERO {
                // Note on self-transfers (from == to): REVM's transfer_loaded() early-returns
                // without touching balances for self-transfers. Here we still call transfer()
                // which performs balance_decr + balance_incr (net zero change). This is
                // functionally correct and intentionally kept to preserve identical gas costs
                // across the Zero5 hardfork boundary — skipping the balance ops would reduce
                // gas consumption and break the "gas cost unchanged" invariant.
                transfer(&mut precompile_input.internals, args.from, args.to, args.amount, &mut gas_counter, hardfork_flags)?;

                // Emit event: ERC-20 Transfer under Zero5, NativeCoinTransferred otherwise.
                // EIP-7708: self-transfers (from == to) do not emit a log.
                if hardfork_flags.is_active(ArcHardfork::Zero5) {
                    if args.from != args.to {
                        emit_event(&mut precompile_input.internals, SYSTEM_ADDRESS, &Transfer {
                            from: args.from,
                            to: args.to,
                            value: args.amount,
                        }, &mut gas_counter)?;
                    }
                } else {
                    emit_event(&mut precompile_input.internals, NATIVE_COIN_AUTHORITY_ADDRESS, &NativeCoinTransferred {
                        from: args.from,
                        to: args.to,
                        amount: args.amount,
                    }, &mut gas_counter)?;
                }
            }

            // Return response
            let output = true.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },
    INativeCoinAuthority::totalSupplyCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;

            if !hardfork_flags.is_active(ArcHardfork::Zero6) && !input.is_empty() {
                return Err(PrecompileErrorOrRevert::new_reverted_with_penalty(
                    gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED));
            }

            // Early return if not enough gas
            check_gas_remaining(&gas_counter, TOTAL_SUPPLY_GAS_COST)?;

            // Read the total supply
            let total_supply_output = read(
                &mut precompile_input.internals,
                NATIVE_COIN_AUTHORITY_ADDRESS,
                TOTAL_SUPPLY_STORAGE_KEY,
                &mut gas_counter,
                hardfork_flags,
            )?;
            let total_supply = U256::from_be_slice(&total_supply_output);

            // Return response
            let output = total_supply.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },
});
