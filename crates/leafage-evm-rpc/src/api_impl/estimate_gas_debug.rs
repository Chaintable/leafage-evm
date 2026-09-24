//! Always-on, request-local diagnostics for the estimateGas debug image.
//! Timings use the RPC metric's boundary. Only requests (or cancelled workers)
//! taking at least 500ms emit a log; no CLI flags or log-level changes are needed.

use alloy::primitives::{hex, Address, B256, U256};
use jsonrpsee::types::Request;
use leafage_evm_types::CallRequest;
use revm::{bytecode::Bytecode, context::result::ExecutionResult, state::AccountInfo, DatabaseRef};
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::Future,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Semaphore, task::JoinError};
use tokio_util::sync::CancellationToken;

const SLOW_THRESHOLD: Duration = Duration::from_millis(500);
const MAX_PARAMS_BYTES: usize = 16 * 1024;
const MAX_ROUNDS: usize = 64;
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
static HOSTNAME: LazyLock<String> =
    LazyLock::new(|| std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()));

tokio::task_local! {
    static CURRENT: EstimateTrace;
}

#[derive(Clone, Default)]
pub(crate) struct EstimateTrace(Option<Arc<TraceInner>>);

struct TraceInner {
    started: Instant,
    started_unix_ms: u64,
    request_id: u64,
    rpc_id: BoundedText,
    params: BoundedText,
    stats: Mutex<Stats>,
}

#[derive(Serialize)]
struct BoundedText {
    text: String,
    original_bytes: usize,
    truncated: bool,
}

impl BoundedText {
    fn new(text: &str, limit: usize) -> Self {
        let mut end = text.len().min(limit);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: text[..end].to_owned(),
            original_bytes: text.len(),
            truncated: end < text.len(),
        }
    }
}

#[derive(Default, Serialize)]
struct Stats {
    chain_id: Option<u64>,
    chain_config_version: Option<String>,
    from: Option<String>,
    to: Option<String>,
    selector: Option<String>,
    input_bytes: usize,
    resolved_block_number: Option<u64>,
    resolved_block_hash: Option<String>,
    outcome: Option<&'static str>,
    return_code: Option<i32>,
    local_return_code: Option<i32>,
    error_message: Option<BoundedText>,
    estimated_gas: Option<String>,
    rpc_total_ms: Option<f64>,
    last_stage: Option<&'static str>,
    stages_ms: BTreeMap<&'static str, f64>,
    limiter_enabled: bool,
    worker_started: bool,
    worker_finished: bool,
    worker_panicked: bool,
    initial_gas_range: Option<[u64; 2]>,
    final_gas_range: Option<[u64; 2]>,
    exit_reason: Option<&'static str>,
    executions: usize,
    first_execution_ms: Option<f64>,
    subsequent_executions_ms: f64,
    slowest_round: Option<Round>,
    rounds: Vec<Round>,
    reads: ReadStats,
}

#[derive(Clone, Copy, Serialize)]
struct Round {
    phase: &'static str,
    gas_limit: u64,
    elapsed_ms: f64,
    outcome: &'static str,
    gas_used: Option<u64>,
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

impl EstimateTrace {
    pub(crate) fn new(started: Instant, request: &Request<'_>) -> Self {
        Self(Some(Arc::new(TraceInner {
            started,
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            request_id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            rpc_id: BoundedText::new(&request.id().to_string(), 256),
            params: BoundedText::new(
                request.params().as_str().unwrap_or("null"),
                MAX_PARAMS_BYTES,
            ),
            stats: Mutex::new(Stats::default()),
        })))
    }

