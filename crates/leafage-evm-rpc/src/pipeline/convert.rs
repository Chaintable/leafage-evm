//! Flatten a revm-inspectors call arena into pipeline traces and events.
//!
//! Based on the reth-x `build_debank_traces` conversion, but aligned with the
//! Go `callTracer` of `github.com/Chaintable/pipeline` (v0.0.69) where the two
//! disagree:
//! - events under a failed frame get `idx = 0`; successful events take the
//!   block-wide receipt log index;
//! - a SELFDESTRUCT frame is the last child of its parent and its
//!   `trace_address` is `parent + [number of earlier call children]`, also
//!   when the parent made no other call;
//! - output order follows `addTraceAndLog`: a frame's descendants first, then
//!   its own events, then its direct child traces;
//! - a log without topics has `topics = null`.

use super::types::{addr_hex, hash_hex, to_hash, BlockFile, PipelineEvent, PipelineTrace};
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::sol_types::{Panic, Revert, SolError};
use revm::interpreter::InstructionResult;
use revm_inspectors::tracing::{
    types::{CallKind, CallTraceNode, TraceMemberOrder},
    CallTraceArena,
};
use std::collections::BTreeSet;

const PARENT_CALL_FAILED: &str = "parent call failed";

enum Member {
    Call(usize),
    Log(usize),
}

struct Frame {
    kind: FrameKind,
    from: Address,
    /// `None` for a failed CREATE: Go clears `To` on error.
    to: Option<Address>,
    value: U256,
    gas: u64,
    gas_used: u64,
    input: Bytes,
    output: Bytes,
    error: String,
    failed: bool,
    parent_failed: bool,
    self_storage_change: bool,
    storage_change: bool,
    pos_in_parent: usize,
    id: String,
    calls: Vec<Frame>,
    logs: Vec<PipelineEvent>,
    order: Vec<Member>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Call(CallKind),
    Create,
    Selfdestruct,
}

/// Append the traces and events of one transaction to `block_file`.
///
/// `log_index` is the block-wide index of the next successful log; contracts
/// whose storage an SSTORE touched are added to `storage_contracts`.
/// `sstore[i]` tells whether arena node `i` ran an SSTORE.
pub(crate) fn append_tx_traces(
    tx_hash: &B256,
    arena: &CallTraceArena,
    sstore: &[bool],
    log_index: &mut u64,
    storage_contracts: &mut BTreeSet<Address>,
    block_file: &mut BlockFile,
) {
    let nodes = arena.nodes();
    if nodes.is_empty() {
        return;
    }
    let tx_id = hash_hex(tx_hash);
    let mut top = build_frame(nodes, sstore, 0);
    set_parent_failed(&mut top, false);
    assign_log_index(&mut top, log_index);
    set_storage_change(&mut top, storage_contracts);

    top.id = to_hash(&[&tx_id, "", "0"]);
    let top_trace = to_trace(&top, &tx_id, "", Vec::new());
    if top.failed {
        block_file.error_traces.push(top_trace);
    } else {
        block_file.traces.push(top_trace);
    }
    add_trace_and_log(&mut top, &tx_id, &[], block_file);
}

