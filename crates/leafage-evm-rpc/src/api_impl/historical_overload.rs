use futures::future::BoxFuture;
use futures::FutureExt;
use http::{Request as HttpRequest, Response, StatusCode};
use jsonrpsee::core::ClientError;
use jsonrpsee::http_client::transport::Error as HttpTransportError;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::{HttpBody, MethodResponse};
use jsonrpsee::types::{ErrorObjectOwned, Request};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::{Layer, Service};

use crate::error::rpc_error_with_code;

// This code is an internal signal between the RPC and HTTP middleware. The
// HTTP server is configured as HTTP-only, and the signal is converted to a 429
// response before it can be observed by a caller.
const HISTORICAL_RPC_OVERLOADED_CODE: i32 = -32042;
const SERVICE_OVERLOADED: &str = "Service overloaded";

#[derive(Debug, Default)]
struct HistoricalOverloadState {
    overloaded: AtomicBool,
}

impl HistoricalOverloadState {
    fn mark_overloaded(&self) {
        self.overloaded.store(true, Ordering::Release);
    }

    fn is_overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Acquire)
    }
}

/// Returns true when jsonrpsee rejected a Historical RPC request with HTTP 429.
pub(crate) fn is_historical_rpc_overloaded(error: &ClientError) -> bool {
    let ClientError::Transport(error) = error else {
        return false;
    };

    error
        .downcast_ref::<HttpTransportError>()
        .is_some_and(|error| {
            matches!(
                error,
                HttpTransportError::Rejected { status_code }
                    if *status_code == StatusCode::TOO_MANY_REQUESTS.as_u16()
            )
        })
}

/// Creates an internal JSON-RPC error that the middleware converts to HTTP 429.
pub(crate) fn historical_rpc_overloaded_error() -> ErrorObjectOwned {
    rpc_error_with_code(HISTORICAL_RPC_OVERLOADED_CODE, "Historical RPC overloaded")
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HistoricalOverloadHttpLayer;

#[derive(Debug, Clone)]
pub(crate) struct HistoricalOverloadHttp<S> {
    service: S,
}

impl<S> Layer<S> for HistoricalOverloadHttpLayer {
    type Service = HistoricalOverloadHttp<S>;

    fn layer(&self, service: S) -> Self::Service {
        HistoricalOverloadHttp { service }
    }
}

impl<S, B> Service<HttpRequest<B>> for HistoricalOverloadHttp<S>
where
    S: Service<HttpRequest<B>, Response = Response<HttpBody>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, mut request: HttpRequest<B>) -> Self::Future {
        let state = Arc::new(HistoricalOverloadState::default());
        request.extensions_mut().insert(state.clone());
        let response = self.service.call(request);

        async move {
            let response = response.await?;
            if state.is_overloaded() {
                Ok(Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(HttpBody::from(SERVICE_OVERLOADED))
                    .expect("the Historical RPC overload response is valid"))
            } else {
                Ok(response)
            }
        }
        .boxed()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HistoricalOverloadRpc<S> {
    service: S,
}

impl<S> HistoricalOverloadRpc<S> {
    pub(crate) fn new(service: S) -> Self {
        Self { service }
    }
}

impl<'a, S> RpcServiceT<'a> for HistoricalOverloadRpc<S>
where
    S: RpcServiceT<'a> + Send + Sync + 'static,
{
    type Future = BoxFuture<'a, MethodResponse>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        let state = request
            .extensions()
            .get::<Arc<HistoricalOverloadState>>()
            .cloned();
        let response = self.service.call(request);

        async move {
            let response = response.await;
            if response.as_error_code() == Some(HISTORICAL_RPC_OVERLOADED_CODE) {
                if let Some(state) = state {
                    state.mark_overloaded();
                }
            }
            response
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::core::client::ClientT;
    use jsonrpsee::core::params::BatchRequestBuilder;
    use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
    use jsonrpsee::rpc_params;
    use jsonrpsee::server::{RpcModule, RpcServiceBuilder, ServerBuilder, ServerHandle};
    use tower::ServiceBuilder;

    fn rejected(status_code: u16) -> ClientError {
        ClientError::Transport(Box::new(HttpTransportError::Rejected { status_code }))
    }

    fn assert_rejected_with_status(error: ClientError, expected_status: StatusCode) {
        let ClientError::Transport(error) = error else {
            panic!("expected transport error, got {error:?}");
        };
        let error = error
            .downcast::<HttpTransportError>()
            .expect("transport error should come from the HTTP client");
        assert!(matches!(
            *error,
            HttpTransportError::Rejected { status_code }
                if status_code == expected_status.as_u16()
        ));
    }

    #[test]
    fn only_http_429_is_classified_as_historical_overload() {
        assert!(is_historical_rpc_overloaded(&rejected(
            StatusCode::TOO_MANY_REQUESTS.as_u16()
        )));
        assert!(!is_historical_rpc_overloaded(&rejected(
            StatusCode::SERVICE_UNAVAILABLE.as_u16()
        )));
        assert!(!is_historical_rpc_overloaded(&ClientError::RequestTimeout));
        assert!(!is_historical_rpc_overloaded(&ClientError::Call(
            ErrorObjectOwned::owned(-32000, "ordinary RPC error", None::<()>),
        )));
    }

    async fn test_server() -> (HttpClient, ServerHandle) {
        let http_middleware = ServiceBuilder::new().layer(HistoricalOverloadHttpLayer);
        let rpc_middleware =
            RpcServiceBuilder::new().layer_fn(|service| HistoricalOverloadRpc::new(service));
        let server = ServerBuilder::default()
            .http_only()
            .set_http_middleware(http_middleware)
            .set_rpc_middleware(rpc_middleware)
            .build("127.0.0.1:0")
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();

        let mut module = RpcModule::new(());
        module.register_method("ok", |_, _, _| "ok").unwrap();
        module
            .register_method::<Result<(), ErrorObjectOwned>, _>("overloaded", |_, _, _| {
                Err(historical_rpc_overloaded_error())
            })
            .unwrap();
        module
            .register_method::<Result<(), ErrorObjectOwned>, _>("rpc_error", |_, _, _| {
                Err(ErrorObjectOwned::owned(
                    -32000,
                    "ordinary RPC error",
                    None::<()>,
                ))
            })
            .unwrap();

        let handle = server.start(module);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();
        (client, handle)
    }

    #[tokio::test]
    async fn middleware_preserves_normal_responses_and_only_converts_overload() {
        let (client, handle) = test_server().await;

        let result: String = client.request("ok", rpc_params![]).await.unwrap();
        assert_eq!(result, "ok");

        let rpc_error = client
            .request::<(), _>("rpc_error", rpc_params![])
            .await
            .unwrap_err();
        assert!(matches!(rpc_error, ClientError::Call(error) if error.code() == -32000));

        let overload = client
            .request::<(), _>("overloaded", rpc_params![])
            .await
            .unwrap_err();
        assert_rejected_with_status(overload, StatusCode::TOO_MANY_REQUESTS);

        let result: String = client.request("ok", rpc_params![]).await.unwrap();
        assert_eq!(result, "ok");

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn one_overloaded_call_rejects_the_entire_batch() {
        let (client, handle) = test_server().await;
        let mut batch = BatchRequestBuilder::new();
        batch.insert("ok", rpc_params![]).unwrap();
        batch.insert("overloaded", rpc_params![]).unwrap();

        let error = client.batch_request::<String>(batch).await.unwrap_err();
        assert_rejected_with_status(error, StatusCode::TOO_MANY_REQUESTS);

        handle.stop().unwrap();
        handle.stopped().await;
    }
}
