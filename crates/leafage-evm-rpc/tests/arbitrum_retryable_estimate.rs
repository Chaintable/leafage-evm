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
    Bytes, CallRequest, CfgEnv, IndexValuePair, NewAccount, NewCode, H256, U256,
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
    info.inner.header.inner.base_fee_per_gas = Some(1);
    info.inner.header.inner.difficulty = U256::ONE;
    info.inner.header.inner.extra_data = Bytes::from(vec![0; 32]);
    let mut mix_hash = [0u8; 32];
    mix_hash[16..24].copy_from_slice(&51u64.to_be_bytes());
    info.inner.header.inner.mix_hash = H256::from(mix_hash);
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
async fn arbitrum_rpc_estimation_matches_nitro_gas_semantics() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-arbitrum-retryable-estimate-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let sender = Address::repeat_byte(0x11);
    let target = Address::repeat_byte(0x22);
    let reverting_target = Address::repeat_byte(0x33);
    let high_gas_target = Address::repeat_byte(0x44);
    let target_code = Bytes::from_static(&[
        0x36, 0x15, 0x60, 0x12, 0x57, 0x60, 0x2a, 0x5f, 0x55, 0x60, 0x01, 0x5f, 0x53, 0x60, 0x01,
        0x5f, 0xa0, 0x00, 0x5b, 0x5f, 0x54, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
    ]);
    let reverting_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]);
    // Expand memory to 96,000 words: 18,288,000 execution gas plus intrinsic gas.
    let high_gas_code = Bytes::from_static(&[0x5f, 0x62, 0x2e, 0xdf, 0xe0, 0x52, 0x00]);
    let target_code_hash = keccak256(target_code.as_ref());
    let reverting_code_hash = keccak256(reverting_code.as_ref());
    let high_gas_code_hash = keccak256(high_gas_code.as_ref());
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
            NewAccount {
                address: keccak256(target.as_slice()),
                balance: U256::ZERO,
                nonce: 1,
                code_hash: target_code_hash,
            },
            NewAccount {
                address: keccak256(reverting_target.as_slice()),
                balance: U256::ZERO,
                nonce: 1,
                code_hash: reverting_code_hash,
            },
            NewAccount {
                address: keccak256(high_gas_target.as_slice()),
                balance: U256::ZERO,
                nonce: 1,
                code_hash: high_gas_code_hash,
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
        new_codes: vec![
            NewCode {
                code_hash: target_code_hash,
                code: target_code,
            },
            NewCode {
                code_hash: reverting_code_hash,
                code: reverting_code,
            },
            NewCode {
                code_hash: high_gas_code_hash,
                code: high_gas_code,
            },
        ],
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

    let handle = ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Arbitrum((cfg, None)))
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
        call_request.clone(),
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(ticket_id.len(), 32);

    let read_request = CallRequest {
        inner: TransactionRequest::default().from(sender).to(target),
        tempo: None,
    };
    let simulation = DebankApiClient::simulate_transactions(
        &client,
        vec![call_request, read_request],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(simulation.stats.success);
    assert_eq!(simulation.results.len(), 2);
    assert_eq!(simulation.results[0].code, 0);
    assert_eq!(simulation.results[1].code, 0);
    assert!(simulation.results[0].gas_used > 21_000);
    assert!(simulation.results[0].gas_used < 100_000);
    assert!(simulation.results[0]
        .traces
        .iter()
        .any(|trace| trace.to_addr == NODE_INTERFACE_ADDRESS));
    assert!(simulation.results[0]
        .traces
        .iter()
        .any(|trace| trace.to_addr == target));
    assert_eq!(simulation.results[0].events.len(), 3);
    let read_trace = simulation.results[1]
        .traces
        .iter()
        .find(|trace| trace.to_addr == target)
        .expect("second simulation should execute the target contract");
    assert_eq!(read_trace.output.len(), 32);
    assert_eq!(read_trace.output[31], 0x2a);

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
    assert!(estimates[0] > U256::from(21_000u64), "{estimates:?}");
    assert!(estimates[0] < U256::from(100_000u64));

    let low_gas_request = CallRequest {
        inner: TransactionRequest::default()
            .from(sender)
            .to(NODE_INTERFACE_ADDRESS)
            .gas_limit(21_000)
            .input(TransactionInput::new(Bytes::from(calldata.clone()))),
        tempo: None,
    };
    assert!(EthApiClient::call(
        &client,
        low_gas_request,
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .is_err());

    let reverting_calldata = estimateRetryableTicketCall {
        sender,
        deposit: U256::ZERO,
        to: reverting_target,
        l2CallValue: U256::ZERO,
        excessFeeRefundAddress: sender,
        callValueRefundAddress: sender,
        data: Bytes::new(),
    }
    .abi_encode();
    let reverting_request = CallRequest {
        inner: TransactionRequest::default()
            .from(sender)
            .to(NODE_INTERFACE_ADDRESS)
            .input(TransactionInput::new(Bytes::from(reverting_calldata))),
        tempo: None,
    };
    assert!(EthApiClient::call(
        &client,
        reverting_request.clone(),
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .is_err());
    assert!(
        DebankApiClient::estimate_gas(&client, reverting_request.clone(), None, None,)
            .await
            .is_err()
    );

    let reverting_simulation =
        DebankApiClient::simulate_transactions(&client, vec![reverting_request], None, None)
            .await
            .unwrap();
    assert!(!reverting_simulation.stats.success);
    assert_ne!(reverting_simulation.results[0].code, 0);
    assert!(reverting_simulation.results[0]
        .traces
        .iter()
        .any(|trace| trace.to_addr == NODE_INTERFACE_ADDRESS));

    let above_eip7825_cap = CallRequest {
        inner: TransactionRequest::default()
            .from(sender)
            .to(target)
            // Hood tx 0x2f21d99a...aaa3325 was included successfully with this
            // declared gas limit and replays successfully against Nitro at its parent block.
            .gas_limit(20_000_000),
        tempo: None,
    };
    EthApiClient::call(
        &client,
        above_eip7825_cap.clone(),
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .expect("an explicit RPC cap above EIP-7825 must allow this Arbitrum call");

    handle.stop().unwrap();

    let mut osaka_cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Osaka);
    osaka_cfg.disable_balance_check = true;
    osaka_cfg.disable_eip3607 = true;
    osaka_cfg.disable_block_gas_limit = true;
    osaka_cfg.disable_base_fee = true;
    osaka_cfg.chain_id = 42161;
    osaka_cfg.tx_gas_limit_cap = None;

    let osaka_handle = ApiBuilder::new(tree, MultiChainCfgEnv::Arbitrum((osaka_cfg, None)))
        .build_and_run(
            "127.0.0.1:18551",
            100,
            Duration::from_secs(10),
            false,
            false,
            "arbitrum-osaka-gas-cap-test".to_string(),
            100,
            1024,
        )
        .await
        .unwrap();
    let osaka_client = HttpClientBuilder::default()
        .build("http://127.0.0.1:18551")
        .unwrap();
    EthApiClient::call(
        &osaka_client,
        above_eip7825_cap,
        BlockId::Number(BlockNumberOrTag::Latest),
        None,
        None,
    )
    .await
    .expect("Arbitrum eth_call must not apply Ethereum's fixed EIP-7825 gas cap");

    let high_gas_estimate = DebankApiClient::estimate_gas(
        &osaka_client,
        CallRequest {
            inner: TransactionRequest::default()
                .from(sender)
                .to(high_gas_target),
            tempo: None,
        },
        None,
        None,
    )
    .await
    .expect("Arbitrum estimateGas must use the ArbOS compute limit");
    assert!(
        high_gas_estimate > U256::from(revm::primitives::eip7825::TX_GAS_LIMIT_CAP),
        "estimate must not be capped at Ethereum's EIP-7825 boundary: {high_gas_estimate}",
    );
    assert!(
        high_gas_estimate < U256::from(32_000_000u64),
        "estimate must remain below the ArbOS per-transaction limit: {high_gas_estimate}",
    );

    osaka_handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}
