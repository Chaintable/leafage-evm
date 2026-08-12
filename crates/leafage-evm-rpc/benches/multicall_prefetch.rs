//! End-to-end contractMultiCall benchmark for the account/code
//! prefetch: one 20-call multicall (the leafage-py chunk size) against
//! 20 distinct contracts, each executing `SLOAD(0); RETURN`, served by
//! a real RocksDB-backed StateTree behind a jsonrpsee HTTP server.
//!
//! Two servers isolate the two regimes:
//! - `cold_cache_disabled`: the CacheDiskLayer moka caches are off, so
//!   every account/code/storage read reaches RocksDB — the first-touch
//!   miss path the prefetch batches (2 MultiGets instead of 40 scalar
//!   point reads; storage stays on demand). The DB is flushed to SST
//!   and padded with filler keys so reads pay the SST + block-cache
//!   path rather than the memtable.
//! - `warm_cache_enabled`: moka caches on and warmed after the first
//!   iteration, so the prefetch resolves from cache — this variant
//!   guards against a regression on the hot path.
//!
//! Compare against the pre-prefetch baseline:
//!   git stash push -- crates/leafage-evm-rpc/src/api_impl/debank.rs
//!   cargo bench -p leafage-evm-rpc --bench multicall_prefetch -- --save-baseline noprefetch
//!   git stash pop
//!   cargo bench -p leafage-evm-rpc --bench multicall_prefetch -- --baseline noprefetch

use alloy::primitives::keccak256;
use alloy::rpc::types::TransactionRequest;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    Bytes, CallRequest, CfgEnv, DebankBlockContext, IndexValuePair, MainnetSpecId, NewAccount,
    NewCode, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

const CALLS: usize = 20;

fn block_info(number: u64, hash: H256, parent_hash: H256) -> BlockInfo {
    let mut info = BlockInfo {
        inner: Block::empty(Default::default()),
        other: Default::default(),
    };
    info.inner.header.hash = hash;
    info.inner.header.inner.number = number;
    info.inner.header.inner.parent_hash = parent_hash;
    info.inner.header.inner.gas_limit = 30_000_000;
    info
}

fn contract(n: usize) -> Address {
    Address::repeat_byte(0x30 + n as u8)
}

/// Runtime code `PUSH1 0; SLOAD; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0;
/// RETURN` plus never-executed distinct padding, sized like a small
/// real contract so the code read dominates the account read.
fn sload0_code(n: usize) -> Bytes {
    let mut code = vec![
        0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ];
    code.extend_from_slice(&vec![n as u8; 500]);
    Bytes::from(code)
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    client: HttpClient,
    requests: Vec<CallRequest>,
    ctx: DebankBlockContext,
    _handle: jsonrpsee::server::ServerHandle,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(enable_cache: bool, addr: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "leafage-bench-multicall-prefetch-{}-{}",
        std::process::id(),
        enable_cache
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let alice = Address::repeat_byte(0x11);
    let mut genesis = BlockStorageDiff::default();
    genesis.new_accounts.push(NewAccount {
        address: keccak256(alice.as_slice()),
        balance: U256::from(1_000_000_000_000_000_000u128),
        nonce: 0,
        code_hash: H256::ZERO,
    });
    for n in 0..CALLS {
        let code = sload0_code(n);
        genesis.new_codes.push(NewCode {
            code_hash: keccak256(&code),
            code: code.clone(),
        });
        genesis.new_accounts.push(NewAccount {
            address: keccak256(contract(n).as_slice()),
            balance: U256::ZERO,
            nonce: 1,
            code_hash: keccak256(&code),
        });
        genesis.storage_diffs.push(AccountStorageDiff {
            address: keccak256(contract(n).as_slice()),
            diffs: vec![IndexValuePair {
                index: keccak256([0u8; 32]),
                value: U256::from(n as u64 + 1),
            }],
        });
    }
    // Filler keys so lookups walk real SST index/bloom work instead of
    // finding everything in a near-empty database.
    for n in 0..50_000u64 {
        let address = keccak256(n.to_be_bytes());
        genesis.new_accounts.push(NewAccount {
            address,
            balance: U256::from(n),
            nonce: 1,
            code_hash: H256::ZERO,
        });
        genesis.storage_diffs.push(AccountStorageDiff {
            address,
            diffs: vec![IndexValuePair {
                index: keccak256(n.to_le_bytes()),
                value: U256::from(n),
            }],
        });
    }

    let db = MultiStorage::open(&dir, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(block_info(0, H256::repeat_byte(0xaa), H256::ZERO), genesis)
    .unwrap();
    // Move the freshly written state out of the memtable: memtable point
    // reads are so cheap that batching would show nothing.
    if let MultiStorage::RocksDBState(inner) = &db {
        inner.flush_all();
    }
    let tree = Arc::new(
        StateTree::new(
            db,
            StateTreeConfig::new(4, 10000, 10000, 10000, enable_cache),
        )
        .unwrap(),
    );
    tree.update_block(
        block_info(1, H256::repeat_byte(0xbb), H256::repeat_byte(0xaa)),
        BlockStorageDiff::default(),
    )
    .unwrap();
    tree.update_block(
        block_info(2, H256::repeat_byte(0xcc), H256::repeat_byte(0xbb)),
        BlockStorageDiff::default(),
    )
    .unwrap();

    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_base_fee = true;
    cfg.chain_id = 1;
    cfg.tx_gas_limit_cap = Some(100_000_000);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let handle = rt
        .block_on(
            ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Mainnet(cfg)).build_and_run(
                addr,
                100,
                Duration::from_secs(10),
                false,
                false,
                "bench".to_string(),
                100,
                1024,
            ),
        )
        .unwrap();
    let client = HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();

    let requests = (0..CALLS)
        .map(|n| CallRequest {
            inner: TransactionRequest::default().from(alice).to(contract(n)),
            tempo: None,
        })
        .collect();
    let ctx = DebankBlockContext {
        block_id: BlockId::Number(BlockNumberOrTag::Number(2)),
        block_type: leafage_evm_types::BlockType::Equals,
    };
    Fixture {
        rt,
        client,
        requests,
        ctx,
        _handle: handle,
        dir,
    }
}

fn run_multicall(fixture: &Fixture) {
    fixture.rt.block_on(async {
        let resp = DebankApiClient::contract_multi_call(
            &fixture.client,
            fixture.requests.clone(),
            Some(fixture.ctx.clone()),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(resp.stats.success, "{:?}", resp.results);
        assert_eq!(resp.results.len(), CALLS);
    })
}

fn bench_multicall(c: &mut Criterion) {
    let mut group = c.benchmark_group("multicall_20");
    group.throughput(Throughput::Elements(CALLS as u64));

    let cold = setup(false, "127.0.0.1:18582");
    group.bench_function("cold_cache_disabled", |b| b.iter(|| run_multicall(&cold)));
    drop(cold);

    let warm = setup(true, "127.0.0.1:18583");
    group.bench_function("warm_cache_enabled", |b| b.iter(|| run_multicall(&warm)));
    drop(warm);

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50);
    targets = bench_multicall
}
criterion_main!(benches);
