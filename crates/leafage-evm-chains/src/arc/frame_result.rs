use super::native::revert_message;
use revm::{
    handler::FrameResult,
    interpreter::{
        interpreter_action::{FrameInit, FrameInput},
        CallOutcome, CreateOutcome, Gas, InstructionResult, InterpreterResult,
    },
};

pub(crate) fn revert_frame(frame_init: &FrameInit, message: &str) -> FrameResult {
    let output = revert_message(message);

    // Mirror revm's early-return frames: the child keeps its whole regular gas
    // budget and hands its reservoir back, carrying the parent's `charged_*` flags.
    match &frame_init.frame_input {
        FrameInput::Call(inputs) => FrameResult::Call(CallOutcome {
            result: InterpreterResult::new(
                InstructionResult::Revert,
                output,
                Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir),
            ),
            memory_offset: inputs.return_memory_offset.clone(),
            was_precompile_called: false,
            precompile_call_logs: Vec::new(),
            charged_new_account_state_gas: inputs.charged_new_account_state_gas,
        }),
        FrameInput::Create(inputs) => FrameResult::Create(CreateOutcome {
            result: InterpreterResult::new(
                InstructionResult::Revert,
                output,
                Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit(), inputs.reservoir()),
            ),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
        }),
        FrameInput::Empty => unreachable!("empty frame cannot transfer value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::native::ERR_BLOCKED_ADDRESS;
    use alloy::primitives::{Address, Bytes, U256};
    use revm::interpreter::{
        interpreter_action::{CreateInputs, FrameInput},
        CreateScheme, SharedMemory,
    };

    #[test]
    fn create_revert_has_no_created_address_and_preserves_gas() {
        let frame_init = FrameInit {
            depth: 1,
            memory: SharedMemory::default(),
            frame_input: FrameInput::Create(Box::new(CreateInputs::new(
                Address::with_last_byte(1),
                CreateScheme::Create,
                U256::ONE,
                Bytes::new(),
                55_000,
                0,
            ))),
        };

        let FrameResult::Create(outcome) = revert_frame(&frame_init, ERR_BLOCKED_ADDRESS) else {
            panic!("CREATE rejection must return a create outcome");
        };
        assert_eq!(outcome.result.result, InstructionResult::Revert);
        assert_eq!(outcome.result.gas.remaining(), 55_000);
        assert_eq!(outcome.address, None);
    }
}
