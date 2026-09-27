//! The native-token `balanceOf` shortcut of both multicall endpoints must
//! surface an account read failure instead of answering a zero balance.
//!
//! The failure is a real one: a standard account record read under the
//! Blast codec. The codec is process-wide, so this test has its own binary.

use alloy::primitives::keccak256;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolValue;
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    set_state_diff_codec, EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper,
    StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{
    Address, Block, BlockId, BlockInfo, BlockStorageDiff, Bytes, CallRequest, CfgEnv,
    DebankErrorCode, MainnetSpecId, MultiCallErrorCode, NewAccount, StateDiffCodec, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

const ADDR: &str = "127.0.0.1:18571";

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

fn native_balance_of(user: Address) -> CallRequest {
    let sentinel: Address = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        .parse()
        .unwrap();
    let mut input = vec![0x70, 0xa0, 0x82, 0x31];
    input.extend_from_slice(&[0u8; 12]);
    input.extend_from_slice(user.as_slice());
    CallRequest {
        inner: TransactionRequest::default()
            .to(sentinel)
            .input(Bytes::from(input).into()),
        tempo: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_balance_shortcut_reports_account_read_errors() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-native-balance-errors-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let alice = Address::repeat_byte(0x11);
    let balance = U256::from(123_456u64);
    let db = MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let genesis: BlockStorageDiff = BlockStorageDiff {
        new_accounts: vec![NewAccount {
            address: keccak256(alice.as_slice()),
            balance,
            nonce: 0,
            code_hash: H256::ZERO,
        }],
        ..Default::default()
    };
    StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
        .update_block(
            block_info(0, H256::repeat_byte(0xaa), H256::ZERO),
            genesis.into(),
        )
        .unwrap();
    // Shared cache off: every read decodes the record from disk.
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, false)).unwrap());

    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
    cfg.chain_id = 1;
    cfg.tx_gas_limit_cap = Some(100_000_000);
    let _handle = ApiBuilder::new(tree, MultiChainCfgEnv::Mainnet(cfg))
        .build_and_run(
            ADDR,
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
        .build(format!("http://{ADDR}"))
        .unwrap();
    let request = native_balance_of(alice);

    // Readable record: both shortcuts answer the balance.
    let debank = DebankApiClient::contract_multi_call(
        &client,
        vec![request.clone()],
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(debank.results[0].code, 0);
    assert_eq!(debank.results[0].result, Bytes::from(balance.abi_encode()));
    let eth = EthApiClient::multi_call(
        &client,
        vec![request.clone()],
        BlockId::latest(),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(eth.results[0].code, MultiCallErrorCode::Success as i32);
    assert_eq!(eth.results[0].result, Bytes::from(balance.abi_encode()));

    // Unreadable record: both shortcuts report the failure.
    set_state_diff_codec(StateDiffCodec::BlastV1);
    let debank = DebankApiClient::contract_multi_call(
        &client,
        vec![request.clone()],
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        debank.results[0].code,
        DebankErrorCode::DataBaseFailed as i32
    );
    assert!(!debank.results[0].err.is_empty());
    let eth = EthApiClient::multi_call(&client, vec![request], BlockId::latest(), None, None, None)
        .await
        .unwrap();
    assert_eq!(
        eth.results[0].code,
        MultiCallErrorCode::NativeMethodStateError as i32
    );
    assert!(!eth.results[0].err.is_empty());

    let _ = std::fs::remove_dir_all(&db_path);
}
