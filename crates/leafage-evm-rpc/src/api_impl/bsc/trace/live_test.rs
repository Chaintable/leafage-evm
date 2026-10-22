//! Replay recent BSC blocks against state fetched from a full node and write
//! the output for comparison with the node's own `trace_debankBlock`.
//!
//! ```sh
//! BSC_RPC_URL=http://127.0.0.1:8545 BSC_BLOCKS=12345,12346 \
//! BSC_TRACE_OUT=/tmp/traces \
//!   cargo test -p leafage-evm-rpc --lib live_replay -- --ignored --nocapture
//! ```
//!
//! The node only needs the state of the last few blocks: the touched keys come
//! from `debug_traceBlockByHash` with the prestate tracer and their values are
//! read at the parent (and replayed) height. Keys the replay reads beyond that
//! are fetched and the block is replayed again.
//!
//! For older blocks, `BSC_STATE_URL` reads the state from an archive (e.g. a
//! leafage node) instead and `BSC_PRESTATE=0` skips the prestate tracer: the
//! state is then fetched key by key as the replay reads it. `BSC_STRICT=0`
//! writes the output even when the replay fails its checks.

use super::replay::{replay_range, RangeBlock};
use crate::pipeline::chain::{ReplayOptions, ReplayOutput};
use crate::pipeline::types::DebankBlockOutput;
use alloy::primitives::{Address, Bytes, B256, U256};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::BatchRequestBuilder;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee::rpc_params;
use revm::bytecode::Bytecode;
use revm::database_interface::DBErrorMarker;
use revm::state::AccountInfo;
use revm::DatabaseRef;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
#[error("unreachable")]
struct NoError;
impl DBErrorMarker for NoError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Account(Address),
    Storage(Address, U256),
    BlockHash(u64),
}

/// State at one height; unknown keys read as empty and are recorded.
#[derive(Debug, Default)]
struct MapDb {
    accounts: HashMap<Address, Option<AccountInfo>>,
    storage: HashMap<(Address, U256), U256>,
    codes: HashMap<B256, Bytecode>,
    block_hashes: HashMap<u64, B256>,
    misses: Mutex<BTreeSet<Key>>,
}

impl MapDb {
    fn miss(&self, key: Key) {
        self.misses.lock().unwrap().insert(key);
    }

    fn take_misses(&self) -> Vec<Key> {
        std::mem::take(&mut *self.misses.lock().unwrap())
            .into_iter()
            .collect()
    }
}

