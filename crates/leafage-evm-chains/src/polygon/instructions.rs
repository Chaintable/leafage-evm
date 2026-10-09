use crate::polygon::api::PolygonContext;
use crate::polygon::gas::pip88_costs::{
    COLD_SLOAD_ADDITIONAL_COST, COLD_SSTORE_ADDITIONAL_COST, WARM_STORAGE_READ_COST,
};
use crate::polygon::PolygonHardfork;
use revm::bytecode::opcode::{SLOAD, SSTORE};
use revm::handler::instructions::EthInstructions;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{InputsTr, RuntimeFlag, StackTr};
use revm::interpreter::{
    Host, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
    InterpreterTypes,
};

pub(crate) fn polygon_instructions<DB: revm::database::Database>(
    hardfork: PolygonHardfork,
) -> EthInstructions<EthInterpreter, PolygonContext<DB>> {
    let mut instructions = EthInstructions::new_mainnet_with_spec(hardfork.into());
    if hardfork.is_pip88_enabled() {
        install_pip88_storage_instructions(&mut instructions);
    }
    instructions
}

fn install_pip88_storage_instructions<DB: revm::database::Database>(
    instructions: &mut EthInstructions<EthInterpreter, PolygonContext<DB>>,
) {
    instructions.insert_instruction(
        SLOAD,
        Instruction::new(sload_pip88::<EthInterpreter, PolygonContext<DB>>),
        WARM_STORAGE_READ_COST as u16,
    );
    instructions.insert_instruction(
        SSTORE,
        Instruction::new(sstore_pip88::<EthInterpreter, PolygonContext<DB>>),
        0,
    );
}

fn sload_pip88<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) -> InstructionExecResult {
    let Some([index]) = context.interpreter.stack.popn::<1>() else {
        return Err(InstructionResult::StackUnderflow);
    };
    let target = context.interpreter.input.target_address();

    let skip_cold = context.interpreter.gas.remaining() < COLD_SLOAD_ADDITIONAL_COST;
    // `LoadError::ColdLoadSkipped` maps to OutOfGas, `LoadError::DBError` to FatalExternalError.
    let storage = context
        .host
        .sload_skip_cold_load(target, index, skip_cold)?;
    if storage.is_cold {
        record_cost(context.interpreter, COLD_SLOAD_ADDITIONAL_COST)?;
    }

    if !context.interpreter.stack.push(storage.data) {
        return Err(InstructionResult::StackOverflow);
    }
    Ok(())
}

fn sstore_pip88<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) -> InstructionExecResult {
    if context.interpreter.runtime_flag.is_static() {
        return Err(InstructionResult::StateChangeDuringStaticCall);
    }

    let Some([index, value]) = context.interpreter.stack.popn::<2>() else {
        return Err(InstructionResult::StackUnderflow);
    };

    if context.interpreter.gas.remaining() <= context.host.gas_params().call_stipend() {
        return Err(InstructionResult::ReentrancySentryOOG);
    }

    record_cost(
        context.interpreter,
        context.host.gas_params().sstore_static_gas(),
    )?;

    let target = context.interpreter.input.target_address();
    let skip_cold = context.interpreter.gas.remaining() < COLD_SSTORE_ADDITIONAL_COST;
    // `LoadError::ColdLoadSkipped` maps to OutOfGas, `LoadError::DBError` to FatalExternalError.
    let state_load = context
        .host
        .sstore_skip_cold_load(target, index, value, skip_cold)?;

    record_cost(
        context.interpreter,
        context
            .host
            .gas_params()
            .sstore_dynamic_gas(true, &state_load.data, state_load.is_cold),
    )?;

    context.interpreter.gas.record_refund(
        context
            .host
            .gas_params()
            .sstore_refund(true, &state_load.data),
    );
    Ok(())
}

fn record_cost<WIRE: InterpreterTypes>(
    interpreter: &mut revm::interpreter::Interpreter<WIRE>,
    gas: u64,
) -> InstructionExecResult {
    if interpreter.gas.record_regular_cost(gas) {
        return Ok(());
    }
    Err(InstructionResult::OutOfGas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::database_interface::EmptyDB;

    #[test]
    fn pip88_storage_instruction_static_gas_matches_revm_model() {
        let instructions = polygon_instructions::<EmptyDB>(PolygonHardfork::Chicago);

        assert_eq!(
            instructions.gas_table()[SLOAD as usize] as u64,
            WARM_STORAGE_READ_COST
        );
        assert_eq!(instructions.gas_table()[SSTORE as usize], 0);
    }
}
