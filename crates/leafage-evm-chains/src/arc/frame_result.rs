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

    match &frame_init.frame_input {
        FrameInput::Call(inputs) => FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Revert,
                output,
                Gas::new(inputs.gas_limit),
            ),
            inputs.return_memory_offset.clone(),
        )),
        FrameInput::Create(inputs) => FrameResult::Create(CreateOutcome::new(
            InterpreterResult::new(
                InstructionResult::Revert,
                output,
                Gas::new(inputs.gas_limit()),
            ),
            None,
        )),
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
