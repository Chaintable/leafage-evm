pub(crate) mod bench;
pub(crate) mod export;
pub(crate) mod stress;
pub(crate) mod summary;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::corpus::{ClassLabel, Corpus, CorpusCase};
use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::EthApiClient;
use leafage_evm_types::{BlockId, Bytes};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::str::FromStr;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

// ─── shared types ────────────────────────────────────────────

#[derive(Debug)]
pub struct CaseResult {
    #[allow(dead_code)]
    pub case_id: String,
    pub label: ClassLabel,
    pub latency: Duration,
    pub outcome: Outcome,
}

pub type Outcome = std::result::Result<Bytes, jsonrpsee::core::client::error::Error>;

impl CaseResult {
    pub fn is_ok(&self) -> bool {
        match self.outcome {
            Ok(_) => true,
            Err(ref err) => match err {
                jsonrpsee::core::client::error::Error::Call(call_err) => {
                    let msg = call_err.message();
                    msg.contains("execution reverted") || msg.contains("Reverted")
                }
                _ => false,
            },
        }
    }
}

// ─── shared helpers ──────────────────────────────────────────

/// Build an HTTP JSON-RPC client with the default 30 s timeout.
pub(crate) fn build_client(url: &str) -> Result<HttpClient> {
    Ok(HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(30))
        .build(url)?)
}

/// Load the corpus, apply an optional label filter and optional shuffle.
pub(crate) fn prepare_corpus(
    path: &std::path::Path,
    label: Option<&str>,
    seed: Option<u64>,
) -> Result<Corpus> {
    let label = label.and_then(|l| ClassLabel::from_str(l).ok());
    let mut corpus = Corpus::load(path)?;
    corpus.filter_label(label);
    if let Some(seed) = seed {
        let mut rng = StdRng::seed_from_u64(seed);
        corpus.cases.shuffle(&mut rng);
    }
    Ok(corpus)
}

/// Dispatch all cases to `client` with bounded concurrency.
pub(crate) async fn run_cases(
    client: HttpClient,
    cases: Vec<CorpusCase>,
    concurrency: usize,
    total_requests: usize,
) -> Result<(Vec<CaseResult>, Duration)> {
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    let wall_start = Instant::now();

    for i in 0..total_requests {
        let case = &cases[i % cases.len()];
        let client = client.clone();
        let sem = Arc::clone(&sem);
        let case_id = case.case_id.clone();
        let label = case.classification.label;
        let request = case.request.clone();
        let block_id = BlockId::latest();

        set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            let start = Instant::now();
            let outcome = client.call(request, block_id, None, None).await;
            Ok::<CaseResult, anyhow::Error>(CaseResult {
                case_id,
                label,
                latency: start.elapsed(),
                outcome,
            })
        });
    }

    let mut results = Vec::with_capacity(total_requests);
    while let Some(res) = set.join_next().await {
        results.push(res??);
    }

    Ok((results, wall_start.elapsed()))
}

