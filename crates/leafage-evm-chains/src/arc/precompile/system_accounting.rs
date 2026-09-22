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

use super::helpers::{
    abi_decode_raw_with_zero6_validation, check_delegatecall, check_staticcall,
    new_reverted_with_early_penalty, read, record_cost_or_out_of_gas, write,
    PrecompileErrorOrRevert, ERR_EXECUTION_REVERTED, ERR_INVALID_CALLER,
    PRECOMPILE_EARLY_REVERT_GAS_PENALTY, PRECOMPILE_SLOAD_GAS_COST,
};
use super::macros::precompile;
use crate::arc::ArcHardfork;
use alloy::primitives::B256;
use alloy::primitives::{address, keccak256, Address, StorageKey};
use alloy::sol_types::{sol, SolCall, SolValue};
use revm::handler::SYSTEM_ADDRESS;
use revm::interpreter::Gas;
use revm::precompile::PrecompileOutput;

// System Accounting precompile address
pub const SYSTEM_ACCOUNTING_ADDRESS: Address =
    address!("0x1800000000000000000000000000000000000002");

// Storage key for storing gas values
const GAS_VALUES_STORAGE_KEY: StorageKey = StorageKey::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
]);

/// Ring buffer capacity for historical gas values. Consensus reads only
/// freshly-written slots (the executor reads the parent slot for EMA smoothing;
/// the assembler reads the current slot just written by `finish()`), so no
/// history depth is required for correctness. The extra capacity exists purely
/// as headroom for external readers (RPC, monitoring) and is otherwise arbitrary.
const GAS_VALUES_RING_BUFFER_SIZE: u64 = 64;

// Arc system-accounting caller.
const ARC_SYSTEM_CALLER: Address = SYSTEM_ADDRESS;

sol! {
    struct GasValues {
        uint64 gasUsed;
        uint64 gasUsedSmoothed;
        /// store the computed base fee for next block
        /// max value is 2^64 - 1 ~= 18 USDC
        uint64 nextBaseFee;
    }

    interface ISystemAccounting {
        /// Writes `gasValues` into ring-buffer slot
        /// `blockNumber % GAS_VALUES_RING_BUFFER_SIZE`, overwriting whatever
        /// the slot previously held. ARC_SYSTEM_CALLER-gated; no validation on
        /// `blockNumber`, since writes happen once per block from the block
        /// executor.
        function storeGasValues(uint64 blockNumber, GasValues calldata gasValues) external returns (bool);

        /// Returns ring-buffer slot `blockNumber % GAS_VALUES_RING_BUFFER_SIZE`
        /// as-is, without any freshness check. If `blockNumber` has been
        /// rotated out (more than `GAS_VALUES_RING_BUFFER_SIZE - 1` behind the
        /// latest written block) or is in the future, the slot holds the last
        /// block that mapped to it, i.e. a different block's values. Slots
        /// that have never been written (possible only early in the chain's
        /// life, before every slot has been reached once) read as zero.
        /// Callers needing freshness must cross-check against their own view
        /// of the chain tip. Consensus does not depend on freshness: the
        /// executor reads only the parent slot for EMA smoothing, which was
        /// written by the previous block's `finish()` (or reads as zero at
        /// genesis, the correct EMA baseline), and the block assembler reads
        /// only the current slot just written by the same block's `finish()`.
        function getGasValues(uint64 blockNumber) external view returns (GasValues calldata gasValue);
    }
}

/// Computes the storage slot for a mapping key of type address
///
/// A mapping, while slightly less efficient than a fixed size contiguous array,
/// is more flexible if additional gas values should be added in the future.
///
/// Implements Solidity's mapping storage slot calculation:
/// Formula: keccak256(h(k) . p), where:
/// - k is the mapping key (uint64)
/// - p is the mapping slot position (GAS_VALUES_STORAGE_KEY)
/// - h left-pads the key to 32 bytes
/// - . is concatenation
///
/// `block_number` is reduced mod `GAS_VALUES_RING_BUFFER_SIZE` before hashing,
/// so any two block numbers that differ by a multiple of the ring buffer size
/// collide on the same slot. The mapping carries no identity of the block that
/// last wrote the slot — callers who need that identity must track it
/// out-of-band.
pub fn compute_gas_values_storage_slot(block_number: u64) -> StorageKey {
    // Map block number into ring buffer
    let key_value = block_number % GAS_VALUES_RING_BUFFER_SIZE;

    // Left-pad 8 byte u64 to 32 bytes
    let mut key_bytes = [0u8; 32];
    key_bytes[24..].copy_from_slice(key_value.to_be_bytes().as_ref());

    // Use AVERAGED_HISTORICAL_GAS_STORAGE_KEY as the slot bytes
    let slot_bytes = GAS_VALUES_STORAGE_KEY.0;

    // Concatenate key and slot, then hash
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(&key_bytes);
    data[32..].copy_from_slice(&slot_bytes);

    StorageKey::new(keccak256(data).0)
}

