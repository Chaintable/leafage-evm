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

use alloy::primitives::{Address, Bytes, StorageKey, U256};
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use alloy_evm::precompiles::PrecompileInput;
use alloy_evm::EvmInternals;
use revm::context_interface::cfg::gas::CALL_STIPEND;
use revm::context_interface::journaled_state::account::JournaledAccountTr;
use revm::context_interface::journaled_state::TransferError;
use revm::interpreter::Gas;
use revm::precompile::{PrecompileError, PrecompileOutput};
use revm::primitives::{address, KECCAK_EMPTY};
use revm::state::AccountInfo;

use crate::arc::{ArcHardfork, ArcHardforkFlags};

// system addresses in genesis
pub const NATIVE_FIAT_TOKEN_ADDRESS: Address =
    address!("0x3600000000000000000000000000000000000000");

/// Selector for the Solidity Error(string) format used in revert messages.
pub const REVERT_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

/// Approximate gas costs for precompile read / writes
pub const PRECOMPILE_SSTORE_GAS_COST: u64 = 2900;
pub const PRECOMPILE_SLOAD_GAS_COST: u64 = 2100;
/// EIP-161 account creation surcharge when crediting an empty account.
pub const PRECOMPILE_EMPTY_ACCOUNT_GAS_COST: u64 = 25_000;

/// Gas costs for emitting a log
pub const LOG_BASE_COST: u64 = 375; // Base cost for emitting a log
pub const LOG_TOPIC_COST: u64 = 375; // Cost per log topic
pub const LOG_DATA_COST: u64 = 8; // Cost per byte of log data

/// Common precompile revert messages
pub const ERR_EXECUTION_REVERTED: &str = "Execution reverted";
pub const ERR_INSUFFICIENT_FUNDS: &str = "Insufficient funds";
pub const ERR_OVERFLOW: &str = "Arithmetic overflow";
pub const ERR_INVALID_CALLER: &str = "Invalid caller";
pub const ERR_CLEAR_EMPTY: &str = "Cannot clear balance of empty account";
pub const ERR_DELEGATE_CALL_NOT_ALLOWED: &str = "Delegate call not allowed";
pub const ERR_STATE_CHANGE_DURING_STATIC_CALL: &str = "State change during static call";
pub const ERR_BLOCKED_ADDRESS: &str = "Blocked address";
pub const ERR_ZERO_ADDRESS: &str = "Zero address not allowed";
pub const ERR_SELFDESTRUCTED_BALANCE_INCREASED: &str =
    "Cannot increase the balance of selfdestructed account";

/// Encodes a revert error string into ABI‑encoded bytes according to Solidity’s Error(string) format.
///
/// The returned bytes consist of:
/// - 4 bytes selector: 0x08c379a0
/// - ABI-encoded string value of the error message.
pub fn revert_message_to_bytes(msg: &str) -> Bytes {
    let encoded = msg.abi_encode();
    let mut result = Vec::with_capacity(REVERT_SELECTOR.len().saturating_add(encoded.len()));
    result.extend_from_slice(&REVERT_SELECTOR);
    result.extend_from_slice(&encoded);
    Bytes::from(result)
}

/// Gas penalty added to early-path reverts so callers cannot probe precompiles
/// for free.
///
/// Pre-Zero6: applied only to ABI decode failures (truncated input, unknown
/// selector) via `new_reverted_with_penalty`.
///
/// Zero6+: also applied to authorization and validation reverts (unauthorized
/// caller, blocklist, zero address, zero amount, overflow) via
/// [`new_reverted_with_early_penalty`].
///
pub(crate) const PRECOMPILE_EARLY_REVERT_GAS_PENALTY: u64 = 200;

/// Enum to represent either a reverted precompile output or an error
pub(crate) enum PrecompileErrorOrRevert {
    Revert(PrecompileOutput),
    Error(PrecompileError),
}