fn build_frame(nodes: &[CallTraceNode], sstore: &[bool], idx: usize) -> Frame {
    let node = &nodes[idx];
    let trace = &node.trace;
    let failed = !trace.success;
    let kind = if trace.kind.is_any_create() {
        FrameKind::Create
    } else {
        FrameKind::Call(trace.kind)
    };
    let is_revert = trace.status == Some(InstructionResult::Revert);
    let error = if failed {
        error_message(trace.status, &trace.output)
    } else {
        String::new()
    };
    // Go keeps the output of successful frames and of reverts only.
    let output = if !failed || is_revert {
        trace.output.clone()
    } else {
        Bytes::new()
    };
    let to = if failed && kind == FrameKind::Create {
        None
    } else {
        Some(trace.address)
    };
    let self_storage_change = sstore.get(idx).copied().unwrap_or(false);
    // geth burns the whole frame gas on any failure but a revert, except
    // for the checks done before the frame runs. revm leaves some halts
    // (e.g. the SSTORE reentrancy sentry) with gas unspent.
    let burns_all_gas = failed
        && !is_revert
        && !matches!(
            trace.status,
            Some(
                InstructionResult::CallTooDeep
                    | InstructionResult::OutOfFunds
                    | InstructionResult::NonceOverflow
            )
        );
    let gas_used = if burns_all_gas {
        trace.gas_limit
    } else {
        trace.gas_used
    };

    let mut frame = Frame {
        kind,
        from: trace.caller,
        to,
        value: trace.value,
        gas: trace.gas_limit,
        gas_used,
        input: trace.data.clone(),
        output,
        error,
        failed,
        parent_failed: false,
        self_storage_change,
        storage_change: self_storage_change,
        pos_in_parent: 0,
        id: String::new(),
        calls: Vec::new(),
        logs: Vec::new(),
        order: Vec::new(),
    };

    let contract = addr_hex(&node.execution_address());
    for member in &node.ordering {
        let pos = frame.calls.len() + frame.logs.len();
        match member {
            TraceMemberOrder::Call(i) => {
                let mut child = build_frame(nodes, sstore, node.children[*i]);
                child.pos_in_parent = pos;
                frame.order.push(Member::Call(frame.calls.len()));
                frame.calls.push(child);
            }
            TraceMemberOrder::Log(i) => {
                let raw = &node.logs[*i].raw_log;
                let topics = raw.topics();
                frame.order.push(Member::Log(frame.logs.len()));
                frame.logs.push(PipelineEvent {
                    id: String::new(),
                    contract_id: contract.clone(),
                    selector: topics.first().map(hash_hex).unwrap_or_default(),
                    topics: (!topics.is_empty())
                        .then(|| topics[1..].iter().map(hash_hex).collect()),
                    data: raw.data.clone(),
                    parent_trace_id: String::new(),
                    pos_in_parent_trace: pos as i64,
                    idx: 0,
                });
            }
            TraceMemberOrder::Step(_) => {}
        }
    }

    // SELFDESTRUCT ends the frame, so it is always the last member.
    if node.is_selfdestruct() {
        let pos = frame.calls.len() + frame.logs.len();
        frame.order.push(Member::Call(frame.calls.len()));
        frame.calls.push(Frame {
            kind: FrameKind::Selfdestruct,
            from: trace.selfdestruct_address.unwrap_or_default(),
            to: Some(trace.selfdestruct_refund_target.unwrap_or_default()),
            value: trace.selfdestruct_transferred_value.unwrap_or_default(),
            gas: 0,
            gas_used: 0,
            input: Bytes::new(),
            output: Bytes::new(),
            error: String::new(),
            failed: false,
            parent_failed: false,
            self_storage_change: false,
            storage_change: false,
            pos_in_parent: pos,
            id: String::new(),
            calls: Vec::new(),
            logs: Vec::new(),
            order: Vec::new(),
        });
    }
    frame
}

/// `setParentFailed`.
fn set_parent_failed(frame: &mut Frame, parent_failed: bool) {
    frame.parent_failed = parent_failed;
    let failed = frame.failed || parent_failed;
    for child in &mut frame.calls {
        set_parent_failed(child, failed);
    }
}

/// Go `StateDB.AddLog` numbers logs block-wide and a revert gives the index
/// back, so the logs that survive are numbered consecutively in execution
/// order; `addTraceAndLog` zeroes the rest.
fn assign_log_index(frame: &mut Frame, log_index: &mut u64) {
    let dropped = frame.failed || frame.parent_failed;
    for member in &frame.order {
        match member {
            Member::Call(i) => assign_log_index(&mut frame.calls[*i], log_index),
            Member::Log(i) => {
                if !dropped {
                    frame.logs[*i].idx = *log_index as i64;
                    *log_index += 1;
                }
            }
        }
    }
}

/// `setStorageChange`.
fn set_storage_change(frame: &mut Frame, storage_contracts: &mut BTreeSet<Address>) {
    if frame.self_storage_change {
        if let Some(to) = frame.to {
            if frame.kind == FrameKind::Call(CallKind::DelegateCall) {
                storage_contracts.insert(frame.from);
            } else {
                storage_contracts.insert(to);
            }
        }
    }
    let mut child_changed = false;
    for child in &mut frame.calls {
        set_storage_change(child, storage_contracts);
        if child.storage_change && !child.failed {
            child_changed = true;
        }
    }
    if child_changed {
        frame.storage_change = true;
    }
}

