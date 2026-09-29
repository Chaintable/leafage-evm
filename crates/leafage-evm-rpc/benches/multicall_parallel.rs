//! Serial vs parallel contractMultiCall execution (4 workers), same
//! 20-call shape as the prefetch bench. Parallelism is a server-side
//! knob, so one run hosts both variants and compares directly:
//!
//! - `cold_miss`: rotating disjoint contract sets behind a 1MB block
//!   cache with moka off — every read is a block-cache miss, calls are
//!   read-bound.
//! - `cpu_warm`: 20 calls into a ~8k-iteration loop contract with moka
//!   on — calls are compute-bound, the shape parallelism is for.
//! - `tiny_warm`: 20 `SLOAD(0); RETURN` calls with moka on — the worst
//!   case for parallelism, where per-call work is ~µs and scoped-thread
//!   spawn overhead has nothing to hide behind.
//!
//! Run: cargo bench -p leafage-evm-rpc --bench multicall_parallel

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

fn burner(n: usize) -> Address {
    Address::repeat_byte(0x20 + n as u8)
}

/// `PUSH1 0; SLOAD; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN` plus
/// distinct LCG-filled padding.
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

/// `PUSH2 0x2000; JUMPDEST; PUSH1 1; SWAP1; SUB; DUP1; PUSH1 3; JUMPI;
/// STOP` — an 8192-iteration countdown loop (~210k gas), with a
/// distinct trailing byte per contract for unique code hashes.
fn loop_code(n: usize) -> Bytes {
    let mut code = vec![
        0x61, 0x20, 0x00, 0x5b, 0x60, 0x01, 0x90, 0x03, 0x80, 0x60, 0x03, 0x57, 0x00,
    ];
    code.push(n as u8);
    Bytes::from(code)
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    client: HttpClient,
    sload_sets: Vec<Vec<CallRequest>>,
    burn_requests: Vec<CallRequest>,
    ctx: DebankBlockContext,
    _handle: jsonrpsee::server::ServerHandle,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(
    tag: &str,
    enable_cache: bool,
    block_cache_mb: usize,
    sets: usize,
    parallelism: usize,
    addr: &str,
) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "leafage-bench-multicall-parallel-{}-{}",
        std::process::id(),
        tag
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
    for n in 0..CALLS {
        let code = loop_code(n);
        genesis.new_codes.push(NewCode {
            code_hash: keccak256(&code),
            code: code.clone(),
        });
        genesis.new_accounts.push(NewAccount {
            address: keccak256(burner(n).as_slice()),
            balance: U256::ZERO,
            nonce: 1,
            code_hash: keccak256(&code),
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
            ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Mainnet(cfg))
                .with_multicall_parallelism(parallelism)
                .build_and_run(
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

    let call = |to: Address| CallRequest {
        inner: TransactionRequest::default().from(alice).to(to),
        tempo: None,
    };
    let sload_sets = (0..sets)
        .map(|s| (0..CALLS).map(|i| call(contract(s * CALLS + i))).collect())
        .collect();
    let burn_requests = (0..CALLS).map(|i| call(burner(i))).collect();
    let ctx = DebankBlockContext {
        block_id: BlockId::Number(BlockNumberOrTag::Number(2)),
        block_type: leafage_evm_types::BlockType::Equals,
    };
    Fixture {
        rt,
        client,
        sload_sets,
        burn_requests,
        ctx,
        _handle: handle,
        dir,
    }
}

fn run_multicall(fixture: &Fixture, requests: &[CallRequest]) {
    fixture.rt.block_on(async {
        let resp = DebankApiClient::contract_multi_call(
            &fixture.client,
            requests.to_vec(),
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

fn bench_rotating(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    fixture: &Fixture,
) {
    let next = Cell::new(0usize);
    group.bench_function(name, |b| {
        b.iter(|| {
            let set = next.get();
            next.set((set + 1) % fixture.sload_sets.len());
            run_multicall(fixture, &fixture.sload_sets[set])
        })
    });
}

fn bench_multicall_parallel(c: &mut Criterion) {
    let mut group = c.benchmark_group("multicall_20_cold_miss");
    group.throughput(Throughput::Elements(CALLS as u64));
    let serial = setup("cold-serial", false, 1, 1000, 0, "127.0.0.1:18584");
    bench_rotating(&mut group, "serial", &serial);
    drop(serial);
    let par = setup("cold-par4", false, 1, 1000, 4, "127.0.0.1:18585");
    bench_rotating(&mut group, "parallel4", &par);
    drop(par);
    group.finish();

    let serial = setup("warm-serial", true, 64, 1, 0, "127.0.0.1:18586");
    let par = setup("warm-par4", true, 64, 1, 4, "127.0.0.1:18587");

    let mut group = c.benchmark_group("multicall_20_cpu_warm");
    group.throughput(Throughput::Elements(CALLS as u64));
    group.bench_function("serial", |b| {
        b.iter(|| run_multicall(&serial, &serial.burn_requests))
    });
    group.bench_function("parallel4", |b| {
        b.iter(|| run_multicall(&par, &par.burn_requests))
    });
    group.finish();

    let mut group = c.benchmark_group("multicall_20_tiny_warm");
    group.throughput(Throughput::Elements(CALLS as u64));
    group.bench_function("serial", |b| {
        b.iter(|| run_multicall(&serial, &serial.sload_sets[0]))
    });
    group.bench_function("parallel4", |b| {
        b.iter(|| run_multicall(&par, &par.sload_sets[0]))
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50);
    targets = bench_multicall_parallel
}
criterion_main!(benches);