impl PrecompileErrorOrRevert {
    pub(crate) fn new_reverted(gas_counter: Gas, msg: &str) -> Self {
        Self::Revert(PrecompileOutput::new_reverted(
            gas_counter.used(),
            revert_message_to_bytes(msg),
        ))
    }

    pub(crate) fn new_reverted_with_penalty(gas_counter: Gas, gas_penalty: u64, msg: &str) -> Self {
        let mut gas_with_penalty = gas_counter;
        if !gas_with_penalty.record_cost(gas_penalty) {
            return Self::Error(PrecompileError::OutOfGas);
        }
        Self::Revert(PrecompileOutput::new_reverted(
            gas_with_penalty.used(),
            revert_message_to_bytes(msg),
        ))
    }
}

/// Gas cost to load an account balance for stateful precompiles.
///
/// Under Zero6+, applies EIP-2929 warm/cold pricing. Before Zero6, a flat
/// cost is charged (matches pre-hardfork behavior for the `balance_incr`,
/// `balance_decr` and `transfer` helpers).
fn account_load_cost(is_cold: bool, hardfork_flags: ArcHardforkFlags) -> u64 {
    if hardfork_flags.is_active(ArcHardfork::Zero6) {
        if is_cold {
            revm::interpreter::gas::COLD_ACCOUNT_ACCESS_COST
        } else {
            revm::interpreter::gas::WARM_STORAGE_READ_COST
        }
    } else {
        PRECOMPILE_SLOAD_GAS_COST
    }
}

fn storage_io_error(op: &str, e: impl core::fmt::Debug) -> PrecompileErrorOrRevert {
    PrecompileErrorOrRevert::Error(PrecompileError::Other(
        format!("Storage {op} failed: {e:?}").into(),
    ))
}

fn record_zero6_empty_account_creation_cost(
    gas_counter: &mut Gas,
    account_info: &AccountInfo,
    amount: U256,
    hardfork_flags: ArcHardforkFlags,
) -> Result<(), PrecompileErrorOrRevert> {
    if hardfork_flags.is_active(ArcHardfork::Zero6) && !amount.is_zero() && account_info.is_empty()
    {
        record_cost_or_out_of_gas(gas_counter, PRECOMPILE_EMPTY_ACCOUNT_GAS_COST)?;
    }
    Ok(())
}

pub(crate) fn record_cost_or_out_of_gas(
    gas_counter: &mut Gas,
    cost: u64,
) -> Result<(), PrecompileErrorOrRevert> {
    if !gas_counter.record_cost(cost) {
        return Err(PrecompileErrorOrRevert::Error(PrecompileError::OutOfGas));
    }
    Ok(())
}

pub(crate) fn check_gas_remaining(
    gas_counter: &Gas,
    cost: u64,
) -> Result<(), PrecompileErrorOrRevert> {
    if gas_counter.remaining() < cost {
        return Err(PrecompileErrorOrRevert::Error(PrecompileError::OutOfGas));
    }
    Ok(())
}

impl From<PrecompileErrorOrRevert> for Result<PrecompileOutput, PrecompileError> {
    fn from(val: PrecompileErrorOrRevert) -> Self {
        match val {
            PrecompileErrorOrRevert::Revert(output) => Ok(output.reverted()),
            PrecompileErrorOrRevert::Error(error) => Err(error),
        }
    }
}

/// Build a revert that charges [`PRECOMPILE_EARLY_REVERT_GAS_PENALTY`]
/// when Zero6 is active, and zero gas otherwise.
///
/// Use at early-path reverts (unauthorized caller, blocklist, zero address,
/// zero amount, overflow) to give uniform gas accounting under Zero6 and
/// prevent free probing of precompile revert paths.
pub(crate) fn new_reverted_with_early_penalty(
    gas_counter: Gas,
    msg: &str,
    hardfork_flags: ArcHardforkFlags,
) -> PrecompileErrorOrRevert {
    if hardfork_flags.is_active(ArcHardfork::Zero6) {
        PrecompileErrorOrRevert::new_reverted_with_penalty(
            gas_counter,
            PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
            msg,
        )
    } else {
        PrecompileErrorOrRevert::new_reverted(gas_counter, msg)
    }
}