/// `addTraceAndLog`.
fn add_trace_and_log(
    frame: &mut Frame,
    tx_id: &str,
    trace_address: &[i64],
    block_file: &mut BlockFile,
) {
    let parent_id = frame.id.clone();
    for (i, child) in frame.calls.iter_mut().enumerate() {
        child.id = to_hash(&[tx_id, &parent_id, &child.pos_in_parent.to_string()]);
        add_trace_and_log(child, tx_id, &child_address(trace_address, i), block_file);
    }
    let dropped = frame.failed || frame.parent_failed;
    for log in &mut frame.logs {
        log.parent_trace_id = parent_id.clone();
        log.id = to_hash(&[&parent_id, &log.pos_in_parent_trace.to_string()]);
        if dropped {
            block_file.error_events.push(log.clone());
        } else {
            block_file.events.push(log.clone());
        }
    }
    for (i, child) in frame.calls.iter().enumerate() {
        let trace = to_trace(child, tx_id, &parent_id, child_address(trace_address, i));
        if child.failed || child.parent_failed {
            block_file.error_traces.push(trace);
        } else {
            block_file.traces.push(trace);
        }
    }
}

fn child_address(trace_address: &[i64], i: usize) -> Vec<i64> {
    let mut child = Vec::with_capacity(trace_address.len() + 1);
    child.extend_from_slice(trace_address);
    child.push(i as i64);
    child
}

/// `callTracer.ToTrace`.
fn to_trace(frame: &Frame, tx_id: &str, parent_id: &str, trace_address: Vec<i64>) -> PipelineTrace {
    let (call_create_type, call_type) = match frame.kind {
        FrameKind::Create => ("create", ""),
        FrameKind::Selfdestruct => ("suicide", ""),
        FrameKind::Call(kind) => ("call", call_type(kind)),
    };
    let error = if frame.failed {
        frame.error.clone()
    } else if frame.parent_failed {
        PARENT_CALL_FAILED.to_string()
    } else {
        String::new()
    };
    PipelineTrace {
        id: frame.id.clone(),
        from_addr: addr_hex(&frame.from),
        // Go converts with big.NewInt(int64(gas)).
        gas_limit: frame.gas as i64,
        input: frame.input.clone(),
        to_addr: addr_hex(&frame.to.unwrap_or_default()),
        value: frame.value,
        gas_used: frame.gas_used as i64,
        output: frame.output.clone(),
        call_create_type: call_create_type.to_string(),
        call_type: call_type.to_string(),
        tx_id: tx_id.to_string(),
        parent_trace_id: parent_id.to_string(),
        pos_in_parent_trace: frame.pos_in_parent as i64,
        self_storage_change: frame.self_storage_change,
        storage_change: frame.storage_change,
        subtraces: frame.calls.len() as i64,
        trace_address,
        error,
    }
}

fn call_type(kind: CallKind) -> &'static str {
    match kind {
        CallKind::StaticCall => "staticcall",
        CallKind::CallCode => "callcode",
        CallKind::DelegateCall => "delegatecall",
        _ => "call",
    }
}

/// Approximation of the go-ethereum VM error text; exact wording is not part
/// of the comparison.
fn error_message(status: Option<InstructionResult>, output: &Bytes) -> String {
    let Some(status) = status else {
        return "execution failed".to_string();
    };
    match status {
        InstructionResult::Revert => match revert_reason(output) {
            Some(reason) => format!("execution reverted: {reason}"),
            None => "execution reverted".to_string(),
        },
        InstructionResult::OutOfGas
        | InstructionResult::MemoryOOG
        | InstructionResult::MemoryLimitOOG
        | InstructionResult::PrecompileOOG
        | InstructionResult::InvalidOperandOOG => "out of gas".to_string(),
        InstructionResult::ReentrancySentryOOG => {
            "out of gas: not enough gas for reentrancy sentry".to_string()
        }
        InstructionResult::OpcodeNotFound | InstructionResult::InvalidFEOpcode => {
            "invalid opcode".to_string()
        }
        InstructionResult::InvalidJump => "invalid jump destination".to_string(),
        InstructionResult::CallTooDeep => "max call depth exceeded".to_string(),
        InstructionResult::OutOfFunds => "insufficient balance for transfer".to_string(),
        InstructionResult::StateChangeDuringStaticCall => "write protection".to_string(),
        InstructionResult::CreateCollision => "contract address collision".to_string(),
        InstructionResult::CreateContractSizeLimit => "max code size exceeded".to_string(),
        InstructionResult::CreateInitCodeSizeLimit => "max initcode size exceeded".to_string(),
        InstructionResult::CreateContractStartingWithEF => {
            "invalid code: must not begin with 0xef".to_string()
        }
        InstructionResult::StackUnderflow => "stack underflow".to_string(),
        InstructionResult::StackOverflow => "stack limit reached 1024".to_string(),
        InstructionResult::OutOfOffset => "return data out of bounds".to_string(),
        InstructionResult::NonceOverflow => "nonce uint64 overflow".to_string(),
        // Not a frame result, or one go-ethereum has no text for.
        _ => "execution failed".to_string(),
    }
}

