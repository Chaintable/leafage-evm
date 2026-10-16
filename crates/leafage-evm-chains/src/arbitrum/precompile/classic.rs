//! Classic builtin dispatch, deliberately separate from Nitro ArbOS storage.
mod abi;
mod builtins;
mod modexp;
mod pairing;

use self::abi::sys;
use self::builtins::encode;
use super::{ArbitrumContext, EthPrecompiles, PrecompileProvider};
use crate::arbitrum::config::{ClassicBuiltinUpgrades, ClassicModexpUpgrades};
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use revm::context::{ContextTr, JournalTr, LocalContextTr};
use revm::interpreter::{
    CallInput, CallInputs, CallScheme, Gas, InstructionResult, InterpreterResult,
};
use revm::{Database, DatabaseRef};

// Classic ArbOwner is 0x6b (not Nitro's 0x70). ArbInfo at 0x65 is an
// ordinary EVM contract, whose historical code must run from the database.
// Nitro-only 0x70+ must also remain ordinary accounts.
pub(super) fn addresses() -> impl Iterator<Item = Address> {
    [
        0x64, 0x66, 0x67, 0x68, 0x69, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0xc8,
    ]
    .into_iter()
    .map(Address::with_last_byte)
}

pub(super) fn run<DB: Database + DatabaseRef>(
    eth: &mut EthPrecompiles,
    ctx: &mut ArbitrumContext<DB>,
    inputs: &CallInputs,
    modexp_upgrades: Option<ClassicModexpUpgrades>,
    builtin_upgrades: Option<ClassicBuiltinUpgrades>,
) -> Result<Option<InterpreterResult>, String> {
    let address = inputs.bytecode_address;
    if !addresses().any(|a| a == address) {
        return run_eth(eth, ctx, inputs, modexp_upgrades);
    }
    let unsupported = || {
        format!(
            "Arbitrum Classic: builtin {address} requires unavailable Classic ArbOS state or call context"
        )
    };
    if inputs.target_address != address
        || matches!(
            inputs.scheme,
            CallScheme::DelegateCall | CallScheme::CallCode
        )
    {
        return Err(unsupported());
    }
    let data = calldata(ctx, inputs);
    let builtin_upgrades =
        builtin_upgrades.or_else(|| ClassicBuiltinUpgrades::for_chain_id(ctx.cfg().chain_id));
    if builtin_upgrades.is_some_and(|u| {
        [
            u.arbos_34_block,
            u.arbos_39_block,
            u.arbos_42_block,
            u.arbos_54_block,
        ]
        .windows(2)
        .any(|w| w[0] > w[1])
    }) {
        return Err("Arbitrum Classic: invalid classic_builtin_upgrades order".into());
    }
    let number = historical_block_number(ctx);
    let rules = builtin_upgrades.map(|u| builtins::Rules {
        arbos34: number >= U256::from(u.arbos_34_block),
        arbos39: number >= U256::from(u.arbos_39_block),
        arbos42: number >= U256::from(u.arbos_42_block),
        arbos54: number >= U256::from(u.arbos_54_block),
    });
    if address != Address::with_last_byte(0x64) {
        let output = match builtins::run(inputs, &data, ctx.chain().current_call().depth, rules) {
            Ok(bytes) => Some(bytes),
            Err(builtins::Error::Revert) => None,
            Err(builtins::Error::Unavailable) => return Err(unsupported()),
        };
        return Ok(finish(inputs, output));
    }
    let revert = || Ok(finish(inputs, None));
    let Some(selector) = data.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return revert();
    };
    let fixed = |value| if data.len() == 4 { Some(value) } else { None };
    let value = match selector {
        sys::arbBlockNumberCall::SELECTOR => fixed(encode::<sys::arbBlockNumberCall>(
            &ctx.chain()
                .current_l2_block_number()
                .ok_or_else(unsupported)?,
        )),
        sys::arbChainIDCall::SELECTOR => fixed(encode::<sys::arbChainIDCall>(&U256::from(
            ctx.cfg().chain_id,
        ))),
        sys::isTopLevelCallCall::SELECTOR => fixed(encode::<sys::isTopLevelCallCall>(
            &(ctx.chain().current_call().depth == 2),
        )),
        sys::getTransactionCountCall::SELECTOR => {
            if data.len() != 36 {
                return revert();
            }
            let Ok(call) = sys::getTransactionCountCall::abi_decode(&data) else {
                return revert();
            };
            Some(encode::<sys::getTransactionCountCall>(&U256::from(
                ctx.journal_mut()
                    .load_account(call.account)
                    .map_err(|e| format!("{e:?}"))?
                    .info
                    .nonce,
            )))
        }
        sys::getStorageAtCall::SELECTOR => {
            if !inputs.caller.is_zero() {
                return revert();
            }
            // Mini zero-pads truncated words and ignores trailing calldata.
            let mut padded = [0; 68];
            let len = data.len().min(padded.len());
            padded[..len].copy_from_slice(&data[..len]);
            let Ok(call) = sys::getStorageAtCall::abi_decode(&padded) else {
                return revert();
            };
            ctx.journal_mut()
                .load_account(call.account)
                .map_err(|e| format!("{e:?}"))?;
            Some(encode::<sys::getStorageAtCall>(
                &ctx.journal_mut()
                    .sload(call.account, call.index)
                    .map_err(|e| format!("{e:?}"))?
                    .data,
            ))
        }
        // Caller aliasing and version depend on historical private state. A
        // fatal error cannot be swallowed by a Solidity low-level CALL.
        sys::arbOSVersionCall::SELECTOR
        | sys::wasMyCallersAddressAliasedCall::SELECTOR
        | sys::myCallersAddressWithoutAliasingCall::SELECTOR
        | sys::getStorageGasAvailableCall::SELECTOR => {
            if data.len() != 4 {
                return revert();
            }
            return Err(unsupported());
        }
        sys::mapL1SenderContractAddressToL2AliasCall::SELECTOR => {
            if data.len() != 68 {
                return revert();
            }
            return Err(unsupported());
        }
        sys::withdrawEthCall::SELECTOR => {
            if inputs.is_static || data.len() != 36 {
                return revert();
            }
            return Err(unsupported());
        }
        sys::sendTxToL1Call::SELECTOR => {
            if inputs.is_static
                || (rules.is_some_and(|r| r.arbos42)
                    && !builtins::padded_bytes_length(&data, 100, 68))
                || (rules.is_some_and(|r| r.arbos54) && builtins::word(&data, 36) != U256::from(64))
            {
                return revert();
            }
            return Err(unsupported());
        }
        // Unknown selectors revert in Classic. Capability probes may catch
        // this revert and continue; only recognized missing-data paths fail.
        _ => return revert(),
    };
    Ok(finish(inputs, value))
}

