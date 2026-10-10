//! Classic builtin dispatch, deliberately separate from Nitro ArbOS storage.
mod modexp;
mod pairing;

use super::{ArbitrumContext, EthPrecompiles, PrecompileProvider};
use crate::arbitrum::config::ClassicModexpUpgrades;
use alloy::primitives::{Address, Bytes, U256};
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
    if address != Address::with_last_byte(0x64) {
        return Err(unsupported());
    }
    if inputs.target_address != address
        || matches!(
            inputs.scheme,
            CallScheme::DelegateCall | CallScheme::CallCode
        )
    {
        return Err(unsupported());
    }
    let data = match &inputs.input {
        CallInput::Bytes(b) => b.clone(),
        CallInput::SharedBuffer(range) => ctx
            .local_mut()
            .shared_memory_buffer_slice(range.clone())
            .map(|s| Bytes::copy_from_slice(&s))
            .unwrap_or_default(),
    };
    let result = |value: Option<U256>| {
        Some(InterpreterResult {
            result: if value.is_some() {
                InstructionResult::Return
            } else {
                InstructionResult::Revert
            },
            gas: Gas::new(inputs.gas_limit),
            output: value
                .map(|v| Bytes::copy_from_slice(&v.to_be_bytes::<32>()))
                .unwrap_or_default(),
        })
    };
    let Some(selector) = data.get(..4) else {
        return Ok(result(None));
    };
    let fixed = |value| if data.len() == 4 { Some(value) } else { None };
    let value = match selector {
        [0xa3, 0xb1, 0xb3, 0x1d] => fixed(
            ctx.chain()
                .current_l2_block_number()
                .ok_or_else(unsupported)?,
        ),
        [0xd1, 0x27, 0xf5, 0x4a] => fixed(U256::from(ctx.cfg().chain_id)),
        [0x08, 0xbd, 0x62, 0x4c] => fixed(U256::from(ctx.chain().current_call().depth == 2)),
        [0x23, 0xca, 0x0c, 0xd2] => {
            if data.len() != 36 {
                return Ok(result(None));
            }
            let account = Address::from_slice(&data[16..36]);
            Some(U256::from(
                ctx.journal_mut()
                    .load_account(account)
                    .map_err(|e| format!("{e:?}"))?
                    .info
                    .nonce,
            ))
        }
        [0xa1, 0x69, 0x62, 0x5f] => {
            if !inputs.caller.is_zero() {
                return Ok(result(None));
            }
            // Classic bytearray_get256 zero-pads truncated calldata.
            let word = |start: usize| {
                let mut out = [0u8; 32];
                for (i, b) in out.iter_mut().enumerate() {
                    *b = data.get(start + i).copied().unwrap_or(0);
                }
                out
            };
            let account = Address::from_slice(&word(4)[12..]);
            let slot = U256::from_be_bytes(word(36));
            ctx.journal_mut()
                .load_account(account)
                .map_err(|e| format!("{e:?}"))?;
            Some(
                ctx.journal_mut()
                    .sload(account, slot)
                    .map_err(|e| format!("{e:?}"))?
                    .data,
            )
        }
        // Caller aliasing and version depend on historical private state. A
        // fatal error cannot be swallowed by a Solidity low-level CALL.
        [0x05, 0x10, 0x38, 0xf2] // arbOSVersion
        | [0x17, 0x5a, 0x26, 0x0b] // wasMyCallersAddressAliased
        | [0xd7, 0x45, 0x23, 0xb3] // myCallersAddressWithoutAliasing
        | [0xa9, 0x45, 0x97, 0xff] => { // getStorageGasAvailable
            if data.len() != 4 { return Ok(result(None)); }
            return Err(unsupported());
        }
        [0x4d, 0xbb, 0xd5, 0x06] => { // mapL1SenderContractAddressToL2Alias
            if data.len() != 68 { return Ok(result(None)); }
            return Err(unsupported());
        }
        [0x25, 0xe1, 0x60, 0x63] | [0x92, 0x8c, 0x16, 0x9a] => { // withdrawEth / sendTxToL1
            if inputs.is_static { return Ok(result(None)); }
            return Err(unsupported());
        }
        // Unknown selectors revert in Classic. Capability probes may catch
        // this revert and continue; only recognized missing-data paths fail.
        _ => return Ok(result(None)),
    };
    Ok(result(value))
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
    let data = match &inputs.input {
        CallInput::Bytes(b) => b.clone(),
        CallInput::SharedBuffer(range) => ctx
            .local_mut()
            .shared_memory_buffer_slice(range.clone())
            .map(|s| Bytes::copy_from_slice(&s))
            .unwrap_or_default(),
    };
    let revert = || {
        Ok(Some(InterpreterResult {
            result: InstructionResult::Revert,
            output: Bytes::new(),
            gas: Gas::new(inputs.gas_limit),
        }))
    };
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
        let number = ctx
            .tx
            .context
            .classic_block_number
            .map(U256::from)
            .unwrap_or(ctx.block.number);
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