/// ABI-decodes raw precompile call arguments.
///
/// Pre-Zero6, this preserves the legacy lenient Alloy decode behavior. Zero6
/// switches to validated decoding, which rejects non-canonical ABI padding for
/// short static types such as `address`, `bool`, and `uint64`.
pub(crate) fn abi_decode_raw_with_zero6_validation<C: SolCall>(
    input: &[u8],
    hardfork_flags: ArcHardforkFlags,
) -> alloy::sol_types::Result<C> {
    if hardfork_flags.is_active(ArcHardfork::Zero6) {
        C::abi_decode_raw_validate(input)
    } else {
        C::abi_decode_raw(input)
    }
}

/// Reads a value from storage for stateful precompiles.
///
/// # Parameters
/// - `internals`: The execution context with journal access
/// - `address`: The address whose storage to read from
/// - `storage_key`: The storage slot to read
/// - `gas_counter`: Available gas for this operation
/// - `hardfork`: The current hardfork for gas calculation
///
/// # Gas Cost
/// - Pre-Zero5: Fixed cost of 2,100 gas units
/// - Zero5+: EIP-2929 warm/cold aware (100 warm, 2100 cold)
///
/// # Returns
/// - `Ok(Bytes)`: The stored value as big-endian bytes
/// - `Err(PrecompileErrorOrRevert)`: If out of gas or storage read fails
///
/// # Example
/// ```rust,ignore
/// let output = read(internals, precompile_address, StorageKey::ZERO, gas_counter, &hardfork)?;
/// let value = U256::from_be_slice(&output);
/// ```
pub(crate) fn read(
    internals: &mut EvmInternals,
    address: Address,
    storage_key: StorageKey,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<Bytes, PrecompileErrorOrRevert> {
    if hardfork_flags.is_active(ArcHardfork::Zero5) {
        let mut account = internals
            .load_account_mut(address)
            .map_err(|e| storage_io_error("read", e))?
            .data;

        // Probe slot warmth without DB I/O (skip_cold_load=true).
        // Warm → Ok with cached value. Cold → ColdLoadSkipped error, retry after charging.
        match account.sload(storage_key.into(), true) {
            Ok(slot_load) => {
                record_cost_or_out_of_gas(
                    gas_counter,
                    revm::interpreter::gas::WARM_STORAGE_READ_COST,
                )?;
                Ok(slot_load.data.present_value().to_be_bytes_vec().into())
            }
            Err(e) if e.is_cold_load_skipped() => {
                record_cost_or_out_of_gas(gas_counter, revm::interpreter::gas::COLD_SLOAD_COST)?;
                let slot_load = account
                    .sload(storage_key.into(), false)
                    .map_err(|e| storage_io_error("read", e))?;
                Ok(slot_load.data.present_value().to_be_bytes_vec().into())
            }
            Err(e) => Err(storage_io_error("read", e)),
        }
    } else {
        record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SLOAD_GAS_COST)?;
        let state_load = internals
            .sload(address, storage_key.into())
            .map_err(|e| storage_io_error("read", e))?;
        Ok(state_load.data.to_be_bytes_vec().into())
    }
}

/// Value-change component of SSTORE gas, excluding the cold-load penalty.
///
/// Mirrors revm v29 `istanbul_sstore_cost<WARM_STORAGE_READ_COST, WARM_SSTORE_RESET>`.
fn sstore_base_cost(original: U256, present: U256, new: U256) -> u64 {
    if new == present {
        revm::interpreter::gas::WARM_STORAGE_READ_COST
    } else if original == present {
        if original.is_zero() {
            revm::interpreter::gas::SSTORE_SET
        } else {
            revm::interpreter::gas::WARM_SSTORE_RESET
        }
    } else {
        revm::interpreter::gas::WARM_STORAGE_READ_COST
    }
}

