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
use alloy::primitives::{Address, Bytes, Log, B256 as H256, U256};
use revm_inspectors::tracing::{
    types::{CallKind, CallTraceNode},
    CallTraceArena,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct DebankEvent {
    pub id: String,
    pub contract_id: Address,
    pub selector: String,
    pub topics: Vec<String>,
    pub data: Bytes,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: usize,
    pub idx: usize,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct DebankTrace {
    pub id: String,
    pub from_addr: Address,
    pub gas_limit: u64,
    pub input: Bytes,
    pub to_addr: Address,
    pub value: U256,
    pub gas_used: u64,
    pub output: Bytes,
    pub call_create_type: String,
    pub call_type: String,
    pub tx_id: String,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: usize,
    pub self_storage_change: bool,
    pub storage_change: bool,
    pub subtraces: usize,
    pub trace_address: Vec<usize>,
    pub error: String,
}
trait DebankID {
    fn debank_id(&self) -> String;
}
impl DebankID for DebankEvent {
    fn debank_id(&self) -> String {
        to_hash(&[&self.parent_trace_id, &self.pos_in_parent_trace.to_string()])
    }
}
impl DebankID for DebankTrace {
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
// Trace node conversion: CallTraceNode → DebankTrace
// ---------------------------------------------------------------------------

impl From<&CallTraceNode> for DebankTrace {
    fn from(call_trace: &CallTraceNode) -> Self {
        let trace = &call_trace.trace;
        let (call_create_type, call_type) = debank_call_types(trace.kind);
        let error = trace.status.and_then(fmt_error_msg);
        let mut debank_trace = Self {
            from_addr: trace.caller,
            gas_limit: trace.gas_limit,
            input: trace.data.clone(),
            to_addr: trace.address,
            value: trace.value,
            gas_used: trace.gas_used,
            output: trace.output.clone(),
            call_create_type: call_create_type.to_string(),
            call_type: call_type.to_string(),
            subtraces: call_trace.children.len(),
            error: error.unwrap_or_default(),
            ..Default::default()
        };
        for op in &trace.steps {
            if op.op == revm::bytecode::opcode::OpCode::SSTORE {
                debank_trace.self_storage_change = true;
                debank_trace.storage_change = true;
                break;
            }
        }
        debank_trace
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

impl From<&Log> for DebankEvent {
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
            contract_id: log.address,
            selector,
            topics,
            data: log.data.data.clone(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Trace tree building
// ---------------------------------------------------------------------------

// Trace 节点比 Event 大 ~376 vs ~168 bytes; 整树只在一笔 tx 内构建后即消费，
// 不进入持久存储，不 Box 以避免热路径上的一次 heap 分配。
#[allow(clippy::large_enum_variant)]
enum DebankTraceOrLog {
    Trace(DebankTraceNode),
    Log { event: DebankEvent, success: bool },
}

const PARENT_CALL_FAILED_ERROR: &str = "parent call failed";

struct DebankTraceNode {
    trace: DebankTrace,
    children: Vec<DebankTraceOrLog>,
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
) -> Result<DebankTraceNode, String> {
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
    let mut trace: DebankTrace = node.into();
    if let Some(completion) = frame.subcall_completion() {
        trace.output = completion.child_output.clone();
        trace.gas_used = completion.child_gas_used;
        trace.gas_limit = completion.child_gas_limit;
    }
    if let Some(status) = frame.raw_failure_status() {
        trace.error = fmt_error_msg(status).unwrap_or_default();
    } else if frame.completion_rolled_back() {
        trace.error = PARENT_CALL_FAILED_ERROR.to_string();
    }
    if (!parent_success || !effective_frame_success) && trace.error.is_empty() {
        trace.error = PARENT_CALL_FAILED_ERROR.to_string();
    }
    let mut debank_node = DebankTraceNode {
        trace,
        children: Vec::new(),
        frame_success: raw_frame_success,
        effective_frame_success,
        success: effective_frame_success && parent_success,
    };
    debank_node.trace.trace_address = trace_address.clone();
    debank_node.trace.parent_trace_id = parent_trace_id.to_owned();
    debank_node.trace.pos_in_parent_trace = pos_in_parent_trace;
    debank_node.trace.tx_id = tx_id.to_owned();
    debank_node.trace.id = debank_node.trace.debank_id();

    let id = debank_node.trace.id.clone();
    let mut next_trace_index = 0;
    append_captured_members(
        &mut debank_node,
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
    debank_node.trace.subtraces = debank_node
        .children
        .iter()
        .filter(|child| matches!(child, DebankTraceOrLog::Trace(_)))
        .count();

    // selfdestruct handling
    if node.is_selfdestruct() {
        let mut selfdestruct_ta = trace_address;
        selfdestruct_ta.push(next_trace_index);
        debank_node.trace.subtraces += 1;
        let mut selfdestruct_trace = DebankTrace {
            from_addr: node.trace.selfdestruct_address.unwrap_or_default(),
            to_addr: node.trace.selfdestruct_refund_target.unwrap_or_default(),
            value: node
                .trace
                .selfdestruct_transferred_value
                .unwrap_or_default(),
            trace_address: selfdestruct_ta,
            parent_trace_id: id,
            pos_in_parent_trace: debank_node.children.len(),
            tx_id: tx_id.to_owned(),
            call_create_type: "suicide".to_string(),
            ..Default::default()
        };
        if debank_node.frame_success && !debank_node.success {
            selfdestruct_trace.error = PARENT_CALL_FAILED_ERROR.to_string();
        }
        selfdestruct_trace.id = selfdestruct_trace.debank_id();
        debank_node
            .children
            .push(DebankTraceOrLog::Trace(DebankTraceNode {
                trace: selfdestruct_trace,
                children: vec![],
                frame_success: debank_node.frame_success,
                effective_frame_success: debank_node.effective_frame_success,
                success: debank_node.success,
            }));
    }
    Ok(debank_node)
}

#[allow(clippy::too_many_arguments)]
fn append_captured_members(
    debank_node: &mut DebankTraceNode,
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
                        debank_node.children.len(),
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
                        debank_node.trace.storage_change = true;
                    }
                    debank_node
                        .children
                        .push(DebankTraceOrLog::Trace(child_trace));
                } else {
                    append_captured_members(
                        debank_node,
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
                let mut event: DebankEvent = (&captured_event.log).into();
                event.parent_trace_id = parent_trace_id.to_string();
                event.pos_in_parent_trace = debank_node.children.len();
                event.id = event.debank_id();
                if frame_success {
                    event.idx = *log_index;
                    *log_index += 1;
                }
                debank_node.children.push(DebankTraceOrLog::Log {
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
    node: DebankTraceNode,
    traces: &mut Vec<DebankTrace>,
    error_traces: &mut Vec<DebankTrace>,
    events: &mut Vec<DebankEvent>,
    error_events: &mut Vec<DebankEvent>,
) {
    if node.success {
        traces.push(node.trace);
    } else {
        error_traces.push(node.trace);
    }
    for child in node.children {
        match child {
            DebankTraceOrLog::Trace(trace) => {
                finish_build_traces(trace, traces, error_traces, events, error_events);
            }
            DebankTraceOrLog::Log { event, success } => {
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
pub(crate) type DebankTraceResult = (
    Vec<DebankTrace>,
    Vec<DebankTrace>,
    Vec<DebankEvent>,
    Vec<DebankEvent>,
);

pub(crate) fn build_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
    captured: CapturedEvents,
    log_index: &mut usize,
) -> Result<DebankTraceResult, String> {
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

impl From<DebankTrace> for PipelineTrace {
    fn from(t: DebankTrace) -> Self {
        Self {
            id: t.id,
            from_addr: addr_hex(&t.from_addr),
            gas_limit: t.gas_limit as i64,
            input: t.input,
            to_addr: addr_hex(&t.to_addr),
            value: t.value,
            gas_used: t.gas_used as i64,
            output: t.output,
            call_create_type: t.call_create_type,
            call_type: t.call_type,
            tx_id: t.tx_id,
            parent_trace_id: t.parent_trace_id,
            pos_in_parent_trace: t.pos_in_parent_trace as i64,
            self_storage_change: t.self_storage_change,
            storage_change: t.storage_change,
            subtraces: t.subtraces as i64,
            trace_address: t.trace_address.into_iter().map(|v| v as i64).collect(),
            error: t.error,
        }
    }
}
impl From<DebankEvent> for PipelineEvent {
    fn from(e: DebankEvent) -> Self {
        Self {
            id: e.id,
            contract_id: addr_hex(&e.contract_id),
            selector: e.selector,
            topics: Some(e.topics),
            data: e.data,
            parent_trace_id: e.parent_trace_id,
            pos_in_parent_trace: e.pos_in_parent_trace as i64,
            idx: e.idx as i64,
        }
    }
}
pub(crate) fn append(file: &mut BlockFile, result: DebankTraceResult) {
    let (traces, errors, events, error_events) = result;
    file.traces.extend(traces.into_iter().map(Into::into));
    file.error_traces.extend(errors.into_iter().map(Into::into));
    file.events.extend(events.into_iter().map(Into::into));
    file.error_events
        .extend(error_events.into_iter().map(Into::into));
}

pub(super) fn validate_transaction_classification(
    tx_success: bool,
    traces: &[DebankTrace],
    error_traces: &[DebankTrace],
    events: &[DebankEvent],
    error_events: &[DebankEvent],
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
