use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{
    AccountOverride, BlockOverrides, Bytecode, DebankEvent, DebankID, DebankTrace, StateOverride,
    H256, U256,
};
use revm::context::BlockEnv;
use revm::database::{CacheDB, DatabaseRef};
use revm::primitives::Address;
use revm::state::{Account, AccountStatus, EvmStorageSlot};
use revm::{Database, DatabaseCommit};
use revm_inspectors::tracing::types::{CallTraceNode, TraceMemberOrder};
use revm_inspectors::tracing::CallTraceArena;
use std::collections::HashMap;

pub fn apply_block_overrides<DB>(
    overrides: BlockOverrides,
    db: &mut CacheDB<DB>,
    env: &mut BlockEnv,
) {
    let BlockOverrides {
        number,
        difficulty,
        time,
        gas_limit,
        coinbase,
        random,
        base_fee,
        block_hash,
    } = overrides;

    if let Some(block_hashes) = block_hash {
        // override block hashes
        db.cache.block_hashes.extend(
            block_hashes
                .into_iter()
                .map(|(num, hash)| (U256::from(num), hash)),
        )
    }

    if let Some(number) = number {
        env.number = number.saturating_to();
    }
    if let Some(difficulty) = difficulty {
        env.difficulty = difficulty;
    }
    if let Some(time) = time {
        env.timestamp = U256::from(time);
    }
    if let Some(gas_limit) = gas_limit {
        env.gas_limit = gas_limit;
    }
    if let Some(coinbase) = coinbase {
        env.beneficiary = coinbase;
    }
    if let Some(random) = random {
        env.prevrandao = Some(random);
    }
    if let Some(base_fee) = base_fee {
        env.basefee = base_fee.saturating_to();
    }
}

/// Applies the given state overrides (a set of [`AccountOverride`]) to the [`CacheDB`].
pub fn apply_state_overrides<DB>(overrides: StateOverride, db: &mut CacheDB<DB>) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    for (account, account_overrides) in overrides {
        apply_account_override(account, account_overrides, db)?;
    }
    Ok(())
}

/// Applies a single [`AccountOverride`] to the [`CacheDB`].
fn apply_account_override<DB>(
    account: Address,
    account_override: AccountOverride,
    db: &mut CacheDB<DB>,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    let mut info = db
        .basic(account)
        .map_err(|_| internal_rpc_err("Failed to get basic account info"))?
        .unwrap_or_default();

    if let Some(nonce) = account_override.nonce {
        info.nonce = nonce;
    }
    if let Some(code) = account_override.code {
        info.code = Some(
            Bytecode::new_raw_checked(code)
                .map_err(|err| invalid_params_rpc_err(format!("Invalid bytecode {}", err)))?,
        );
    }
    if let Some(balance) = account_override.balance {
        info.balance = balance;
    }

    // Create a new account marked as touched
    let mut acc = Account {
        info,
        status: AccountStatus::Touched,
        storage: HashMap::default(),
        transaction_id: 0,
    };

    let storage_diff = match (account_override.state, account_override.state_diff) {
        (Some(_), Some(_)) => {
            return Err(invalid_params_rpc_err(format!(
                "account {:?} has both 'state' and 'stateDiff'",
                account
            )))
        }
        (None, None) => None,
        // If we need to override the entire state, we firstly mark account as destroyed to clear
        // its storage, and then we mark it is "NewlyCreated" to make sure that old storage won't be
        // used.
        (Some(state), None) => {
            // Destroy the account to ensure that its storage is cleared
            db.commit(HashMap::from_iter([(
                account,
                Account {
                    status: AccountStatus::SelfDestructed | AccountStatus::Touched,
                    ..Default::default()
                },
            )]));
            // Mark the account as created to ensure that old storage is not read
            acc.mark_created();
            Some(state)
        }
        (None, Some(state)) => Some(state),
    };

    if let Some(state) = storage_diff {
        for (slot, value) in state {
            acc.storage.insert(
                slot.into(),
                EvmStorageSlot {
                    // we use inverted value here to ensure that storage is treated as changed
                    original_value: (!value).into(),
                    present_value: value.into(),
                    transaction_id: 0,
                    is_cold: false,
                },
            );
        }
    }

    db.commit(HashMap::from_iter([(account, acc)]));

    Ok(())
}

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
    nodes: &Vec<CallTraceNode>,
) -> DebankTraceNode {
    let mut debank_node = DebankTraceNode {
        trace: node.into(),
        children: Vec::new(),
    };

    debank_node.trace.parent_trace_id = parent_trace_id;
    debank_node.trace.pos_in_parent_trace = pos_in_parent_trace;
    debank_node.trace.tx_id = tx_id;
    debank_node.trace.id = debank_node.trace.debank_id();

    let id = debank_node.trace.id.clone();
    let contract_id = node.execution_address();

    for pos in node.ordering.iter() {
        match &pos {
            TraceMemberOrder::Call(i) => {
                let child_node = &nodes[node.children[*i]];
                if !child_node.trace.success {
                    continue;
                }
                let child_trace = build_trace_node(
                    tx_id,
                    id.clone(),
                    debank_node.children.len(),
                    child_node,
                    nodes,
                );
                if child_trace.trace.storage_change {
                    debank_node.trace.storage_change = true;
                }
                debank_node
                    .children
                    .push(DebankTraceOrLog::Trace(child_trace));
            }
            TraceMemberOrder::Log(i) => {
                let mut child_event: DebankEvent = (&node.logs[*i]).into();
                child_event.pos_in_parent_trace = debank_node.children.len();
                child_event.contract_id = contract_id;
                child_event.tx_id = tx_id;
                child_event.parent_trace_id = id.clone();
                child_event.id = child_event.debank_id();
                debank_node
                    .children
                    .push(DebankTraceOrLog::Log(child_event));
            }
            _ => {}
        }
    }
    // selfdestructs are not recorded as individual call traces but are derived from
    // the call trace and are added as additional `TransactionTrace` objects in the
    // trace array
    if node.is_selfdestruct() {
        let mut selfdestruct_trace = DebankTrace {
            from_addr: node.trace.selfdestruct_address.unwrap_or_default(),
            to_addr: node.trace.selfdestruct_refund_target.unwrap_or_default(),
            value: node
                .trace
                .selfdestruct_transferred_value
                .unwrap_or_default(),
            parent_trace_id: id.clone(),
            pos_in_parent_trace: debank_node.children.len(),
            tx_id,
            call_create_type: "suicide".to_string(),
            ..Default::default()
        };
        selfdestruct_trace.id = selfdestruct_trace.debank_id();
        debank_node
            .children
            .push(DebankTraceOrLog::Trace(DebankTraceNode {
                trace: selfdestruct_trace,
                children: vec![],
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
    for child in node.children.iter_mut() {
        match child {
            DebankTraceOrLog::Trace(trace) => {
                trace.trace.parent_trace_id = node.trace.id.clone();
                finish_build_traces(trace, traces, events);
            }
            DebankTraceOrLog::Log(log) => {
                events.push(log.clone());
            }
        }
    }
}

pub(crate) fn build_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
) -> (Vec<DebankTrace>, Vec<DebankEvent>) {
    let nodes = traces.into_nodes();
    if nodes.is_empty() {
        return (vec![], vec![]);
    }
    let mut top = build_trace_node(tx_id, "".to_string(), 0, &nodes[0], &nodes);
    let mut traces = vec![];
    let mut events = vec![];
    finish_build_traces(&mut top, &mut traces, &mut events);
    (traces, events)
}
