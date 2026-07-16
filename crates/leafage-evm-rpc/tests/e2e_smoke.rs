//! In-process end-to-end smoke test: a real RocksDB-backed StateTree
//! with in-memory diff layers, served over a real jsonrpsee HTTP server,
//! exercised through the client traits. Covers the multicall shared
//! request cache, the native-token sentinel path and the estimateGas
//! binary search over the request-scoped cache.

use alloy::primitives::keccak256;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, Bytes, CallRequest,
    CfgEnv, MainnetSpecId, NewAccount, H256, U256,
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
    .update_block(genesis, genesis_diff.into())
    .unwrap();

    // Two empty diff layers on top keep reads walking the in-memory chain.
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    tree.update_block(
        block_info(1, h(0xbb), h(0xaa)),
        BlockStorageDiff::default().into(),
    )
    .unwrap();
    tree.update_block(
        block_info(2, h(0xcc), h(0xbb)),
        BlockStorageDiff::default().into(),
    )
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