/// Writes a value to storage for stateful precompiles.
///
/// # Parameters
/// - `internals`: The execution context with journal access
/// - `address`: The address whose storage to write to
/// - `storage_key`: The storage slot to write
/// - `input`: The value to store (as big-endian bytes)
/// - `gas_counter`: Available gas for this operation
/// - `hardfork`: The current hardfork for gas calculation
///
/// # Gas Cost
/// - Pre-Zero5: Fixed cost of 2,900 gas units
/// - Zero5+: EIP-2929/EIP-2200 aware (varies based on warm/cold and value changes)
///
/// # EIP-2200 Sentry (Zero6+)
/// Mirrors revm's SSTORE opcode behavior: if the remaining gas is less than or
/// equal to [`CALL_STIPEND`] (2,300), the call frame fails with `OutOfGas`
/// before any storage mutation is journaled.
///
/// # Returns
/// - `Ok(())`: Success
/// - `Err(PrecompileErrorOrRevert)`: If out of gas or storage write fails
///
/// # Example
/// ```rust,ignore
/// let new_value = U256::from(42);
/// write(
///     internals,
///     precompile_address,
///     StorageKey::ZERO,
///     &new_value.to_be_bytes_vec(),
///     gas_counter,
///     &hardfork
/// )?;
/// ```
pub(crate) fn write(
    internals: &mut EvmInternals,
    address: Address,
    storage_key: StorageKey,
    input: &[u8],
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<(), PrecompileErrorOrRevert> {
    // EIP-2200 reentrancy sentry: refuse SSTORE when remaining gas does not
    // exceed the call stipend.
    if hardfork_flags.is_active(ArcHardfork::Zero6) && gas_counter.remaining() <= CALL_STIPEND {
        return Err(PrecompileErrorOrRevert::Error(PrecompileError::OutOfGas));
    }

    let value = U256::from_be_slice(input);

    if hardfork_flags.is_active(ArcHardfork::Zero5) {
        let mut account = internals
            .load_account_mut(address)
            .map_err(|e| storage_io_error("write", e))?
            .data;

        // Probe slot warmth via sload to get current values for gas calculation.
        // This lets us charge all gas before the actual sstore mutation.
        let slot = match account.sload(storage_key.into(), true) {
            Ok(slot_load) => slot_load.data,
            Err(e) if e.is_cold_load_skipped() => {
                record_cost_or_out_of_gas(gas_counter, revm::interpreter::gas::COLD_SLOAD_COST)?;
                account
                    .sload(storage_key.into(), false)
                    .map_err(|e| storage_io_error("write", e))?
                    .data
            }
            Err(e) => return Err(storage_io_error("write", e)),
        };

        record_cost_or_out_of_gas(
            gas_counter,
            sstore_base_cost(slot.original_value, slot.present_value, value),
        )?;

        // All gas charged — safe to mutate. Slot is warm from the sload.
        account
            .sstore(storage_key.into(), value, false)
            .map_err(|e| storage_io_error("write", e))?;
        Ok(())
    } else {
        record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SSTORE_GAS_COST)?;
        internals
            .sstore(address, storage_key.into(), value)
            .map_err(|e| storage_io_error("write", e))?;
        Ok(())
    }
}

