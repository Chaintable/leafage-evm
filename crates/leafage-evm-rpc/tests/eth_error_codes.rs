//! JSON-RPC error codes on the legacy `eth_*` methods, served over a
//! real jsonrpsee HTTP server: execution failures use the DeBank
//! -39000..-39004 codes, and an unknown block is -39006 on a state node
//! (nodex-proxy retries archive) and -39007 on archive.

use alloy::primitives::keccak256;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee::server::ServerHandle;
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, Bytes, CallRequest,
    CfgEnv, DebankBlock, DebankErrorCode, MainnetSpecId, NewAccount, NewCode, H256, U256,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `PUSH1 0; PUSH1 0; REVERT`
const REVERTER: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xfd];
/// `INVALID`
const INVALID: [u8; 1] = [0xfe];
/// `JUMPDEST; PUSH1 0; JUMP`: loops until gas runs out.
const LOOP: [u8; 4] = [0x5b, 0x60, 0x00, 0x56];

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

fn contract(n: u8) -> Address {
    Address::repeat_byte(n)
}

/// Genesis with one contract per runtime code above, plus one empty
/// diff layer on top.
fn open_tree(db_path: &PathBuf, genesis_height: u64) -> Arc<StateTree<MultiStorage>> {
    let _ = std::fs::remove_dir_all(db_path);
    std::fs::create_dir_all(db_path).unwrap();
    let db = MultiStorage::open(db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let mut diff = BlockStorageDiff::default();
    for (n, code) in [
        (0x01, &REVERTER[..]),
        (0x02, &INVALID[..]),
        (0x03, &LOOP[..]),
    ] {
        let code = Bytes::copy_from_slice(code);
        diff.new_codes.push(NewCode {
            code_hash: keccak256(&code),
            code: code.clone(),
        });
        diff.new_accounts.push(NewAccount {
            address: keccak256(contract(n).as_slice()),
            balance: U256::ZERO,
            nonce: 1,
            code_hash: keccak256(&code),
        });
    }
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(
        block_info(genesis_height, H256::repeat_byte(0xaa), H256::ZERO),
        diff,
    )
    .unwrap();
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    tree.update_block(
        block_info(
            genesis_height + 1,
            H256::repeat_byte(0xbb),
            H256::repeat_byte(0xaa),
        ),
        BlockStorageDiff::default(),
    )
    .unwrap();
    tree
}

async fn serve(
    tree: Arc<StateTree<MultiStorage>>,
    is_archive: bool,
    block_zero: Option<DebankBlock>,
    historical_rpc: Option<String>,
) -> (ServerHandle, HttpClient) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
    cfg.disable_balance_check = true;
    cfg.disable_base_fee = true;
    cfg.chain_id = 1;
    serve_configured(
        tree,
        is_archive,
        block_zero,
        historical_rpc,
        addr,
        MultiChainCfgEnv::Mainnet(cfg),
    )
    .await
}

async fn serve_configured(
    tree: Arc<StateTree<MultiStorage>>,
    is_archive: bool,
    block_zero: Option<DebankBlock>,
    historical_rpc: Option<String>,
    addr: std::net::SocketAddr,
    cfg: MultiChainCfgEnv,
) -> (ServerHandle, HttpClient) {
    let handle = ApiBuilder::new(tree, cfg)
        .with_rpc_block_zero(block_zero)
        .with_historical_config(historical_rpc, Some(1))
        .build_and_run(
            &addr.to_string(),
            100,
            Duration::from_secs(10),
            is_archive,
            false,
            "eth-error-codes-test".to_string(),
            100,
            1024,
        )
        .await
        .unwrap();
    let client = HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();
    (handle, client)
}

fn call_to(n: u8) -> CallRequest {
    CallRequest {
        inner: TransactionRequest::default()
            .from(Address::repeat_byte(0x11))
            .to(contract(n))
            .gas_limit(100_000),
        other: Default::default(),
    }
}

