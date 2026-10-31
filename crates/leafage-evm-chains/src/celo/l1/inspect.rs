//! Forward a shared inspector through a borrowed system-call context. Erasing
//! the inspector type prevents recursively nested precompiles from producing
//! an unbounded chain of generic wrapper types.
use super::evm::CeloL1Context;
use revm::handler::FrameResult;
use revm::interpreter::{
    CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Interpreter,
};
use revm::primitives::{Address, Log, U256};
use revm::{Database, Inspector};

pub(super) struct BorrowedInspector<'a, DB: Database>(pub &'a mut dyn Inspector<CeloL1Context<DB>>);
macro_rules! forward {
    ($ctx:ty) => {
        impl<DB: Database> Inspector<$ctx> for BorrowedInspector<'_, DB> {
            fn initialize_interp(&mut self, i: &mut Interpreter, c: &mut $ctx) {
                self.0.initialize_interp(i, c);
            }
            fn step(&mut self, i: &mut Interpreter, c: &mut $ctx) {
                self.0.step(i, c);
            }
            fn step_end(&mut self, i: &mut Interpreter, c: &mut $ctx) {
                self.0.step_end(i, c);
            }
            fn log(&mut self, c: &mut $ctx, l: Log) {
                self.0.log(c, l);
            }
            fn log_full(&mut self, i: &mut Interpreter, c: &mut $ctx, l: Log) {
                self.0.log_full(i, c, l);
            }
            fn frame_start(&mut self, c: &mut $ctx, f: &mut FrameInput) -> Option<FrameResult> {
                self.0.frame_start(c, f)
            }
            fn frame_end(&mut self, c: &mut $ctx, f: &FrameInput, r: &mut FrameResult) {
                self.0.frame_end(c, f, r);
            }
            fn call(&mut self, c: &mut $ctx, i: &mut CallInputs) -> Option<CallOutcome> {
                self.0.call(c, i)
            }
            fn call_end(&mut self, c: &mut $ctx, i: &CallInputs, r: &mut CallOutcome) {
                self.0.call_end(c, i, r);
            }
            fn create(&mut self, c: &mut $ctx, i: &mut CreateInputs) -> Option<CreateOutcome> {
                self.0.create(c, i)
            }
            fn create_end(&mut self, c: &mut $ctx, i: &CreateInputs, r: &mut CreateOutcome) {
                self.0.create_end(c, i, r);
            }
            fn selfdestruct(&mut self, a: Address, b: Address, v: U256) {
                self.0.selfdestruct(a, b, v);
            }
        }
    };
}
forward!(CeloL1Context<DB>);
forward!(&mut CeloL1Context<DB>);