precompile!(run_system_accounting, precompile_input, hardfork_flags; {
    ISystemAccounting::storeGasValuesCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;

            // Check if static call is attempting to modify state
            check_staticcall(
                &precompile_input,
                &mut gas_counter,
            )?;

            // Decode arguments passed to blocklist function
            let args = abi_decode_raw_with_zero6_validation::<ISystemAccounting::storeGasValuesCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            // Redundant 2100-gas charge — no SLOAD occurs here, but kept pre-Zero6 to
            // preserve consensus on already-finalized blocks.
            if !hardfork_flags.is_active(ArcHardfork::Zero6) {
                record_cost_or_out_of_gas(&mut gas_counter, PRECOMPILE_SLOAD_GAS_COST)?;
            }

            // Check caller
            if precompile_input.caller != ARC_SYSTEM_CALLER {
                return Err(new_reverted_with_early_penalty(gas_counter, ERR_INVALID_CALLER, hardfork_flags));
            }

            // Check delegatecall
            check_delegatecall(
                SYSTEM_ACCOUNTING_ADDRESS,
                &precompile_input,
                &gas_counter,
                hardfork_flags,
            )?;

            // Update storage
            let storage_slot = compute_gas_values_storage_slot(args.blockNumber);
            let updated_value_bytes = pack_gas_values_for_storage(args.gasValues);
            write(
                &mut precompile_input.internals,
                SYSTEM_ACCOUNTING_ADDRESS,
                storage_slot,
                &updated_value_bytes,
                &mut gas_counter,
                hardfork_flags,
            )?;

            let output = true.abi_encode();
            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },
    ISystemAccounting::getGasValuesCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let mut gas_counter = Gas::new(precompile_input.gas);
            let mut precompile_input = precompile_input;

            // Decode arguments passed to blocklist function
            let args = abi_decode_raw_with_zero6_validation::<ISystemAccounting::getGasValuesCall>(
                input,
                hardfork_flags,
            )
                .map_err(|_|
                    PrecompileErrorOrRevert::new_reverted_with_penalty(
                        gas_counter, PRECOMPILE_EARLY_REVERT_GAS_PENALTY, ERR_EXECUTION_REVERTED,
                    )
                )?;

            // Read stored value
            let storage_slot = compute_gas_values_storage_slot(args.blockNumber);
            let slot_value = read(
                &mut precompile_input.internals,
                SYSTEM_ACCOUNTING_ADDRESS,
                storage_slot,
                &mut gas_counter,
                hardfork_flags,
            )?;
            let gas_values = unpack_gas_values_from_storage(B256::from_slice(slot_value.as_ref()));
            let output = gas_values.abi_encode();

            Ok(PrecompileOutput::new(gas_counter.used(), output.into()))
        })()
    },
});

/// Packs GasValues into a single 32-byte storage slot
/// The layout is:
/// - `gasUsedSmoothed` (u64): bytes [16..24]
/// - `gasUsed` (u64):         bytes [24..32]
fn pack_gas_values_for_storage(g: GasValues) -> [u8; 32] {
    let mut slot = [0u8; 32];
    slot[24..32].copy_from_slice(&g.gasUsed.to_be_bytes());
    slot[16..24].copy_from_slice(&g.gasUsedSmoothed.to_be_bytes());
    slot[8..16].copy_from_slice(&g.nextBaseFee.to_be_bytes());
    slot
}

pub fn unpack_gas_values_from_storage(slot: B256) -> GasValues {
    let bytes = slot.as_slice();
    let gas_used = u64::from_be_bytes(
        bytes[24..32]
            .try_into()
            .expect("8-byte slice from 32-byte array"),
    );
    let gas_used_smoothed = u64::from_be_bytes(
        bytes[16..24]
            .try_into()
            .expect("8-byte slice from 32-byte array"),
    );
    let next_base_fee = u64::from_be_bytes(
        bytes[8..16]
            .try_into()
            .expect("8-byte slice from 32-byte array"),
    );
    GasValues {
        gasUsed: gas_used,
        gasUsedSmoothed: gas_used_smoothed,
        nextBaseFee: next_base_fee,
    }
}