/// `abi.UnpackRevert`: `Error(string)` or a `Panic(uint256)` description.
fn revert_reason(output: &Bytes) -> Option<String> {
    if output.len() < 4 {
        return None;
    }
    if let Ok(revert) = Revert::abi_decode(output) {
        return Some(revert.reason);
    }
    let panic = Panic::abi_decode(output).ok()?;
    let code: u64 = panic.code.try_into().unwrap_or(u64::MAX);
    Some(
        match code {
            0x00 => "generic panic",
            0x01 => "assert(false)",
            0x11 => "arithmetic underflow or overflow",
            0x12 => "division or modulo by zero",
            0x21 => "enum overflow",
            0x22 => "invalid encoded storage byte array accessed",
            0x31 => "out-of-bounds array access; popping on an empty array",
            0x32 => "out-of-bounds access of an array or bytesN",
            0x41 => "out of memory",
            0x51 => "uninitialized function",
            _ => return Some(format!("unknown panic code: {:#x}", panic.code)),
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256, LogData};
    use revm_inspectors::tracing::types::{CallLog, CallTrace};

    const TX: B256 = b256!("00000000000000000000000000000000000000000000000000000000000000aa");

    fn node(idx: usize, parent: Option<usize>, kind: CallKind, success: bool) -> CallTraceNode {
        CallTraceNode {
            parent,
            idx,
            trace: CallTrace {
                kind,
                success,
                status: Some(if success {
                    InstructionResult::Return
                } else {
                    InstructionResult::Revert
                }),
                caller: address!("00000000000000000000000000000000000000c0"),
                address: Address::with_last_byte(idx as u8 + 1),
                gas_limit: 100,
                gas_used: 10,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn attach(nodes: &mut [CallTraceNode], parent: usize, child: usize) {
        let position = nodes[parent].children.len();
        nodes[parent]
            .ordering
            .push(TraceMemberOrder::Call(position));
        nodes[parent].children.push(child);
    }

    fn log(nodes: &mut [CallTraceNode], at: usize, topics: usize) {
        let topics = (0..topics).map(|i| B256::with_last_byte(i as u8)).collect();
        let position = nodes[at].logs.len();
        nodes[at].ordering.push(TraceMemberOrder::Log(position));
        nodes[at].logs.push(CallLog {
            raw_log: LogData::new_unchecked(topics, Bytes::new()),
            ..Default::default()
        });
    }

    fn arena(nodes: Vec<CallTraceNode>) -> CallTraceArena {
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = nodes;
        arena
    }

    fn run(nodes: Vec<CallTraceNode>, log_index: &mut u64) -> BlockFile {
        let mut file = BlockFile::default();
        let mut contracts = BTreeSet::new();
        append_tx_traces(
            &TX,
            &arena(nodes),
            &[],
            log_index,
            &mut contracts,
            &mut file,
        );
        file
    }

    #[test]
    fn event_index_is_block_wide_and_zero_when_dropped() {
        // root logs, calls a reverting child that logs, then logs again.
        let mut nodes = vec![
            node(0, None, CallKind::Call, true),
            node(1, Some(0), CallKind::Call, false),
        ];
        log(&mut nodes, 0, 1);
        attach(&mut nodes, 0, 1);
        log(&mut nodes, 1, 2);
        log(&mut nodes, 0, 0);

        let mut log_index = 5;
        let file = run(nodes, &mut log_index);
        assert_eq!(log_index, 7);
        let idx: Vec<_> = file
            .events
            .iter()
            .map(|e| (e.pos_in_parent_trace, e.idx))
            .collect();
        assert_eq!(idx, vec![(0, 5), (2, 6)]);
        assert_eq!(file.error_events.len(), 1);
        assert_eq!(file.error_events[0].idx, 0);
        assert!(file.events[1].topics.is_none());
        assert_eq!(file.events[0].topics, Some(vec![]));
        assert_eq!(file.error_events[0].topics.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn selfdestruct_without_calls_gets_child_address_zero() {
        let mut nodes = vec![node(0, None, CallKind::Call, true)];
        nodes[0].trace.status = Some(InstructionResult::SelfDestruct);
        nodes[0].trace.selfdestruct_address = Some(Address::with_last_byte(1));
        nodes[0].trace.selfdestruct_refund_target = Some(Address::with_last_byte(9));
        nodes[0].trace.selfdestruct_transferred_value = Some(U256::from(7));
        log(&mut nodes, 0, 1);

        let file = run(nodes, &mut 0);
        assert_eq!(file.traces.len(), 2);
        let suicide = &file.traces[1];
        assert_eq!(suicide.call_create_type, "suicide");
        assert_eq!(suicide.trace_address, vec![0]);
        assert_eq!(suicide.pos_in_parent_trace, 1);
        assert_eq!(suicide.value, U256::from(7));
        assert_eq!(suicide.to_addr, addr_hex(&Address::with_last_byte(9)));
        assert_eq!(file.traces[0].subtraces, 1);
    }

    #[test]
    fn output_order_follows_add_trace_and_log() {
        // root -> a -> a1, root -> b
        let mut nodes = vec![
            node(0, None, CallKind::Call, true),
            node(1, Some(0), CallKind::Call, true),
            node(2, Some(1), CallKind::StaticCall, true),
            node(3, Some(0), CallKind::DelegateCall, true),
        ];
        attach(&mut nodes, 0, 1);
        attach(&mut nodes, 1, 2);
        attach(&mut nodes, 0, 3);

        let file = run(nodes, &mut 0);
        let addresses: Vec<_> = file
            .traces
            .iter()
            .map(|t| t.trace_address.clone())
            .collect();
        assert_eq!(addresses, vec![vec![], vec![0, 0], vec![0], vec![1]]);
        assert_eq!(file.traces[1].call_type, "staticcall");
        assert_eq!(file.traces[3].call_type, "delegatecall");
        // ids chain through the parent id and the position.
        assert_eq!(file.traces[0].id, to_hash(&[&hash_hex(&TX), "", "0"]));
        assert_eq!(
            file.traces[2].id,
            to_hash(&[&hash_hex(&TX), &file.traces[0].id, "0"])
        );
        assert_eq!(file.traces[1].parent_trace_id, file.traces[2].id);
    }

    #[test]
    fn children_of_failed_frames_are_error_traces() {
        let mut nodes = vec![
            node(0, None, CallKind::Call, true),
            node(1, Some(0), CallKind::Call, false),
            node(2, Some(1), CallKind::Call, true),
        ];
        attach(&mut nodes, 0, 1);
        attach(&mut nodes, 1, 2);

        let file = run(nodes, &mut 0);
        assert_eq!(file.traces.len(), 1);
        assert_eq!(file.error_traces.len(), 2);
        let grandchild = file
            .error_traces
            .iter()
            .find(|t| t.trace_address == vec![0, 0])
            .unwrap();
        assert_eq!(grandchild.error, PARENT_CALL_FAILED);
        let child = file
            .error_traces
            .iter()
            .find(|t| t.trace_address == vec![0])
            .unwrap();
        assert_eq!(child.error, "execution reverted");
    }

    #[test]
    fn storage_contracts_use_the_storage_context() {
        let mut nodes = vec![
            node(0, None, CallKind::Call, true),
            node(1, Some(0), CallKind::DelegateCall, true),
        ];
        nodes[1].trace.caller = Address::with_last_byte(0x42);
        attach(&mut nodes, 0, 1);

        let mut file = BlockFile::default();
        let mut contracts = BTreeSet::new();
        append_tx_traces(
            &TX,
            &arena(nodes),
            &[false, true],
            &mut 0,
            &mut contracts,
            &mut file,
        );
        assert_eq!(
            contracts.into_iter().collect::<Vec<_>>(),
            vec![Address::with_last_byte(0x42)]
        );
        assert!(file.traces[0].storage_change);
        assert!(!file.traces[0].self_storage_change);
    }
}