fn error_code<T: std::fmt::Debug>(res: Result<T, ClientError>) -> i32 {
    match res {
        Err(ClientError::Call(err)) => err.code(),
        other => panic!("expected a JSON-RPC error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn eth_methods_use_debank_error_codes() {
    let db_path =
        std::env::temp_dir().join(format!("leafage-eth-error-codes-{}", std::process::id()));
    let tree = open_tree(&db_path, 0);
    let latest = BlockId::Number(BlockNumberOrTag::Latest);
    let missing = BlockId::Number(BlockNumberOrTag::Number(100));

    let (state_handle, state) = serve(tree.clone(), false, None, None).await;

    // eth_call execution failures.
    let revert = EthApiClient::call(&state, call_to(0x01), latest, None, None).await;
    match &revert {
        Err(ClientError::Call(err)) => assert_eq!(err.message(), "Reverted: \"\""),
        other => panic!("expected a revert error, got {other:?}"),
    }
    assert_eq!(error_code(revert), -39000);
    let invalid = EthApiClient::call(&state, call_to(0x02), latest, None, None).await;
    assert_eq!(error_code(invalid), -39004);
    let out_of_gas = EthApiClient::call(&state, call_to(0x03), latest, None, None).await;
    assert_eq!(error_code(out_of_gas), -39001);
    // 100 nonzero calldata bytes: intrinsic gas 22600, EIP-7623 floor 25000.
    let below_floor = CallRequest {
        inner: TransactionRequest::default()
            .from(Address::repeat_byte(0x11))
            .to(Address::repeat_byte(0x22))
            .gas_limit(23_000)
            .input(TransactionInput::new(Bytes::from(vec![0xff; 100]))),
        other: Default::default(),
    };
    let below_floor = EthApiClient::call(&state, below_floor, latest, None, None).await;
    match &below_floor {
        Err(ClientError::Call(err)) => assert!(err.message().contains("gas floor"), "{err:?}"),
        other => panic!("expected a gas floor error, got {other:?}"),
    }
    assert_eq!(error_code(below_floor), -39001);

    // No backend serves the safe/finalized/earliest tags: a bad parameter.
    let safe = BlockId::Number(BlockNumberOrTag::Safe);
    let balance = EthApiClient::get_balance(&state, contract(0x01), safe).await;
    assert_eq!(error_code(balance), -32602);

    // A block the state node does not have: -39006 everywhere,
    // including eth_multiCall (previously -32602).
    let multi = EthApiClient::multi_call(&state, vec![call_to(0x01)], missing, None, None, None);
    assert_eq!(error_code(multi.await), -39006);
    let call = EthApiClient::call(&state, call_to(0x01), missing, None, None).await;
    assert_eq!(error_code(call), -39006);
    let balance = EthApiClient::get_balance(&state, contract(0x01), missing).await;
    assert_eq!(error_code(balance), -39006);

    // eth_multiCall keeps its per-call -400xx codes.
    let resp = EthApiClient::multi_call(&state, vec![call_to(0x01)], latest, None, None, None)
        .await
        .unwrap();
    assert_eq!(resp.results[0].code, -40014);

    // Archive has every block, so an unknown one is an invalid id.
    let (archive_handle, archive) = serve(tree, true, None, None).await;
    let multi = EthApiClient::multi_call(&archive, vec![call_to(0x01)], missing, None, None, None);
    assert_eq!(error_code(multi.await), -39007);
    let call = EthApiClient::call(&archive, call_to(0x01), missing, None, None).await;
    assert_eq!(error_code(call), -39007);

    state_handle.stop().unwrap();
    archive_handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}

async fn serve_historical_error(code: DebankErrorCode, message: &str) -> (ServerHandle, String) {
    let error = jsonrpsee::types::ErrorObjectOwned::owned(code as i32, message, None::<()>);
    let mut rpc = jsonrpsee::RpcModule::new(error);
    rpc.register_method("getBlockByHeight", |_, error, _| {
        Err::<DebankBlock, _>(error.clone())
    })
    .unwrap();
    let server = jsonrpsee::server::ServerBuilder::default()
        .build("127.0.0.1:0")
        .await
        .unwrap();
    let url = format!("http://{}", server.local_addr().unwrap());
    (server.start(rpc), url)
}

#[tokio::test]
async fn rpc_block_zero_only_fills_missing_height_zero() {
    let db_path =
        std::env::temp_dir().join(format!("leafage-rpc-block-zero-{}", std::process::id()));
    let missing_tree = open_tree(&db_path.join("missing"), 1);
    let real_tree = open_tree(&db_path.join("real"), 0);
    let placeholder = DebankBlock {
        timestamp: 1_653_498_000,
        base_fee_per_gas: 1_000_000_000_000_000_000_001,
        ..Default::default()
    };
    let expected = serde_json::to_value(&placeholder).unwrap();

    let (historical, historical_url) =
        serve_historical_error(DebankErrorCode::InvalidBlockID, "block height 0 is invalid").await;
    let (upstream, upstream_url) =
        serve_historical_error(DebankErrorCode::DataBaseFailed, "upstream database failed").await;

    for (historical_rpc, expect_failure) in [
        (None, false),
        (Some(historical_url), false),
        (Some(upstream_url), true),
    ] {
        let (handle, client) = serve(
            missing_tree.clone(),
            true,
            Some(placeholder.clone()),
            historical_rpc,
        )
        .await;
        let result = client.get_block_by_height(U256::ZERO).await;
        if expect_failure {
            let Err(ClientError::Call(err)) = result else {
                panic!("upstream failure must reach the client");
            };
            assert!(err.message().contains("upstream database failed"));
        } else {
            assert_eq!(serde_json::to_value(result.unwrap()).unwrap(), expected);
        }
        assert_eq!(
            client.get_block_by_height(U256::from(1)).await.unwrap().id,
            H256::repeat_byte(0xaa)
        );
        assert_eq!(
            error_code(client.get_block_by_height(U256::from(99)).await),
            -39007
        );
        handle.stop().unwrap();
        handle.stopped().await;
    }

    let zero = BlockNumberOrTag::Number(0);
    let (handle, client) =
        serve(missing_tree.clone(), false, Some(placeholder.clone()), None).await;
    let block = client.get_block_by_height(U256::ZERO).await.unwrap();
    assert_eq!(serde_json::to_value(block).unwrap(), expected);
    for full in [false, true] {
        let block = client
            .get_block_by_number(zero, full)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block["number"], "0x0");
        assert_eq!(block["hash"], expected["id"]);
        assert_eq!(block["parentHash"], expected["parent_id"]);
        assert_eq!(block["miner"], expected["miner"]);
        assert_eq!(block["timestamp"], format!("0x{:x}", placeholder.timestamp));
        assert_eq!(
            block["baseFeePerGas"],
            format!("0x{:x}", placeholder.base_fee_per_gas)
        );
        assert_eq!(block["gasLimit"], "0x0");
        assert_eq!(block["gasUsed"], "0x0");
        assert_eq!(block["transactions"], serde_json::json!([]));
    }
    assert_eq!(
        error_code(client.get_block_by_hash(H256::ZERO, false).await),
        -39006
    );
    assert_eq!(
        error_code(
            client
                .get_block_by_number(BlockNumberOrTag::Number(99), false)
                .await
        ),
        -39006
    );
    handle.stop().unwrap();
    handle.stopped().await;

    let (handle, client) = serve(real_tree, false, Some(placeholder), None).await;
    let block = client
        .get_block_by_number(zero, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(block["hash"], serde_json::json!(H256::repeat_byte(0xaa)));
    assert_eq!(
        client.get_block_by_height(U256::ZERO).await.unwrap().id,
        H256::repeat_byte(0xaa)
    );
    handle.stop().unwrap();
    handle.stopped().await;

    let (handle, client) = serve(missing_tree, false, None, None).await;
    assert_eq!(
        error_code(client.get_block_by_number(zero, false).await),
        -39006
    );
    assert_eq!(
        error_code(client.get_block_by_height(U256::ZERO).await),
        -39006
    );
    handle.stop().unwrap();
    handle.stopped().await;

    historical.stop().unwrap();
    upstream.stop().unwrap();
    let _ = std::fs::remove_dir_all(db_path);
}

#[tokio::test]
async fn celo_l1_rejects_oversized_block_fee_overrides() {
    use leafage_evm_chains::celo::{l1::GINGERBREAD_BLOCK, CeloCfgEnv, CELO_CHAIN_ID};
    use leafage_evm_types::BlockOverrides;

    let db_path = std::env::temp_dir().join(format!("leafage-celo-fee-{}", std::process::id()));
    let tree = open_tree(&db_path, GINGERBREAD_BLOCK);
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::LONDON);
    cfg.chain_id = CELO_CHAIN_ID;
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let (handle, client) = serve_configured(
        tree,
        false,
        None,
        None,
        addr,
        MultiChainCfgEnv::Celo(CeloCfgEnv::L1(cfg)),
    )
    .await;
    let overrides = BlockOverrides {
        base_fee: Some(U256::from(u128::MAX) + U256::ONE),
        ..Default::default()
    };
    let call = EthApiClient::call(
        &client,
        call_to(0x01),
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        Some(overrides.clone()),
    )
    .await;
    let estimate =
        DebankApiClient::estimate_gas(&client, call_to(0x01), None, Some(overrides)).await;
    for error in [call.unwrap_err(), estimate.unwrap_err()] {
        let ClientError::Call(error) = error else {
            panic!("expected a JSON-RPC error")
        };
        assert_eq!(error.code(), -32602);
        assert!(error.message().contains("baseFeePerGas exceeds u128"));
    }
    handle.stop().unwrap();
    handle.stopped().await;
    let _ = std::fs::remove_dir_all(db_path);
}

#[tokio::test]
async fn zksync_estimate_rejects_base_fees_outside_its_native_width() {
    use leafage_evm_types::BlockOverrides;

    let db_path = std::env::temp_dir().join(format!("leafage-era-fee-{}", std::process::id()));
    let tree = open_tree(&db_path, 0);
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
    cfg.chain_id = 324;
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let (handle, client) =
        serve_configured(tree, false, None, None, addr, MultiChainCfgEnv::Zksync(cfg)).await;
    let overrides = BlockOverrides {
        base_fee: Some(U256::from(u64::MAX) + U256::ONE),
        ..Default::default()
    };
    let error = DebankApiClient::estimate_gas(&client, call_to(0x01), None, Some(overrides))
        .await
        .unwrap_err();
    let ClientError::Call(error) = error else {
        panic!("expected a JSON-RPC error")
    };
    assert_eq!(error.code(), -32602);
    assert!(error
        .message()
        .contains("ZKsync Era baseFeePerGas exceeds u64"));
    handle.stop().unwrap();
    handle.stopped().await;
    let _ = std::fs::remove_dir_all(db_path);
}
