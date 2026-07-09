use futures::future::BoxFuture;
use futures::FutureExt;
use hyper::body::Bytes;
use hyper::Response;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::{HttpBody, HttpRequest, MethodResponse};
use jsonrpsee::types::Request;
use metrics::{counter, histogram};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{BoxError, Layer, Service};

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

/// HTTP-layer middleware timing the **whole** request — everything the
/// RPC-layer `leafage_rpc_call_time` excludes: HTTP body read, JSON-RPC
/// envelope parse, the interceptor / timeout layers, method dispatch, and
/// response serialization. Installed as the outermost HTTP layer, so it also
/// counts requests rejected by load shedding (429) or aborted by the request
/// timeout (recorded with `status="error"`).
///
/// Emits `leafage_rpc_http_time` labeled by HTTP `status` only — the method
/// name isn't available at the HTTP layer without buffering + parsing the body
/// (and is ambiguous for JSON-RPC batches), so per-method latency stays on the
/// RPC-layer `leafage_rpc_call_time`. The end-to-end vs per-method gap is the
/// pipeline overhead.
#[derive(Clone)]
pub struct HttpMetricLayer;

impl<S> Layer<S> for HttpMetricLayer {
    type Service = HttpMetric<S>;
    fn layer(&self, inner: S) -> Self::Service {
        HttpMetric { inner }
    }
}

#[derive(Clone)]
pub struct HttpMetric<S> {
    inner: S,
}

impl<S, B> Service<HttpRequest<B>> for HttpMetric<S>
where
    S: Service<HttpRequest<B>, Response = Response<HttpBody>>,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: HttpRequest<B>) -> Self::Future {
        let start = std::time::Instant::now();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let res = fut.await.map_err(Into::into);
            let status = match &res {
                Ok(resp) => resp.status().as_u16().to_string(),
                Err(_) => "error".to_string(),
            };
            histogram!("leafage_rpc_http_time", &[("status", status)])
                .record(start.elapsed().as_secs_f64());
            res
        })
    }
}