    pub(crate) fn current() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_default()
    }

    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT.scope(self.clone(), future).await
    }

    fn update(&self, update: impl FnOnce(&mut Stats)) {
        if let Some(inner) = &self.0 {
            let mut stats = inner.stats.lock().unwrap_or_else(|err| err.into_inner());
            update(&mut stats);
        }
    }

    pub(crate) fn rpc_guard(&self) -> RpcGuard {
        RpcGuard {
            trace: self.clone(),
            finished: false,
        }
    }

    pub(crate) fn context(&self, chain_id: u64, version: &str, request: &CallRequest) {
        self.update(|stats| {
            stats.chain_id = Some(chain_id);
            stats.chain_config_version = Some(version.to_owned());
            stats.from = request.from.map(|from| from.to_string());
            stats.to = request.to.map(|to| format!("{to:?}"));
            let input = request
                .input
                .input()
                .map(|data| data.as_ref())
                .unwrap_or(&[]);
            stats.input_bytes = input.len();
            stats.selector = (input.len() >= 4).then(|| hex::encode_prefixed(&input[..4]));
        });
    }

    pub(crate) fn resolved_block(&self, number: u64, hash: B256) {
        self.update(|stats| {
            stats.resolved_block_number = Some(number);
            stats.resolved_block_hash = Some(hash.to_string());
        });
    }

    pub(crate) fn stage(&self, name: &'static str) -> Stage {
        self.update(|stats| stats.last_stage = Some(name));
        Stage {
            trace: self.clone(),
            name,
            started: Instant::now(),
        }
    }

    pub(crate) fn gas_range(&self, low: u64, high: u64) {
        self.update(|stats| {
            stats.initial_gas_range.get_or_insert([low, high]);
            stats.final_gas_range = Some([low, high]);
        });
    }

    pub(crate) fn exit_reason(&self, reason: &'static str) {
        self.update(|stats| stats.exit_reason = Some(reason));
    }

    pub(crate) fn local_result(&self, result: &jsonrpsee::core::RpcResult<U256>) {
        self.update(|stats| {
            stats.local_return_code = Some(result.as_ref().err().map_or(0, |err| err.code()));
            if let Err(err) = result {
                stats.error_message = Some(BoundedText::new(err.message(), 512));
                if stats.exit_reason != Some("cancelled") {
                    stats.exit_reason = Some("error");
                }
            }
        });
    }

    pub(crate) fn final_result(&self, result: &jsonrpsee::core::RpcResult<U256>) {
        self.update(|stats| match result {
            Ok(gas) => stats.estimated_gas = Some(gas.to_string()),
            Err(err) => stats.error_message = Some(BoundedText::new(err.message(), 512)),
        });
    }

    pub(crate) fn execute<H, E>(
        &self,
        phase: &'static str,
        gas_limit: u64,
        execute: impl FnOnce() -> Result<ExecutionResult<H>, E>,
    ) -> Result<ExecutionResult<H>, E> {
        let stage = self.stage("evm");
        let result = execute();
        let elapsed_ms = ms(stage.started.elapsed());
        let (outcome, gas_used) = match &result {
            Ok(ExecutionResult::Success { .. }) => {
                ("success", result.as_ref().ok().map(|r| r.gas_used()))
            }
            Ok(ExecutionResult::Revert { .. }) => {
                ("revert", result.as_ref().ok().map(|r| r.gas_used()))
            }
            Ok(ExecutionResult::Halt { .. }) => {
                ("halt", result.as_ref().ok().map(|r| r.gas_used()))
            }
            Err(_) => ("error", None),
        };
        self.update(|stats| {
            stats.executions += 1;
            if stats.first_execution_ms.is_none() {
                stats.first_execution_ms = Some(elapsed_ms);
            } else {
                stats.subsequent_executions_ms += elapsed_ms;
            }
            let round = Round {
                phase,
                gas_limit,
                elapsed_ms,
                outcome,
                gas_used,
            };
            if stats
                .slowest_round
                .as_ref()
                .is_none_or(|old| elapsed_ms > old.elapsed_ms)
            {
                stats.slowest_round = Some(round);
            }
            if stats.rounds.len() < MAX_ROUNDS {
                stats.rounds.push(round);
            }
        });
        result
    }
}

