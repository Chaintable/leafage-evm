//! In-process end-to-end smoke test: a real RocksDB-backed StateTree
//! with in-memory diff layers, served over a real jsonrpsee HTTP server,
//! exercised through the client traits. Covers the multicall shared
//! request cache, the native-token sentinel path and the estimateGas
//! binary search over the request-scoped cache.

use alloy::primitives::keccak256;
use alloy::rpc::types::state::{AccountOverride, StateOverride};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    Bytes, CallRequest, CfgEnv, IndexValuePair, MainnetSpecId, NewAccount, NewCode, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

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

fn h(n: u8) -> H256 {
    H256::repeat_byte(n)
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_smoke_over_layered_state() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-e2e-smoke-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let alice = Address::repeat_byte(0x11);
    let bob = Address::repeat_byte(0x22);

    // Genesis holds alice with 1 ETH; committed straight to the DB.
    let db = MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let mut genesis_diff = BlockStorageDiff::default();
    genesis_diff.new_accounts.push(NewAccount {
        address: keccak256(alice.as_slice()),
        balance: U256::from(ONE_ETH),
        nonce: 0,
        code_hash: H256::ZERO,
    });
    let genesis = block_info(0, h(0xaa), H256::ZERO);
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(genesis, genesis_diff)
    .unwrap();

    // Two empty diff layers on top keep reads walking the in-memory chain.
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    tree.update_block(block_info(1, h(0xbb), h(0xaa)), BlockStorageDiff::default())
        .unwrap();
    tree.update_block(block_info(2, h(0xcc), h(0xbb)), BlockStorageDiff::default())
        .unwrap();

    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_base_fee = true;
    cfg.chain_id = 1;
    cfg.tx_gas_limit_cap = Some(100_000_000);

    let addr = "127.0.0.1:18549";
    // Cap EVM execution at 2 so requests exercise the limiter path.
    let handle = ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Mainnet(cfg))
        .with_evm_exec_concurrency(2)
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
        .unwrap();

    let client = HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();

    assert_eq!(
        EthApiClient::chain_id(&client).await.unwrap(),
        U256::from(1u64)
    );

    let latest = DebankApiClient::get_latest_block(&client).await.unwrap();
    assert_eq!(latest.height, 2u64);

    // balanceOf(alice) against the native-token sentinel.
    let mut balance_of = vec![0x70u8, 0xa0, 0x82, 0x31, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    balance_of.extend_from_slice(alice.as_slice());
    let sentinel: Address = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        .parse()
        .unwrap();
    let balance_req = CallRequest {
        inner: TransactionRequest::default()
            .to(sentinel)
            .input(TransactionInput::new(Bytes::from(balance_of))),
        tempo: None,
    };
    // Plain value transfer to an empty account (goes through the EVM).
    let transfer_req = CallRequest {
        inner: TransactionRequest::default()
            .from(alice)
            .to(bob)
            .value(U256::from(1u64)),
        tempo: None,
    };

    // The repeated balance call exercises the shared request cache.
    let resp = DebankApiClient::contract_multi_call(
        &client,
        vec![
            balance_req.clone(),
            transfer_req.clone(),
            balance_req.clone(),
        ],
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(resp.stats.success, "multicall failed: {:?}", resp.results);
    assert_eq!(resp.results.len(), 3);
    let expected_balance = Bytes::from(U256::from(ONE_ETH).to_be_bytes::<32>().to_vec());
    assert_eq!(resp.results[0].result, expected_balance);
    assert_eq!(resp.results[2].result, expected_balance);
    assert_eq!(resp.results[1].code, 0);

    // estimateGas of a plain transfer resolves to the intrinsic cost.
    let gas = DebankApiClient::estimate_gas(&client, transfer_req, None, None)
        .await
        .unwrap();
    assert_eq!(gas, U256::from(21_000u64));

    handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}

/// Runtime code `PUSH1 0; SLOAD; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0;
/// RETURN` returning storage slot 0, plus trailing never-executed
/// bytes so each contract gets a distinct code hash.
fn sload0_code(n: u8) -> Bytes {
    let mut code = vec![
        0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ];
    code.extend_from_slice(&[n; 4]);
    Bytes::from(code)
}

fn word(n: u64) -> Bytes {
    Bytes::from(U256::from(n).to_be_bytes::<32>().to_vec())
}

/// Multicall over real contracts: the account/code prefetch that warms
/// the request cache before the serial call loop must return the same
/// results as the on-demand path, and must never replace entries put in
/// place by state overrides (code override and storage-diff override).
#[tokio::test(flavor = "multi_thread")]
async fn multicall_prefetch_matches_scalar_semantics() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-e2e-multicall-prefetch-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let alice = Address::repeat_byte(0x11);
    let bob = Address::repeat_byte(0x22);
    let contract = |n: u8| Address::repeat_byte(0x40 + n);

    // Genesis: 6 contracts, each with distinct code and storage slot
    // 0 = n + 100, committed straight to the DB. Alice deliberately
    // lands in the in-memory diff layer of block 1 instead: the DB
    // decode path rewrites a zero code hash to KECCAK_EMPTY, but diff
    // layers serve the raw `NewAccount` value, which is where the
    // prefetch/lazy divergence is observable.
    let db = MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let mut genesis_diff = BlockStorageDiff::default();
    for n in 0..6u8 {
        let code = sload0_code(n);
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
    // contract(6): `CALLER; EXTCODEHASH; MSTORE; RETURN` — alice is
    // stored with a raw zero code hash, and the prefetch must expose it
    // to the EVM unchanged instead of normalizing it to KECCAK_EMPTY
    // the way the lazy load_account path never does.
    let extcodehash_code = Bytes::from(vec![
        0x33, 0x3f, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ]);
    genesis_diff.new_codes.push(NewCode {
        code_hash: keccak256(&extcodehash_code),
        code: extcodehash_code.clone(),
    });
    genesis_diff.new_accounts.push(NewAccount {
        address: keccak256(contract(6).as_slice()),
        balance: U256::ZERO,
        nonce: 1,
        code_hash: keccak256(&extcodehash_code),
    });
    let genesis = block_info(0, h(0xaa), H256::ZERO);
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(genesis, genesis_diff)
    .unwrap();

    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    // Alice — an EOA whose stored code hash is literally zero — enters
    // through block 1's diff so reads hit the diff layer, not the DB.
    let mut layer1_diff = BlockStorageDiff::default();
    layer1_diff.new_accounts.push(NewAccount {
        address: keccak256(alice.as_slice()),
        balance: U256::from(ONE_ETH),
        nonce: 0,
        code_hash: H256::ZERO,
    });
    tree.update_block(block_info(1, h(0xbb), h(0xaa)), layer1_diff)
        .unwrap();
    tree.update_block(block_info(2, h(0xcc), h(0xbb)), BlockStorageDiff::default())
        .unwrap();

    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_base_fee = true;
    cfg.chain_id = 1;
    cfg.tx_gas_limit_cap = Some(100_000_000);

    let addr = "127.0.0.1:18551";
    let handle = ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Mainnet(cfg))
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
        .unwrap();
    let client = HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();

    // contract(0): code overridden to `PUSH1 42; ... RETURN` -> 42.
    // contract(1): storage slot 0 overridden via state_diff -> 999.
    let mut overrides = StateOverride::default();
    overrides.insert(
        contract(0),
        AccountOverride {
            code: Some(Bytes::from(vec![
                0x60, 0x2a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
            ])),
            ..Default::default()
        },
    );
    let mut slot_diff = alloy::primitives::map::B256HashMap::default();
    slot_diff.insert(H256::ZERO, U256::from(999u64).into());
    overrides.insert(
        contract(1),
        AccountOverride {
            state_diff: Some(slot_diff),
            ..Default::default()
        },
    );

    let call = |to: Address| CallRequest {
        inner: TransactionRequest::default().from(alice).to(to),
        tempo: None,
    };
    let mut requests: Vec<CallRequest> = (0..6u8).map(|n| call(contract(n))).collect();
    // An EOA callee (no code) and a value transfer to a nonexistent
    // account keep None/empty accounts on the prefetch path.
    requests.push(call(alice));
    requests.push(CallRequest {
        inner: TransactionRequest::default()
            .from(alice)
            .to(bob)
            .value(U256::from(1u64)),
        tempo: None,
    });
    // EXTCODEHASH(CALLER) with alice as caller: her stored code hash
    // is literally zero and must survive the prefetch unnormalized.
    requests.push(call(contract(6)));

    let resp = DebankApiClient::contract_multi_call(
        &client,
        requests,
        None,
        None,
        Some(overrides),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(resp.stats.success, "multicall failed: {:?}", resp.results);
    assert_eq!(resp.results.len(), 9);
    assert_eq!(resp.results[0].result, word(42), "code override clobbered");
    assert_eq!(
        resp.results[1].result,
        word(999),
        "storage override clobbered"
    );
    for n in 2..6usize {
        assert_eq!(resp.results[n].result, word(n as u64 + 100), "call {n}");
    }
    assert_eq!(resp.results[6].result, Bytes::default());
    assert_eq!(resp.results[7].code, 0);
    // eth_call never prefetches, so it pins the lazy load_account
    // semantics the prefetched multicall must reproduce byte for byte.
    let lazy = EthApiClient::call(
        &client,
        call(contract(6)),
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        resp.results[8].result, lazy,
        "prefetched EXTCODEHASH diverged from the lazy path"
    );
    assert_eq!(
        resp.results[8].result,
        word(0),
        "zero code_hash was normalized on the way into the cache"
    );

    handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}
