//! Diagnostics for the multicall debug image. Only RPCs (or workers surviving
//! caller cancellation) >= 500ms emit a bounded INFO record. No raw calldata or
//! state values are logged. Execution, caching and RPC responses are unchanged.

mod reads;
pub(crate) use reads::ObservedDb;
use reads::{ReadScope, ReadSummary};

use alloy::primitives::{hex, B256};
use jsonrpsee::{core::RpcResult, types::Request};
use leafage_evm_types::{CallRequest, DebankMultiCallResp, DebankSingleCallResult};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Semaphore, task::JoinError};
use tokio_util::sync::CancellationToken;

const SLOW_THRESHOLD: Duration = Duration::from_millis(500);
const MAX_CALLS: usize = 16;
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
static HOSTNAME: LazyLock<String> =
    LazyLock::new(|| std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()));

tokio::task_local! {
    static CURRENT: MulticallTrace;
}

#[derive(Clone, Default)]
pub(crate) struct MulticallTrace(Option<Arc<TraceInner>>);

struct TraceInner {
    started: Instant,
    started_unix_ms: u64,
    request_id: u64,
    rpc_id: String,
    stats: Mutex<Stats>,
}

#[derive(Default, Serialize)]
struct Stats {
    chain_id: u64,
    chain_config_version: String,
    requested_calls: usize,
    workers: usize,
    fast_fail: bool,
    resolved_block_number: Option<u64>,
    resolved_block_hash: Option<B256>,
    outcome: Option<&'static str>,
    return_code: Option<i32>,
    local_return_code: Option<i32>,
    rpc_total_ms: Option<f64>,
    stages_ms: BTreeMap<&'static str, f64>,
    setup_reads: Option<ReadSummary>,
    prefetch_requested_keys: BTreeMap<&'static str, usize>,
    limiter_enabled: bool,
    worker_started: bool,
    worker_finished: bool,
    worker_panicked: bool,
    executed_calls: usize,
    failed_calls: usize,
    // Includes speculative parallel calls discarded by fast_fail. It is a sum
    // of call durations and can exceed request wall time when workers overlap.
    calls_total_ms: f64,
    slowest_calls: Vec<CallStats>,
}