pub(crate) struct RpcGuard {
    trace: EstimateTrace,
    finished: bool,
}

impl RpcGuard {
    pub(crate) fn finish(&mut self, elapsed: Duration, return_code: i32) {
        self.trace.update(|stats| {
            stats.rpc_total_ms = Some(ms(elapsed));
            stats.return_code = Some(return_code);
            stats.outcome = Some(if return_code == 0 { "success" } else { "error" });
        });
        self.finished = true;
    }
}

impl Drop for RpcGuard {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(inner) = &self.trace.0 {
                self.trace.update(|stats| {
                    stats.rpc_total_ms = Some(ms(inner.started.elapsed()));
                    stats.outcome = Some(if std::thread::panicking() {
                        "panic"
                    } else {
                        "caller_cancelled"
                    });
                });
            }
        }
    }
}

pub(crate) struct Stage {
    trace: EstimateTrace,
    name: &'static str,
    started: Instant,
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.trace.update(|stats| {
            *stats.stages_ms.entry(self.name).or_default() += ms(self.started.elapsed());
            if self.name == "worker" {
                stats.worker_finished = true;
                stats.worker_panicked = std::thread::panicking();
            }
        });
    }
}

impl TraceInner {
    fn record(&self, observed: Duration) -> Option<serde_json::Value> {
        let stats = self.stats.lock().unwrap_or_else(|err| err.into_inner());
        let rpc_ms = stats.rpc_total_ms.unwrap_or_default();
        let cancelled_worker_slow =
            stats.outcome == Some("caller_cancelled") && observed >= SLOW_THRESHOLD;
        if rpc_ms < ms(SLOW_THRESHOLD) && !cancelled_worker_slow {
            return None;
        }
        Some(serde_json::json!({
            "debug_profile": "estimate-gas-v1",
            "pod": HOSTNAME.as_str(),
            "pid": std::process::id(),
            "request_id": self.request_id,
            "started_unix_ms": self.started_unix_ms,
            "rpc_id": self.rpc_id,
            "method": "estimateGas",
            "threshold_ms": SLOW_THRESHOLD.as_millis(),
            "params": self.params,
            "observed_lifetime_ms": ms(observed),
            "rounds_truncated": stats.executions > stats.rounds.len(),
            "stats": &*stats,
        }))
    }
}

impl Drop for TraceInner {
    fn drop(&mut self) {
        // A cancelled spawn_blocking task may outlive the RPC future. Its Arc
        // keeps this record alive until timings/reads are complete; RPC latency
        // and lifetime until worker completion are deliberately separate fields.
        if let Some(record) = self.record(self.started.elapsed()) {
            tracing::info!(target: "leafage_evm_rpc::estimate_gas_debug",
                event = "estimate_gas_slow", details = %record,
                "slow estimateGas request");
        }
    }
}

/// Matches the existing cancellation/permit semantics, while recording both
/// waits separately. The permit remains owned by the worker after caller drop.
pub(crate) async fn spawn_blocking<F, R>(
    limiter: Option<Arc<Semaphore>>,
    trace: EstimateTrace,
    task: F,
) -> Result<R, JoinError>
where
    F: FnOnce(CancellationToken) -> R + Send + 'static,
    R: Send + 'static,
{
    trace.update(|stats| stats.limiter_enabled = limiter.is_some());
    let permit = {
        let _wait = trace.stage("limiter_wait");
        match limiter {
            Some(sem) => sem.acquire_owned().await.ok(),
            None => None,
        }
    };
    let token = CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    let queue = trace.stage("blocking_queue");
    tokio::task::spawn_blocking(move || {
        drop(queue);
        let _permit = permit;
        trace.update(|stats| stats.worker_started = true);
        let _worker = trace.stage("worker");
        task(token)
    })
    .await
}

