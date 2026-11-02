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

//! Arc blockfile conversion, ported from arc-node 830ad37 debank_trace.rs.
use super::inspector::{CapturedEvents, CapturedMember};
use crate::pipeline::types::{addr_hex, to_hash, BlockFile, PipelineEvent, PipelineTrace};
use alloy::primitives::{Log, B256 as H256};
use revm_inspectors::tracing::{
    types::{CallKind, CallTraceNode},
    CallTraceArena,
};

trait DebankID {
    fn debank_id(&self) -> String;
}
impl DebankID for PipelineEvent {
    fn debank_id(&self) -> String {
        to_hash(&[&self.parent_trace_id, &self.pos_in_parent_trace.to_string()])
    }
}
impl DebankID for PipelineTrace {
    fn debank_id(&self) -> String {
        to_hash(&[
            &self.tx_id,
            &self.parent_trace_id,
            &self.pos_in_parent_trace.to_string(),
        ])
    }
}
pub(crate) fn fmt_error_msg(res: revm::interpreter::InstructionResult) -> Option<String> {
    use revm::interpreter::InstructionResult;
    if res.is_ok() {
        return None;
    }
    let msg = match res {
        InstructionResult::Revert => "Reverted".to_string(),
        InstructionResult::OutOfGas
        | InstructionResult::PrecompileOOG
        | InstructionResult::MemoryOOG
        | InstructionResult::MemoryLimitOOG
        | InstructionResult::InvalidOperandOOG
        | InstructionResult::ReentrancySentryOOG => "Out of gas".to_string(),
        InstructionResult::OutOfFunds => "Insufficient balance for transfer".to_string(),
        InstructionResult::OpcodeNotFound | InstructionResult::InvalidFEOpcode => {
            "Bad instruction".to_string()
        }
        InstructionResult::StackOverflow => "Out of stack".to_string(),
        InstructionResult::InvalidJump => "Bad jump destination".to_string(),
        InstructionResult::PrecompileError => "Built-in failed".to_string(),
        status => format!("{status:?}"),
    };
    Some(msg)
}

// ---------------------------------------------------------------------------
// Trace node conversion: CallTraceNode → PipelineTrace
// ---------------------------------------------------------------------------

impl From<&CallTraceNode> for PipelineTrace {
    fn from(call_trace: &CallTraceNode) -> Self {
        let trace = &call_trace.trace;
        let (call_create_type, call_type) = debank_call_types(trace.kind);
        let error = trace.status.and_then(fmt_error_msg);
        let mut converted = Self {
            from_addr: addr_hex(&trace.caller),
            gas_limit: trace.gas_limit as i64,
            input: trace.data.clone(),
            to_addr: addr_hex(&trace.address),
            value: trace.value,
            gas_used: trace.gas_used as i64,
            output: trace.output.clone(),
            call_create_type: call_create_type.to_string(),
            call_type: call_type.to_string(),
            subtraces: call_trace.children.len() as i64,
            error: error.unwrap_or_default(),
            ..Default::default()
        };
        for op in &trace.steps {
            if op.op == revm::bytecode::opcode::OpCode::SSTORE {
                converted.self_storage_change = true;
                converted.storage_change = true;
                break;
            }
        }
        converted
    }
}

fn debank_call_types(kind: CallKind) -> (&'static str, &'static str) {
    match kind {
        CallKind::Call => ("call", "call"),
        CallKind::StaticCall => ("call", "staticcall"),
        CallKind::CallCode => ("call", "callcode"),
        CallKind::DelegateCall => ("call", "delegatecall"),
        CallKind::AuthCall => ("call", "authcall"),
        CallKind::Create | CallKind::Create2 => ("create", ""),
    }
}

