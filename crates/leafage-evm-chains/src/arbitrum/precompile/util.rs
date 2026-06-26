use super::{ADDRESS_ALIAS_OFFSET, BASE_PRECOMPILE_GAS};
use alloy::primitives::{Address, Bytes, B256, I256, U256};
use alloy::sol_types::{SolCall, SolError, SolInterface};
use revm::interpreter::{Gas, InstructionResult, InterpreterResult};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

const COPY_GAS: u64 = 3;
const LOG_GAS: u64 = 375;
const LOG_TOPIC_GAS: u64 = 375;
const LOG_DATA_GAS: u64 = 8;

pub(super) fn to_interpreter_result(
    gas_limit: u64,
    result: PrecompileResult,
) -> Result<InterpreterResult, String> {
    let mut interpreter_result = InterpreterResult {
        result: InstructionResult::Return,
        gas: Gas::new(gas_limit),
        output: Bytes::new(),
    };

    match result {
        Ok(output) => {
            if !interpreter_result.gas.record_cost(output.gas_used) {
                interpreter_result.result = InstructionResult::PrecompileOOG;
                return Ok(interpreter_result);
            }
            interpreter_result.result = if output.reverted {
                InstructionResult::Revert
            } else {
                InstructionResult::Return
            };
            interpreter_result.output = output.bytes;
        }
        Err(PrecompileError::Fatal(e)) => return Err(e),
        Err(e) => {
            interpreter_result.result = if e.is_oog() {
                InstructionResult::PrecompileOOG
            } else {
                InstructionResult::PrecompileError
            };
        }
    }

    Ok(interpreter_result)
}

pub(super) fn copy_gas(byte_count: usize) -> u64 {
    COPY_GAS.saturating_mul((byte_count as u64).div_ceil(32))
}

pub(super) fn log_gas(indexed_topics: u64, data_len: usize) -> u64 {
    LOG_GAS
        .saturating_add(LOG_TOPIC_GAS.saturating_mul(indexed_topics.saturating_add(1)))
        .saturating_add(LOG_DATA_GAS.saturating_mul(data_len as u64))
}

fn finish(gas_limit: u64, gas_used: u64, bytes: Bytes) -> PrecompileResult {
    let gas_used = gas_used.saturating_add(copy_gas(bytes.len()));
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }
    Ok(PrecompileOutput::new(gas_used, bytes))
}

fn finish_revert(gas_limit: u64, gas_used: u64, bytes: Bytes) -> PrecompileResult {
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }
    let gas_used = gas_used.saturating_add(copy_gas(bytes.len()));
    if gas_used > gas_limit {
        return empty_revert(gas_limit, gas_limit);
    }
    Ok(PrecompileOutput::new_reverted(gas_used, bytes))
}

pub(super) fn finish_call<T: SolCall>(
    gas_limit: u64,
    gas_used: u64,
    ret: T::Return,
) -> PrecompileResult {
    finish(gas_limit, gas_used, T::abi_encode_returns(&ret).into())
}

pub(super) fn empty_revert(gas_limit: u64, gas_used: u64) -> PrecompileResult {
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }
    Ok(PrecompileOutput::new_reverted(gas_used, Bytes::new()))
}

pub(super) fn sol_error_revert<T: SolError>(
    gas_limit: u64,
    gas_used: u64,
    error: T,
) -> PrecompileResult {
    finish_revert(gas_limit, gas_used, error.abi_encode().into())
}

pub(super) fn decode_revert(gas_limit: u64, _reason: &str) -> PrecompileResult {
    empty_revert(gas_limit, gas_limit)
}

pub(super) fn address_from_word(word: U256) -> Address {
    let bytes = word.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

pub(super) fn address_key(address: Address) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[12..].copy_from_slice(address.as_slice());
    key
}

pub(super) fn topic_address(address: Address) -> B256 {
    let mut topic = [0u8; 32];
    topic[12..].copy_from_slice(address.as_slice());
    B256::from(topic)
}

pub(super) fn topic_u256(value: U256) -> B256 {
    B256::from(value.to_be_bytes::<32>())
}

pub(super) fn low_u64_as_i64(word: U256) -> i64 {
    word.to::<u64>() as i64
}

pub(super) fn signed_word(word: U256) -> I256 {
    I256::from_raw(word)
}

