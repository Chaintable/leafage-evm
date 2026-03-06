use anyhow::Context;
use leafage_evm_types::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

const NATIVE_ADDRESS: [u8; 20] = [0xee; 20];
const DEFAULT_MAX_COLLECTED_TOKENS: usize = 50_000;

#[derive(Debug, Serialize, Deserialize)]
struct TokenAddressFile {
    tokens: Vec<Address>,
}

#[derive(Clone)]
pub struct TokenCollector {
    inner: Arc<TokenCollectorInner>,
    add_tx: tokio::sync::mpsc::Sender<Address>,
}

struct TokenCollectorInner {
    addresses: RwLock<HashSet<Address>>,
    file_path: PathBuf,
    max_size: usize,
    current_size: AtomicUsize,
}


impl TokenCollector {
    pub async fn new(file_path: PathBuf) -> anyhow::Result<Self> {
        let mut initial = HashSet::new();

        if file_path.exists() {
            match tokio::fs::read_to_string(&file_path).await {
                Ok(content) => {
                    if let Ok(token_file) = serde_json::from_str::<TokenAddressFile>(&content) {
                        for addr in token_file.tokens {
                            initial.insert(addr);
                        }
                        info!(target: "token_collector", "loaded {} existing token addresses", initial.len());
                    }
                }
                Err(e) => {
                    warn!(target: "token_collector", "failed to read existing token file: {}", e)
                }
            }
        }

        let (add_tx, add_rx) = tokio::sync::mpsc::channel(10_000);
        let (flush_tx, flush_rx) = tokio::sync::mpsc::channel::<()>(1);

        let inner = Arc::new(TokenCollectorInner {
            current_size: AtomicUsize::new(initial.len()),
            addresses: RwLock::new(initial),
            file_path,
            max_size: DEFAULT_MAX_COLLECTED_TOKENS,
        });

        tokio::spawn(Self::add_loop(inner.clone(), add_rx, flush_tx));
        tokio::spawn(Self::flush_loop(inner.clone(), flush_rx));

        Ok(Self { inner, add_tx })
    }

    pub fn maybe_collect_call(&self, to: Option<Address>, data: &[u8]) {
        let Some(to) = to else { return };

        if self.len() >= self.inner.max_size {
            return;
        }

        if to.as_slice() == &NATIVE_ADDRESS || data.len() < 4 {
            return;
        }

        let selector: [u8; 4] = [data[0], data[1], data[2], data[3]];
        if Self::is_erc20_selector(&selector) {
            self.add_address(to)
        }
    }

    fn add_address(&self, address: Address) {
        let _ = self.add_tx.try_send(address);
    }

    pub fn len(&self) -> usize {
        self.inner.current_size.load(Ordering::Relaxed)
    }

    pub async fn get_all(&self) -> Vec<Address> {
        self.inner.addresses.read().await.iter().cloned().collect()
    }

    fn is_erc20_selector(selector: &[u8; 4]) -> bool {
        matches!(
            selector,
            [0x70, 0xa0, 0x82, 0x31] | // balanceOf(address)
            [0xa9, 0x05, 0x9c, 0xbb] | // transfer(address,uint256)
            [0x23, 0xb8, 0x72, 0xdd] | // transferFrom(address,address,uint256)
            [0x09, 0x5e, 0xa7, 0xb3] | // approve(address,uint256)
            [0xdd, 0x62, 0xed, 0x3e] | // allowance(address,address)
            [0x18, 0x16, 0x0d, 0xdd] | // totalSupply()
            [0x31, 0x3c, 0xe5, 0x67] | // decimals()
            [0x06, 0xfd, 0xde, 0x03] | // name()
            [0x95, 0xd8, 0x9b, 0x41] // symbol()
        )
    }

    async fn add_loop(
        inner: Arc<TokenCollectorInner>,
        mut add_rx: tokio::sync::mpsc::Receiver<Address>,
        flush_tx: tokio::sync::mpsc::Sender<()>,
    ) {
        while let Some(address) = add_rx.recv().await {
            if inner.addresses.write().await.insert(address) {
                inner.current_size.fetch_add(1, Ordering::Relaxed);
                let _ = flush_tx.try_send(());
                debug!(target: "token_collector", "collected new token address: {:?}", address);
            }
        }
    }

    async fn flush_to_disk(inner: Arc<TokenCollectorInner>) -> anyhow::Result<()> {
        let addrs: Vec<Address> = {
            let set = inner.addresses.read().await;
            let mut addresses: Vec<Address> = set.iter().cloned().collect();
            addresses.sort();
            addresses
        };

        let json = tokio::task::spawn_blocking(move || {
            serde_json::to_string(&TokenAddressFile { tokens: addrs })
        })
        .await
        .context("spawn_blocking failed")?
        .context("failed to serialize token file")?;

        let tmp_path = inner.file_path.with_extension("json.tmp");

        tokio::fs::write(&tmp_path, &json)
            .await
            .context("failed to write tmp token file")?;

        tokio::fs::rename(&tmp_path, &inner.file_path)
            .await
            .context("failed to rename token file")?;

        info!(target: "token_collector", "flushed token addresses to {}", inner.file_path.display());
        Ok(())
    }

    async fn flush_loop(
        inner: Arc<TokenCollectorInner>,
        mut flush_rx: tokio::sync::mpsc::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut dirty = false;

        loop {
            tokio::select! {
            signal = flush_rx.recv() => {
                    if signal.is_none() {
                        break;
                    }
                    dirty = true
                }

            _ = interval.tick() => {
                    if dirty {
                        if let Err(err) = Self::flush_to_disk(inner.clone()).await {
                            error!(target: "token_collector", "failed to flush token file: {:#}", err);
                        } else {
                            dirty = false;
                        }
                    }
                }
            }
        }
        info!(target: "token_collector", "Shutting down, performing final flush");
        if dirty {
            if let Err(err) = Self::flush_to_disk(inner).await {
                error!(target: "token_collector", "failed to flush on shutdown: {:#}", err);
            }
        }
    }
}