impl From<&Log> for PipelineEvent {
    fn from(log: &Log) -> Self {
        let selector = log
            .topics()
            .first()
            .map(|h| h.to_string())
            .unwrap_or_default();
        let topics = if log.topics().len() > 1 {
            log.topics()[1..].iter().map(|h| h.to_string()).collect()
        } else {
            vec![]
        };
        Self {
            contract_id: addr_hex(&log.address),
            selector,
            topics: Some(topics),
            data: log.data.data.clone(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Trace tree building
// ---------------------------------------------------------------------------

// 整树只在一笔 tx 内构建后即消费，
// 不进入持久存储，不 Box 以避免热路径上的一次 heap 分配。
#[allow(clippy::large_enum_variant)]
enum PipelineTraceOrLog {
    Trace(PipelineTraceNode),
    Log { event: PipelineEvent, success: bool },
}

const PARENT_CALL_FAILED_ERROR: &str = "parent call failed";

struct PipelineTraceNode {
    trace: PipelineTrace,
    children: Vec<PipelineTraceOrLog>,
    frame_success: bool,
    effective_frame_success: bool,
    success: bool,
}

#[allow(clippy::too_many_arguments)]
fn build_trace_node(
    tx_id: &str,
    parent_trace_id: &str,
    pos_in_parent_trace: usize,
    node_id: usize,
    nodes: &[CallTraceNode],
    captured: &CapturedEvents,
    visible: &[bool],
    parent_success: bool,
    trace_address: Vec<usize>,
    log_index: &mut usize,
) -> Result<PipelineTraceNode, String> {
    let node = nodes
        .get(node_id)
        .ok_or_else(|| format!("missing trace node {node_id}"))?;
    let frame = captured
        .frames
        .get(node_id)
        .ok_or_else(|| format!("missing captured frame {node_id}"))?;
    if node.trace.success != frame.success {
        return Err(format!(
            "trace and event inspectors disagree on frame {node_id} success"
        ));
    }

    let raw_frame_success = frame.raw_success();
    let effective_frame_success = frame.effective_success();
    let mut trace: PipelineTrace = node.into();
    if let Some(completion) = frame.subcall_completion() {
        trace.output = completion.child_output.clone();
        trace.gas_used = completion.child_gas_used as i64;
        trace.gas_limit = completion.child_gas_limit as i64;
    }
    if let Some(status) = frame.raw_failure_status() {
        trace.error = fmt_error_msg(status).unwrap_or_default();
    } else if frame.completion_rolled_back() {
        trace.error = PARENT_CALL_FAILED_ERROR.to_string();
    }
    if (!parent_success || !effective_frame_success) && trace.error.is_empty() {
        trace.error = PARENT_CALL_FAILED_ERROR.to_string();
    }
    let mut trace_node = PipelineTraceNode {
        trace,
        children: Vec::new(),
        frame_success: raw_frame_success,
        effective_frame_success,
        success: effective_frame_success && parent_success,
    };
    trace_node.trace.trace_address = trace_address.iter().map(|index| *index as i64).collect();
    trace_node.trace.parent_trace_id = parent_trace_id.to_owned();
    trace_node.trace.pos_in_parent_trace = pos_in_parent_trace as i64;
    trace_node.trace.tx_id = tx_id.to_owned();
    trace_node.trace.id = trace_node.trace.debank_id();

    let id = trace_node.trace.id.clone();
    let mut next_trace_index = 0;
    append_captured_members(
        &mut trace_node,
        node_id,
        tx_id,
        &id,
        nodes,
        captured,
        visible,
        parent_success,
        &trace_address,
        &mut next_trace_index,
        log_index,
    )?;
    trace_node.trace.subtraces = trace_node
        .children
        .iter()
        .filter(|child| matches!(child, PipelineTraceOrLog::Trace(_)))
        .count() as i64;

    // selfdestruct handling
    if node.is_selfdestruct() {
        let mut selfdestruct_ta = trace_address;
        selfdestruct_ta.push(next_trace_index);
        trace_node.trace.subtraces += 1;
        let mut selfdestruct_trace = PipelineTrace {
            from_addr: addr_hex(&node.trace.selfdestruct_address.unwrap_or_default()),
            to_addr: addr_hex(&node.trace.selfdestruct_refund_target.unwrap_or_default()),
            value: node
                .trace
                .selfdestruct_transferred_value
                .unwrap_or_default(),
            trace_address: selfdestruct_ta
                .into_iter()
                .map(|index| index as i64)
                .collect(),
            parent_trace_id: id,
            pos_in_parent_trace: trace_node.children.len() as i64,
            tx_id: tx_id.to_owned(),
            call_create_type: "suicide".to_string(),
            ..Default::default()
        };
        if trace_node.frame_success && !trace_node.success {
            selfdestruct_trace.error = PARENT_CALL_FAILED_ERROR.to_string();
        }
        selfdestruct_trace.id = selfdestruct_trace.debank_id();
        trace_node
            .children
            .push(PipelineTraceOrLog::Trace(PipelineTraceNode {
                trace: selfdestruct_trace,
                children: vec![],
                frame_success: trace_node.frame_success,
                effective_frame_success: trace_node.effective_frame_success,
                success: trace_node.success,
            }));
    }
    Ok(trace_node)
}

#[allow(clippy::too_many_arguments)]
fn append_captured_members(
    trace_node: &mut PipelineTraceNode,
    frame_id: usize,
    tx_id: &str,
    parent_trace_id: &str,
    nodes: &[CallTraceNode],
    captured: &CapturedEvents,
    visible: &[bool],
    parent_success: bool,
    trace_address: &[usize],
    next_trace_index: &mut usize,
    log_index: &mut usize,
) -> Result<(), String> {
    let frame = captured
        .frames
        .get(frame_id)
        .ok_or_else(|| format!("missing captured frame {frame_id}"))?;
    let frame_success = parent_success && frame.effective_success();

    for member in &frame.members {
        match member {
            CapturedMember::Call(child_id) => {
                if visible.get(*child_id).copied().unwrap_or(false) {
                    let mut child_trace_address = trace_address.to_vec();
                    child_trace_address.push(*next_trace_index);
                    *next_trace_index += 1;
                    let child_trace = build_trace_node(
                        tx_id,
                        parent_trace_id,
                        trace_node.children.len(),
                        *child_id,
                        nodes,
                        captured,
                        visible,
                        frame_success,
                        child_trace_address,
                        log_index,
                    )?;
                    // `storage_change` describes what this frame executed. Keep
                    // propagating a successful child's SSTORE even if an ancestor
                    // later reverts the transaction.
                    if child_trace.trace.storage_change && child_trace.effective_frame_success {
                        trace_node.trace.storage_change = true;
                    }
                    trace_node
                        .children
                        .push(PipelineTraceOrLog::Trace(child_trace));
                } else {
                    append_captured_members(
                        trace_node,
                        *child_id,
                        tx_id,
                        parent_trace_id,
                        nodes,
                        captured,
                        visible,
                        frame_success,
                        trace_address,
                        next_trace_index,
                        log_index,
                    )?;
                }
            }
            CapturedMember::Event(event_id) => {
                let captured_event = captured
                    .events
                    .get(*event_id)
                    .ok_or_else(|| format!("missing captured event {event_id}"))?;
                if captured_event.frame != frame_id {
                    return Err(format!(
                        "captured event {event_id} belongs to frame {}, expected {frame_id}",
                        captured_event.frame
                    ));
                }
                let mut event: PipelineEvent = (&captured_event.log).into();
                event.parent_trace_id = parent_trace_id.to_string();
                event.pos_in_parent_trace = trace_node.children.len() as i64;
                event.id = event.debank_id();
                if frame_success {
                    event.idx = *log_index as i64;
                    *log_index += 1;
                }
                trace_node.children.push(PipelineTraceOrLog::Log {
                    event,
                    success: frame_success,
                });
            }
            CapturedMember::SubcallCompletion(_) => {}
        }
    }
    Ok(())
}

fn finish_build_traces(
    node: PipelineTraceNode,
    traces: &mut Vec<PipelineTrace>,
    error_traces: &mut Vec<PipelineTrace>,
    events: &mut Vec<PipelineEvent>,
    error_events: &mut Vec<PipelineEvent>,
) {
    if node.success {
        traces.push(node.trace);
    } else {
        error_traces.push(node.trace);
    }
    for child in node.children {
        match child {
            PipelineTraceOrLog::Trace(trace) => {
                finish_build_traces(trace, traces, error_traces, events, error_events);
            }
            PipelineTraceOrLog::Log { event, success } => {
                if success {
                    events.push(event);
                } else {
                    error_events.push(event);
                }
            }
        }
    }
}

/// Build DeBank traces and events from a revm `CallTraceArena`.
///
/// Returns `(traces, error_traces, events, error_events)`.
pub(crate) type PipelineTraceResult = (
    Vec<PipelineTrace>,
    Vec<PipelineTrace>,
    Vec<PipelineEvent>,
    Vec<PipelineEvent>,
);

pub(crate) fn build_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
    captured: CapturedEvents,
    log_index: &mut usize,
) -> Result<PipelineTraceResult, String> {
    let nodes = traces.into_nodes();
    captured.validate(nodes.len())?;
    if nodes.is_empty() {
        return Ok((vec![], vec![], vec![], vec![]));
    }
    let mut visible = vec![false; nodes.len()];
    visible[0] = true;
    for node in &nodes {
        for child in &node.children {
            if let Some(is_visible) = visible.get_mut(*child) {
                *is_visible = true;
            }
        }
    }
    let tx_id = tx_id.to_string();
    let top = build_trace_node(
        &tx_id,
        "",
        0,
        0,
        &nodes,
        &captured,
        &visible,
        true,
        vec![],
        log_index,
    )?;
    let mut traces = vec![];
    let mut error_traces = vec![];
    let mut events = vec![];
    let mut error_events = vec![];
    finish_build_traces(
        top,
        &mut traces,
        &mut error_traces,
        &mut events,
        &mut error_events,
    );
    Ok((traces, error_traces, events, error_events))
}

pub(crate) fn append(file: &mut BlockFile, result: PipelineTraceResult) {
    let (traces, errors, events, error_events) = result;
    file.traces.extend(traces);
    file.error_traces.extend(errors);
    file.events.extend(events);
    file.error_events.extend(error_events);
}

pub(super) fn validate_transaction_classification(
    tx_success: bool,
    traces: &[PipelineTrace],
    error_traces: &[PipelineTrace],
    events: &[PipelineEvent],
    error_events: &[PipelineEvent],
) -> Result<(), &'static str> {
    if traces.iter().any(|trace| !trace.error.is_empty()) {
        return Err("successful trace has an error");
    }
    if error_traces.iter().any(|trace| trace.error.is_empty()) {
        return Err("error trace has no error");
    }
    if error_events.iter().any(|event| event.idx != 0) {
        return Err("error event has a non-zero index");
    }

    let success_roots = traces
        .iter()
        .filter(|trace| trace.parent_trace_id.is_empty())
        .count();
    let error_roots = error_traces
        .iter()
        .filter(|trace| trace.parent_trace_id.is_empty())
        .count();
    if tx_success {
        if success_roots != 1 || error_roots != 0 {
            return Err("successful receipt does not have exactly one successful root trace");
        }
    } else if success_roots != 0 || error_roots != 1 || !traces.is_empty() || !events.is_empty() {
        return Err("failed receipt has successful traces or events");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, LogData};

    #[test]
    fn event_without_topics_keeps_arc_array_encoding() {
        let log = Log {
            address: Address::repeat_byte(0xab),
            data: LogData::empty(),
        };
        let event: PipelineEvent = (&log).into();
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["contract_id"], addr_hex(&log.address));
        assert_eq!(json["selector"], "");
        assert_eq!(json["topics"], serde_json::json!([]));
    }

    #[test]
    fn trace_numbers_keep_go_signed_integer_encoding() {
        let mut node = CallTraceNode::default();
        node.trace.caller = Address::repeat_byte(0xab);
        node.trace.address = Address::repeat_byte(0xcd);
        node.trace.gas_limit = u64::MAX;
        node.trace.gas_used = u64::MAX;
        let trace: PipelineTrace = (&node).into();
        let json = serde_json::to_value(trace).unwrap();
        assert_eq!(json["from_addr"], addr_hex(&node.trace.caller));
        assert_eq!(json["to_addr"], addr_hex(&node.trace.address));
        assert_eq!(json["gas_limit"], -1);
        assert_eq!(json["gas_used"], -1);
    }
}