#[derive(Serialize)]
struct CallStats {
    index: usize,
    thread: String,
    from: Option<String>,
    to: Option<String>,
    selector: Option<String>,
    input_bytes: usize,
    elapsed_ms: f64,
    // Remainder includes EVM execution, request-cache work and instrumentation;
    // it is NOT a CPU timer. Backing reads are nested inside cache reads, so
    // their durations must never be added together.
    non_backing_ms: f64,
    outcome: &'static str,
    return_code: Option<i32>,
    gas_used: Option<i64>,
    reads: ReadSummary,
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

impl MulticallTrace {
    pub(crate) fn new(started: Instant, request: &Request<'_>) -> Self {
        Self(Some(Arc::new(TraceInner {
            started,
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            request_id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            rpc_id: request.id().to_string().chars().take(128).collect(),
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
            update(&mut inner.stats.lock().unwrap_or_else(|err| err.into_inner()));
        }
    }

    pub(crate) fn context(
        &self,
        chain_id: u64,
        version: &str,
        calls: usize,
        workers: usize,
        fast_fail: bool,
    ) {
        self.update(|s| {
            s.chain_id = chain_id;
            s.chain_config_version = version.to_owned();
            s.requested_calls = calls;
            s.workers = workers;
            s.fast_fail = fast_fail;
        });
    }

    pub(crate) fn resolved_block(&self, number: u64, hash: B256) {
        self.update(|s| {
            s.resolved_block_number = Some(number);
            s.resolved_block_hash = Some(hash);
        });
    }

    pub(crate) fn prefetch_keys(&self, kind: &'static str, count: usize) {
        self.update(|s| {
            s.prefetch_requested_keys.insert(kind, count);
        });
    }

    pub(crate) fn local_result(&self, result: &RpcResult<DebankMultiCallResp>) {
        self.update(|s| s.local_return_code = Some(result.as_ref().err().map_or(0, |e| e.code())));
    }

    pub(crate) fn rpc_guard(&self) -> RpcGuard {
        RpcGuard {
            trace: self.clone(),
            finished: false,
        }
    }

    pub(crate) fn stage(&self, name: &'static str) -> Stage {
        Stage {
            trace: self.clone(),
            name,
            started: Instant::now(),
        }
    }

    pub(crate) fn setup(&self) -> SetupGuard {
        SetupGuard {
            stage: self.stage("setup"),
            reads: self.0.is_some().then(ReadScope::new),
        }
    }

    pub(crate) fn call(&self, index: usize, request: &CallRequest) -> Option<CallGuard> {
        self.0.as_ref()?;
        let input = request.input.input().map(|d| d.as_ref()).unwrap_or(&[]);
        Some(CallGuard {
            trace: self.clone(),
            started: Instant::now(),
            reads: ReadScope::new(),
            stats: Some(CallStats {
                index,
                thread: format!("{:?}", std::thread::current().id()),
                from: request.from.map(|a| a.to_string()),
                to: request.to.map(|a| format!("{a:?}")),
                selector: (input.len() >= 4).then(|| hex::encode_prefixed(&input[..4])),
                input_bytes: input.len(),
                elapsed_ms: 0.0,
                non_backing_ms: 0.0,
                outcome: "incomplete",
                return_code: None,
                gas_used: None,
                reads: ReadSummary::default(),
            }),
        })
    }

    fn record_call(&self, call: CallStats) {
        self.update(|s| {
            s.executed_calls += 1;
            s.failed_calls += usize::from(call.return_code != Some(0));
            s.calls_total_ms += call.elapsed_ms;
            if s.slowest_calls.len() < MAX_CALLS {
                s.slowest_calls.push(call);
            } else if let Some((index, smallest)) = s
                .slowest_calls
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| a.elapsed_ms.total_cmp(&b.elapsed_ms))
            {
                if call.elapsed_ms > smallest.elapsed_ms {
                    s.slowest_calls[index] = call;
                }
            }
        });
    }
}

pub(crate) struct RpcGuard {
    trace: MulticallTrace,
    finished: bool,
}

impl RpcGuard {
    pub(crate) fn finish(&mut self, elapsed: Duration, return_code: i32) {
        self.trace.update(|s| {
            s.rpc_total_ms = Some(ms(elapsed));
            s.return_code = Some(return_code);
            s.outcome = Some(if return_code == 0 { "success" } else { "error" });
        });
        self.finished = true;
    }
}

impl Drop for RpcGuard {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(inner) = &self.trace.0 {
                self.trace.update(|s| {
                    s.rpc_total_ms = Some(ms(inner.started.elapsed()));
                    s.outcome = Some(if std::thread::panicking() {
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
    trace: MulticallTrace,
    name: &'static str,
    started: Instant,
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.trace.update(|s| {
            *s.stages_ms.entry(self.name).or_default() += ms(self.started.elapsed());
            if self.name == "worker" {
                s.worker_finished = true;
                s.worker_panicked = std::thread::panicking();
            }
        });
    }
}

pub(crate) struct SetupGuard {
    stage: Stage,
    reads: Option<ReadScope>,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        if let Some(reads) = &self.reads {
            self.stage
                .trace
                .update(|s| s.setup_reads = Some(reads.snapshot()));
        }
    }
}

pub(crate) struct CallGuard {
    trace: MulticallTrace,
    started: Instant,
    reads: ReadScope,
    stats: Option<CallStats>,
}

impl CallGuard {
    pub(crate) fn finish(&mut self, result: &RpcResult<DebankSingleCallResult>) {
        if let Some(s) = &mut self.stats {
            match result {
                Ok(res) => {
                    s.return_code = Some(res.code);
                    s.gas_used = Some(res.gas_used);
                    s.outcome = if res.code == 0 {
                        "success"
                    } else {
                        "call_error"
                    };
                }
                Err(err) => {
                    s.return_code = Some(err.code());
                    s.outcome = "rpc_error";
                }
            }
        }
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if let Some(mut call) = self.stats.take() {
            call.elapsed_ms = ms(self.started.elapsed());
            call.reads = self.reads.snapshot();
            call.non_backing_ms = (call.elapsed_ms - call.reads.backing_total_ms()).max(0.0);
            if std::thread::panicking() {
                call.outcome = "panic";
            }
            self.trace.record_call(call);
        }
    }
}

impl TraceInner {
    fn record(&self, observed: Duration) -> Option<serde_json::Value> {
        let mut s = self.stats.lock().unwrap_or_else(|err| err.into_inner());
        let slow_cancelled = s.outcome == Some("caller_cancelled") && observed >= SLOW_THRESHOLD;
        if s.rpc_total_ms.unwrap_or_default() < ms(SLOW_THRESHOLD) && !slow_cancelled {
            return None;
        }
        s.slowest_calls
            .sort_by(|a, b| b.elapsed_ms.total_cmp(&a.elapsed_ms));
        Some(serde_json::json!({
            "debug_profile": "multicall-reads-v1",
            "pod": HOSTNAME.as_str(), "pid": std::process::id(),
            "request_id": self.request_id, "rpc_id": self.rpc_id,
            "started_unix_ms": self.started_unix_ms,
            "method": "contractMultiCall", "threshold_ms": SLOW_THRESHOLD.as_millis(),
            "observed_lifetime_ms": ms(observed),
            "call_details_omitted": s.executed_calls.saturating_sub(s.slowest_calls.len()),
            "stats": &*s,
        }))
    }
}

impl Drop for TraceInner {
    fn drop(&mut self) {
        // The blocking worker retains this Arc after caller cancellation so the
        // final record includes its completed reads, not a partial early snapshot.
        if let Some(record) = self.record(self.started.elapsed()) {
            tracing::info!(target: "leafage_evm_rpc::multicall_debug",
                event = "multicall_slow", details = %record, "slow contractMultiCall request");
        }
    }
}

/// Same permit/cancellation semantics as spawn_blocking_limited_many_with_cancel.
pub(crate) async fn spawn_blocking<F, R>(
    limiter: Option<Arc<Semaphore>>,
    permits: u32,
    trace: MulticallTrace,
    task: F,
) -> Result<R, JoinError>
where
    F: FnOnce(CancellationToken) -> R + Send + 'static,
    R: Send + 'static,
{
    trace.update(|s| s.limiter_enabled = limiter.is_some());
    let permit = {
        let _wait = trace.stage("limiter_wait");
        match limiter {
            Some(sem) => sem.acquire_many_owned(permits).await.ok(),
            None => None,
        }
    };
    let token = CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    let queue = trace.stage("blocking_queue");
    tokio::task::spawn_blocking(move || {
        drop(queue);
        let _permit = permit;
        trace.update(|s| s.worker_started = true);
        let _worker = trace.stage("worker");
        task(token)
    })
    .await
}

#[cfg(test)]
mod tests;