#[derive(Clone, Copy, Default, Serialize)]
struct ReadMetric {
    count: u64,
    errors: u64,
    total_ms: f64,
    max_ms: f64,
}

impl ReadMetric {
    fn record(&mut self, elapsed: Duration, error: bool) {
        let elapsed = ms(elapsed);
        self.count += 1;
        self.errors += u64::from(error);
        self.total_ms += elapsed;
        self.max_ms = self.max_ms.max(elapsed);
    }
}

#[derive(Clone, Copy, Default, Serialize)]
struct Reads {
    account: ReadMetric,
    storage: ReadMetric,
    code: ReadMetric,
    block_hash: ReadMetric,
}

#[derive(Clone, Copy)]
enum ReadKind {
    Account,
    Storage,
    Code,
    BlockHash,
}

impl Reads {
    fn get_mut(&mut self, kind: ReadKind) -> &mut ReadMetric {
        match kind {
            ReadKind::Account => &mut self.account,
            ReadKind::Storage => &mut self.storage,
            ReadKind::Code => &mut self.code,
            ReadKind::BlockHash => &mut self.block_hash,
        }
    }

    fn count(&self) -> u64 {
        self.account.count + self.storage.count + self.code.count + self.block_hash.count
    }
}

#[derive(Clone, Copy, Default, Serialize)]
struct ReadStats {
    // Includes cache access and backing state time. Backing state is a nested
    // subset, including key hashing, shared caches, diff layers and disk reads.
    request_cache: Reads,
    backing_state: Reads,
    cache_hits: u64,
    cache_misses: u64,
}

pub(crate) struct ReadTracker {
    trace: EstimateTrace,
    stats: Rc<RefCell<ReadStats>>,
}

impl ReadTracker {
    pub(crate) fn new(trace: EstimateTrace) -> Self {
        Self {
            trace,
            stats: Rc::new(RefCell::new(ReadStats::default())),
        }
    }

    pub(crate) fn backing<DB>(&self, db: DB) -> ObservedDb<DB> {
        ObservedDb {
            db,
            backing: true,
            stats: self.stats.clone(),
        }
    }

    pub(crate) fn cached<DB>(&self, db: DB) -> ObservedDb<DB> {
        ObservedDb {
            db,
            backing: false,
            stats: self.stats.clone(),
        }
    }
}

impl Drop for ReadTracker {
    fn drop(&mut self) {
        self.trace
            .update(|stats| stats.reads = *self.stats.borrow());
    }
}

pub(crate) struct ObservedDb<DB> {
    db: DB,
    backing: bool,
    stats: Rc<RefCell<ReadStats>>,
}

impl<DB> std::fmt::Debug for ObservedDb<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservedDb").finish_non_exhaustive()
    }
}

impl<DB: DatabaseRef> ObservedDb<DB> {
    fn read<T>(
        &self,
        kind: ReadKind,
        read: impl FnOnce(&DB) -> Result<T, DB::Error>,
    ) -> Result<T, DB::Error> {
        let before = self.stats.borrow().backing_state.count();
        let started = Instant::now();
        // Never retain a RefCell borrow across the DB call: a cache miss enters
        // another wrapper using the same counters, on this one blocking thread.
        let result = read(&self.db);
        let elapsed = started.elapsed();
        let mut stats = self.stats.borrow_mut();
        if self.backing {
            stats
                .backing_state
                .get_mut(kind)
                .record(elapsed, result.is_err());
        } else {
            stats
                .request_cache
                .get_mut(kind)
                .record(elapsed, result.is_err());
            if stats.backing_state.count() == before {
                stats.cache_hits += 1;
            } else {
                stats.cache_misses += 1;
            }
        }
        result
    }
}

