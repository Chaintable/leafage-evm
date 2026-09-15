// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
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
    record_cost_or_out_of_gas, PrecompileErrorOrRevert, ERR_EXECUTION_REVERTED,
    PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
};
use super::macros::precompile;
use alloy::primitives::{address, Address};
use alloy::sol_types::{sol, SolCall, SolValue};
use revm::interpreter::gas::KECCAK256WORD;
use revm::interpreter::Gas;
use revm::precompile::PrecompileOutput;
use slh_dsa::{signature::Verifier, Sha2_128s, Signature, VerifyingKey as SlhDsaVerifyingKey};

pub const PQ_ADDRESS: Address = address!("1800000000000000000000000000000000000004");

/// Base gas for SLH-DSA-SHA2-128s verification.
///
/// Conservative relative to the SHA-256 precompile's per-word work anchor. See
/// `crates/precompiles/benches/pq.rs` for the benchmark context comparing this
/// price against SLH-DSA-SHA2-128s verification and 64-byte SHA-256 / KECCAK256
/// work.
const VERIFY_BASE_GAS: u64 = 230_000;

/// Dynamic gas cost per 32-byte word of message input.
///
/// SLH-DSA-SHA2-128s hashes the message once via `H_msg` (SHA-256 + MGF1).
/// This is comparable to KECCAK256, so we use the same per-word rate.
const GAS_PER_MSG_WORD: u64 = KECCAK256WORD;

sol! {
    /// Experimental PQ Signature Verifier precompile interface.
    interface IPQ {
        /// Verify an SLH-DSA-SHA2-128s signature.
        ///
        /// Since PQ signatures are still very new, we recommend not to solely
        /// rely on them for authentication, but pair them with classical
        /// signatures.
        ///
        /// Gas cost: 230,000 base + 6 per 32-byte word of message (same as KECCAK256)
        function verifySlhDsaSha2128s(bytes calldata vk, bytes calldata message, bytes calldata sig) external returns (bool isValid);
    }
}

precompile!(run_pq, precompile_input, hardfork_flags; {
    IPQ::verifySlhDsaSha2128sCall => |input| {
        (|| -> Result<PrecompileOutput, PrecompileErrorOrRevert> {
            let _ = hardfork_flags;
            let mut gas_counter = Gas::new(precompile_input.gas);

            let args = IPQ::verifySlhDsaSha2128sCall::abi_decode_raw_validate(input).map_err(|_| {
                PrecompileErrorOrRevert::new_reverted_with_penalty(
                    gas_counter,
                    PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
                    ERR_EXECUTION_REVERTED,
                )
            })?;

            // Charge base gas, then per-word message gas, then validate inputs
            record_cost_or_out_of_gas(&mut gas_counter, VERIFY_BASE_GAS)?;

            // GAS_PER_MSG_WORD (6) < 32, so the product cannot exceed u64::MAX
            #[allow(clippy::arithmetic_side_effects)]
            let msg_word_gas = (args.message.len() as u64).div_ceil(32) * GAS_PER_MSG_WORD;
            record_cost_or_out_of_gas(&mut gas_counter, msg_word_gas)?;

            // SLH-DSA-SHA2-128s constants from FIPS 205
            const VK_LEN: usize = 32;
            const SIG_LEN: usize = 7856;

            if args.vk.len() != VK_LEN {
                return Err(PrecompileErrorOrRevert::new_reverted(
                    gas_counter,
                    "Invalid verifying key length",
                ));
            }

            if args.sig.len() != SIG_LEN {
                return Err(PrecompileErrorOrRevert::new_reverted(
                    gas_counter,
                    "Invalid signature length",
                ));
            }

            let verifying_key = SlhDsaVerifyingKey::<Sha2_128s>::try_from(args.vk.as_ref())
                .map_err(|_| PrecompileErrorOrRevert::new_reverted(gas_counter, "Failed to parse verifying key"))?;

            let signature = Signature::<Sha2_128s>::try_from(args.sig.as_ref())
                .map_err(|_| PrecompileErrorOrRevert::new_reverted(gas_counter, "Failed to parse signature"))?;

            let is_valid = verifying_key.verify(args.message.as_ref(), &signature).is_ok();

            Ok(PrecompileOutput::new(gas_counter.used(), is_valid.abi_encode().into()))
        })()
    },
});
