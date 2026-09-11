use leafage_evm_chains::arc::{ArcSubcallTraceCompletion, ArcSubcallTraceCompletionPhase};
use revm::{
    inspector::Inspector,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome},
};
use revm_inspectors::tracing::TracingInspector;

#[derive(Debug)]
pub(super) struct ArcSubcallTraceSidecar {
    frame_stack: Vec<usize>,
    frames: Vec<Option<ArcSubcallTraceCompletion>>,
    last_finished_frame: Option<usize>,
    valid: bool,
}

impl Default for ArcSubcallTraceSidecar {
    fn default() -> Self {
        Self {
            frame_stack: Vec::new(),
            frames: Vec::new(),
            last_finished_frame: None,
            valid: true,
        }
    }
}

impl ArcSubcallTraceSidecar {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn push_frame(&mut self) {
        // TracingInspector allocates one arena node for every call/create callback. Its
        // precompile PushOnly mode changes parent attachment, not arena allocation, so this
        // callback ordinal is the stable node ID without consulting the real EVM depth.
        let frame_id = self.frames.len();
        self.frames.push(None);
        self.frame_stack.push(frame_id);
    }

    fn finish_frame(&mut self) {
        let Some(frame_id) = self.frame_stack.pop() else {
            self.valid = false;
            return;
        };
        self.last_finished_frame = Some(frame_id);
    }

    fn record_completion(&mut self, completion: ArcSubcallTraceCompletion) {
        let frame_id = match completion.phase {
            ArcSubcallTraceCompletionPhase::BeforeFrameEnd => self.frame_stack.last().copied(),
            ArcSubcallTraceCompletionPhase::AfterFrameEnd => self.last_finished_frame.take(),
        };
        let Some(frame_id) = frame_id else {
            self.valid = false;
            return;
        };
        let Some(frame) = self.frames.get_mut(frame_id) else {
            self.valid = false;
            return;
        };
        if frame.replace(completion).is_some() {
            self.valid = false;
        }
    }

    pub(super) fn apply(self, inspector: &mut TracingInspector) -> Result<(), String> {
        let nodes = inspector.traces_mut().nodes_mut();
        if !self.valid || !self.frame_stack.is_empty() || self.frames.len() != nodes.len() {
            return Err(format!(
                "Arc subcall trace association failed: frames={}, nodes={}, open_frames={}, valid={}",
                self.frames.len(),
                nodes.len(),
                self.frame_stack.len(),
                self.valid
            ));
        }

        for (frame_id, completion) in self.frames.into_iter().enumerate() {
            let node = &mut nodes[frame_id];
            if node.idx != frame_id {
                return Err(format!(
                    "Arc subcall trace association failed: node {} has index {}",
                    frame_id, node.idx
                ));
            }
            let Some(completion) = completion else {
                continue;
            };

            // The visible logical child is the folded subcall outcome. Preserve raw execution
            // failures; otherwise a completion failure makes that logical child unsuccessful.
            let effective_status = if !completion.child_status.is_ok() {
                completion.child_status
            } else if !completion.final_status.is_ok() {
                completion.final_status
            } else {
                completion.child_status
            };
            node.trace.output = completion.child_output;
            node.trace.gas_used = completion.child_gas_used;
            node.trace.gas_limit = completion.child_gas_limit;
            node.trace.success = effective_status.is_ok();
            node.trace.status = Some(effective_status);
        }
        Ok(())
    }
}

pub(super) fn record_subcall_trace_completion(
    inspector: &mut &mut (TracingInspector, ArcSubcallTraceSidecar),
    completion: ArcSubcallTraceCompletion,
) {
    inspector.1.record_completion(completion);
}

impl<CTX> Inspector<CTX> for ArcSubcallTraceSidecar {
    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.push_frame();
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.finish_frame();
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.push_frame();
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.finish_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Bytes;
    use revm::interpreter::InstructionResult;
    use revm_inspectors::tracing::{
        types::{CallTrace, CallTraceNode},
        CallTraceArena, TracingInspectorConfig,
    };

