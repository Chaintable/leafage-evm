use futures::future::BoxFuture;
use futures::FutureExt;
use hyper::Response;
use jsonrpsee::core::http_helpers::read_body;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::{HttpBody, HttpRequest, HttpResponse, MethodResponse};
use jsonrpsee::types::Request;
use metrics::{counter, histogram};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{BoxError, Layer, Service};

/// Max request body we buffer to peek the JSON-RPC method name. Larger requests
/// (or ones without a `Content-Length`) are forwarded un-buffered and labeled
/// `method_name="unbuffered"`; buffering never exceeds this. Well above any
/// real state-read/`contractMultiCall` request.
const METHOD_PEEK_MAX_BODY: u32 = 1024 * 1024;

/// Extract the JSON-RPC `method` from a single-request body. Best-effort — a
/// missing/mistyped `method` yields `"unknown"`.
fn peek_method(bytes: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct MethodOnly<'a> {
        method: &'a str,
    }
    serde_json::from_slice::<MethodOnly>(bytes)
        .map(|m| m.method.to_string())
        .unwrap_or_else(|_| "unknown".to_string())
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

/// HTTP-layer middleware timing the **whole** request — everything the
/// RPC-layer `leafage_rpc_call_time` excludes: HTTP body read, JSON-RPC
/// envelope parse, the interceptor / timeout layers, method dispatch, and
/// response serialization. Installed as the outermost HTTP layer, so it also
/// counts requests rejected by load shedding (429) or aborted by the request
/// timeout.
///
/// Emits `leafage_rpc_http_time` labeled by `method_name` (mirroring
/// `leafage_rpc_call_time`) and HTTP `status`. The method is peeked by buffering
/// the request body (bounded by [`METHOD_PEEK_MAX_BODY`]); JSON-RPC batches are
/// labeled `"batch"` and oversized/length-less bodies `"unbuffered"`.
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

impl<S> Service<HttpRequest<HttpBody>> for HttpMetric<S>
where
    S: Service<HttpRequest<HttpBody>, Response = HttpResponse> + Clone + Send + 'static,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: HttpRequest<HttpBody>) -> Self::Future {
        // Move the ready service into the future; leave a clone to be readied
        // before the next call (standard tower idiom).
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let start = std::time::Instant::now();
            let (parts, body) = req.into_parts();

            // Only buffer small, length-declared bodies to peek the method.
            let content_len = parts
                .headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok());

            let (method, res) = if content_len.map_or(false, |c| c <= METHOD_PEEK_MAX_BODY) {
                match read_body(&parts.headers, body, METHOD_PEEK_MAX_BODY).await {
                    Ok((bytes, single)) => {
                        let method = if single {
                            peek_method(&bytes)
                        } else {
                            "batch".to_string()
                        };
                        let req = HttpRequest::from_parts(parts, HttpBody::from(bytes));
                        (method, inner.call(req).await.map_err(Into::into))
                    }
                    // Too large / malformed: jsonrpsee would reject these too.
                    Err(_) => {
                        let resp = Response::builder()
                            .status(hyper::StatusCode::BAD_REQUEST)
                            .body(HttpBody::from("invalid request body"))
                            .expect("static response is valid");
                        ("error".to_string(), Ok(resp))
                    }
                }
            } else {
                // No/oversized Content-Length: forward without buffering.
                let req = HttpRequest::from_parts(parts, body);
                ("unbuffered".to_string(), inner.call(req).await.map_err(Into::into))
            };

            let status = match &res {
                Ok(resp) => resp.status().as_u16().to_string(),
                Err(_) => "error".to_string(),
            };
            histogram!(
                "leafage_rpc_http_time",
                &[("method_name", method), ("status", status)]
            )
            .record(start.elapsed().as_secs_f64());
            res
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_method_extracts_or_unknowns() {
        assert_eq!(
            peek_method(br#"{"jsonrpc":"2.0","method":"getStorageAt","id":1}"#),
            "getStorageAt"
        );
        // params before method still works
        assert_eq!(peek_method(br#"{"params":[],"method":"getBalance"}"#), "getBalance");
        // missing/invalid method
        assert_eq!(peek_method(br#"{"jsonrpc":"2.0","id":1}"#), "unknown");
        assert_eq!(peek_method(b"not json"), "unknown");
    }

    /// Inner service that echoes the request body back — lets us assert the body
    /// survives HttpMetric's buffer + rebuild unchanged.
    #[derive(Clone)]
    struct Echo;
    impl Service<HttpRequest<HttpBody>> for Echo {
        type Response = HttpResponse;
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, BoxError>> + Send>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: HttpRequest<HttpBody>) -> Self::Future {
            Box::pin(async move {
                let headers = req.headers().clone();
                let (_, body) = req.into_parts();
                let (bytes, _single) = read_body(&headers, body, u32::MAX).await.unwrap();
                Ok(Response::builder()
                    .status(200)
                    .body(HttpBody::from(bytes))
                    .unwrap())
            })
        }
    }

    async fn run(body: &'static str, content_length: Option<usize>) -> (u16, Vec<u8>) {
        let mut svc = HttpMetric { inner: Echo };
        let mut b = HttpRequest::builder().method("POST").uri("/");
        if let Some(c) = content_length {
            b = b.header("content-length", c.to_string());
        }
        let req = b.body(HttpBody::from(body)).unwrap();
        let resp = svc.call(req).await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let (_, rbody) = resp.into_parts();
        let (echoed, _) = read_body(&headers, rbody, u32::MAX).await.unwrap();
        (status, echoed)
    }

    #[tokio::test]
    async fn single_request_body_roundtrips() {
        let body = r#"{"jsonrpc":"2.0","method":"getStorageAt","params":[],"id":1}"#;
        let (status, echoed) = run(body, Some(body.len())).await;
        assert_eq!(status, 200);
        assert_eq!(echoed, body.as_bytes(), "body must survive buffer+rebuild");
    }

    #[tokio::test]
    async fn batch_request_body_roundtrips() {
        let body = r#"[{"method":"a","id":1},{"method":"b","id":2}]"#;
        let (status, echoed) = run(body, Some(body.len())).await;
        assert_eq!(status, 200);
        assert_eq!(echoed, body.as_bytes());
    }

    #[tokio::test]
    async fn unbuffered_path_forwards_body() {
        // No Content-Length -> forwarded without buffering, still intact.
        let body = r#"{"method":"getBalance","id":1}"#;
        let (status, echoed) = run(body, None).await;
        assert_eq!(status, 200);
        assert_eq!(echoed, body.as_bytes());
    }
}
