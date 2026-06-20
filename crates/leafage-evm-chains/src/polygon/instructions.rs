use crate::polygon::api::PolygonContext;
use crate::polygon::gas::pip88_costs::{
    COLD_SLOAD_ADDITIONAL_COST, COLD_SSTORE_ADDITIONAL_COST, WARM_STORAGE_READ_COST,
};
use crate::polygon::PolygonHardfork;
use revm::bytecode::opcode::{SLOAD, SSTORE};
use revm::context_interface::host::LoadError;
use revm::handler::instructions::EthInstructions;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{InputsTr, RuntimeFlag, StackTr};
use revm::interpreter::{
    Host, Instruction, InstructionContext, InstructionResult, InterpreterTypes,
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
        Instruction::new(
            sload_pip88::<EthInterpreter, PolygonContext<DB>>,
            WARM_STORAGE_READ_COST,
        ),
    );
    instructions.insert_instruction(
        SSTORE,
        Instruction::new(
            sstore_pip88::<EthInterpreter, PolygonContext<DB>>,
            WARM_STORAGE_READ_COST,
        ),
    );
}

fn sload_pip88<WIRE: InterpreterTypes, H: Host + ?Sized>(context: InstructionContext<'_, H, WIRE>) {
    let Some([index]) = context.interpreter.stack.popn::<1>() else {
        context.interpreter.halt_underflow();
        return;
    };
    let target = context.interpreter.input.target_address();

    let skip_cold = context.interpreter.gas.remaining() < COLD_SLOAD_ADDITIONAL_COST;
    let res = context.host.sload_skip_cold_load(target, index, skip_cold);
    match res {
        Ok(storage) => {
            if storage.is_cold && !record_cost(context.interpreter, COLD_SLOAD_ADDITIONAL_COST) {
                return;
            }

            if !context.interpreter.stack.push(storage.data) {
                context.interpreter.halt_overflow();
            }
        }
        Err(LoadError::ColdLoadSkipped) => context.interpreter.halt_oog(),
        Err(LoadError::DBError) => context.interpreter.halt_fatal(),
    }
}

fn sstore_pip88<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    if context.interpreter.runtime_flag.is_static() {
        context
            .interpreter
            .halt(InstructionResult::StateChangeDuringStaticCall);
        return;
    }

    let Some([index, value]) = context.interpreter.stack.popn::<2>() else {
        context.interpreter.halt_underflow();
        return;
    };

    if context.interpreter.gas.remaining() <= context.host.gas_params().call_stipend() {
        context
            .interpreter
            .halt(InstructionResult::ReentrancySentryOOG);
        return;
    }

    if !record_cost(
        context.interpreter,
        context.host.gas_params().sstore_static_gas(),
    ) {
        return;
    }

    let target = context.interpreter.input.target_address();
    let skip_cold = context.interpreter.gas.remaining() < COLD_SSTORE_ADDITIONAL_COST;
    let state_load = match context
        .host
        .sstore_skip_cold_load(target, index, value, skip_cold)
    {
        Ok(load) => load,
        Err(LoadError::ColdLoadSkipped) => return context.interpreter.halt_oog(),
        Err(LoadError::DBError) => return context.interpreter.halt_fatal(),
    };

    if !record_cost(
        context.interpreter,
        context
            .host
            .gas_params()
            .sstore_dynamic_gas(true, &state_load.data, state_load.is_cold),
    ) {
        return;
    }

    context.interpreter.gas.record_refund(
        context
            .host
            .gas_params()
            .sstore_refund(true, &state_load.data),
    );
}

fn record_cost<WIRE: InterpreterTypes>(
    interpreter: &mut revm::interpreter::Interpreter<WIRE>,
    gas: u64,
) -> bool {
    if interpreter.gas.record_cost(gas) {
        return true;
    }

    interpreter.halt_oog();
    false
}
