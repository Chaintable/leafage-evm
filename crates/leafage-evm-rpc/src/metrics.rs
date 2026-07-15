use futures::future::BoxFuture;
use futures::FutureExt;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::MethodResponse;
use jsonrpsee::types::Request;
use metrics::{counter, histogram};
use std::task::{Context, Poll};
use std::time::Instant;
use tower::{BoxError, Layer, Service};

#[derive(Debug, Clone, Copy, Default)]
pub struct HttpMetricLayer;

#[derive(Debug, Clone)]
pub struct HttpMetric<S> {
    service: S,
}

impl<S> Layer<S> for HttpMetricLayer {
    type Service = HttpMetric<S>;

    fn layer(&self, service: S) -> Self::Service {
        HttpMetric { service }
    }
}

impl<S, B, R> Service<http::Request<B>> for HttpMetric<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>, Error = BoxError>,
    S::Future: Send + 'static,
    B: 'static,
    R: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let http_method = http_method_label(request.method());
        let started_at = Instant::now();
        let response = self.service.call(request);

        async move {
            let result = response.await;
            let duration = started_at.elapsed().as_secs_f64();

            histogram!(
                "leafage_http_call_time",
                "http_method" => http_method
            )
            .record(duration);

            match &result {
                Ok(response) => {
                    counter!(
                        "leafage_http_call_status",
                        "http_method" => http_method,
                        "status_code" => response.status().as_u16().to_string(),
                        "outcome" => "response",
                    )
                    .increment(1);
                }
                Err(error) => {
                    counter!(
                        "leafage_http_call_status",
                        "http_method" => http_method,
                        "status_code" => "none",
                        "outcome" => http_error_outcome(error),
                    )
                    .increment(1);
                }
            }

            result
        }
        .boxed()
    }
}

fn http_method_label(method: &http::Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    }
}

fn http_error_outcome(error: &BoxError) -> &'static str {
    if error.is::<tower::timeout::error::Elapsed>() {
        "timeout"
    } else {
        "error"
    }
}

#[derive(Debug)]
pub struct RpcMetric<S> {
    pub service: S,
}

impl<'a, S> RpcServiceT<'a> for RpcMetric<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = BoxFuture<'a, MethodResponse>;

    fn call(&self, req: Request<'a>) -> Self::Future {
        let service = self.service.clone();
        async move {
            let method_name = req.method_name().to_string();
            let start = std::time::Instant::now();
            let call_time_metric = histogram!(
                "leafage_rpc_call_time",
                &[("method_name", method_name.clone())]
            );
            let rsp = service.call(req).await;
            let duration = start.elapsed().as_secs_f64();
            call_time_metric.record(duration);
            let mut return_code = 0;
            if let Some(code) = rsp.as_error_code() {
                return_code = code
            };
            let call_status_metric = counter!(
                "leafage_rpc_call_status",
                &[
                    ("method_name", method_name.clone()),
                    ("return_code", format!("{}", return_code))
                ]
            );
            call_status_metric.increment(1);
            rsp
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use metrics_util::MetricKind;
    use std::time::Duration;
    use tower::{service_fn, ServiceBuilder};

    #[test]
    fn normalizes_http_method_labels() {
        assert_eq!(http_method_label(&http::Method::GET), "GET");
        assert_eq!(http_method_label(&http::Method::POST), "POST");
        assert_eq!(http_method_label(&http::Method::OPTIONS), "OPTIONS");
        assert_eq!(
            http_method_label(&http::Method::from_bytes(b"CUSTOM").unwrap()),
            "OTHER"
        );
    }

    #[test]
    fn classifies_http_errors() {
        let timeout: BoxError = Box::new(tower::timeout::error::Elapsed::new());
        assert_eq!(http_error_outcome(&timeout), "timeout");

        let error: BoxError = Box::new(std::io::Error::other("service failed"));
        assert_eq!(http_error_outcome(&error), "error");
    }

    #[test]
    fn http_metric_preserves_response_and_records_metrics() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let inner = service_fn(|request: http::Request<()>| async move {
            assert_eq!(request.into_body(), ());
            Ok::<_, BoxError>(
                http::Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body("overloaded")
                    .unwrap(),
            )
        });
        let mut service = HttpMetricLayer.layer(inner);

        let response = metrics::with_local_recorder(&recorder, || {
            futures::executor::block_on(
                service.call(
                    http::Request::builder()
                        .method(http::Method::POST)
                        .body(())
                        .unwrap(),
                ),
            )
        })
        .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.into_body(), "overloaded");

        let snapshot = snapshotter.snapshot().into_vec();
        let (_, _, _, histogram) = snapshot
            .iter()
            .find(|(key, _, _, _)| {
                key.kind() == MetricKind::Histogram
                    && key.key().name() == "leafage_http_call_time"
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "http_method" && label.value() == "POST")
            })
            .expect("HTTP latency histogram should be recorded");
        assert!(matches!(histogram, DebugValue::Histogram(values) if values.len() == 1));

        let (_, _, _, status) = snapshot
            .iter()
            .find(|(key, _, _, _)| {
                key.kind() == MetricKind::Counter
                    && key.key().name() == "leafage_http_call_status"
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "http_method" && label.value() == "POST")
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "status_code" && label.value() == "429")
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "outcome" && label.value() == "response")
            })
            .expect("HTTP status counter should be recorded");
        assert_eq!(status, &DebugValue::Counter(1));
    }

    #[tokio::test]
    async fn http_metric_preserves_error() {
        let inner = service_fn(|_request: http::Request<()>| async move {
            Err::<http::Response<()>, BoxError>(Box::new(std::io::Error::other("service failed")))
        });
        let mut service = HttpMetricLayer.layer(inner);

        let error = service
            .call(http::Request::new(()))
            .await
            .expect_err("inner error should be returned");

        assert_eq!(error.to_string(), "service failed");
    }

    #[test]
    fn outer_http_metric_records_tower_timeout() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let inner = service_fn(|_request: http::Request<()>| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<_, BoxError>(http::Response::new(()))
        });
        let mut service = ServiceBuilder::new()
            .layer(HttpMetricLayer)
            .timeout(Duration::from_millis(1))
            .service(inner);

        let error = metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async { service.call(http::Request::new(())).await })
        })
        .expect_err("request should time out");
        assert!(error.is::<tower::timeout::error::Elapsed>());

        let snapshot = snapshotter.snapshot().into_vec();
        let (_, _, _, status) = snapshot
            .iter()
            .find(|(key, _, _, _)| {
                key.kind() == MetricKind::Counter
                    && key.key().name() == "leafage_http_call_status"
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "status_code" && label.value() == "none")
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "outcome" && label.value() == "timeout")
            })
            .expect("timeout status counter should be recorded");
        assert_eq!(status, &DebugValue::Counter(1));
    }
}
