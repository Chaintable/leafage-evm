// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Sender-preserving CallFrom subcall precompile.

use super::subcall::{
    SubcallCompletionResult, SubcallContinuationData, SubcallError, SubcallInitResult,
    SubcallPrecompile,
};
use alloy::{
    primitives::{address, Address, U256},
    sol_types::{sol, SolCall},
};
use revm::{
    context_interface::cfg::gas,
    handler::FrameResult,
    interpreter::{CallInput, CallInputs, CallScheme, CallValue},
};

pub(crate) const CALL_FROM_ADDRESS: Address = address!("1800000000000000000000000000000000000003");
pub(crate) const MEMO_ADDRESS: Address = address!("5294E9927c3306DcBaDb03fe70b92e01cCede505");
pub(crate) const MULTICALL3_FROM_ADDRESS: Address =
    address!("522fAf9A91c41c443c66765030741e4AaCe147D0");

pub(crate) const ABI_DECODE_BASE_GAS: u64 = 100;
pub(crate) const ABI_ENCODE_BASE_GAS: u64 = 100;

pub(crate) fn abi_decode_gas(data_len: usize) -> u64 {
    let data_len = u64::try_from(data_len).unwrap_or(u64::MAX);
    ABI_DECODE_BASE_GAS.saturating_add(data_len.div_ceil(32).saturating_mul(gas::COPY))
}

pub(crate) fn abi_encode_gas(data_len: usize) -> u64 {
    let data_len = u64::try_from(data_len).unwrap_or(u64::MAX);
    ABI_ENCODE_BASE_GAS.saturating_add(data_len.div_ceil(32).saturating_mul(gas::COPY))
}

sol! {
    interface ICallFrom {
        function callFrom(address sender, address target, bytes calldata data)
            external returns (bool success, bytes memory returnData);
    }
}

#[derive(Debug)]
pub(crate) struct CallFromPrecompile;

fn decode_child_call(inputs: &CallInputs) -> Result<(CallInputs, u64), SubcallError> {
    let input = match &inputs.input {
        CallInput::Bytes(input) => input,
        CallInput::SharedBuffer(_) => {
            return Err(SubcallError::AbiDecodeError(
                "unexpected shared buffer input".into(),
            ));
        }
    };
    let decoded = ICallFrom::callFromCall::abi_decode_validate(input)
        .map_err(|error| SubcallError::AbiDecodeError(format!("callFrom: {error}")))?;
    let overhead = abi_decode_gas(decoded.data.len());
    let available = inputs.gas_limit.checked_sub(overhead).ok_or_else(|| {
        SubcallError::InsufficientGas("gas limit below ABI decode overhead".into())
    })?;
    let child_gas_limit = available - available / 64;

    Ok((
        CallInputs {
            input: CallInput::Bytes(decoded.data),
            return_memory_offset: 0..0,
            gas_limit: child_gas_limit,
            bytecode_address: decoded.target,
            known_bytecode: None,
            target_address: decoded.target,
            caller: decoded.sender,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
        },
        overhead,
    ))
}

impl SubcallPrecompile for CallFromPrecompile {
    fn init_subcall(&self, inputs: &CallInputs) -> Result<SubcallInitResult, SubcallError> {
        let (child_inputs, gas_overhead) = decode_child_call(inputs)?;
        Ok(SubcallInitResult {
            child_inputs: Box::new(child_inputs),
            continuation_data: SubcallContinuationData,
            gas_overhead,
        })
    }

    fn complete_subcall(
        &self,
        _continuation_data: SubcallContinuationData,
        child_result: &FrameResult,
    ) -> Result<SubcallCompletionResult, SubcallError> {
        let FrameResult::Call(outcome) = child_result else {
            return Err(SubcallError::UnexpectedFrameResult);
        };
        let output = outcome.result.output.clone();
        let encoded = ICallFrom::callFromCall::abi_encode_returns(&ICallFrom::callFromReturn {
            success: outcome.result.result.is_ok(),
            returnData: output.clone(),
        });

        Ok(SubcallCompletionResult {
            output: encoded.into(),
            success: true,
            gas_overhead: abi_encode_gas(output.len()),
        })
    }

