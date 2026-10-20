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
//! Before Homestead, a contract creation without the gas to deposit its code
//! succeeds with empty code; revm drops the code its constructor returned.
//! geth keeps it as the frame's output and fails the transaction
//! (`ErrCodeStoreOutOfGas`), so the code is taken from the constructor's
//! RETURN once it has run, after its memory expansion was paid for.
//!
//! The tracer records no steps, so its own per-instruction hooks only check
//! a byte budget that is unlimited here; they are skipped, leaving one opcode
//! comparison and one flag check per instruction.

use alloy::primitives::{Address, Bytes, Log, U256};
use revm::{
    bytecode::opcode,
    context::{ContextTr, JournalTr},
    inspector::JournalExt,
    interpreter::{
        interpreter_types::{Jumps, LoopControl},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
        InterpreterAction,
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
    /// For the open frame with the same position: `Some` for a creation,
    /// holding the code its constructor returned once it returns.
    returned: Vec<Option<Bytes>>,
    /// The instruction being run is a RETURN.
    returning: bool,
    /// The transaction's own creation could not deposit its code.
    code_deposit_failed: bool,
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
            returned: Vec::new(),
            returning: false,
            code_deposit_failed: false,
        }
    }

    /// Whether the transaction creates a contract that could not pay for its
    /// code (before Homestead): geth counts it as failed.
    pub(crate) fn code_deposit_failed(&self) -> bool {
        self.code_deposit_failed
    }

    /// The call traces, and whether each of their nodes ran an SSTORE.
    pub(crate) fn into_traces(self) -> (CallTraceArena, Vec<bool>) {
        (self.tracer.into_traces(), self.sstore)
    }

    fn open_frame<CTX: ContextTr>(&mut self, context: &CTX, nodes_before: usize, create: bool) {
        // The root frame overwrites node 0; nested frames are appended.
        let idx = if context.journal_ref().depth() == 0 {
            0
        } else {
            nodes_before
        };
        self.frames.push(idx);
        self.pending.push(None);
        self.returned.push(create.then(Bytes::new));
    }

    fn close_frame(&mut self, result: InstructionResult) {
        self.returned.pop();
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

    /// The code a constructor returned, once its RETURN has run: before
    /// that, the stack's length is not paid for and may be huge.
    #[cold]
    fn record_return(&mut self, interp: &mut Interpreter) {
        self.returning = false;
        let Some(Some(returned)) = self.returned.last_mut() else {
            return;
        };
        if let Some(InterpreterAction::Return(result)) = interp.bytecode.action() {
            if result.result == InstructionResult::Return {
                *returned = result.output.clone();
            }
        }
    }

    /// A creation that succeeded with no code although its constructor
    /// returned some: it could not pay for the code.
    fn check_code_deposit(&mut self, outcome: &CreateOutcome) {
        let Some(Some(returned)) = self.returned.last() else {
            return;
        };
        if returned.is_empty()
            || !outcome.result.result.is_ok()
            || !outcome.result.output.is_empty()
        {
            return;
        }
        let returned = returned.clone();
        let Some(&idx) = self.frames.last() else {
            return;
        };
        if idx == 0 && self.frames.len() == 1 {
            self.code_deposit_failed = true;
        }
        if let Some(node) = self.tracer.traces_mut().nodes_mut().get_mut(idx) {
            node.trace.output = returned;
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
        } else if op == opcode::RETURN {
            self.returning = true;
        }
    }

    #[inline(always)]
    fn step_end(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        if self.returning {
            self.record_return(interp);
        }
    }

    fn log(&mut self, context: &mut CTX, log: Log) {
        self.tracer.log(context, log);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let nodes_before = self.tracer.traces().nodes().len();
        let outcome = self.tracer.call(context, inputs);
        if outcome.is_none() {
            self.open_frame(context, nodes_before, false);
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
            self.open_frame(context, nodes_before, true);
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
        self.check_code_deposit(outcome);
        self.close_frame(outcome.result.result);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        Inspector::<CTX>::selfdestruct(&mut self.tracer, contract, target, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::TxEnv;
    use revm::database::{CacheDB, EmptyDB};
    use revm::primitives::hardfork::SpecId;
    use revm::primitives::TxKind;
    use revm::{Context, InspectEvm, MainBuilder, MainContext};

    /// PUSH1 100, PUSH1 0, RETURN: 100 zero bytes of code.
    const RETURN_100: &[u8] = &[0x60, 0x64, 0x60, 0x00, 0xf3];

    /// Deploy `initcode` with `gas_limit`.
    fn deploy(spec: SpecId, initcode: &'static [u8], gas_limit: u64) -> (bool, PipelineInspector) {
        let mut evm = Context::mainnet()
            .with_db(CacheDB::new(EmptyDB::default()))
            .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(spec))
            .build_mainnet_with_inspector(PipelineInspector::new());
        let tx = TxEnv::builder()
            .caller(Address::with_last_byte(1))
            .kind(TxKind::Create)
            .data(Bytes::from_static(initcode))
            .gas_limit(gas_limit)
            .gas_price(0)
            .build()
            .unwrap();
        let result = evm.inspect_tx(tx).unwrap();
        (result.result.is_success(), evm.inspector)
    }

    fn output(inspector: PipelineInspector) -> Bytes {
        let (arena, _) = inspector.into_traces();
        arena.nodes()[0].trace.output.clone()
    }

    #[test]
    fn frontier_creation_without_gas_for_its_code_keeps_it_as_output() {
        // Enough to run the constructor (21000 + 276 calldata + 18), not
        // to deposit 100 bytes (20000).
        let (success, inspector) = deploy(SpecId::FRONTIER, RETURN_100, 22_000);
        assert!(success, "revm keeps the contract, without code");
        assert!(inspector.code_deposit_failed());
        assert_eq!(output(inspector), Bytes::from(vec![0u8; 100]));

        let (success, inspector) = deploy(SpecId::FRONTIER, RETURN_100, 100_000);
        assert!(success);
        assert!(!inspector.code_deposit_failed());
        assert_eq!(output(inspector), Bytes::from(vec![0u8; 100]));

        // Homestead fails the creation instead.
        let (success, inspector) = deploy(SpecId::HOMESTEAD, RETURN_100, 60_000);
        assert!(!success);
        assert!(!inspector.code_deposit_failed());
    }

    #[test]
    fn unpaid_return_length_is_not_allocated() {
        // PUSH4 2^31, PUSH1 0, RETURN: runs out of gas expanding memory; the
        // inspector must not read 2 GiB before that.
        let initcode = &[0x63, 0x80, 0x00, 0x00, 0x00, 0x60, 0x00, 0xf3];
        for spec in [SpecId::FRONTIER, SpecId::CANCUN] {
            let (success, inspector) = deploy(spec, initcode, 100_000);
            assert!(!success);
            assert!(!inspector.code_deposit_failed());
            assert!(output(inspector).is_empty());
        }
    }
}
