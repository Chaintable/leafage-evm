//! [`TracingInspector`] plus the SELFDESTRUCT frames it can miss, and which
//! frames ran an SSTORE.
//!
//! revm only reports a selfdestruct to inspectors when the opcode leaves a
//! journal entry. After Cancun (EIP-6780) a contract not created in the same
//! transaction that selfdestructs to itself changes nothing, so the hook never
//! fires and the trace node is left with `SelfDestruct` status but no
//! addresses. geth still emits `OnEnter(SELFDESTRUCT, self, self, balance)`
//! for it, so the missing data is recorded from the opcode step instead.
//!
//! The tracer records no steps, so its own per-instruction hooks only check
//! a byte budget that is unlimited here; they are skipped, leaving one opcode
//! comparison per instruction.

use alloy::primitives::{Address, Log, U256};
use revm::{
    bytecode::opcode,
    context::{ContextTr, JournalTr},
    inspector::JournalExt,
    interpreter::{
        interpreter_types::Jumps, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        InstructionResult, Interpreter,
    },
    Inspector,
};
use revm_inspectors::tracing::{
    types::CallTraceNode, CallTraceArena, TracingInspector, TracingInspectorConfig,
};

#[derive(Debug, Clone, Copy)]
struct PendingSelfdestruct {
    contract: Address,
    target: Address,
    balance: U256,
}

#[derive(Debug)]
pub(crate) struct PipelineInspector {
    tracer: TracingInspector,
    /// Arena index of every open call frame, innermost last.
    frames: Vec<usize>,
    /// SELFDESTRUCT seen in the open frame with the same position.
    pending: Vec<Option<PendingSelfdestruct>>,
    /// Whether the frame with this arena index ran an SSTORE. Recording
    /// the tracer's steps for that costs a step entry per instruction.
    sstore: Vec<bool>,
}

impl PipelineInspector {
    pub(crate) fn new() -> Self {
        let config = TracingInspectorConfig::default_parity()
            .set_steps(false)
            .set_record_logs(true)
            .set_exclude_precompile_calls(false);
        Self {
            tracer: TracingInspector::new(config),
            frames: Vec::new(),
            pending: Vec::new(),
            sstore: Vec::new(),
        }
    }

    /// The call traces, and whether each of their nodes ran an SSTORE.
    pub(crate) fn into_traces(self) -> (CallTraceArena, Vec<bool>) {
        (self.tracer.into_traces(), self.sstore)
    }

    fn open_frame<CTX: ContextTr>(&mut self, context: &CTX, nodes_before: usize) {
        // The root frame overwrites node 0; nested frames are appended.
        let idx = if context.journal_ref().depth() == 0 {
            0
        } else {
            nodes_before
        };
        self.frames.push(idx);
        self.pending.push(None);
    }

    fn close_frame(&mut self, result: InstructionResult) {
        let (Some(idx), Some(pending)) = (self.frames.pop(), self.pending.pop()) else {
            return;
        };
        let Some(pending) = pending else {
            return;
        };
        if result != InstructionResult::SelfDestruct {
            return;
        }
        let Some(node) = self.tracer.traces_mut().nodes_mut().get_mut(idx) else {
            return;
        };
        fill_selfdestruct(node, pending);
    }
}

impl PipelineInspector {
    fn mark_sstore(&mut self) {
        if let Some(&idx) = self.frames.last() {
            if self.sstore.len() <= idx {
                self.sstore.resize(idx + 1, false);
            }
            self.sstore[idx] = true;
        }
    }

    #[cold]
    fn record_selfdestruct<CTX: ContextTr>(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let Ok(target) = interp.stack.peek(0) else {
            return;
        };
        let contract = interp.input.target_address;
        let balance = context
            .journal_mut()
            .load_account(contract)
            .map(|account| account.info.balance)
            .unwrap_or_default();
        if let Some(slot) = self.pending.last_mut() {
            *slot = Some(PendingSelfdestruct {
                contract,
                target: Address::from_word(target.into()),
                balance,
            });
        }
    }
}

fn fill_selfdestruct(node: &mut CallTraceNode, pending: PendingSelfdestruct) {
    let trace = &mut node.trace;
    if trace.selfdestruct_refund_target.is_some() {
        return;
    }
    trace.selfdestruct_address = Some(pending.contract);
    trace.selfdestruct_refund_target = Some(pending.target);
    trace.selfdestruct_transferred_value = Some(pending.balance);
}

impl<CTX> Inspector<CTX> for PipelineInspector
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.tracer.initialize_interp(interp, context);
    }

    #[inline(always)]
    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let op = interp.bytecode.opcode();
        if op == opcode::SSTORE {
            self.mark_sstore();
        } else if op == opcode::SELFDESTRUCT {
            self.record_selfdestruct(interp, context);
        }
    }

    fn log(&mut self, context: &mut CTX, log: Log) {
        self.tracer.log(context, log);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let nodes_before = self.tracer.traces().nodes().len();
        let outcome = self.tracer.call(context, inputs);
        if outcome.is_none() {
            self.open_frame(context, nodes_before);
        }
        outcome
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.tracer.call_end(context, inputs, outcome);
        self.close_frame(outcome.result.result);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let nodes_before = self.tracer.traces().nodes().len();
        let outcome = self.tracer.create(context, inputs);
        if outcome.is_none() {
            self.open_frame(context, nodes_before);
        }
        outcome
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.tracer.create_end(context, inputs, outcome);
        self.close_frame(outcome.result.result);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        Inspector::<CTX>::selfdestruct(&mut self.tracer, contract, target, value);
    }
}