    fn trace_child_call(&self, inputs: &CallInputs) -> Option<CallInputs> {
        decode_child_call(inputs).ok().map(|(inputs, _)| inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Bytes;
    use revm::interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult};

    const CALLER: Address = address!("c000000000000000000000000000000000000001");
    const SENDER: Address = address!("e000000000000000000000000000000000000001");
    const TARGET: Address = address!("c000000000000000000000000000000000000002");

    fn inputs(data: Vec<u8>, gas_limit: u64) -> CallInputs {
        let input = ICallFrom::callFromCall {
            sender: SENDER,
            target: TARGET,
            data: data.into(),
        }
        .abi_encode();

        CallInputs {
            input: CallInput::Bytes(input.into()),
            return_memory_offset: 0..0,
            gas_limit,
            bytecode_address: CALL_FROM_ADDRESS,
            known_bytecode: None,
            target_address: CALL_FROM_ADDRESS,
            caller: CALLER,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
        }
    }

    fn call_result(result: InstructionResult, output: Bytes) -> FrameResult {
        FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(result, output, Gas::new(0)),
            0..0,
        ))
    }

    fn continuation() -> SubcallContinuationData {
        SubcallContinuationData
    }

    #[test]
    fn gas_is_base_plus_copy_per_word() {
        assert_eq!(gas::COPY, 3);
        for (len, words) in [(0, 0), (1, 1), (32, 1), (33, 2), (64, 2)] {
            assert_eq!(abi_decode_gas(len), ABI_DECODE_BASE_GAS + words * 3);
            assert_eq!(abi_encode_gas(len), ABI_ENCODE_BASE_GAS + words * 3);
        }
    }

    #[test]
    fn init_decodes_child_and_applies_eip150() {
        let data = vec![0x42; 33];
        let gas_limit = 100_000;
        let init = CallFromPrecompile
            .init_subcall(&inputs(data.clone(), gas_limit))
            .expect("valid CallFrom input");
        let available = gas_limit - abi_decode_gas(data.len());

        assert_eq!(init.gas_overhead, abi_decode_gas(data.len()));
        assert_eq!(init.child_inputs.caller, SENDER);
        assert_eq!(init.child_inputs.target_address, TARGET);
        assert_eq!(init.child_inputs.bytecode_address, TARGET);
        assert_eq!(init.child_inputs.gas_limit, available - available / 64);
        assert_eq!(init.child_inputs.input, CallInput::Bytes(data.into()));
        assert_eq!(init.child_inputs.known_bytecode, None);
    }

    #[test]
    fn init_rejects_malformed_abi_and_insufficient_gas() {
        let mut malformed = inputs(Vec::new(), 100_000);
        malformed.input = CallInput::Bytes(Bytes::from_static(b"bad"));
        assert!(matches!(
            CallFromPrecompile.init_subcall(&malformed),
            Err(SubcallError::AbiDecodeError(_))
        ));

        assert!(matches!(
            CallFromPrecompile.init_subcall(&inputs(Vec::new(), ABI_DECODE_BASE_GAS - 1)),
            Err(SubcallError::InsufficientGas(_))
        ));
    }

    #[test]
    fn trace_identity_matches_executed_child() {
        let original = inputs(vec![0x42, 0x43], 100_000);
        let initialized = CallFromPrecompile
            .init_subcall(&original)
            .expect("valid CallFrom input");
        let traced = CallFromPrecompile
            .trace_child_call(&original)
            .expect("valid trace identity");

        assert_eq!(traced, *initialized.child_inputs);
    }

    #[test]
    fn completion_encodes_child_success_and_revert() {
        for (result, expected_success, output) in [
            (InstructionResult::Return, true, vec![0xde, 0xad]),
            (InstructionResult::Revert, false, vec![0xba, 0xd0]),
        ] {
            let output: Bytes = output.into();
            let completion = CallFromPrecompile
                .complete_subcall(continuation(), &call_result(result, output.clone()))
                .expect("call result completes CallFrom");
            let decoded = ICallFrom::callFromCall::abi_decode_returns(&completion.output)
                .expect("valid CallFrom output");

            assert!(completion.success);
            assert_eq!(completion.gas_overhead, abi_encode_gas(output.len()));
            assert_eq!(decoded.success, expected_success);
            assert_eq!(decoded.returnData.as_ref(), output.as_ref());
        }
    }
}
