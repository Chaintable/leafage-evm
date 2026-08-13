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
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = BoxFuture<'a, MethodResponse>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        let state = request
            .extensions()
            .get::<Arc<HistoricalOverloadState>>()
            .cloned();
        let service = self.service.clone();

        async move {
            let response = service.call(request).await;
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
