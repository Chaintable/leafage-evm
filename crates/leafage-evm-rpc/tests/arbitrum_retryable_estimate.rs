use alloy::primitives::keccak256;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::sol_types::SolCall;
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_chains::arbitrum::arbos_state::ARBOS_STATE_ADDRESS;
use leafage_evm_chains::arbitrum::precompile::NODE_INTERFACE_ADDRESS;
use leafage_evm_chains::arbitrum::ArbitrumHardfork;
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    Bytes, CallRequest, CfgEnv, IndexValuePair, NewAccount, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

alloy::sol! {
    function estimateRetryableTicket(
        address sender,
        uint256 deposit,
        address to,
        uint256 l2CallValue,
        address excessFeeRefundAddress,
        address callValueRefundAddress,
        bytes data
    ) external;
}

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

fn arbos_slot_at(storage_key: &[u8], offset: u64) -> U256 {
    let key = U256::from(offset).to_be_bytes::<32>();
    let mut input = Vec::with_capacity(storage_key.len() + 31);
    input.extend_from_slice(storage_key);
    input.extend_from_slice(&key[..31]);
    let hashed = keccak256(input);
    let mut slot = [0u8; 32];
    slot[..31].copy_from_slice(&hashed[..31]);
    slot[31] = key[31];
    U256::from_be_bytes(slot)
}

fn storage_index(slot: U256) -> H256 {
    keccak256(slot.to_be_bytes::<32>())
}

#[tokio::test(flavor = "multi_thread")]
async fn retryable_estimate_does_not_follow_requested_gas_cap() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-arbitrum-retryable-estimate-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let sender = Address::repeat_byte(0x11);
    let target = Address::repeat_byte(0x22);
    let arbos_address_hash = keccak256(ARBOS_STATE_ADDRESS.as_slice());
    let l2_pricing_key = keccak256([1u8]);

    let db = MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let genesis = block_info(0, H256::repeat_byte(0xaa), H256::ZERO);
    let genesis_diff = BlockStorageDiff {
        new_accounts: vec![
            NewAccount {
                address: keccak256(sender.as_slice()),
                balance: U256::from(1_000_000_000_000_000_000u128),
                nonce: 0,
                code_hash: H256::ZERO,
            },
            NewAccount {
                address: arbos_address_hash,
                balance: U256::ZERO,
                nonce: 0,
                code_hash: H256::ZERO,
            },
        ],
        storage_diffs: vec![AccountStorageDiff {
            address: arbos_address_hash,
            diffs: vec![
                IndexValuePair {
                    index: storage_index(arbos_slot_at(&[], 0)),
                    value: U256::from(51),
                },
                IndexValuePair {
                    index: storage_index(arbos_slot_at(l2_pricing_key.as_slice(), 2)),
                    value: U256::ZERO,
                },
                IndexValuePair {
                    index: storage_index(arbos_slot_at(l2_pricing_key.as_slice(), 7)),
                    value: U256::from(32_000_000u64),
                },
            ],
        }],
        ..Default::default()
    };
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(genesis, genesis_diff)
    .unwrap();

    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    tree.update_block(
        block_info(1, H256::repeat_byte(0xbb), H256::repeat_byte(0xaa)),
        BlockStorageDiff::default(),
    )
    .unwrap();

    let mut cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Prague);
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_base_fee = true;
    cfg.chain_id = 42161;
    cfg.tx_gas_limit_cap = Some(100_000_000);

    let handle = ApiBuilder::new(tree, MultiChainCfgEnv::Arbitrum((cfg, None)))
        .build_and_run(
            "127.0.0.1:18550",
            100,
            Duration::from_secs(10),
            false,
            false,
            "arbitrum-retryable-test".to_string(),
            100,
            1024,
        )
        .await
        .unwrap();
    let client = HttpClientBuilder::default()
        .build("http://127.0.0.1:18550")
        .unwrap();

    let calldata = estimateRetryableTicketCall {
        sender,
        deposit: U256::ZERO,
        to: target,
        l2CallValue: U256::ZERO,
        excessFeeRefundAddress: sender,
        callValueRefundAddress: sender,
        data: Bytes::from_static(&[1, 2, 3, 4]),
    }
    .abi_encode();

    let call_request = CallRequest {
        inner: TransactionRequest::default()
            .from(sender)
            .to(NODE_INTERFACE_ADDRESS)
            .input(TransactionInput::new(Bytes::from(calldata.clone()))),
        tempo: None,
    };
    let ticket_id = EthApiClient::call(
        &client,
        call_request,
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(ticket_id.len(), 32);

    let mut estimates = Vec::new();
    for requested_gas in [None, Some(100_000), Some(1_000_000), Some(5_000_000)] {
        let mut tx = TransactionRequest::default()
            .from(sender)
            .to(NODE_INTERFACE_ADDRESS)
            .input(TransactionInput::new(Bytes::from(calldata.clone())));
        if let Some(gas) = requested_gas {
            tx = tx.gas_limit(gas);
        }
        let estimate = DebankApiClient::estimate_gas(
            &client,
            CallRequest {
                inner: tx,
                tempo: None,
            },
            None,
            None,
        )
        .await
        .unwrap();
        estimates.push(estimate);
    }

    assert!(estimates.iter().all(|estimate| *estimate == estimates[0]));
    assert!(estimates[0] >= U256::from(21_000u64));
    assert!(estimates[0] < U256::from(100_000u64));

    handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}