    fn completion(
        child_status: InstructionResult,
        final_status: InstructionResult,
        phase: ArcSubcallTraceCompletionPhase,
    ) -> ArcSubcallTraceCompletion {
        ArcSubcallTraceCompletion {
            child_status,
            child_output: Bytes::from_static(b"raw-child"),
            child_gas_used: 17,
            child_gas_limit: 23,
            final_status,
            phase,
        }
    }

    fn inspector_with_nodes(nodes: usize) -> TracingInspector {
        let mut inspector = TracingInspector::new(TracingInspectorConfig::default_parity());
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = (0..nodes)
            .map(|idx| CallTraceNode {
                idx,
                trace: CallTrace {
                    success: true,
                    output: Bytes::from_static(b"wrapped"),
                    gas_used: 101,
                    gas_limit: 202,
                    status: Some(InstructionResult::Return),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        *inspector.traces_mut() = arena;
        inspector
    }

    #[test]
    fn raw_child_fields_and_failure_priority_are_applied_for_both_phases() {
        for (child_status, final_status, phase, expected_status) in [
            (
                InstructionResult::Revert,
                InstructionResult::Return,
                ArcSubcallTraceCompletionPhase::BeforeFrameEnd,
                InstructionResult::Revert,
            ),
            (
                InstructionResult::Return,
                InstructionResult::OutOfGas,
                ArcSubcallTraceCompletionPhase::AfterFrameEnd,
                InstructionResult::OutOfGas,
            ),
        ] {
            let mut sidecar = ArcSubcallTraceSidecar::new();
            sidecar.push_frame();
            if phase == ArcSubcallTraceCompletionPhase::AfterFrameEnd {
                sidecar.finish_frame();
            }
            sidecar.record_completion(completion(child_status, final_status, phase));
            if phase == ArcSubcallTraceCompletionPhase::BeforeFrameEnd {
                sidecar.finish_frame();
            }

            let mut inspector = inspector_with_nodes(1);
            sidecar.apply(&mut inspector).unwrap();
            let trace = &inspector.traces().nodes()[0].trace;
            assert_eq!(trace.output.as_ref(), b"raw-child");
            assert_eq!(trace.gas_used, 17);
            assert_eq!(trace.gas_limit, 23);
            assert!(!trace.success);
            assert_eq!(trace.status, Some(expected_status));
        }
    }

    #[test]
    fn nested_and_consecutive_frames_bind_without_evm_depth() {
        let mut sidecar = ArcSubcallTraceSidecar::new();
        sidecar.push_frame();
        sidecar.push_frame();
        sidecar.record_completion(completion(
            InstructionResult::Stop,
            InstructionResult::Return,
            ArcSubcallTraceCompletionPhase::BeforeFrameEnd,
        ));
        sidecar.finish_frame();
        sidecar.finish_frame();
        sidecar.push_frame();
        sidecar.finish_frame();
        sidecar.record_completion(completion(
            InstructionResult::Return,
            InstructionResult::Return,
            ArcSubcallTraceCompletionPhase::AfterFrameEnd,
        ));

        let mut inspector = inspector_with_nodes(3);
        sidecar.apply(&mut inspector).unwrap();
        let nodes = inspector.traces().nodes();
        assert_eq!(nodes[0].trace.output.as_ref(), b"wrapped");
        assert_eq!(nodes[1].trace.output.as_ref(), b"raw-child");
        assert!(nodes[1].trace.success);
        assert_eq!(nodes[1].trace.status, Some(InstructionResult::Stop));
        assert_eq!(nodes[2].trace.output.as_ref(), b"raw-child");
        assert!(nodes[2].trace.success);
        assert_eq!(nodes[2].trace.status, Some(InstructionResult::Return));
    }

    #[test]
    fn inconsistent_frame_and_arena_counts_fail_closed() {
        let mut sidecar = ArcSubcallTraceSidecar::new();
        sidecar.push_frame();
        sidecar.finish_frame();
        let mut inspector = inspector_with_nodes(2);
        assert!(sidecar.apply(&mut inspector).is_err());
        assert_eq!(
            inspector.traces().nodes()[0].trace.output.as_ref(),
            b"wrapped"
        );
    }
}
