//! Differential test for parallel contractMultiCall: the same
//! StateTree served by a serial server and a parallel server (4
//! workers) must produce field-identical responses (excluding
//! per-call wall-clock time_cost) for success-only batches, for
//! fast_fail batches with a mid-list revert (speculative execution +
//! clone-fill), and for non-fast_fail batches containing failures.

use alloy::primitives::keccak256;
use alloy::rpc::types::TransactionRequest;
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    Bytes, CallRequest, CfgEnv, DebankMultiCallResp, IndexValuePair, MainnetSpecId, NewAccount,
    NewCode, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

const CALLS: usize = 30;
const REVERT_AT: usize = 7;

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
    Address::repeat_byte(0x40 + n as u8)
}

/// `PUSH1 0; SLOAD; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN` with
/// distinct trailing padding per contract.
fn sload0_code(n: usize) -> Bytes {
    let mut code = vec![
        0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ];
    code.extend_from_slice(&[n as u8; 4]);
    Bytes::from(code)
}

/// `PUSH1 0; PUSH1 0; REVERT`.
fn revert_code() -> Bytes {
    Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd])
}

/// Everything the wire response carries except time_cost, which is
/// wall-clock and legitimately differs between servers.
fn normalize(resp: &DebankMultiCallResp) -> Vec<(i32, String, Bytes, i64, bool)> {
    resp.results
        .iter()
        .map(|r| {
            (
                r.code,
                r.err.clone(),
                r.result.clone(),
                r.gas_used,
                r.from_cache,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_multicall_matches_serial() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-e2e-multicall-parallel-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let alice = Address::repeat_byte(0x11);
    let db = MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let mut genesis_diff = BlockStorageDiff::default();
    genesis_diff.new_accounts.push(NewAccount {
        address: keccak256(alice.as_slice()),
        balance: U256::from(1_000_000_000_000_000_000u128),
        nonce: 0,
        code_hash: H256::ZERO,
    });
    for n in 0..CALLS {
        let code = if n == REVERT_AT {
            revert_code()
        } else {
            sload0_code(n)
        };
        genesis_diff.new_codes.push(NewCode {
            code_hash: keccak256(&code),
            code: code.clone(),
        });
        genesis_diff.new_accounts.push(NewAccount {
            address: keccak256(contract(n).as_slice()),
            balance: U256::ZERO,
            nonce: 1,
            code_hash: keccak256(&code),
        });
        genesis_diff.storage_diffs.push(AccountStorageDiff {
            address: keccak256(contract(n).as_slice()),
            diffs: vec![IndexValuePair {
                index: keccak256([0u8; 32]),
                value: U256::from(n as u64 + 100),
            }],
        });
    }
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(
        block_info(0, H256::repeat_byte(0xaa), H256::ZERO),
        genesis_diff,
    )
    .unwrap();
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
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

    let cfg = || {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        cfg.chain_id = 1;
        cfg.tx_gas_limit_cap = Some(100_000_000);
        cfg
    };
    let run_args = |builder: ApiBuilder<Arc<StateTree<MultiStorage>>>, addr: &'static str| async move {
        builder
            .build_and_run(
                addr,
                100,
                Duration::from_secs(10),
                false,
                false,
                "e2e-test".to_string(),
                100,
                1024,
            )
            .await
            .unwrap()
    };
    let serial_addr = "127.0.0.1:18552";
    let parallel_addr = "127.0.0.1:18553";
    let serial_handle = run_args(
        ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Mainnet(cfg())),
        serial_addr,
    )
    .await;
    // exec concurrency 8 so the 4-permit acquire_many path is real.
    let parallel_handle = run_args(
        ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Mainnet(cfg()))
            .with_evm_exec_concurrency(8)
            .with_multicall_parallelism(4),
        parallel_addr,
    )
    .await;
    let serial = HttpClientBuilder::default()
        .build(format!("http://{serial_addr}"))
        .unwrap();
    let parallel = HttpClientBuilder::default()
        .build(format!("http://{parallel_addr}"))
        .unwrap();

    let call = |to: Address| CallRequest {
        inner: TransactionRequest::default().from(alice).to(to),
        tempo: None,
    };
    let ok_requests: Vec<CallRequest> = (0..CALLS)
        .filter(|n| *n != REVERT_AT)
        .map(|n| call(contract(n)))
        .collect();
    let all_requests: Vec<CallRequest> = (0..CALLS).map(|n| call(contract(n))).collect();

    for (name, requests, fast_fail) in [
        ("all-success", &ok_requests, None),
        ("fast-fail-mid-revert", &all_requests, Some(true)),
        ("no-fast-fail-with-revert", &all_requests, Some(false)),
    ] {
        let mut resps = Vec::new();
        for client in [&serial, &parallel] {
            resps.push(
                DebankApiClient::contract_multi_call(
                    client,
                    requests.clone(),
                    None,
                    None,
                    None,
                    fast_fail,
                    None,
                    None,
                )
                .await
                .unwrap(),
            );
        }
        let (s, p) = (&resps[0], &resps[1]);
        assert_eq!(normalize(s), normalize(p), "{name}: results diverged");
        assert_eq!(s.stats.success, p.stats.success, "{name}: success flag");
        assert_eq!(s.stats.block_hash, p.stats.block_hash, "{name}");
    }

    // fast_fail semantics inside the parallel response: everything
    // after the revert is a clone of the revert result.
    let resp = DebankApiClient::contract_multi_call(
        &parallel,
        all_requests.clone(),
        None,
        None,
        None,
        Some(true),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!resp.stats.success);
    assert_ne!(resp.results[REVERT_AT].code, 0);
    for n in REVERT_AT + 1..CALLS {
        assert_eq!(resp.results[n].code, resp.results[REVERT_AT].code);
        assert_eq!(resp.results[n].result, resp.results[REVERT_AT].result);
        assert_eq!(resp.results[n].err, resp.results[REVERT_AT].err);
    }
    // Prefix executed normally.
    for n in 0..REVERT_AT {
        assert_eq!(resp.results[n].code, 0, "call {n}");
    }

    // Explicit client opt-out on the parallel server still matches.
    let opt_out = DebankApiClient::contract_multi_call(
        &parallel,
        ok_requests.clone(),
        None,
        None,
        None,
        None,
        Some(false),
        None,
    )
    .await
    .unwrap();
    let baseline = DebankApiClient::contract_multi_call(
        &serial,
        ok_requests.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(normalize(&opt_out), normalize(&baseline));

    serial_handle.stop().unwrap();
    parallel_handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}
