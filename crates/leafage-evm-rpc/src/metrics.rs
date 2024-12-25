use futures::future::BoxFuture;
use futures::FutureExt;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::MethodResponse;
use jsonrpsee::types::Request;
use leafage_evm_types::{
    exponential_buckets, try_create_histogram_vec, try_create_int_counter_vec,
};
use once_cell::sync::Lazy;
use prometheus::{HistogramVec, IntCounterVec};

pub static RPC_PROCESSING_TIME: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "leafage_rpc_processing_time",
        "Time taken to process rpc queries",
        &["method"],
        Some(exponential_buckets(0.001, 1.5, 16).unwrap()),
    )
    .unwrap()
});

pub static RPC_REQUEST_COUNT: Lazy<IntCounterVec> = Lazy::new(|| {
    try_create_int_counter_vec(
        "leafage_rpc_total_count",
        "Total count of HTTP RPC requests received, by method",
        &["method", "status"],
    )
    .unwrap()
});

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
            let timer = RPC_PROCESSING_TIME
                .with_label_values(&[method_name.as_str()])
                .start_timer();
            let rsp = service.call(req).await;
            timer.observe_duration();
            let mut return_code = 0;
            if let Some(code) = rsp.as_error_code() {
                return_code = code
            }
            RPC_REQUEST_COUNT
                .with_label_values(&[method_name.as_str(), return_code.to_string().as_str()])
                .inc();
            rsp
        }
        .boxed()
    }
}