/// Helper to transfer funds between two accounts using the Journal
///
/// Account gas is charged after the load because `load_account_mut_skip_cold_load`
/// panics on cold accounts in revm ≤36. Storage slot helpers (`read`/`write`)
/// use `skip_cold_load` to charge before I/O; accounts cannot until revm ≥37.
pub(crate) fn transfer(
    internals: &mut EvmInternals,
    from: Address,
    to: Address,
    amount: U256,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<(), PrecompileErrorOrRevert> {
    let loaded_from_account = internals.load_account(from).map_err(|_| {
        PrecompileErrorOrRevert::Error(PrecompileError::Other(ERR_EXECUTION_REVERTED.into()))
    })?;
    record_cost_or_out_of_gas(
        gas_counter,
        account_load_cost(loaded_from_account.is_cold, hardfork_flags),
    )?;

    // Check that the account can be decremented by the amount
    check_can_decr_account(&loaded_from_account.info, amount, gas_counter)?;

    // Mirrors prior balance_decr + balance_incr; Zero6+ uses cold/warm via account_load_cost.
    record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SSTORE_GAS_COST)?;

    let to_load = internals.load_account(to).map_err(|_| {
        PrecompileErrorOrRevert::Error(PrecompileError::Other(ERR_EXECUTION_REVERTED.into()))
    })?;
    record_cost_or_out_of_gas(
        gas_counter,
        account_load_cost(to_load.is_cold, hardfork_flags),
    )?;

    record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SSTORE_GAS_COST)?;

    if hardfork_flags.is_active(ArcHardfork::Zero5) && to_load.is_selfdestructed() {
        return Err(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_SELFDESTRUCTED_BALANCE_INCREASED,
        ));
    }

    record_zero6_empty_account_creation_cost(gas_counter, &to_load.info, amount, hardfork_flags)?;

    let transfer_result = internals.transfer(from, to, amount).map_err(|_e| {
        PrecompileErrorOrRevert::new_reverted(*gas_counter, ERR_EXECUTION_REVERTED)
    })?;

    match transfer_result {
        None => Ok(()),
        Some(error) => match error {
            // This should never be hit, due to the check prior
            TransferError::OutOfFunds => Err(PrecompileErrorOrRevert::new_reverted(
                *gas_counter,
                ERR_INSUFFICIENT_FUNDS,
            )),
            TransferError::OverflowPayment => Err(PrecompileErrorOrRevert::new_reverted(
                *gas_counter,
                ERR_OVERFLOW,
            )),
            TransferError::CreateCollision => Err(PrecompileErrorOrRevert::new_reverted(
                *gas_counter,
                ERR_EXECUTION_REVERTED,
            )),
        },
    }
}

/// Helper to increment an account's balance by an amount using the Journal
pub(crate) fn balance_incr(
    internals: &mut EvmInternals,
    to: Address,
    amount: U256,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<(), PrecompileErrorOrRevert> {
    // Balance check, but doesn't touch state
    let account = internals.load_account(to).map_err(|_| {
        PrecompileErrorOrRevert::Error(PrecompileError::Other(ERR_EXECUTION_REVERTED.into()))
    })?;
    record_cost_or_out_of_gas(
        gas_counter,
        account_load_cost(account.is_cold, hardfork_flags),
    )?;

    if hardfork_flags.is_active(ArcHardfork::Zero5) && account.is_selfdestructed() {
        return Err(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_SELFDESTRUCTED_BALANCE_INCREASED,
        ));
    }

    let account_balance = account.info.balance;
    account_balance
        .checked_add(amount)
        .ok_or(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_OVERFLOW,
        ))?;

    // Update state
    record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SSTORE_GAS_COST)?;
    record_zero6_empty_account_creation_cost(gas_counter, &account.info, amount, hardfork_flags)?;
    internals.balance_incr(to, amount).map_err(|_| {
        PrecompileErrorOrRevert::Error(PrecompileError::Other(ERR_EXECUTION_REVERTED.into()))
    })?;

    Ok(())
}

