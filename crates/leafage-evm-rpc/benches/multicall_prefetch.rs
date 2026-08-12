//! End-to-end contractMultiCall benchmark for the account/code
//! prefetch: one 20-call multicall (the leafage-py chunk size) against
//! 20 distinct contracts, each executing `SLOAD(0); RETURN`, served by
//! a real RocksDB-backed StateTree behind a jsonrpsee HTTP server.
//!
//! Two servers isolate the two regimes:
//! - `cold_block_cache_miss`: the CacheDiskLayer moka caches are off,
//!   the DB is flushed to SST, the RocksDB block cache is squeezed to
//!   1MB, and every iteration calls the next of 1000 disjoint 20-
//!   contract sets (~15MB of state), so by the time a set comes around
//!   again its blocks have been evicted — every account/code/storage
//!   read is a real block-cache miss. This is the first-touch path the
//!   prefetch batches: 2 MultiGets instead of 40 scalar point reads
//!   (storage stays on demand).
//! - `warm_cache_enabled`: one fixed set, moka caches on and warmed
//!   after the first iteration, so the prefetch resolves from cache —
//!   this variant guards against a regression on the hot path.
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
use std::cell::Cell;
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
    Address::from_slice(&keccak256((n as u64).to_be_bytes())[..20])
}

/// Runtime code `PUSH1 0; SLOAD; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0;
/// RETURN` plus never-executed distinct LCG-filled padding, sized like
/// a small real contract so the code read dominates the account read
/// and SST blocks don't collapse under compression.
fn sload0_code(n: usize) -> Bytes {
    let mut code = vec![
        0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ];
    let mut x = n as u32 ^ 0x9e37_79b9;
    code.extend((0..500).map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (x >> 16) as u8
    }));
    Bytes::from(code)
}

fn word(n: u64) -> Bytes {
    Bytes::from(U256::from(n).to_be_bytes::<32>().to_vec())
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    client: HttpClient,
    request_sets: Vec<Vec<CallRequest>>,
    ctx: DebankBlockContext,
    _handle: jsonrpsee::server::ServerHandle,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(enable_cache: bool, block_cache_mb: usize, sets: usize, addr: &str) -> Fixture {
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
    for n in 0..sets * CALLS {
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

    let db = MultiStorage::open(
        &dir,
        block_cache_mb,
        StorageKind::Rocksdb,
        false,
        false,
        false,
    )
    .unwrap();
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

    let request_sets = (0..sets)
        .map(|s| {
            (0..CALLS)
                .map(|i| CallRequest {
                    inner: TransactionRequest::default()
                        .from(alice)
                        .to(contract(s * CALLS + i)),
                    tempo: None,
                })
                .collect()
        })
        .collect();
    let ctx = DebankBlockContext {
        block_id: BlockId::Number(BlockNumberOrTag::Number(2)),
        block_type: leafage_evm_types::BlockType::Equals,
    };
    Fixture {
        rt,
        client,
        request_sets,
        ctx,
        _handle: handle,
        dir,
    }
}

fn run_multicall(fixture: &Fixture, set: usize) {
    fixture.rt.block_on(async {
        let resp = DebankApiClient::contract_multi_call(
            &fixture.client,
            fixture.request_sets[set].clone(),
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
        assert_eq!(resp.results[0].result, word((set * CALLS) as u64 + 1));
    })
}

fn bench_multicall(c: &mut Criterion) {
    let mut group = c.benchmark_group("multicall_20");
    group.throughput(Throughput::Elements(CALLS as u64));

    let cold = setup(false, 1, 1000, "127.0.0.1:18582");
    let next = Cell::new(0usize);
    group.bench_function("cold_block_cache_miss", |b| {
        b.iter(|| {
            let set = next.get();
            next.set((set + 1) % cold.request_sets.len());
            run_multicall(&cold, set)
        })
    });
    drop(cold);

    let warm = setup(true, 64, 1, "127.0.0.1:18583");
    group.bench_function("warm_cache_enabled", |b| b.iter(|| run_multicall(&warm, 0)));
    drop(warm);

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50);
    targets = bench_multicall
}
criterion_main!(benches);
