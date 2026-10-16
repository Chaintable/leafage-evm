use super::*;
use jsonrpsee::types::Id;

fn trace() -> MulticallTrace {
    MulticallTrace::new(
        Instant::now(),
        &Request::new("contractMultiCall".into(), None, Id::Number(17)),
    )
}

#[test]
fn threshold_uses_rpc_duration_and_keeps_slowest_late_calls() {
    let trace = trace();
    for index in 0..MAX_CALLS + 3 {
        let mut call = trace.call(index, &CallRequest::default()).unwrap();
        let mut stats = call.stats.take().unwrap();
        stats.elapsed_ms = index as f64;
        stats.return_code = Some(0);
        // Bypass real time so ordering is deterministic even on busy CI hosts.
        trace.record_call(stats);
    }
    let mut guard = trace.rpc_guard();
    guard.finish(Duration::from_millis(499), 0);
    let inner = trace.0.as_ref().unwrap();
    assert!(inner.record(Duration::from_secs(2)).is_none());
    guard.finish(SLOW_THRESHOLD, 0);
    let record = inner.record(SLOW_THRESHOLD).unwrap();
    assert_eq!(record["stats"]["rpc_total_ms"], 500.0);
    assert_eq!(record["stats"]["executed_calls"], MAX_CALLS + 3);
    assert_eq!(record["call_details_omitted"], 3);
    assert_eq!(
        record["stats"]["slowest_calls"].as_array().unwrap().len(),
        MAX_CALLS
    );
    assert_eq!(record["stats"]["slowest_calls"][0]["index"], MAX_CALLS + 2);
}

#[test]
fn call_errors_and_panics_are_recorded_without_changing_results() {
    let trace = trace();
    {
        let mut guard = trace.call(4, &CallRequest::default()).unwrap();
        let result = Err(crate::error::internal_rpc_err("original failure"));
        guard.finish(&result);
        assert_eq!(result.unwrap_err().message(), "original failure");
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = trace.call(8, &CallRequest::default()).unwrap();
        panic!("simulated EVM panic");
    }));
    trace.update(|s| {
        assert_eq!(s.executed_calls, 2);
        assert_eq!(s.failed_calls, 2);
        assert!(s
            .slowest_calls
            .iter()
            .any(|c| c.index == 4 && c.outcome == "rpc_error"));
        assert!(s
            .slowest_calls
            .iter()
            .any(|c| c.index == 8 && c.outcome == "panic"));
    });
}

#[tokio::test]
async fn concurrent_requests_keep_separate_contexts() {
    let first = trace();
    let second = trace();
    let (one, two) = tokio::join!(
        first.scope(async {
            tokio::task::yield_now().await;
            MulticallTrace::current().0.unwrap().request_id
        }),
        second.scope(async {
            tokio::task::yield_now().await;
            MulticallTrace::current().0.unwrap().request_id
        }),
    );
    assert_ne!(one, two);
    assert_eq!(one, first.0.as_ref().unwrap().request_id);
    assert_eq!(two, second.0.as_ref().unwrap().request_id);
    assert!(MulticallTrace::current().0.is_none());
}

#[tokio::test]
async fn cancellation_while_waiting_never_starts_worker() {
    let trace = trace();
    let mut future = Box::pin(async {
        let _rpc = trace.rpc_guard();
        spawn_blocking(Some(Arc::new(Semaphore::new(0))), 2, trace.clone(), |_| {
            panic!("cancelled queued request must not execute");
        })
        .await
    });
    assert!(futures::poll!(future.as_mut()).is_pending());
    drop(future);
    trace.update(|s| {
        assert_eq!(s.outcome, Some("caller_cancelled"));
        assert!(!s.worker_started);
        assert!(s.stages_ms.contains_key("limiter_wait"));
    });
}

#[tokio::test]
async fn cancelled_worker_keeps_all_permits_and_completes_diagnostics() {
    let trace = trace();
    let limiter = Arc::new(Semaphore::new(2));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_trace = trace.clone();
    let worker_limiter = limiter.clone();
    let caller = tokio::spawn(async move {
        let _rpc = worker_trace.rpc_guard();
        spawn_blocking(
            Some(worker_limiter),
            2,
            worker_trace.clone(),
            move |token| {
                let _call = worker_trace.call(0, &CallRequest::default());
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(token.is_cancelled());
            },
        )
        .await
    });
    started_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(limiter.available_permits(), 0);
    trace.update(|s| {
        assert_eq!(s.outcome, Some("caller_cancelled"));
        assert!(s.worker_started);
        assert!(!s.worker_finished);
    });
    release_tx.send(()).unwrap();
    let _permit = tokio::time::timeout(Duration::from_secs(5), limiter.acquire_many_owned(2))
        .await
        .unwrap()
        .unwrap();
    trace.update(|s| {
        assert!(s.worker_finished);
        assert!(!s.worker_panicked);
        assert_eq!(s.executed_calls, 1);
        assert!(s.stages_ms.contains_key("blocking_queue"));
    });
    let record = trace
        .0
        .as_ref()
        .unwrap()
        .record(Duration::from_secs(2))
        .unwrap();
    assert_eq!(record["stats"]["outcome"], "caller_cancelled");
    assert_eq!(record["observed_lifetime_ms"], 2000.0);
}