/// Helper to decrement an account's balance by an amount using the Journal
pub(crate) fn balance_decr(
    internals: &mut EvmInternals,
    from: Address,
    amount: U256,
    gas_counter: &mut Gas,
    hardfork_flags: ArcHardforkFlags,
) -> Result<(), PrecompileErrorOrRevert> {
    let loaded_from_account = internals.load_account(from).map_err(|_| {
        PrecompileErrorOrRevert::Error(PrecompileError::Other(ERR_EXECUTION_REVERTED.into()))
    })?;
    record_cost_or_out_of_gas(
        gas_counter,
        account_load_cost(loaded_from_account.is_cold, hardfork_flags),
    )?;

    // Check that the account can be decremented by the amount
    check_can_decr_account(&loaded_from_account.info, amount, gas_counter)?;

    // Perform the decrement
    record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SSTORE_GAS_COST)?;
    let mut account = internals.load_account_mut(from).map_err(|_| {
        PrecompileErrorOrRevert::Error(PrecompileError::Other(ERR_EXECUTION_REVERTED.into()))
    })?;

    // False is only returned if insufficient funds, which should theoretically anyways never be reached due to the prior check
    if !account.decr_balance(amount) {
        return Err(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_INSUFFICIENT_FUNDS,
        ));
    }

    Ok(())
}

/// Helper to prevent state modifications during static calls
pub(crate) fn check_staticcall(
    precompile_input: &PrecompileInput,
    gas_counter: &mut Gas,
) -> Result<(), PrecompileErrorOrRevert> {
    if precompile_input.is_static {
        // Spend all remaining gas
        gas_counter.spend_all();
        return Err(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_STATE_CHANGE_DURING_STATIC_CALL,
        ));
    }
    Ok(())
}

/// Helper to check delegatecall
pub(crate) fn check_delegatecall(
    precompile_address: Address,
    precompile_input: &PrecompileInput,
    gas_counter: &Gas,
    _hardfork_flags: ArcHardforkFlags,
) -> Result<(), PrecompileErrorOrRevert> {
    if precompile_input.target_address != precompile_address
        || precompile_input.bytecode_address != precompile_address
    {
        return Err(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_DELEGATE_CALL_NOT_ALLOWED,
        ));
    }
    Ok(())
}

/// Helper to determine if an account can be decremented by an amount
/// Decrements gas counter if account would be emptied
pub(crate) fn check_can_decr_account(
    loaded_account_info: &AccountInfo,
    amount: U256,
    gas_counter: &mut Gas,
) -> Result<(), PrecompileErrorOrRevert> {
    // Check that the account has sufficient balance
    let from_account_balance = loaded_account_info.balance.checked_sub(amount).ok_or(
        PrecompileErrorOrRevert::new_reverted(*gas_counter, ERR_INSUFFICIENT_FUNDS),
    )?;

    // Check that the account would not be emptied if this transfer goes through
    let from_account_is_empty = from_account_balance.is_zero()
        && loaded_account_info.nonce == 0
        && (loaded_account_info.code_hash() == KECCAK_EMPTY
            || loaded_account_info.code_hash().is_zero());

    if from_account_is_empty {
        record_cost_or_out_of_gas(gas_counter, PRECOMPILE_SSTORE_GAS_COST)?;
        return Err(PrecompileErrorOrRevert::new_reverted(
            *gas_counter,
            ERR_CLEAR_EMPTY,
        ));
    }

    Ok(())
}

/// Stores a log event in the journal
pub(crate) fn emit_event<Event: SolEvent>(
    internals: &mut EvmInternals,
    address: Address,
    event: &Event,
    gas_counter: &mut Gas,
) -> Result<(), PrecompileErrorOrRevert> {
    let data = event.encode_log_data();

    let topic_gas = LOG_TOPIC_COST.saturating_mul(data.topics().len() as u64);
    let data_gas = LOG_DATA_COST.saturating_mul(data.data.len() as u64);
    let log_gas = LOG_BASE_COST
        .saturating_add(topic_gas)
        .saturating_add(data_gas);
    record_cost_or_out_of_gas(gas_counter, log_gas)?;

    let log = revm::primitives::Log { address, data };

    internals.log(log);
    Ok(())
}