impl DatabaseRef for MapDb {
    type Error = NoError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, NoError> {
        match self.accounts.get(&address) {
            Some(info) => Ok(info.clone()),
            None => {
                self.miss(Key::Account(address));
                Ok(None)
            }
        }
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, NoError> {
        Ok(self.codes.get(&code_hash).cloned().unwrap_or_default())
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, NoError> {
        match self.storage.get(&(address, index)) {
            Some(value) => Ok(*value),
            None => {
                self.miss(Key::Storage(address, index));
                Ok(U256::ZERO)
            }
        }
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, NoError> {
        match self.block_hashes.get(&number) {
            Some(hash) => Ok(*hash),
            None => {
                self.miss(Key::BlockHash(number));
                Ok(B256::ZERO)
            }
        }
    }
}

/// State at one height fetched from `BSC_STATE_URL` as the replay reads it.
#[derive(Debug)]
struct LazyDb {
    handle: tokio::runtime::Handle,
    state: HttpClient,
    block: HttpClient,
    height: u64,
    cache: Mutex<MapDb>,
}

impl LazyDb {
    fn get<T>(&self, key: Key, read: impl Fn(&MapDb) -> Option<T>) -> T {
        let mut db = self.cache.lock().unwrap();
        if let Some(value) = read(&db) {
            return value;
        }
        tokio::task::block_in_place(|| {
            self.handle.block_on(fetch(
                &self.state,
                &self.block,
                &mut db,
                self.height,
                &[key],
            ))
        });
        read(&db).expect("fetched key")
    }
}

impl DatabaseRef for LazyDb {
    type Error = NoError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, NoError> {
        Ok(self.get(Key::Account(address), |db| {
            db.accounts.get(&address).cloned()
        }))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, NoError> {
        let db = self.cache.lock().unwrap();
        Ok(db.codes.get(&code_hash).cloned().unwrap_or_default())
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, NoError> {
        Ok(self.get(Key::Storage(address, index), |db| {
            db.storage.get(&(address, index)).copied()
        }))
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, NoError> {
        Ok(self.get(Key::BlockHash(number), |db| {
            db.block_hashes.get(&number).copied()
        }))
    }
}

async fn batch(client: &HttpClient, calls: Vec<(&'static str, Vec<Value>)>) -> Vec<Value> {
    let mut out = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(200) {
        let mut builder = BatchRequestBuilder::new();
        for (method, params) in chunk {
            let mut array = jsonrpsee::core::params::ArrayParams::new();
            for param in params {
                array.insert(param).unwrap();
            }
            builder.insert(method, array).unwrap();
        }
        let response = client
            .batch_request::<Value>(builder)
            .await
            .expect("batch request");
        for item in response.into_iter() {
            out.push(item.expect("batch item"));
        }
    }
    out
}

async fn fetch(
    client: &HttpClient,
    block_client: &HttpClient,
    db: &mut MapDb,
    height: u64,
    keys: &[Key],
) {
    let tag = Value::String(format!("{height:#x}"));
    let mut calls = Vec::new();
    for key in keys {
        match key {
            Key::Account(address) => {
                let address = Value::String(address.to_string());
                calls.push(("eth_getBalance", vec![address.clone(), tag.clone()]));
                calls.push((
                    "eth_getTransactionCount",
                    vec![address.clone(), tag.clone()],
                ));
                calls.push(("eth_getCode", vec![address, tag.clone()]));
            }
            Key::Storage(address, slot) => calls.push((
                "eth_getStorageAt",
                vec![
                    Value::String(address.to_string()),
                    Value::String(format!("{slot:#x}")),
                    tag.clone(),
                ],
            )),
            Key::BlockHash(number) => calls.push((
                "eth_getBlockByNumber",
                vec![Value::String(format!("{number:#x}")), Value::Bool(false)],
            )),
        }
    }
    let (block_calls, state_calls): (Vec<_>, Vec<_>) = calls
        .into_iter()
        .partition(|(method, _)| *method == "eth_getBlockByNumber");
    let mut state_results = batch(client, state_calls).await.into_iter();
    let mut block_results = batch(block_client, block_calls).await.into_iter();
    let mut results = keys.iter().flat_map(|key| match key {
        Key::Account(_) => vec![
            state_results.next().unwrap(),
            state_results.next().unwrap(),
            state_results.next().unwrap(),
        ],
        Key::Storage(..) => vec![state_results.next().unwrap()],
        Key::BlockHash(_) => vec![block_results.next().unwrap()],
    });
    for key in keys {
        match key {
            Key::Account(address) => {
                let balance: U256 = serde_json::from_value(results.next().unwrap()).unwrap();
                let nonce: alloy::primitives::U64 =
                    serde_json::from_value(results.next().unwrap()).unwrap();
                let code: Bytes = serde_json::from_value(results.next().unwrap()).unwrap();
                let info = if balance.is_zero() && nonce.is_zero() && code.is_empty() {
                    None
                } else {
                    let bytecode = Bytecode::new_raw(code);
                    let code_hash = bytecode.hash_slow();
                    db.codes.insert(code_hash, bytecode.clone());
                    Some(AccountInfo {
                        balance,
                        nonce: nonce.to(),
                        code_hash,
                        code: Some(bytecode),
                        ..Default::default()
                    })
                };
                db.accounts.insert(*address, info);
            }
            Key::Storage(address, slot) => {
                let value: U256 = serde_json::from_value(results.next().unwrap()).unwrap();
                db.storage.insert((*address, *slot), value);
            }
            Key::BlockHash(number) => {
                let block = results.next().unwrap();
                let hash: B256 = serde_json::from_value(block["hash"].clone()).unwrap();
                db.block_hashes.insert(*number, hash);
            }
        }
    }
}

/// Keys touched by the block, from the prestate tracer.
async fn prestate_keys(client: &HttpClient, hash: B256) -> Vec<Key> {
    let traces: Vec<Value> = client
        .request(
            "debug_traceBlockByHash",
            rpc_params![hash, serde_json::json!({"tracer": "prestateTracer"})],
        )
        .await
        .expect("prestate trace");
    let mut keys = BTreeSet::new();
    for trace in traces {
        let Some(accounts) = trace["result"].as_object() else {
            continue;
        };
        for (address, account) in accounts {
            let address: Address = address.parse().unwrap();
            keys.insert(Key::Account(address));
            if let Some(storage) = account["storage"].as_object() {
                for slot in storage.keys() {
                    keys.insert(Key::Storage(address, slot.parse().unwrap()));
                }
            }
        }
    }
    keys.into_iter().collect()
}

/// Replay `block` alone on `parent_state`.
fn replay<D>(
    parent_state: D,
    post_state: D,
    parent_timestamp: u64,
    parent_state_root: B256,
    block: alloy::rpc::types::Block,
    strict: bool,
) -> anyhow::Result<ReplayOutput>
where
    D: DatabaseRef + std::fmt::Debug,
    D::Error: Send + Sync + 'static,
{
    let block = RangeBlock {
        post_state,
        parent_state: None,
        parent_timestamp,
        parent_state_root,
        block,
    };
    let mut outputs = replay_range(
        parent_state,
        vec![block],
        ReplayOptions {
            strict,
            check_state: true,
            state_diff: true,
            traces: true,
        },
    )?;
    Ok(outputs.pop().expect("one block replayed"))
}

struct Clients {
    block: HttpClient,
    state: HttpClient,
    prestate: bool,
    strict: bool,
}

async fn replay_live(clients: &Clients, number: u64) -> DebankBlockOutput {
    let client = &clients.block;
    let block: alloy::rpc::types::Block = client
        .request(
            "eth_getBlockByNumber",
            rpc_params![format!("{number:#x}"), true],
        )
        .await
        .expect("block");
    let parent: Value = client
        .request(
            "eth_getBlockByNumber",
            rpc_params![format!("{:#x}", number - 1), false],
        )
        .await
        .expect("parent");
    let parent_timestamp: alloy::primitives::U64 =
        serde_json::from_value(parent["timestamp"].clone()).unwrap();
    let parent_state_root: B256 = serde_json::from_value(parent["stateRoot"].clone()).unwrap();

    if !clients.prestate {
        let lazy = |height| LazyDb {
            handle: tokio::runtime::Handle::current(),
            state: clients.state.clone(),
            block: client.clone(),
            height,
            cache: Mutex::new(MapDb::default()),
        };
        let (parent_db, post_db) = (lazy(number - 1), lazy(number));
        let output = replay(
            &parent_db,
            &post_db,
            parent_timestamp.to(),
            parent_state_root,
            block,
            clients.strict,
        )
        .unwrap_or_else(|err| panic!("block {number}: {err:#}"));
        let validation_hash = output.block_file.validation_hash();
        return DebankBlockOutput {
            block_file: output.block_file,
            header: output.header,
            state_diff: output.state_diff,
            validation_hash,
        };
    }
    let keys = prestate_keys(client, block.header.hash).await;
    let state = &clients.state;
    let mut parent_db = MapDb::default();
    let mut post_db = MapDb::default();
    fetch(state, client, &mut parent_db, number - 1, &keys).await;
    fetch(state, client, &mut post_db, number, &keys).await;

    for round in 0.. {
        let result = replay(
            &parent_db,
            &post_db,
            parent_timestamp.to(),
            parent_state_root,
            block.clone(),
            clients.strict,
        );
        let parent_misses = parent_db.take_misses();
        let post_misses = post_db.take_misses();
        if parent_misses.is_empty() && post_misses.is_empty() {
            let output = result.unwrap_or_else(|err| panic!("block {number}: {err:#}"));
            println!("block {number}: replayed after {round} refetch rounds");
            let validation_hash = output.block_file.validation_hash();
            return DebankBlockOutput {
                block_file: output.block_file,
                header: output.header,
                state_diff: output.state_diff,
                validation_hash,
            };
        }
        eprintln!(
            "block {number} round {round}: {} parent / {} post misses, e.g. {:?}",
            parent_misses.len(),
            post_misses.len(),
            parent_misses
                .iter()
                .chain(&post_misses)
                .take(3)
                .collect::<Vec<_>>()
        );
        assert!(
            round < 200,
            "block {number}: still missing state after {round} rounds"
        );
        fetch(state, client, &mut parent_db, number - 1, &parent_misses).await;
        fetch(state, client, &mut post_db, number, &post_misses).await;
    }
    unreachable!()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs BSC_RPC_URL pointing at a BSC full node"]
async fn live_replay() {
    let url = std::env::var("BSC_RPC_URL").expect("BSC_RPC_URL");
    let blocks = std::env::var("BSC_BLOCKS").expect("BSC_BLOCKS");
    let out_dir = std::env::var("BSC_TRACE_OUT").unwrap_or_else(|_| ".".to_string());
    let state_url = std::env::var("BSC_STATE_URL").unwrap_or_else(|_| url.clone());
    let flag = |name: &str| std::env::var(name).map_or(true, |value| value != "0");
    let build = |url: String| {
        HttpClientBuilder::default()
            .max_response_size(u32::MAX)
            .request_timeout(std::time::Duration::from_secs(600))
            .build(url)
            .unwrap()
    };
    let clients = Clients {
        block: build(url),
        state: build(state_url),
        prestate: flag("BSC_PRESTATE"),
        strict: flag("BSC_STRICT"),
    };
    for number in blocks.split(',').map(|n| n.trim().parse::<u64>().unwrap()) {
        let output = replay_live(&clients, number).await;
        let path = format!("{out_dir}/leafage-{number}.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&output).unwrap()).unwrap();
        println!("block {number}: wrote {path}");
    }
}
