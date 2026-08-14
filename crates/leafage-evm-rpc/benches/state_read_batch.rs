//! End-to-end comparison of the spec's state-read workload (15 x
//! getAddressCode + 14 x getStorageAt on one fixed block) sent as 29
//! concurrent single requests vs one blockx_stateReadBatch, against a
//! real RocksDB-backed StateTree behind a jsonrpsee HTTP server on
//! localhost. After warmup both variants are served from the shared
//! cache, so the difference isolates per-request overhead: HTTP round
//! trips, handler dispatch, per-request state_at and blocking-pool
//! hops — exactly what the batch amortizes.
//!
//! Run: cargo bench -p leafage-evm-rpc --bench state_read_batch

use alloy::primitives::keccak256;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{ApiBuilder, BlockxApiClient, DebankApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    BlockType, BsrbContext, BsrbRead, BsrbRequest, BsrbResponse, Bytes, CfgEnv, DebankBlockContext,
    IndexValuePair, JsonStorageKey, MainnetSpecId, NewAccount, NewCode, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

const CODE_READS: usize = 15;
const STORAGE_READS: usize = 14;

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

fn position(n: usize) -> H256 {
    H256::with_last_byte(n as u8 + 1)
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    client: HttpClient,
    ctx: DebankBlockContext,
    batch: BsrbRequest,
    _handle: jsonrpsee::server::ServerHandle,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "leafage-bench-state-read-batch-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 15 contracts sharing 3 distinct codes (proxy-heavy workloads
    // repeat code hashes), storage slots spread over 3 contracts.
    let mut genesis = BlockStorageDiff::default();
    let codes: Vec<Bytes> = (0..3u8).map(|n| Bytes::from(vec![0x60 + n; 256])).collect();
    for code in &codes {
        genesis.new_codes.push(NewCode {
            code_hash: keccak256(code),
            code: code.clone(),
        });
    }
    for n in 0..CODE_READS {
        genesis.new_accounts.push(NewAccount {
            address: keccak256(contract(n).as_slice()),
            balance: U256::ZERO,
            nonce: 1,
            code_hash: keccak256(&codes[n % codes.len()]),
        });
    }
    let mut block1 = BlockStorageDiff::default();
    for n in 0..STORAGE_READS {
        block1.storage_diffs.push(AccountStorageDiff {
            address: keccak256(contract(n % 3).as_slice()),
            diffs: vec![IndexValuePair {
                index: keccak256(position(n).as_slice()),
                value: U256::from(n as u64 + 1),
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
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 10000, 10000, 10000, true)).unwrap());
    tree.update_block(
        block_info(1, H256::repeat_byte(0xbb), H256::repeat_byte(0xaa)),
        block1,
    )
    .unwrap();
    tree.update_block(
        block_info(2, H256::repeat_byte(0xcc), H256::repeat_byte(0xbb)),
        BlockStorageDiff::default(),
    )
    .unwrap();

    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
    cfg.chain_id = 1;
    let addr = "127.0.0.1:18581";
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

    let ctx = DebankBlockContext {
        block_id: BlockId::Number(BlockNumberOrTag::Number(2)),
        block_type: BlockType::Equals,
    };
    let mut reads = Vec::new();
    for n in 0..CODE_READS {
        reads.push(BsrbRead::AddressCode {
            address: contract(n),
        });
    }
    for n in 0..STORAGE_READS {
        reads.push(BsrbRead::StorageAt {
            address: contract(n % 3),
            slot: position(n),
        });
    }
    let batch = BsrbRequest {
        context: BsrbContext::Number(2),
        reads,
    };
    Fixture {
        rt,
        client,
        ctx,
        batch,
        _handle: handle,
        dir,
    }
}

fn bench_state_read_workload(c: &mut Criterion) {
    let fixture = setup();
    let mut group = c.benchmark_group("state_read_workload_29");
    group.throughput(Throughput::Elements((CODE_READS + STORAGE_READS) as u64));

    group.bench_function("singles_concurrent", |b| {
        b.iter(|| {
            fixture.rt.block_on(async {
                let code_calls = (0..CODE_READS).map(|n| {
                    DebankApiClient::get_address_code(
                        &fixture.client,
                        contract(n),
                        Some(fixture.ctx.clone()),
                    )
                });
                let storage_calls = (0..STORAGE_READS).map(|n| {
                    DebankApiClient::get_storage_at(
                        &fixture.client,
                        contract(n % 3),
                        JsonStorageKey::from(position(n)),
                        Some(fixture.ctx.clone()),
                    )
                });
                let (codes, storages) = tokio::join!(
                    futures::future::join_all(code_calls),
                    futures::future::join_all(storage_calls)
                );
                for code in codes {
                    code.unwrap();
                }
                for storage in storages {
                    storage.unwrap();
                }
            })
        })
    });

    group.bench_function("blockx_state_read_batch", |b| {
        b.iter(|| {
            fixture.rt.block_on(async {
                // Encoding is part of the per-request client cost.
                let payload = Bytes::from(fixture.batch.encode());
                let resp = BlockxApiClient::state_read_batch(&fixture.client, payload)
                    .await
                    .unwrap();
                let resp = BsrbResponse::decode(&resp).unwrap();
                assert_eq!(resp.results.len(), CODE_READS + STORAGE_READS);
            })
        })
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50);
    targets = bench_state_read_workload
}
criterion_main!(benches);
