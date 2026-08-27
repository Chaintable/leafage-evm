use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{
    AccountInfo, AccountOverride, BlockOverrides, Bytecode, DebankEvent, DebankID, DebankTrace,
    Header, StateOverride, H256, U256,
};
use revm::context::BlockEnv;
use revm::database::{CacheDB, DatabaseRef};
use revm::primitives::{keccak256, Address};
use revm::state::{Account, AccountStatus, EvmStorageSlot};
use revm::{Database, DatabaseCommit};
use revm_inspectors::tracing::types::{CallTraceNode, TraceMemberOrder};
use revm_inspectors::tracing::CallTraceArena;
use std::cell::RefCell;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// The pseudo token address debank clients use to query the chain's
/// native token through ERC20-shaped calls. Parsed once instead of per
/// request on the multicall hot path.
pub(crate) static NATIVE_TOKEN_SENTINEL: LazyLock<Address> = LazyLock::new(|| {
    Address::from_str("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap()
});

/// Adapter exposing revm's caching `Database` methods (`&mut self`)
/// through `DatabaseRef`, so repeated reads inside one RPC request —
/// across the calls of a multicall, or the re-executions of an
/// estimateGas binary search — hit this request-local cache instead of
/// re-walking the layered state (keccak + diff layers + shared cache)
/// every time. Single-threaded by design: it lives inside one blocking
/// task, which the `RefCell` makes explicit.
pub(crate) struct RequestCacheDB<DB: DatabaseRef>(RefCell<CacheDB<DB>>);

impl<DB: DatabaseRef> RequestCacheDB<DB> {
    pub(crate) fn new(db: CacheDB<DB>) -> Self {
        Self(RefCell::new(db))
    }
}

impl<DB: DatabaseRef> std::fmt::Debug for RequestCacheDB<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestCacheDB").finish_non_exhaustive()
    }
}

