use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{
    AccountOverride, BlockOverrides, Bytecode, DebankEvent, DebankID, DebankTrace, Header,
    StateOverride, H256, U256,
};
use revm::context::BlockEnv;
use revm::database::{CacheDB, DatabaseRef};
use revm::primitives::Address;
use revm::state::{Account, AccountStatus, EvmStorageSlot};
use revm::{Database, DatabaseCommit};
use revm_inspectors::tracing::types::{CallTraceNode, TraceMemberOrder};
use revm_inspectors::tracing::CallTraceArena;
use std::collections::HashMap;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// Applies the given block overrides to the [`CacheDB`] and [`BlockEnv`].
///
/// When `overrides.number` is greater than the current `env.number`, ensures that
/// `block_hash[number - 1]` is set (defaults to `current_block_hash` if not provided),
/// and returns `Some(hash)` as the parent block hash for EIP-2935 system call.
pub fn apply_block_overrides<DB>(
    mut overrides: BlockOverrides,
    db: &mut CacheDB<DB>,
    env: &mut BlockEnv,
    mut latest_header: Header,
) -> Option<Header> {
    let mut header = None;

    if let Some(number) = overrides.number {
        if number > env.number {
            let number_u64: u64 = number.saturating_to();
            let block_hashes = overrides.block_hash.get_or_insert_with(Default::default);
            block_hashes
                .entry(number_u64 - 1)
                .or_insert(latest_header.parent_hash);
            block_hashes.entry(number_u64).or_insert(latest_header.hash);
            latest_header.number = number_u64;
            header = Some(latest_header);
        }
    }

    let BlockOverrides {
        number,
        difficulty,
        time,
        gas_limit,
        coinbase,
        random,
        base_fee,
        block_hash,
        blob_base_fee: _,
        beacon_root: _,
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

    header
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
        info: info.clone(),
        original_info: Box::new(info),
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

/// Spawns a blocking task with automatic cancellation handling.
///
/// 1. Internally initializes a `CancellationToken` and a `DropGuard`.
/// 2. Triggers cancellation automatically if the returned Future is dropped.
/// 3. Provides the token to the closure to allow for internal cancellation checks.
pub async fn spawn_blocking_with_cancel<F, R>(task: F) -> Result<R, JoinError>
where
    F: FnOnce(CancellationToken) -> R + Send + 'static,
    R: Send + 'static,
{
    let token = CancellationToken::new();

    let _guard = token.clone().drop_guard();

    tokio::task::spawn_blocking(move || task(token)).await
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::{atomic, Arc};
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_normal_execution() {
        let result = spawn_blocking_with_cancel(|_token| {
            std::thread::sleep(Duration::from_millis(10));
            42
        })
        .await
        .expect("Task failed");

        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_spawn_blocking_with_cancel() {
        let val = Arc::new(AtomicU64::new(0));
        let val_clone = val.clone();
        let _ = timeout(
            Duration::from_millis(50),
            spawn_blocking_with_cancel(move |token| {
                for _ in 0..10 {
                    println!(
                        "val: {}, canceled: {}",
                        val_clone.load(atomic::Ordering::Relaxed),
                        token.is_cancelled()
                    );
                    if token.is_cancelled() {
                        return;
                    }
                    val_clone.fetch_add(1, atomic::Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }),
        )
        .await;
        assert_eq!(val.load(atomic::Ordering::SeqCst), 5);
    }
}
