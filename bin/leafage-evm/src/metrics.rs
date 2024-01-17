use hyper::{
    header::CONTENT_TYPE,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use leafage_evm_storage::RocksDBStorage;
use prometheus::{gather, Encoder, TextEncoder};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{error, info};

async fn prometheus_handle(_req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    let encoder = TextEncoder::new();
    let metric_families = gather();
    let mut buffer = vec![];
    encoder.encode(&metric_families, &mut buffer).unwrap();
    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buffer))
        .unwrap();
    Ok(response)
}

pub fn prometheus_build(db: Arc<RocksDBStorage>, addr: String) -> watch::Sender<()> {
    let (tx, mut rx) = watch::channel(());
    let mut interval = interval(std::time::Duration::from_secs(60));
    let mut rx1 = rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = rx1.changed() => {
                    break;
                }
                _ = interval.tick()=> {
                    db.report_cache_usage();
                }
            }
        }
    });
    tokio::spawn(async move {
        let make_svc =
            make_service_fn(|_conn| async { Ok::<_, hyper::Error>(service_fn(prometheus_handle)) });
        let server = Server::bind(&addr.parse().unwrap()).serve(make_svc);
        let graceful = server.with_graceful_shutdown(async move {
            rx.changed().await.ok();
        });
        info!("prometheus server listening on {}", addr);
        if let Err(e) = graceful.await {
            error!("prometheus server error: {}", e);
        }
    });
    tx
}
