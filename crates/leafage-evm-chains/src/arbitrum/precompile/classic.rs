//! Classic builtin dispatch, deliberately separate from Nitro ArbOS storage.
use super::{ArbitrumContext, EthPrecompiles, PrecompileProvider};
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
) -> Result<Option<InterpreterResult>, String> {
    let address = inputs.bytecode_address;
    if !addresses().any(|a| a == address) {
        return PrecompileProvider::<ArbitrumContext<DB>>::run(eth, ctx, inputs);
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