impl<DB: DatabaseRef> DatabaseRef for ObservedDb<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.read(ReadKind::Account, |db| db.basic_ref(address))
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.read(ReadKind::Storage, |db| db.storage_ref(address, index))
    }

    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.read(ReadKind::Code, |db| db.code_by_hash_ref(hash))
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.read(ReadKind::BlockHash, |db| db.block_hash_ref(number))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_impl::utils::RequestCacheDB;
    use jsonrpsee::types::Id;
    use revm::database::{CacheDB, InMemoryDB};

    fn trace() -> EstimateTrace {
        let request = Request::new("estimateGas".into(), None, Id::Number(17));
        EstimateTrace::new(Instant::now(), &request)
    }

    #[test]
    fn slow_success_and_error_use_the_rpc_metric_duration() {
        let trace = trace();
        let mut guard = trace.rpc_guard();
        guard.finish(Duration::from_millis(499), 0);
        let inner = trace.0.as_ref().unwrap();
        // Time after the response (including diagnostics) cannot make a fast
        // successful RPC qualify as slow.
        assert!(inner.record(Duration::from_secs(2)).is_none());
        guard.finish(SLOW_THRESHOLD, 0);
        let record = inner.record(SLOW_THRESHOLD).unwrap();
        assert_eq!(record["stats"]["outcome"], "success");
        assert_eq!(record["stats"]["return_code"], 0);
        assert_eq!(record["stats"]["rpc_total_ms"], 500.0);
        guard.finish(Duration::from_millis(1100), -32000);
        let record = inner.record(Duration::from_millis(1100)).unwrap();
        assert_eq!(record["stats"]["outcome"], "error");
        assert_eq!(record["stats"]["return_code"], -32000);
    }

    #[test]
    fn replay_params_are_complete_or_explicitly_truncated_at_utf8_boundaries() {
        let params = serde_json::value::RawValue::from_string(
            r#"[{"to":"0x0000000000000000000000000000000000000001","input":"0x12345678"},{"block_id":"latest"},{"number":"0x123"}]"#.to_owned(),
        ).unwrap();
        let request = Request::new("estimateGas".into(), Some(&params), Id::Number(17));
        let trace = EstimateTrace::new(Instant::now(), &request);
        let inner = trace.0.as_ref().unwrap();
        assert_eq!(inner.params.text, params.get());
        assert!(!inner.params.truncated);

        let long = "中".repeat(MAX_PARAMS_BYTES);
        let bounded = BoundedText::new(&long, MAX_PARAMS_BYTES);
        assert!(bounded.truncated);
        assert_eq!(bounded.original_bytes, long.len());
        assert!(bounded.text.len() <= MAX_PARAMS_BYTES);
        assert!(long.starts_with(&bounded.text));
    }

    #[test]
    fn repeated_reads_preserve_cache_behavior_and_do_not_double_count_misses() {
        let trace = trace();
        let tracker = ReadTracker::new(trace.clone());
        let address = Address::repeat_byte(1);
        let mut backing = InMemoryDB::default();
        backing.insert_account_info(
            address,
            AccountInfo {
                nonce: 1,
                ..Default::default()
            },
        );
        backing
            .insert_account_storage(address, U256::ZERO, U256::from(42))
            .unwrap();
        let cached = tracker.cached(RequestCacheDB::new(CacheDB::new(tracker.backing(backing))));

        // Loading a storage slot also loads its account on the first miss.
        // Counting hits as outer reads minus per-kind backing reads is wrong.
        assert_eq!(
            cached.storage_ref(address, U256::ZERO).unwrap(),
            U256::from(42)
        );
        assert_eq!(
            cached.storage_ref(address, U256::ZERO).unwrap(),
            U256::from(42)
        );
        assert_eq!(cached.basic_ref(address).unwrap().unwrap().nonce, 1);
        let stats = *tracker.stats.borrow();
        assert_eq!(stats.request_cache.storage.count, 2);
        assert_eq!(stats.request_cache.account.count, 1);
        assert_eq!(stats.backing_state.storage.count, 1);
        assert_eq!(stats.backing_state.account.count, 1);
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.cache_hits, 2);
        drop(cached);
        drop(tracker);
        trace.update(|stats| assert_eq!(stats.reads.backing_state.storage.count, 1));
    }

    #[test]
    fn execution_errors_are_unchanged_and_round_details_are_bounded() {
        let trace = trace();
        for _ in 0..MAX_ROUNDS + 1 {
            let result = trace.execute::<(), _>("binary_search", 100_000, || Err("original error"));
            assert_eq!(result.unwrap_err(), "original error");
        }
        trace.update(|stats| {
            assert_eq!(stats.executions, MAX_ROUNDS + 1);
            assert_eq!(stats.rounds.len(), MAX_ROUNDS);
            assert_eq!(stats.rounds[0].outcome, "error");
            assert_eq!(stats.rounds[0].gas_limit, 100_000);
            assert!(stats.first_execution_ms.is_some());
        });
    }

    #[tokio::test]
    async fn concurrent_requests_keep_separate_contexts() {
        let first = trace();
        let second = trace();
        let (one, two) = tokio::join!(
            first.scope(async {
                tokio::task::yield_now().await;
                EstimateTrace::current().0.as_ref().unwrap().request_id
            }),
            second.scope(async {
                tokio::task::yield_now().await;
                EstimateTrace::current().0.as_ref().unwrap().request_id
            }),
        );
        assert_ne!(one, two);
        assert_eq!(one, first.0.as_ref().unwrap().request_id);
        assert_eq!(two, second.0.as_ref().unwrap().request_id);
        assert!(EstimateTrace::current().0.is_none());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_does_not_start_a_worker() {
        let trace = trace();
        let limiter = Arc::new(Semaphore::new(0));
        let mut future = Box::pin(async {
            let _rpc = trace.rpc_guard();
            spawn_blocking(Some(limiter.clone()), trace.clone(), |_| {
                panic!("cancelled queued request must not execute");
            })
            .await
        });
        assert!(futures::poll!(future.as_mut()).is_pending());
        drop(future);
        trace.update(|stats| {
            assert_eq!(stats.outcome, Some("caller_cancelled"));
            assert!(!stats.worker_started);
            assert!(stats.stages_ms.contains_key("limiter_wait"));
        });
    }

    #[tokio::test]
    async fn cancelled_worker_keeps_its_permit_and_final_diagnostics() {
        let trace = trace();
        let limiter = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_trace = trace.clone();
        let worker_limiter = limiter.clone();
        let caller = tokio::spawn(async move {
            let _rpc = worker_trace.rpc_guard();
            spawn_blocking(Some(worker_limiter), worker_trace.clone(), move |token| {
                let _state = worker_trace.stage("state_acquisition");
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(token.is_cancelled());
            })
            .await
        });
        started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(limiter.available_permits(), 0);
        trace.update(|stats| {
            assert_eq!(stats.outcome, Some("caller_cancelled"));
            assert!(stats.worker_started);
            assert!(!stats.worker_finished);
        });
        release_tx.send(()).unwrap();
        let _permit = tokio::time::timeout(Duration::from_secs(5), limiter.acquire_owned())
            .await
            .unwrap()
            .unwrap();
        trace.update(|stats| {
            assert!(stats.worker_finished);
            assert!(!stats.worker_panicked);
            assert!(stats.stages_ms.contains_key("blocking_queue"));
            assert!(stats.stages_ms.contains_key("state_acquisition"));
        });
        // A worker running after a fast caller cancellation still qualifies,
        // while the record retains the caller's original elapsed time.
        let record = trace
            .0
            .as_ref()
            .unwrap()
            .record(Duration::from_secs(2))
            .unwrap();
        assert_eq!(record["stats"]["outcome"], "caller_cancelled");
        assert_eq!(record["observed_lifetime_ms"], 2000.0);
    }
}
