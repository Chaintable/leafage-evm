use futures::future::BoxFuture;
use futures::FutureExt;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::MethodResponse;
use jsonrpsee::types::Request;
use metrics::{counter, histogram};

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