impl<DB: DatabaseRef> DatabaseRef for RequestCacheDB<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.borrow_mut().basic(address)
    }

    fn code_by_hash_ref(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        self.0.borrow_mut().code_by_hash(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.0.borrow_mut().storage(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<H256, Self::Error> {
        self.0.borrow_mut().block_hash(number)
    }
}


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
    apply_state_overrides_with_policy(overrides, db, StateOverrideErrorPolicy::Leafage)
}

/// Applies state overrides with Reth-compatible error messages. This is kept
/// separate from the public helper so existing non-Arc RPC behavior is stable.
pub(crate) fn apply_state_overrides_reth<DB>(
    overrides: StateOverride,
    db: &mut CacheDB<DB>,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    apply_state_overrides_with_policy(overrides, db, StateOverrideErrorPolicy::Reth)
}

/// Applies Arc's state semantics while preserving DeBank's existing error
/// messages for custom RPC methods.
pub(crate) fn apply_state_overrides_arc_debank<DB>(
    overrides: StateOverride,
    db: &mut CacheDB<DB>,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    apply_state_overrides_with_policy(overrides, db, StateOverrideErrorPolicy::ArcDebank)
}

#[derive(Clone, Copy)]
enum StateOverrideErrorPolicy {
    Leafage,
    ArcDebank,
    Reth,
}

fn apply_state_overrides_with_policy<DB>(
    overrides: StateOverride,
    db: &mut CacheDB<DB>,
    error_policy: StateOverrideErrorPolicy,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    for (account, account_overrides) in overrides {
        apply_account_override(account, account_overrides, db, error_policy)?;
    }
    Ok(())
}

/// Applies a single [`AccountOverride`] to the [`CacheDB`].
fn apply_account_override<DB>(
    account: Address,
    account_override: AccountOverride,
    db: &mut CacheDB<DB>,
    error_policy: StateOverrideErrorPolicy,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    let mut info = db
        .basic(account)
        .map_err(|error| match error_policy {
            StateOverrideErrorPolicy::Leafage | StateOverrideErrorPolicy::ArcDebank => {
                internal_rpc_err("Failed to get basic account info")
            }
            StateOverrideErrorPolicy::Reth => internal_rpc_err(error.to_string()),
        })?
        .unwrap_or_default();

    if let Some(nonce) = account_override.nonce {
        info.nonce = nonce;
    }
    if let Some(code) = account_override.code {
        if !matches!(error_policy, StateOverrideErrorPolicy::Leafage) {
            info.code_hash = keccak256(&code);
        }
        info.code = Some(Bytecode::new_raw_checked(code).map_err(|error| {
            let message = match error_policy {
                StateOverrideErrorPolicy::Leafage | StateOverrideErrorPolicy::ArcDebank => {
                    format!("Invalid bytecode {error}")
                }
                StateOverrideErrorPolicy::Reth => format!("Invalid bytecode: {error}"),
            };
            invalid_params_rpc_err(message)
        })?);
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
        (None, Some(state)) => {
            // revm 36: empty+touched accounts are cleared by EIP-161 on commit.
            // Mark as Created so State::commit() preserves the stateDiff storage
            // instead of discarding it via touch_empty_eip161().
            if acc.info.is_empty() && !state.is_empty() {
                acc.mark_created();
            }
            Some(state)
        }
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
    arc_semantics: bool,
    arc_log_emitters: Option<&[Address]>,
) -> DebankTraceNode {
    let mut debank_node = DebankTraceNode {
        trace: node.into(),
        children: Vec::new(),
    };

    // Arc exposes SELFDESTRUCT as an action beneath the executing frame. The
    // shared converter preserves Leafage's historical behavior for other
    // chains by turning such a frame itself into `suicide`; restore the Arc
    // frame type here before appending the action as a child below.
    if arc_semantics && node.is_selfdestruct() {
        debank_node.trace.call_create_type = match node.trace.kind {
            revm_inspectors::tracing::types::CallKind::Call
            | revm_inspectors::tracing::types::CallKind::StaticCall
            | revm_inspectors::tracing::types::CallKind::CallCode
            | revm_inspectors::tracing::types::CallKind::DelegateCall
            | revm_inspectors::tracing::types::CallKind::AuthCall => "call".to_string(),
            revm_inspectors::tracing::types::CallKind::Create
            | revm_inspectors::tracing::types::CallKind::Create2 => "create".to_string(),
        };
    }

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
                    arc_semantics,
                    arc_log_emitters,
                );
                if child_trace.trace.storage_change {
                    debank_node.trace.storage_change = true;
                }
                debank_node
                    .children
                    .push(DebankTraceOrLog::Trace(child_trace));
            }
            TraceMemberOrder::Log(i) => {
                let log = &node.logs[*i];
                let mut child_event: DebankEvent = log.into();
                child_event.pos_in_parent_trace = debank_node.children.len();
                child_event.contract_id = if arc_semantics {
                    usize::try_from(log.index)
                        .ok()
                        .and_then(|index| arc_log_emitters.and_then(|emitters| emitters.get(index)))
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
                        })
                } else {
                    contract_id
                };
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

    if arc_semantics && node.is_selfdestruct() {
        let mut selfdestruct_trace = DebankTrace {
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
        selfdestruct_trace.id = selfdestruct_trace.debank_id();
        debank_node
            .children
            .push(DebankTraceOrLog::Trace(DebankTraceNode {
                trace: selfdestruct_trace,
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
    build_debank_traces_with_semantics(tx_id, traces, false, None)
}

pub(crate) fn build_arc_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
    log_emitters: &[Address],
) -> (Vec<DebankTrace>, Vec<DebankEvent>) {
    build_debank_traces_with_semantics(tx_id, traces, true, Some(log_emitters))
}

fn build_debank_traces_with_semantics(
    tx_id: H256,
    traces: CallTraceArena,
    arc_semantics: bool,
    arc_log_emitters: Option<&[Address]>,
) -> (Vec<DebankTrace>, Vec<DebankEvent>) {
    let nodes = traces.into_nodes();
    if nodes.is_empty() {
        return (vec![], vec![]);
    }
    let mut top = build_trace_node(
        tx_id,
        "".to_string(),
        0,
        &nodes[0],
        &nodes,
        arc_semantics,
        arc_log_emitters,
    );
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

/// [`spawn_blocking_with_cancel`] gated by an optional concurrency
/// limiter (`None` keeps the old unbounded behavior) — used for both
/// the EVM execution limiter and the state-read limiter. Waiting
/// happens on the async side (cheap and cancellable — a dropped caller
/// releases its queue slot); the permit is moved into the blocking task
/// so it is held until execution really finishes.
pub async fn spawn_blocking_limited_with_cancel<F, R>(
    limiter: Option<Arc<Semaphore>>,
    task: F,
) -> Result<R, JoinError>
where
    F: FnOnce(CancellationToken) -> R + Send + 'static,
    R: Send + 'static,
{
    let permit = match limiter {
        // acquire_owned only errors when the semaphore is closed, which never happens here.
        Some(sem) => sem.acquire_owned().await.ok(),
        None => None,
    };

    let token = CancellationToken::new();

    let _guard = token.clone().drop_guard();

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        task(token)
    })
    .await
}
#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::Bytes;
    use revm::primitives::Log;
    use revm_inspectors::tracing::types::CallLog;
    use std::sync::atomic::AtomicU64;
    use std::sync::{atomic, Arc};
    use std::time::Duration;
    use tokio::time::timeout;

    #[derive(Debug, thiserror::Error)]
    #[error("mock error")]
    struct MockErr;
    impl revm::database_interface::DBErrorMarker for MockErr {}

    #[derive(Debug, thiserror::Error)]
    #[error("injected state override database failure")]
    struct OverrideDbError;
    impl revm::database_interface::DBErrorMarker for OverrideDbError {}

    #[derive(Debug)]
    struct FailingOverrideDb;

    impl DatabaseRef for &FailingOverrideDb {
        type Error = OverrideDbError;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(OverrideDbError)
        }

        fn code_by_hash_ref(&self, _code_hash: H256) -> Result<Bytecode, Self::Error> {
            Err(OverrideDbError)
        }

        fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(OverrideDbError)
        }

        fn block_hash_ref(&self, _number: u64) -> Result<H256, Self::Error> {
            Err(OverrideDbError)
        }
    }

    /// DatabaseRef mock counting underlying reads.
    #[derive(Debug, Default)]
    struct Counting {
        reads: AtomicU64,
    }

    impl DatabaseRef for &Counting {
        type Error = MockErr;
        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, MockErr> {
            self.reads.fetch_add(1, atomic::Ordering::SeqCst);
            let mut info = AccountInfo::default();
            info.nonce = 7;
            Ok(Some(info))
        }
        fn code_by_hash_ref(&self, _code_hash: H256) -> Result<Bytecode, MockErr> {
            self.reads.fetch_add(1, atomic::Ordering::SeqCst);
            Ok(Bytecode::default())
        }
        fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, MockErr> {
            self.reads.fetch_add(1, atomic::Ordering::SeqCst);
            Ok(U256::from(42u64))
        }
        fn block_hash_ref(&self, _number: u64) -> Result<H256, MockErr> {
            self.reads.fetch_add(1, atomic::Ordering::SeqCst);
            Ok(H256::ZERO)
        }
    }

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
    fn arc_log_emitter_sidecar_uses_global_index_and_missing_entry_falls_back() {
        let frame = Address::with_last_byte(1);
        let first_emitter = Address::with_last_byte(2);
        let second_emitter = Address::with_last_byte(3);
        let mut emitters = vec![Address::ZERO; 4];
        emitters[1] = first_emitter;
        emitters[3] = second_emitter;
        let identical_raw_logs = [(first_emitter, 1), (second_emitter, 3)];

        let (_, events) = build_arc_debank_traces(
            H256::ZERO,
            trace_arena_with_logs(frame, &identical_raw_logs),
            &emitters,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].contract_id, first_emitter);
        assert_eq!(events[1].contract_id, second_emitter);

        let (_, events) = build_arc_debank_traces(
            H256::ZERO,
            trace_arena_with_logs(frame, &identical_raw_logs),
            &emitters[..2],
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].contract_id, first_emitter);
        assert_eq!(events[1].contract_id, frame);
    }

    #[test]
    fn request_cache_db_caches_repeated_reads() {
        let counting = Counting::default();
        let db = RequestCacheDB::new(CacheDB::new(&counting));
        let addr = Address::with_last_byte(1);

        // Values pass through unchanged...
        assert_eq!(db.basic_ref(addr).unwrap().unwrap().nonce, 7);
        assert_eq!(db.storage_ref(addr, U256::from(5u64)).unwrap(), U256::from(42u64));
        let after_first = counting.reads.load(atomic::Ordering::SeqCst);

        // ...and repeats are served from the request-local cache.
        for _ in 0..10 {
            assert_eq!(db.basic_ref(addr).unwrap().unwrap().nonce, 7);
            assert_eq!(db.storage_ref(addr, U256::from(5u64)).unwrap(), U256::from(42u64));
        }
        assert_eq!(counting.reads.load(atomic::Ordering::SeqCst), after_first);
    }

    #[test]
    fn arc_state_override_updates_code_hash_and_preserves_endpoint_errors() {
        let address = Address::with_last_byte(1);
        let code = Bytes::from_static(&[0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
        let mut overrides = StateOverride::default();
        overrides.insert(address, AccountOverride::default().with_code(code.clone()));

        let mut reth_db = CacheDB::new(revm::database::EmptyDB::default());
        apply_state_overrides_reth(overrides.clone(), &mut reth_db).unwrap();
        let info = reth_db.basic(address).unwrap().unwrap();
        assert_eq!(info.code_hash, keccak256(&code));
        assert_eq!(info.code.unwrap().original_bytes(), code);

        let mut debank_db = CacheDB::new(revm::database::EmptyDB::default());
        apply_state_overrides_arc_debank(overrides, &mut debank_db).unwrap();
        let info = debank_db.basic(address).unwrap().unwrap();
        assert_eq!(info.code_hash, keccak256(&code));

        let mut failing = CacheDB::new(&FailingOverrideDb);
        let mut override_balance = StateOverride::default();
        override_balance.insert(address, AccountOverride::default().with_balance(U256::ONE));
        let reth_error =
            apply_state_overrides_reth(override_balance.clone(), &mut failing).unwrap_err();
        assert_eq!(reth_error.code(), -32603);
        assert_eq!(
            reth_error.message(),
            "injected state override database failure"
        );

        let mut failing = CacheDB::new(&FailingOverrideDb);
        let debank_error =
            apply_state_overrides_arc_debank(override_balance, &mut failing).unwrap_err();
        assert_eq!(debank_error.code(), -32603);
        assert_eq!(debank_error.message(), "Failed to get basic account info");
    }

    #[test]
    fn reth_state_override_keeps_exact_invalid_bytecode_prefix() {
        let address = Address::with_last_byte(1);
        let mut overrides = StateOverride::default();
        overrides.insert(
            address,
            AccountOverride::default().with_code(Bytes::from_static(&[0xef, 0x01])),
        );
        let mut db = CacheDB::new(revm::database::EmptyDB::default());
        let error = apply_state_overrides_reth(overrides, &mut db).unwrap_err();

        assert_eq!(error.code(), jsonrpsee::types::error::INVALID_PARAMS_CODE);
        assert!(error.message().starts_with("Invalid bytecode: "));
    }

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