fn calldata<DB: Database>(ctx: &mut ArbitrumContext<DB>, inputs: &CallInputs) -> Bytes {
    match &inputs.input {
        CallInput::Bytes(b) => b.clone(),
        CallInput::SharedBuffer(range) => ctx
            .local_mut()
            .shared_memory_buffer_slice(range.clone())
            .map(|s| Bytes::copy_from_slice(&s))
            .unwrap_or_default(),
    }
}

fn finish(inputs: &CallInputs, output: Option<Bytes>) -> Option<InterpreterResult> {
    Some(InterpreterResult {
        result: if output.is_some() {
            InstructionResult::Return
        } else {
            InstructionResult::Revert
        },
        gas: Gas::new(inputs.gas_limit),
        output: output.unwrap_or_default(),
    })
}

fn historical_block_number<DB: Database>(ctx: &ArbitrumContext<DB>) -> U256 {
    // RPC block overrides must not select a different historical implementation.
    ctx.tx
        .context
        .classic_block_number
        .map(U256::from)
        .unwrap_or(ctx.block.number)
}

fn run_eth<DB: Database + DatabaseRef>(
    eth: &mut EthPrecompiles,
    ctx: &mut ArbitrumContext<DB>,
    inputs: &CallInputs,
    modexp_upgrades: Option<ClassicModexpUpgrades>,
) -> Result<Option<InterpreterResult>, String> {
    let address = inputs.bytecode_address;
    let is_eth = address >= Address::with_last_byte(1) && address <= Address::with_last_byte(9);
    if !is_eth {
        return Ok(None);
    }
    let data = calldata(ctx, inputs);
    let revert = || Ok(finish(inputs, None));
    let id = address.as_slice()[19];
    let modexp_rules = if id == 5 {
        let upgrades = modexp_upgrades
            .or_else(|| {
                (ctx.cfg().chain_id == 42161).then_some(ClassicModexpUpgrades {
                    // Native archive: ArbOS 48 -> 49 at 2_965_603, then
                    // ArbOS 49 -> 50 at 3_696_126 (post-block call state).
                    exponent_size_block: 2_965_603,
                    uint_fast_path_block: 3_696_126,
                })
            })
            .ok_or_else(|| {
                "Arbitrum Classic: MODEXP requires classic_modexp_upgrades for this chain"
                    .to_owned()
            })?;
        if upgrades.exponent_size_block > upgrades.uint_fast_path_block {
            return Err("Arbitrum Classic: invalid classic_modexp_upgrades order".to_owned());
        }
        let number = historical_block_number(ctx);
        if number < U256::from(upgrades.exponent_size_block) {
            modexp::Rules::Legacy
        } else if number < U256::from(upgrades.uint_fast_path_block) {
            modexp::Rules::ExponentSize
        } else {
            modexp::Rules::UintFastPath
        }
    } else {
        modexp::Rules::UintFastPath
    };
    // The native Classic AVM has no RIPEMD160F (0x25) or BLAKE2F (0x26),
    // although the Mini source and Rust emulator implement both. Archive
    // calls to these builtins therefore revert, even with valid input.
    if matches!(id, 3 | 9) {
        return revert();
    }
    if id == 1 && data.len() != 128 {
        return revert();
    }
    if id == 8 && data.len() / 192 > 30 {
        return revert();
    }
    let mut result = if id == 8 {
        Some(revm::handler::precompile_output_to_interpreter_result(
            revm::precompile::PrecompileOutput::from_eth_result(
                pairing::run(&data, inputs.gas_limit),
                inputs.reservoir,
            ),
            inputs.gas_limit,
        ))
    } else {
        let mut normalized = inputs.clone();
        normalized.input = CallInput::Bytes(data.clone());
        PrecompileProvider::<ArbitrumContext<DB>>::run(eth, ctx, &normalized)?
    };
    if let Some(output) = &mut result {
        if id == 1 && output.result.is_ok() && output.output.is_empty() {
            output.output = Bytes::from(vec![0; 32]);
        }
        if id == 5 && output.result.is_ok() {
            if modexp::adjust_output(&data, &mut output.output, inputs.gas_limit, modexp_rules)
                .is_err()
            {
                output.result = InstructionResult::Revert;
                output.output = Bytes::new();
            }
        }
        if output.result == InstructionResult::PrecompileError {
            output.result = InstructionResult::Revert;
        }
    }
    Ok(result)
}