pub(super) fn alias_l1_address(address: Address) -> Address {
    let value = U256::from_be_slice(address.as_slice());
    let mask = (U256::from(1u8) << 160) - U256::from(1u8);
    let aliased = value.wrapping_add(ADDRESS_ALIAS_OFFSET) & mask;
    address_from_word(aliased)
}

pub(super) fn inverse_alias_l1_address(address: Address) -> Address {
    let value = U256::from_be_slice(address.as_slice());
    let mask = (U256::from(1u8) << 160) - U256::from(1u8);
    let unaliased = value.wrapping_sub(ADDRESS_ALIAS_OFFSET) & mask;
    address_from_word(unaliased)
}

pub(super) fn dispatch<T: SolInterface>(
    data: &[u8],
    gas_limit: u64,
    f: impl FnOnce(T, u64) -> PrecompileResult,
) -> PrecompileResult {
    let Some((selector, args)) = data.split_first_chunk::<4>() else {
        return decode_revert(gas_limit, "unknown Arbitrum precompile selector");
    };
    if T::type_check(*selector).is_err() {
        return decode_revert(gas_limit, "unknown Arbitrum precompile selector");
    }

    let initial_gas = BASE_PRECOMPILE_GAS.saturating_add(copy_gas(args.len()));
    if initial_gas > gas_limit {
        return empty_revert(gas_limit, gas_limit);
    }

    match T::abi_decode_raw(*selector, args) {
        Ok(call) => f(call, initial_gas),
        Err(_) => decode_revert(gas_limit, "invalid Arbitrum precompile calldata"),
    }
}

pub(super) fn signed_diff(lhs: U256, rhs: U256) -> I256 {
    if lhs >= rhs {
        I256::from_raw(lhs - rhs)
    } else {
        I256::from_raw(U256::ZERO.wrapping_sub(rhs - lhs))
    }
}

#[cfg(test)]
mod tests {
    use super::super::abi::{IArbosActs, IArbosTest};
    use super::*;
    use alloy::sol_types::SolCall;

    #[test]
    fn dispatch_adds_args_copy_to_initial_gas() {
        let call = IArbosTest::burnArbGasCall {
            gasAmount: U256::ZERO,
        };
        let data = call.abi_encode();
        let output =
            dispatch::<IArbosTest::IArbosTestCalls>(&data, u64::MAX, |call, initial_gas| {
                assert!(matches!(
                    call,
                    IArbosTest::IArbosTestCalls::burnArbGas(call) if call.gasAmount.is_zero()
                ));
                Ok(PrecompileOutput::new(initial_gas, Bytes::new()))
            })
            .unwrap();

        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS + copy_gas(data.len() - 4)
        );
    }

    #[test]
    fn sol_error_revert_charges_selector_copy_gas() {
        let output =
            sol_error_revert(u64::MAX, BASE_PRECOMPILE_GAS, IArbosActs::CallerNotArbOS {}).unwrap();

        assert!(output.reverted);
        assert_eq!(output.bytes.len(), 4);
        assert_eq!(output.gas_used, BASE_PRECOMPILE_GAS + COPY_GAS);
    }

    #[test]
    fn log_gas_matches_nitro_event_formula() {
        assert_eq!(log_gas(1, 32), 1_381);
        assert_eq!(log_gas(3, 0), 1_875);
        assert_eq!(log_gas(3, 4 * 32), 2_899);
    }

    #[test]
    fn revert_data_copy_oog_consumes_all_gas_without_data() {
        let gas_limit = BASE_PRECOMPILE_GAS + COPY_GAS - 1;
        let output = sol_error_revert(
            gas_limit,
            BASE_PRECOMPILE_GAS,
            IArbosActs::CallerNotArbOS {},
        )
        .unwrap();

        assert!(output.reverted);
        assert_eq!(output.gas_used, gas_limit);
        assert!(output.bytes.is_empty());
    }

    #[test]
    fn interpreter_result_over_gas_output_becomes_oog() {
        let result = to_interpreter_result(10, Ok(PrecompileOutput::new(11, Bytes::new())))
            .expect("convert precompile result");

        assert_eq!(result.result, InstructionResult::PrecompileOOG);
        assert!(result.output.is_empty());
    }
}
