use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

/// Spawn a background task that registers this node with rpc-proxy.
///
/// If `admin_url` is empty, returns `None` immediately (feature disabled).
///
/// Otherwise, the task:
/// 1. Calls `POST {admin_url}/chains/{chain_id}/upstreams` with `{"url":"http://{meta}"}`.
/// 2. On failure, waits 60 s and retries indefinitely.
/// 3. After successful registration, it watches the shutdown channel.
/// 4. On shutdown, calls `DELETE {admin_url}/chains/{chain_id}/upstreams/{name}`.
///
/// Returns a `watch::Sender<()>` – drop it or call `.send(())` to trigger deregistration.
pub async fn rpc_proxy_register_build(
    admin_url: String,
    chain_id: String,
    meta: String,
) -> Option<watch::Sender<()>> {
    if admin_url.is_empty() || meta.is_empty() {
        return None;
    }

    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    let client = reqwest::Client::new();

    tokio::spawn(async move {
        let register_url = format!("{}/chains/{}/upstreams", admin_url, chain_id);
        let node_url = format!("http://{}", meta);
        // Use meta (host:port) as the node name for deregistration.
        let node_name = meta.replace(':', "_");
        let deregister_url = format!(
            "{}/chains/{}/upstreams/{}",
            admin_url, chain_id, node_name
        );

        // --- registration loop ---
        loop {
            match client
                .post(&register_url)
                .json(&json!({ "url": node_url }))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!(
                        target: "rpc_proxy_register",
                        "registered with rpc-proxy at {} (chain {})",
                        register_url, chain_id
                    );
                    break;
                }
                Ok(resp) => {
                    warn!(
                        target: "rpc_proxy_register",
                        "rpc-proxy registration returned {}, retrying in 60s",
                        resp.status()
                    );
                }
                Err(e) => {
                    warn!(
                        target: "rpc_proxy_register",
                        "rpc-proxy registration failed: {}, retrying in 60s",
                        e
                    );
                }
            }

            // Wait 60 s, but bail early if shutdown arrives.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                _ = shutdown_rx.changed() => {
                    info!(target: "rpc_proxy_register", "shutdown before registration succeeded, skipping deregister");
                    return;
                }
            }
        }

        // --- wait for shutdown ---
        let _ = shutdown_rx.changed().await;

        // --- deregistration (best-effort) ---
        info!(
            target: "rpc_proxy_register",
            "deregistering from rpc-proxy: DELETE {}",
            deregister_url
        );
        if let Err(e) = client.delete(&deregister_url).send().await {
            warn!(
                target: "rpc_proxy_register",
                "rpc-proxy deregistration failed: {}",
                e
            );
        } else {
            info!(target: "rpc_proxy_register", "deregistered from rpc-proxy");
        }
    });

    Some(shutdown_tx)
}
