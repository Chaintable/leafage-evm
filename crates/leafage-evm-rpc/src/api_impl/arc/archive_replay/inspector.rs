// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Captures full Arc logs and their execution-frame ordering.
//!
//! Arc emits some EIP-7708 logs directly through the journal during frame
//! initialization and SELFDESTRUCT handling. Those logs do not all reach the
//! standard tracing inspector callbacks. This inspector runs alongside
//! `TracingInspector` and records the complete logs without changing execution.

use alloy::primitives::Log;
use leafage_evm_chains::arc::{
    ArcSubcallTraceCompletion, ArcSubcallTraceCompletionPhase as SubcallTraceCompletionPhase,
};
use revm::{
    context_interface::{ContextTr, JournalTr},
    inspector::Inspector,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
    },
};
use revm_inspectors::tracing::TracingInspector;

// Ported from arc-node 830ad37, crates/debank-rpc/src/event_inspector.rs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubcallTraceCompletion {
    pub(crate) child_status: InstructionResult,
    pub(crate) child_output: alloy::primitives::Bytes,
    pub(crate) child_gas_used: u64,
    pub(crate) child_gas_limit: u64,
    pub(crate) final_status: InstructionResult,
    pub(crate) phase: SubcallTraceCompletionPhase,
    pub(crate) rolled_back_by_completion: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CapturedMember {
    Call(usize),
    Event(usize),
    SubcallCompletion(SubcallTraceCompletion),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CapturedFrame {
    pub parent: Option<usize>,
    pub members: Vec<CapturedMember>,
    pub success: bool,
    log_count_before: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedEvent {
    pub frame: usize,
    pub log: Log,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CapturedEvents {
    pub frames: Vec<CapturedFrame>,
    pub events: Vec<CapturedEvent>,
    valid: bool,
}

impl CapturedEvents {
    #[cfg(test)]
    /// Restore logical child results before building parity traces.
    pub fn apply_subcall_completions(
        &self,
        inspector: &mut TracingInspector,
    ) -> Result<(), String> {
        let nodes = inspector.traces_mut().nodes_mut();
        self.validate(nodes.len())?;
        for (index, (node, frame)) in nodes.iter_mut().zip(&self.frames).enumerate() {
            if node.idx != index || node.trace.success != frame.success {
                return Err(format!(
                    "trace and event inspectors disagree on frame {index}"
                ));
            }
            let Some(completion) = frame.subcall_completion() else {
                continue;
            };
            let status = if !completion.child_status.is_ok() {
                completion.child_status
            } else if !completion.final_status.is_ok() {
                completion.final_status
            } else {
                completion.child_status
            };
            node.trace.output = completion.child_output.clone();
            node.trace.gas_used = completion.child_gas_used;
            node.trace.gas_limit = completion.child_gas_limit;
            node.trace.success = status.is_ok();
            node.trace.status = Some(status);
        }
        Ok(())
    }

    pub fn validate(&self, expected_frames: usize) -> Result<(), String> {
        if !self.valid {
            return Err("event inspector observed an invalid frame stack".to_string());
        }
        if self.frames.len() != expected_frames {
            return Err(format!(
                "event inspector captured {} frames, tracing inspector captured {expected_frames}",
                self.frames.len()
            ));
        }
        for (frame_id, frame) in self.frames.iter().enumerate() {
            let completions = frame
                .members
                .iter()
                .filter_map(|member| match member {
                    CapturedMember::SubcallCompletion(completion) => Some(completion),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if completions.len() > 1 {
                return Err(format!(
                    "event inspector captured multiple subcall completions for frame {frame_id}"
                ));
            }
            if let Some(completion) = completions.first() {
                if completion.rolled_back_by_completion
                    != (completion.child_status.is_ok() && !completion.final_status.is_ok())
                {
                    return Err(format!(
                        "event inspector frame {frame_id} has inconsistent subcall completion"
                    ));
                }
                let observed_success = match completion.phase {
                    SubcallTraceCompletionPhase::BeforeFrameEnd => completion.final_status.is_ok(),
                    SubcallTraceCompletionPhase::AfterFrameEnd => completion.child_status.is_ok(),
                };
                if frame.success != observed_success {
                    return Err(format!(
                        "event inspector frame {frame_id} success disagrees with subcall completion"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn successful_logs(&self) -> Vec<Log> {
        let mut frame_success = Vec::with_capacity(self.frames.len());
        for frame in &self.frames {
            let parent_success = frame
                .parent
                .and_then(|parent| frame_success.get(parent).copied())
                .unwrap_or(true);
            frame_success.push(parent_success && frame.effective_success());
        }

        self.events
            .iter()
            .filter(|event| frame_success.get(event.frame).copied().unwrap_or(false))
            .map(|event| event.log.clone())
            .collect()
    }
}

impl CapturedFrame {
    pub(crate) fn subcall_completion(&self) -> Option<&SubcallTraceCompletion> {
        self.members.iter().find_map(|member| match member {
            CapturedMember::SubcallCompletion(completion) => Some(completion),
            _ => None,
        })
    }

    pub(crate) fn raw_success(&self) -> bool {
        self.subcall_completion()
            .map(|completion| completion.child_status.is_ok())
            .unwrap_or(self.success)
    }

    pub(crate) fn effective_success(&self) -> bool {
        self.raw_success()
            && !self
                .subcall_completion()
                .is_some_and(|completion| completion.rolled_back_by_completion)
    }

    pub(crate) fn raw_failure_status(&self) -> Option<InstructionResult> {
        let completion = self.subcall_completion()?;
        (!completion.child_status.is_ok()).then_some(completion.child_status)
    }

    pub(crate) fn completion_rolled_back(&self) -> bool {
        self.subcall_completion()
            .is_some_and(|completion| completion.rolled_back_by_completion)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ArcEventInspector {
    captured: CapturedEvents,
    frame_stack: Vec<usize>,
    journal_log_count: usize,
    last_finished_frame: Option<usize>,
}

impl Default for ArcEventInspector {
    fn default() -> Self {
        Self {
            captured: CapturedEvents {
                valid: true,
                ..Default::default()
            },
            frame_stack: Vec::new(),
            journal_log_count: 0,
            last_finished_frame: None,
        }
    }
}

impl ArcEventInspector {
    pub fn into_captured(mut self) -> CapturedEvents {
        if !self.frame_stack.is_empty() {
            self.captured.valid = false;
        }
        self.captured
    }

    fn push_frame(&mut self, log_count_before: usize) {
        let parent = self.frame_stack.last().copied();
        let frame_id = self.captured.frames.len();
        if let Some(parent_id) = parent {
            if let Some(parent_frame) = self.captured.frames.get_mut(parent_id) {
                parent_frame.members.push(CapturedMember::Call(frame_id));
            } else {
                self.captured.valid = false;
            }
        }
        self.captured.frames.push(CapturedFrame {
            parent,
            log_count_before,
            ..Default::default()
        });
        self.frame_stack.push(frame_id);
    }

    fn finish_frame(&mut self, success: bool) {
        let Some(frame_id) = self.frame_stack.pop() else {
            self.captured.valid = false;
            return;
        };
        if let Some(frame) = self.captured.frames.get_mut(frame_id) {
            frame.success = success;
            self.last_finished_frame = Some(frame_id);
        } else {
            self.captured.valid = false;
        }
    }

    fn record_subcall_completion(&mut self, completion: SubcallTraceCompletion) {
        let frame_id = match completion.phase {
            SubcallTraceCompletionPhase::BeforeFrameEnd => self.frame_stack.last().copied(),
            SubcallTraceCompletionPhase::AfterFrameEnd => self.last_finished_frame.take(),
        };

        let Some(frame_id) = frame_id else {
            self.captured.valid = false;
            return;
        };
        let Some(frame) = self.captured.frames.get_mut(frame_id) else {
            self.captured.valid = false;
            return;
        };
        if frame
            .members
            .iter()
            .any(|member| matches!(member, CapturedMember::SubcallCompletion(_)))
        {
            self.captured.valid = false;
            return;
        }
        if completion.rolled_back_by_completion {
            self.journal_log_count = frame.log_count_before;
        }
        frame
            .members
            .push(CapturedMember::SubcallCompletion(completion));
    }

    fn push_event(&mut self, log: Log) {
        let Some(frame_id) = self.frame_stack.last().copied() else {
            self.captured.valid = false;
            return;
        };
        let event_id = self.captured.events.len();
        self.captured.events.push(CapturedEvent {
            frame: frame_id,
            log,
        });
        if let Some(frame) = self.captured.frames.get_mut(frame_id) {
            frame.members.push(CapturedMember::Event(event_id));
        } else {
            self.captured.valid = false;
        }
    }

    fn sync_journal<CTX: ContextTr>(&mut self, context: &CTX) {
        let logs = context.journal_ref().logs();
        if logs.len() < self.journal_log_count {
            self.journal_log_count = logs.len();
            return;
        }
        let new_logs = logs[self.journal_log_count..].to_vec();
        self.journal_log_count = logs.len();
        for log in new_logs {
            self.push_event(log);
        }
    }

    fn capture_callback_log<CTX: ContextTr>(&mut self, context: &CTX, log: Log) {
        self.push_event(log);
        // Precompile execution reports all of its logs after the frame returns,
        // so the journal may already contain more than the current callback.
        // Those entries are delivered by subsequent callbacks and must not be
        // copied again by `sync_journal`.
        self.journal_log_count = context.journal_ref().logs().len();
    }
}

pub(crate) fn record_subcall_trace_completion(
    inspector: &mut &mut (TracingInspector, ArcEventInspector),
    completion: ArcSubcallTraceCompletion,
) {
    inspector
        .1
        .record_subcall_completion(SubcallTraceCompletion {
            rolled_back_by_completion: completion.child_status.is_ok()
                && !completion.final_status.is_ok(),
            child_status: completion.child_status,
            child_output: completion.child_output,
            child_gas_used: completion.child_gas_used,
            child_gas_limit: completion.child_gas_limit,
            final_status: completion.final_status,
            phase: completion.phase,
        });
}

impl<CTX> Inspector<CTX> for ArcEventInspector
where
    CTX: ContextTr,
{
    fn initialize_interp(&mut self, _interp: &mut Interpreter, context: &mut CTX) {
        self.sync_journal(context);
    }

    fn step_end(&mut self, _interp: &mut Interpreter, context: &mut CTX) {
        self.sync_journal(context);
    }

    fn log(&mut self, context: &mut CTX, log: Log) {
        self.capture_callback_log(context, log);
    }

    fn call(&mut self, context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.sync_journal(context);
        self.push_frame(context.journal_ref().logs().len());
        None
    }

    fn call_end(&mut self, context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.sync_journal(context);
        self.finish_frame(outcome.instruction_result().is_ok());
    }

    fn create(&mut self, context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.sync_journal(context);
        self.push_frame(context.journal_ref().logs().len());
        None
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.sync_journal(context);
        self.finish_frame(outcome.instruction_result().is_ok());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, Bytes, LogData, B256};
    use revm::interpreter::InstructionResult;
    use revm::{context::Context, MainContext};
    use revm_inspectors::tracing::{
        types::{CallKind, CallTrace, CallTraceNode, CallTraceStep},
        CallTraceArena,
    };

    use super::super::convert::{build_debank_traces, DebankTrace};

    fn log(byte: u8) -> Log {
        Log {
            address: Address::repeat_byte(byte),
            data: LogData::empty(),
        }
    }

    fn call_node(
        idx: usize,
        parent: Option<usize>,
        address: Address,
        success: bool,
        status: InstructionResult,
        children: Vec<usize>,
    ) -> CallTraceNode {
        CallTraceNode {
            parent,
            children,
            idx,
            trace: CallTrace {
                success,
                status: Some(status),
                kind: CallKind::Call,
                address,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn call_arena(nodes: Vec<CallTraceNode>) -> CallTraceArena {
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = nodes;
        arena
    }

    fn trace_for_address(traces: &[DebankTrace], address: Address) -> &DebankTrace {
        traces
            .iter()
            .find(|trace| trace.to_addr == address)
            .unwrap()
    }

    #[test]
    fn failed_subtree_does_not_taint_successful_sibling() {
        let arena = call_arena(vec![
            call_node(
                0,
                None,
                Address::repeat_byte(1),
                true,
                InstructionResult::Stop,
                vec![1, 4],
            ),
            call_node(
                1,
                Some(0),
                Address::repeat_byte(2),
                false,
                InstructionResult::Revert,
                vec![2, 3],
            ),
            call_node(
                2,
                Some(1),
                Address::repeat_byte(3),
                true,
                InstructionResult::Stop,
                vec![],
            ),
            call_node(
                3,
                Some(1),
                Address::repeat_byte(4),
                false,
                InstructionResult::OutOfGas,
                vec![],
            ),
            call_node(
                4,
                Some(0),
                Address::repeat_byte(5),
                true,
                InstructionResult::Stop,
                vec![],
            ),
        ]);
        let logs = [log(2), log(3), log(5)];
        let captured = CapturedEvents {
            frames: vec![
                CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![CapturedMember::Call(1), CapturedMember::Call(4)],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![
                        CapturedMember::Event(0),
                        CapturedMember::Call(2),
                        CapturedMember::Call(3),
                    ],
                    success: false,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(1),
                    members: vec![CapturedMember::Event(1)],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(1),
                    members: vec![],
                    success: false,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![CapturedMember::Event(2)],
                    success: true,
                },
            ],
            events: logs
                .iter()
                .cloned()
                .enumerate()
                .map(|(event, log)| CapturedEvent {
                    frame: [1, 2, 4][event],
                    log,
                })
                .collect(),
            valid: true,
        };

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert_eq!(traces.len(), 2);
        assert!(trace_for_address(&traces, Address::repeat_byte(1))
            .error
            .is_empty());
        assert!(trace_for_address(&traces, Address::repeat_byte(5))
            .error
            .is_empty());
        assert_eq!(error_traces.len(), 3);
        assert_eq!(
            trace_for_address(&error_traces, Address::repeat_byte(2)).error,
            "Reverted"
        );
        assert_eq!(
            trace_for_address(&error_traces, Address::repeat_byte(3)).error,
            "parent call failed"
        );
        assert_eq!(
            trace_for_address(&error_traces, Address::repeat_byte(4)).error,
            "Out of gas"
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].contract_id, logs[2].address);
        assert_eq!(
            events[0].parent_trace_id,
            trace_for_address(&traces, Address::repeat_byte(5)).id
        );
        assert_eq!(events[0].idx, 0);
        assert_eq!(error_events.len(), 2);
        assert_eq!(error_events[0].contract_id, logs[0].address);
        assert_eq!(
            error_events[0].parent_trace_id,
            trace_for_address(&error_traces, Address::repeat_byte(2)).id
        );
        assert_eq!(error_events[1].contract_id, logs[1].address);
        assert_eq!(
            error_events[1].parent_trace_id,
            trace_for_address(&error_traces, Address::repeat_byte(3)).id
        );
        assert!(error_events.iter().all(|event| event.idx == 0));
    }

    // Defensive coverage for any arena node that is not attached to its parent. Standard
    // precompiles are attached in the current Arc configuration, so this shape is synthetic.
    #[test]
    fn hidden_failed_frame_taints_visible_descendant() {
        let root = call_node(
            0,
            None,
            Address::repeat_byte(1),
            true,
            InstructionResult::Stop,
            vec![],
        );
        let mut hidden = call_node(
            1,
            Some(0),
            Address::repeat_byte(2),
            false,
            InstructionResult::Revert,
            vec![2],
        );
        hidden.trace.maybe_precompile = Some(true);
        let descendant = call_node(
            2,
            Some(1),
            Address::repeat_byte(3),
            true,
            InstructionResult::Stop,
            vec![],
        );
        let arena = call_arena(vec![root, hidden, descendant]);
        let descendant_log = log(3);
        let captured = CapturedEvents {
            frames: vec![
                CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![CapturedMember::Call(1)],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![CapturedMember::Call(2)],
                    success: false,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(1),
                    members: vec![CapturedMember::Event(0)],
                    success: true,
                },
            ],
            events: vec![CapturedEvent {
                frame: 2,
                log: descendant_log.clone(),
            }],
            valid: true,
        };

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].to_addr, Address::repeat_byte(1));
        assert_eq!(error_traces.len(), 1);
        assert_eq!(error_traces[0].to_addr, Address::repeat_byte(3));
        assert_eq!(error_traces[0].error, "parent call failed");
        assert_eq!(error_traces[0].trace_address, vec![0]);
        assert_eq!(error_traces[0].parent_trace_id, traces[0].id);
        assert!(events.is_empty());
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].contract_id, descendant_log.address);
        assert_eq!(error_events[0].parent_trace_id, error_traces[0].id);
        assert_eq!(error_events[0].idx, 0);
    }

    #[test]
    fn successful_logs_follow_emission_order_and_filter_failed_frames() {
        let mut inspector = ArcEventInspector::default();
        inspector.push_frame(0);
        inspector.push_event(log(1));
        inspector.push_frame(0);
        inspector.push_event(log(2));
        inspector.finish_frame(false);
        inspector.push_event(log(3));
        inspector.finish_frame(true);

        let captured = inspector.into_captured();
        assert!(captured.validate(2).is_ok());
        assert_eq!(captured.successful_logs(), vec![log(1), log(3)]);
    }

    #[test]
    fn incomplete_frame_stack_is_rejected() {
        let mut inspector = ArcEventInspector::default();
        inspector.push_frame(0);
        assert!(inspector.into_captured().validate(1).is_err());
    }

    #[test]
    fn journal_and_callback_logs_are_captured_once_and_keep_duplicates() {
        let mut context = Context::mainnet();
        let mut inspector = ArcEventInspector::default();
        let duplicate = log(1);
        let direct = log(2);
        inspector.push_frame(0);

        // Precompile callbacks arrive after all returned logs have already been
        // appended to the journal.
        context.journal_mut().log(duplicate.clone());
        context.journal_mut().log(duplicate.clone());
        inspector.capture_callback_log(&context, duplicate.clone());
        inspector.capture_callback_log(&context, duplicate.clone());
        inspector.sync_journal(&context);

        // Arc frame-init and SELFDESTRUCT logs have no inspector callback.
        context.journal_mut().log(direct.clone());
        inspector.sync_journal(&context);
        inspector.finish_frame(true);

        let captured = inspector.into_captured();
        assert_eq!(
            captured.successful_logs(),
            vec![duplicate.clone(), duplicate, direct]
        );
        assert_eq!(
            captured.frames[0].members,
            vec![
                CapturedMember::Event(0),
                CapturedMember::Event(1),
                CapturedMember::Event(2),
            ]
        );
    }

    #[test]
    fn builder_flattens_hidden_precompile_events_without_losing_order_or_emitter() {
        let mut arena = CallTraceArena::default();
        arena.nodes_mut()[0] = CallTraceNode {
            children: vec![2],
            trace: CallTrace {
                success: true,
                status: Some(InstructionResult::Return),
                kind: CallKind::Call,
                address: Address::repeat_byte(0x10),
                ..Default::default()
            },
            ..Default::default()
        };
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(0),
            idx: 1,
            trace: CallTrace {
                success: true,
                status: Some(InstructionResult::Return),
                kind: CallKind::Call,
                address: Address::repeat_byte(0x20),
                maybe_precompile: Some(true),
                ..Default::default()
            },
            ..Default::default()
        });
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(0),
            idx: 2,
            trace: CallTrace {
                success: false,
                status: Some(InstructionResult::Revert),
                kind: CallKind::Call,
                address: Address::repeat_byte(0x30),
                ..Default::default()
            },
            ..Default::default()
        });

        let logs = [log(1), log(2), log(3), log(4)];
        let captured = CapturedEvents {
            frames: vec![
                CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![
                        CapturedMember::Event(0),
                        CapturedMember::Call(1),
                        CapturedMember::Call(2),
                        CapturedMember::Event(3),
                    ],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![CapturedMember::Event(1)],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![CapturedMember::Event(2)],
                    success: false,
                },
            ],
            events: logs
                .iter()
                .cloned()
                .enumerate()
                .map(|(frame, log)| CapturedEvent {
                    frame: [0, 1, 2, 0][frame],
                    log,
                })
                .collect(),
            valid: true,
        };

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert_eq!(traces.len(), 1);
        assert_eq!(error_traces.len(), 1);
        assert_eq!(error_traces[0].trace_address, vec![0]);
        assert_eq!(error_traces[0].pos_in_parent_trace, 2);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].contract_id, logs[0].address);
        assert_eq!(events[0].pos_in_parent_trace, 0);
        assert_eq!(events[1].contract_id, logs[1].address);
        assert_eq!(events[1].pos_in_parent_trace, 1);
        assert_eq!(events[2].contract_id, logs[3].address);
        assert_eq!(events[2].pos_in_parent_trace, 3);
        assert_eq!(
            events.iter().map(|event| event.idx).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].contract_id, logs[2].address);
        assert_eq!(error_events[0].idx, 0);
    }

    #[test]
    fn builder_puts_hidden_failure_before_event_and_selfdestruct() {
        let selfdestruct_address = Address::repeat_byte(0x11);
        let refund_target = Address::repeat_byte(0x22);
        let transferred_value = alloy::primitives::U256::from(3);
        let mut arena = CallTraceArena::default();
        arena.nodes_mut()[0] = CallTraceNode {
            trace: CallTrace {
                success: true,
                status: Some(InstructionResult::SelfDestruct),
                kind: CallKind::Call,
                address: Address::repeat_byte(0x10),
                selfdestruct_address: Some(selfdestruct_address),
                selfdestruct_refund_target: Some(refund_target),
                selfdestruct_transferred_value: Some(transferred_value),
                ..Default::default()
            },
            ..Default::default()
        };
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(0),
            idx: 1,
            trace: CallTrace {
                success: false,
                status: Some(InstructionResult::Revert),
                kind: CallKind::Call,
                address: Address::repeat_byte(0x20),
                maybe_precompile: Some(true),
                ..Default::default()
            },
            ..Default::default()
        });

        let failed_log = log(1);
        let root_log = log(2);
        let captured = CapturedEvents {
            frames: vec![
                CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![CapturedMember::Call(1), CapturedMember::Event(1)],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![CapturedMember::Event(0)],
                    success: false,
                },
            ],
            events: vec![
                CapturedEvent {
                    frame: 1,
                    log: failed_log,
                },
                CapturedEvent {
                    frame: 0,
                    log: root_log,
                },
            ],
            valid: true,
        };

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert!(error_traces.is_empty());
        assert_eq!(traces.len(), 2);
        assert_eq!(traces[0].subtraces, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].pos_in_parent_trace, 1);
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].pos_in_parent_trace, 0);
        assert_eq!(error_events[0].idx, 0);

        let suicide = &traces[1];
        assert_eq!(suicide.call_create_type, "suicide");
        assert_eq!(suicide.from_addr, selfdestruct_address);
        assert_eq!(suicide.to_addr, refund_target);
        assert_eq!(suicide.value, transferred_value);
        assert_eq!(suicide.pos_in_parent_trace, 2);
        assert_eq!(suicide.trace_address, vec![0]);
    }

    #[test]
    fn successful_child_storage_change_propagates_through_failed_parent() {
        let mut arena = CallTraceArena::default();
        arena.nodes_mut()[0] = CallTraceNode {
            children: vec![1],
            trace: CallTrace {
                success: false,
                status: Some(InstructionResult::Revert),
                kind: CallKind::Call,
                ..Default::default()
            },
            ..Default::default()
        };
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(0),
            children: vec![2],
            idx: 1,
            trace: CallTrace {
                success: true,
                status: Some(InstructionResult::Return),
                kind: CallKind::Call,
                steps: vec![CallTraceStep {
                    pc: 0,
                    op: revm::bytecode::opcode::OpCode::SSTORE,
                    stack: None,
                    push_stack: None,
                    memory: None,
                    returndata: Default::default(),
                    gas_remaining: 0,
                    gas_refund_counter: 0,
                    state_gas_cost: None,
                    state_gas_reservoir: None,
                    state_gas_spent: 0,
                    gas_used: 0,
                    gas_cost: 0,
                    storage_change: None,
                    status: None,
                    immediate_bytes: None,
                    decoded: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        });
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(1),
            idx: 2,
            trace: CallTrace {
                success: false,
                status: Some(InstructionResult::OutOfGas),
                kind: CallKind::Call,
                ..Default::default()
            },
            ..Default::default()
        });
        let captured = CapturedEvents {
            frames: vec![
                CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![CapturedMember::Call(1)],
                    success: false,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![CapturedMember::Call(2)],
                    success: true,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(1),
                    members: vec![],
                    success: false,
                },
            ],
            events: vec![],
            valid: true,
        };

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert!(traces.is_empty());
        assert!(events.is_empty());
        assert!(error_events.is_empty());
        assert_eq!(error_traces.len(), 3);
        assert_eq!(error_traces[0].error, "Reverted");
        assert_eq!(error_traces[1].error, "parent call failed");
        assert_eq!(error_traces[2].error, "Out of gas");
        assert!(error_traces[0].storage_change);
        assert!(error_traces[1].self_storage_change);
    }

    #[test]
    fn selfdestruct_under_failed_parent_has_parent_error() {
        let refund_target = Address::repeat_byte(0x22);
        let mut arena = CallTraceArena::default();
        arena.nodes_mut()[0] = CallTraceNode {
            children: vec![1],
            trace: CallTrace {
                success: false,
                status: Some(InstructionResult::Revert),
                kind: CallKind::Call,
                ..Default::default()
            },
            ..Default::default()
        };
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(0),
            idx: 1,
            trace: CallTrace {
                success: true,
                status: Some(InstructionResult::SelfDestruct),
                kind: CallKind::Call,
                selfdestruct_address: Some(Address::repeat_byte(0x11)),
                selfdestruct_refund_target: Some(refund_target),
                ..Default::default()
            },
            ..Default::default()
        });
        let captured = CapturedEvents {
            frames: vec![
                CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![CapturedMember::Call(1)],
                    success: false,
                },
                CapturedFrame {
                    log_count_before: 0,
                    parent: Some(0),
                    members: vec![],
                    success: true,
                },
            ],
            events: vec![],
            valid: true,
        };

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert!(traces.is_empty());
        assert!(events.is_empty());
        assert!(error_events.is_empty());
        assert_eq!(error_traces.len(), 3);
        let suicide = error_traces
            .iter()
            .find(|trace| trace.call_create_type == "suicide")
            .unwrap();
        assert_eq!(suicide.to_addr, refund_target);
        assert_eq!(suicide.error, "parent call failed");
    }

    #[test]
    fn completion_rollback_keeps_child_error_event_and_captures_reused_parent_log_slot() {
        let mut context = Context::mainnet();
        let mut inspector = ArcEventInspector::default();
        let reused_log = log(0x42);

        inspector.push_frame(0);
        let checkpoint = context.journal_mut().checkpoint();
        inspector.push_frame(0);
        context.journal_mut().log(reused_log.clone());
        inspector.sync_journal(&context);
        inspector.finish_frame(true);

        context.journal_mut().checkpoint_revert(checkpoint);
        inspector.record_subcall_completion(SubcallTraceCompletion {
            child_status: InstructionResult::Stop,
            child_output: Bytes::new(),
            child_gas_used: 0,
            child_gas_limit: 1,
            final_status: InstructionResult::OutOfGas,
            rolled_back_by_completion: true,

            phase: SubcallTraceCompletionPhase::AfterFrameEnd,
        });

        // The parent emits an identical log into the journal slot that the child
        // occupied before completion rolled it back.
        context.journal_mut().log(reused_log.clone());
        inspector.sync_journal(&context);
        inspector.finish_frame(true);

        let captured = inspector.into_captured();
        assert!(captured.validate(2).is_ok());
        assert_eq!(captured.events.len(), 2);
        assert_eq!(captured.successful_logs(), vec![reused_log.clone()]);

        let mut child = call_node(
            1,
            Some(0),
            Address::repeat_byte(2),
            true,
            InstructionResult::Stop,
            vec![],
        );
        child.trace.steps.push(CallTraceStep {
            pc: 0,
            op: revm::bytecode::opcode::OpCode::SSTORE,
            stack: None,
            push_stack: None,
            memory: None,
            returndata: Default::default(),
            gas_remaining: 0,
            gas_refund_counter: 0,
            state_gas_cost: None,
            state_gas_reservoir: None,
            state_gas_spent: 0,
            gas_used: 0,
            gas_cost: 0,
            storage_change: None,
            status: None,
            immediate_bytes: None,
            decoded: None,
        });
        let arena = call_arena(vec![
            call_node(
                0,
                None,
                Address::repeat_byte(1),
                true,
                InstructionResult::Stop,
                vec![1],
            ),
            child,
        ]);

        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert_eq!(traces.len(), 1);
        assert!(!traces[0].storage_change);
        assert_eq!(error_traces.len(), 1);
        assert_eq!(error_traces[0].error, "parent call failed");
        assert!(error_traces[0].self_storage_change);
        assert!(error_traces[0].storage_change);
        assert_eq!(events.len(), 1);
        assert_eq!(error_events.len(), 1);
        assert_eq!(events[0].contract_id, reused_log.address);
        assert_eq!(error_events[0].contract_id, reused_log.address);
        assert_eq!(events[0].parent_trace_id, traces[0].id);
        assert_eq!(error_events[0].parent_trace_id, error_traces[0].id);
    }

    #[test]
    fn ordinary_child_revert_keeps_error_event_when_parent_reuses_log_slot() {
        let mut context = Context::mainnet();
        let mut inspector = ArcEventInspector::default();
        let reused_log = log(0x43);

        inspector.push_frame(0);
        let checkpoint = context.journal_mut().checkpoint();
        inspector.push_frame(0);
        context.journal_mut().log(reused_log.clone());
        inspector.sync_journal(&context);

        // Ordinary frame rollback happens before call_end. Lowering the journal
        // cursor must not delete the captured log: it remains an error_event.
        context.journal_mut().checkpoint_revert(checkpoint);
        inspector.sync_journal(&context);
        inspector.finish_frame(false);

        context.journal_mut().log(reused_log.clone());
        inspector.sync_journal(&context);
        inspector.finish_frame(true);

        let captured = inspector.into_captured();
        assert!(captured.validate(2).is_ok());
        assert_eq!(captured.events.len(), 2);
        assert_eq!(captured.successful_logs(), vec![reused_log.clone()]);
        assert_eq!(captured.frames[0].members.len(), 2);
        assert_eq!(captured.frames[1].members.len(), 1);
        assert!(!captured.frames[1].effective_success());

        let arena = call_arena(vec![
            call_node(
                0,
                None,
                Address::repeat_byte(1),
                true,
                InstructionResult::Stop,
                vec![1],
            ),
            call_node(
                1,
                Some(0),
                Address::repeat_byte(2),
                false,
                InstructionResult::Revert,
                vec![],
            ),
        ]);
        let (traces, error_traces, events, error_events) = build_debank_traces(
            B256::repeat_byte(0xaa),
            arena,
            captured,
            &std::cell::RefCell::new(0),
        )
        .unwrap();

        assert_eq!(traces.len(), 1);
        assert_eq!(error_traces.len(), 1);
        assert_eq!(error_traces[0].error, "Reverted");
        assert_eq!(events.len(), 1);
        assert_eq!(error_events.len(), 1);
        assert_eq!(events[0].contract_id, reused_log.address);
        assert_eq!(error_events[0].contract_id, reused_log.address);
        assert_eq!(events[0].parent_trace_id, traces[0].id);
        assert_eq!(error_events[0].parent_trace_id, error_traces[0].id);
    }

    #[test]
    fn completion_notifications_bind_to_nested_and_reused_trace_depths() {
        let mut inspector = ArcEventInspector::default();
        let completion = || SubcallTraceCompletion {
            child_status: InstructionResult::Stop,
            child_output: Bytes::new(),
            child_gas_used: 0,
            child_gas_limit: 1,
            final_status: InstructionResult::Return,
            rolled_back_by_completion: false,

            phase: SubcallTraceCompletionPhase::AfterFrameEnd,
        };

        inspector.push_frame(0);
        inspector.push_frame(0);
        inspector.push_frame(0);
        inspector.finish_frame(true);
        inspector.record_subcall_completion(completion());
        inspector.finish_frame(true);
        inspector.record_subcall_completion(completion());

        // A sibling reuses the outer frame's depth after the nested frames finish.
        inspector.push_frame(0);
        inspector.finish_frame(true);
        inspector.record_subcall_completion(completion());
        inspector.finish_frame(true);

        let captured = inspector.into_captured();
        assert!(captured.validate(4).is_ok());
        assert!(captured.frames[0]
            .members
            .iter()
            .all(|member| !matches!(member, CapturedMember::SubcallCompletion(_))));
        for frame_id in [1, 2, 3] {
            assert_eq!(
                captured.frames[frame_id]
                    .members
                    .iter()
                    .filter(|member| matches!(member, CapturedMember::SubcallCompletion(_)))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn subcall_error_classification_is_independent_of_completion_phase() {
        let cases = [
            (
                InstructionResult::Stop,
                InstructionResult::OutOfGas,
                "parent call failed",
            ),
            (
                InstructionResult::Revert,
                InstructionResult::Return,
                "Reverted",
            ),
        ];

        for phase in [
            SubcallTraceCompletionPhase::BeforeFrameEnd,
            SubcallTraceCompletionPhase::AfterFrameEnd,
        ] {
            for (child_status, final_status, expected_error) in cases {
                let observed_status = match phase {
                    SubcallTraceCompletionPhase::BeforeFrameEnd => final_status,
                    SubcallTraceCompletionPhase::AfterFrameEnd => child_status,
                };
                let arena = call_arena(vec![call_node(
                    0,
                    None,
                    Address::repeat_byte(1),
                    observed_status.is_ok(),
                    observed_status,
                    vec![],
                )]);
                let captured = CapturedEvents {
                    frames: vec![CapturedFrame {
                        log_count_before: 0,
                        parent: None,
                        members: vec![CapturedMember::SubcallCompletion(SubcallTraceCompletion {
                            child_status,
                            child_output: Bytes::from_static(b"raw child output"),
                            child_gas_used: 7,
                            child_gas_limit: 11,
                            final_status,
                            rolled_back_by_completion: child_status.is_ok()
                                && !final_status.is_ok(),

                            phase,
                        })],
                        success: observed_status.is_ok(),
                    }],
                    events: vec![],
                    valid: true,
                };

                let mut inspector = TracingInspector::new(Default::default());
                *inspector.traces_mut() = arena.clone();
                captured.apply_subcall_completions(&mut inspector).unwrap();
                let trace = &inspector.traces().nodes()[0].trace;
                assert!(!trace.success);
                assert_eq!(
                    trace.status,
                    Some(if child_status.is_ok() {
                        final_status
                    } else {
                        child_status
                    })
                );
                assert_eq!(trace.output.as_ref(), b"raw child output");
                assert_eq!((trace.gas_used, trace.gas_limit), (7, 11));

                let (traces, error_traces, events, error_events) = build_debank_traces(
                    B256::repeat_byte(0xaa),
                    arena,
                    captured,
                    &std::cell::RefCell::new(0),
                )
                .unwrap();
                assert!(traces.is_empty());
                assert!(events.is_empty());
                assert!(error_events.is_empty());
                assert_eq!(error_traces.len(), 1);
                assert_eq!(error_traces[0].error, expected_error);
                assert_eq!(error_traces[0].output.as_ref(), b"raw child output");
                assert_eq!(error_traces[0].gas_used, 7);
                assert_eq!(error_traces[0].gas_limit, 11);
            }
        }
    }

    #[test]
    fn subcall_trace_uses_raw_child_output_and_gas_in_both_completion_phases() {
        for phase in [
            SubcallTraceCompletionPhase::BeforeFrameEnd,
            SubcallTraceCompletionPhase::AfterFrameEnd,
        ] {
            let mut node = call_node(
                0,
                None,
                Address::repeat_byte(1),
                true,
                InstructionResult::Return,
                vec![],
            );
            node.trace.output = Bytes::from_static(b"wrapped output");
            node.trace.gas_used = 99;
            let arena = call_arena(vec![node]);
            let captured = CapturedEvents {
                frames: vec![CapturedFrame {
                    log_count_before: 0,
                    parent: None,
                    members: vec![CapturedMember::SubcallCompletion(SubcallTraceCompletion {
                        child_status: InstructionResult::Stop,
                        child_output: Bytes::from_static(b"raw child output"),
                        child_gas_used: 7,
                        child_gas_limit: 11,
                        final_status: InstructionResult::Return,
                        rolled_back_by_completion: false,

                        phase,
                    })],
                    success: true,
                }],
                events: vec![],
                valid: true,
            };

            let mut inspector = TracingInspector::new(Default::default());
            *inspector.traces_mut() = arena.clone();
            captured.apply_subcall_completions(&mut inspector).unwrap();
            let trace = &inspector.traces().nodes()[0].trace;
            assert!(trace.success);
            assert_eq!(trace.status, Some(InstructionResult::Stop));
            assert_eq!(trace.output.as_ref(), b"raw child output");
            assert_eq!((trace.gas_used, trace.gas_limit), (7, 11));

            let (traces, error_traces, events, error_events) = build_debank_traces(
                B256::repeat_byte(0xaa),
                arena,
                captured,
                &std::cell::RefCell::new(0),
            )
            .unwrap();
            assert_eq!(traces.len(), 1);
            assert_eq!(traces[0].output.as_ref(), b"raw child output");
            assert_eq!(traces[0].gas_used, 7);
            assert_eq!(traces[0].gas_limit, 11);
            assert!(error_traces.is_empty());
            assert!(events.is_empty());
            assert!(error_events.is_empty());
        }
    }
}
