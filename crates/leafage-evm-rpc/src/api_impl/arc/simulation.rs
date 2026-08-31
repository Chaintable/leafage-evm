use leafage_evm_types::{DebankEvent, DebankID, DebankTrace, H256};
use revm::primitives::Address;
use revm_inspectors::tracing::types::{CallKind, CallTraceNode, TraceMemberOrder};
use revm_inspectors::tracing::CallTraceArena;

enum DebankTraceOrLog {
    Trace(DebankTraceNode),
    Log(DebankEvent),
}

struct DebankTraceNode {
    trace: DebankTrace,
    children: Vec<DebankTraceOrLog>,
}

fn build_trace_node(
    tx_id: H256,
    parent_trace_id: String,
    pos_in_parent_trace: usize,
    node: &CallTraceNode,
    nodes: &[CallTraceNode],
    log_emitters: &[Address],
) -> DebankTraceNode {
    let mut debank_node = DebankTraceNode {
        trace: node.into(),
        children: Vec::new(),
    };

    if node.is_selfdestruct() {
        debank_node.trace.call_create_type = match node.trace.kind {
            CallKind::Call
            | CallKind::StaticCall
            | CallKind::CallCode
            | CallKind::DelegateCall
            | CallKind::AuthCall => "call".to_string(),
            CallKind::Create | CallKind::Create2 => "create".to_string(),
        };
    }

    debank_node.trace.parent_trace_id = parent_trace_id;
    debank_node.trace.pos_in_parent_trace = pos_in_parent_trace;
    debank_node.trace.tx_id = tx_id;
    debank_node.trace.id = debank_node.trace.debank_id();

    let id = debank_node.trace.id.clone();
    let contract_id = node.execution_address();

    for member in &node.ordering {
        match member {
            TraceMemberOrder::Call(index) => {
                let child_node = &nodes[node.children[*index]];
                if !child_node.trace.success {
                    continue;
                }
                let child_trace = build_trace_node(
                    tx_id,
                    id.clone(),
                    debank_node.children.len(),
                    child_node,
                    nodes,
                    log_emitters,
                );
                if child_trace.trace.storage_change {
                    debank_node.trace.storage_change = true;
                }
                debank_node
                    .children
                    .push(DebankTraceOrLog::Trace(child_trace));
            }
            TraceMemberOrder::Log(index) => {
                let log = &node.logs[*index];
                let mut event: DebankEvent = log.into();
                event.pos_in_parent_trace = debank_node.children.len();
                event.contract_id = usize::try_from(log.index)
                    .ok()
                    .and_then(|index| log_emitters.get(index))
                    .copied()
                    .unwrap_or_else(|| {
                        metrics::counter!("leafage_arc_log_emitter_sidecar_miss_total")
                            .increment(1);
                        tracing::error!(
                            tx_id = %tx_id,
                            log_index = log.index,
                            frame_address = %contract_id,
                            "missing Arc log emitter; falling back to the frame address"
                        );
                        contract_id
                    });
                event.tx_id = tx_id;
                event.parent_trace_id = id.clone();
                event.id = event.debank_id();
                debank_node.children.push(DebankTraceOrLog::Log(event));
            }
            _ => {}
        }
    }

    if node.is_selfdestruct() {
        let mut trace = DebankTrace {
            from_addr: node.trace.selfdestruct_address.unwrap_or_default(),
            to_addr: node.trace.selfdestruct_refund_target.unwrap_or_default(),
            value: node
                .trace
                .selfdestruct_transferred_value
                .unwrap_or_default(),
            parent_trace_id: id,
            pos_in_parent_trace: debank_node.children.len(),
            tx_id,
            call_create_type: "suicide".to_string(),
            ..Default::default()
        };
        trace.id = trace.debank_id();
        debank_node
            .children
            .push(DebankTraceOrLog::Trace(DebankTraceNode {
                trace,
                children: Vec::new(),
            }));
    }

    debank_node
}

fn finish_build_traces(
    node: &mut DebankTraceNode,
    traces: &mut Vec<DebankTrace>,
    events: &mut Vec<DebankEvent>,
) {
    traces.push(node.trace.clone());
    for child in &mut node.children {
        match child {
            DebankTraceOrLog::Trace(trace) => {
                trace.trace.parent_trace_id = node.trace.id.clone();
                finish_build_traces(trace, traces, events);
            }
            DebankTraceOrLog::Log(log) => events.push(log.clone()),
        }
    }
}

pub(super) fn build_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
    log_emitters: &[Address],
) -> (Vec<DebankTrace>, Vec<DebankEvent>) {
    let nodes = traces.into_nodes();
    if nodes.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut top = build_trace_node(tx_id, String::new(), 0, &nodes[0], &nodes, log_emitters);
    let mut traces = Vec::new();
    let mut events = Vec::new();
    finish_build_traces(&mut top, &mut traces, &mut events);
    (traces, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::Bytes;
    use revm::primitives::Log;
    use revm_inspectors::tracing::types::CallLog;

    fn trace_arena_with_logs(frame: Address, logs: &[(Address, u64)]) -> CallTraceArena {
        let mut traces = CallTraceArena::default();
        let root = &mut traces.nodes_mut()[0];
        root.trace.address = frame;
        root.trace.success = true;
        for (emitter, index) in logs {
            root.logs.push(
                CallLog::from(Log::new_unchecked(*emitter, Vec::new(), Bytes::new()))
                    .with_index(*index),
            );
            root.ordering
                .push(TraceMemberOrder::Log(root.logs.len() - 1));
        }
        traces
    }

    #[test]
    fn log_emitter_sidecar_uses_global_index_and_falls_back_to_frame() {
        let frame = Address::with_last_byte(1);
        let first_emitter = Address::with_last_byte(2);
        let second_emitter = Address::with_last_byte(3);
        let mut emitters = vec![Address::ZERO; 4];
        emitters[1] = first_emitter;
        emitters[3] = second_emitter;
        let logs = [(first_emitter, 1), (second_emitter, 3)];

        let (_, events) =
            build_debank_traces(H256::ZERO, trace_arena_with_logs(frame, &logs), &emitters);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].contract_id, first_emitter);
        assert_eq!(events[1].contract_id, second_emitter);

        let (_, events) = build_debank_traces(
            H256::ZERO,
            trace_arena_with_logs(frame, &logs),
            &emitters[..2],
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].contract_id, first_emitter);
        assert_eq!(events[1].contract_id, frame);
    }
}
