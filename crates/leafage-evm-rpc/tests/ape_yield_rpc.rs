//! ApeChain native yield through the RPC: balances come from the writer's
//! yield slots and the ArbOS share price.

use alloy::primitives::keccak256;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::sol_types::SolCall;
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_chains::arbitrum::arbos_state::ARBOS_STATE_ADDRESS;
use leafage_evm_chains::arbitrum::precompile::{ARB_INFO_ADDRESS, ARB_OWNER_PUBLIC_ADDRESS};
use leafage_evm_chains::arbitrum::{ArbitrumEvmConfig, ArbitrumHardfork};
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::ape_yield::{
    ape_arbos_root_slot, ape_yield_slot, ApeYieldFields, APE_SHARE_COUNT_OFFSET,
    APE_SHARE_PRICE_OFFSET, APE_YIELD_ADDRESS_HASH, APE_YIELD_DELEGATED, APE_YIELD_DISABLED,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    Bytes, CallRequest, CfgEnv, IndexValuePair, NewAccount, NewCode, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

alloy::sol! {
    function getBalance(address account) external view returns (uint256);
    function getBalanceValues(address account) external view returns (uint256, uint256, uint256);
    function getYieldConfiguration(address account) external view returns (uint8);
    function getShareCount() external view returns (uint256);
}

const PRICE: u64 = 1_112_418_342;

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
    // ArbOS 59, as on ApeChain mainnet.
    let mut mix_hash = [0u8; 32];
    mix_hash[16..24].copy_from_slice(&59u64.to_be_bytes());
    info.inner.header.inner.mix_hash = H256::from(mix_hash);
    info
}

fn index(slot: U256) -> H256 {
    keccak256(slot.to_be_bytes::<32>())
}

fn account(address: Address, nonce: u64, code_hash: H256) -> NewAccount {
    // The writer stores ApeChain accounts with balance 0.
    NewAccount {
        address: keccak256(address),
        balance: U256::ZERO,
        nonce,
        code_hash,
    }
}

fn yield_pairs(address: Address, fields: ApeYieldFields) -> Vec<IndexValuePair> {
    let hash = keccak256(address);
    fields
        .to_slots()
        .into_iter()
        .enumerate()
        .filter(|(_, value)| !value.is_zero())
        .map(|(i, value)| IndexValuePair {
            index: index(ape_yield_slot(hash, i)),
            value,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn ape_balances_and_precompiles_follow_yield_fields() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-ape-yield-rpc-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();

    let auto = Address::repeat_byte(0xa0);
    let disabled = Address::repeat_byte(0xb0);
    let delegator = Address::repeat_byte(0xc0);
    let delegate = Address::repeat_byte(0xd0);
    let probe = Address::repeat_byte(0xe0);
    // SELFBALANCE; PUSH0; MSTORE; PUSH1 32; PUSH0; RETURN
    let probe_code = Bytes::from_static(&[0x47, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
    let probe_hash = keccak256(probe_code.as_ref());

    let auto_fields = ApeYieldFields {
        fixed: U256::from(705_768_859u64),
        shares: U256::from(5_514_621_514_919_413u64),
        ..Default::default()
    };
    let auto_balance =
        U256::from(5_514_621_514_919_413u64) * U256::from(PRICE) + U256::from(705_768_859u64);
    let mut yield_values = Vec::new();
    yield_values.extend(yield_pairs(auto, auto_fields));
    yield_values.extend(yield_pairs(
        disabled,
        ApeYieldFields {
            flags: APE_YIELD_DISABLED,
            fixed: U256::from(77),
            ..Default::default()
        },
    ));
    yield_values.extend(yield_pairs(
        delegator,
        ApeYieldFields {
            flags: APE_YIELD_DELEGATED,
            fixed: U256::from(300),
            delegate,
            ..Default::default()
        },
    ));
    yield_values.extend(yield_pairs(
        delegate,
        ApeYieldFields {
            fixed: U256::from(1_300),
            debt: U256::from(300),
            ..Default::default()
        },
    ));
    yield_values.extend(yield_pairs(
        probe,
        ApeYieldFields {
            flags: APE_YIELD_DISABLED,
            ..Default::default()
        },
    ));

    let db = MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let genesis = block_info(0, H256::repeat_byte(0xaa), H256::ZERO);
    let genesis_diff = BlockStorageDiff {
        new_accounts: vec![
            account(auto, 0, H256::ZERO),
            account(disabled, 0, H256::ZERO),
            account(delegator, 0, H256::ZERO),
            account(delegate, 0, H256::ZERO),
            account(probe, 1, probe_hash),
            account(ARBOS_STATE_ADDRESS, 1, H256::ZERO),
        ],
        storage_diffs: vec![
            AccountStorageDiff {
                address: keccak256(ARBOS_STATE_ADDRESS),
                diffs: vec![
                    IndexValuePair {
                        index: index(ape_arbos_root_slot(0)),
                        value: U256::from(59),
                    },
                    IndexValuePair {
                        index: index(ape_arbos_root_slot(APE_SHARE_PRICE_OFFSET)),
                        value: U256::from(PRICE),
                    },
                    IndexValuePair {
                        index: index(ape_arbos_root_slot(APE_SHARE_COUNT_OFFSET)),
                        value: U256::from(5_514_621_514_919_413u64),
                    },
                ],
            },
            AccountStorageDiff {
                address: APE_YIELD_ADDRESS_HASH,
                diffs: yield_values,
            },
        ],
        new_codes: vec![NewCode {
            code_hash: probe_hash,
            code: probe_code,
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
    cfg.chain_id = 33139;
    cfg.tx_gas_limit_cap = Some(100_000_000);
    let ape_cfg = ArbitrumEvmConfig {
        ape_yield: true,
        ..Default::default()
    };
    let handle = ApiBuilder::new(
        tree.clone(),
        MultiChainCfgEnv::Arbitrum((cfg, Some(ape_cfg))),
    )
    .build_and_run(
        "127.0.0.1:18571",
        100,
        Duration::from_secs(10),
        false,
        false,
        "ape-yield-test".to_string(),
        100,
        1024,
    )
    .await
    .unwrap();
    let client = HttpClientBuilder::default()
        .build("http://127.0.0.1:18571")
        .unwrap();
    let latest = BlockId::Number(BlockNumberOrTag::Latest);

    let balance = |address: Address| {
        let client = &client;
        async move {
            EthApiClient::get_balance(client, address, latest)
                .await
                .unwrap()
        }
    };
    assert_eq!(balance(auto).await, auto_balance);
    assert_eq!(balance(disabled).await, U256::from(77));
    assert_eq!(balance(delegator).await, U256::from(300));
    assert_eq!(balance(delegate).await, U256::from(1_000));
    assert_eq!(balance(Address::repeat_byte(0x99)).await, U256::ZERO);

    let call = |from: Address, to: Address, value: U256, data: Vec<u8>| {
        let client = &client;
        async move {
            EthApiClient::call(
                client,
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(from)
                        .to(to)
                        .value(value)
                        .input(TransactionInput::new(Bytes::from(data))),
                    tempo: None,
                },
                latest,
                None,
                None,
            )
            .await
            .unwrap()
        }
    };

    // ArbInfo.getBalance goes through the same resolved balance.
    let out = call(
        auto,
        ARB_INFO_ADDRESS,
        U256::ZERO,
        getBalanceCall { account: delegate }.abi_encode(),
    )
    .await;
    assert_eq!(U256::from_be_slice(&out), U256::from(1_000));
    let out = call(
        auto,
        ARB_INFO_ADDRESS,
        U256::ZERO,
        getBalanceValuesCall { account: delegate }.abi_encode(),
    )
    .await;
    let values = getBalanceValuesCall::abi_decode_returns(&out).unwrap();
    assert_eq!(
        (values._0, values._1, values._2),
        (U256::from(1_300), U256::ZERO, U256::from(300))
    );
    let out = call(
        auto,
        ARB_INFO_ADDRESS,
        U256::ZERO,
        getYieldConfigurationCall { account: delegator }.abi_encode(),
    )
    .await;
    assert_eq!(
        getYieldConfigurationCall::abi_decode_returns(&out).unwrap(),
        APE_YIELD_DELEGATED
    );
    let out = call(
        auto,
        ARB_OWNER_PUBLIC_ADDRESS,
        U256::ZERO,
        getShareCountCall {}.abi_encode(),
    )
    .await;
    assert_eq!(
        U256::from_be_slice(&out),
        U256::from(5_514_621_514_919_413u64)
    );

    // Value moves like any balance inside the EVM.
    let out = call(auto, probe, U256::from(1_000), Vec::new()).await;
    assert_eq!(U256::from_be_slice(&out), U256::from(1_000));

    let gas = DebankApiClient::estimate_gas(
        &client,
        CallRequest {
            inner: TransactionRequest::default()
                .from(auto)
                .to(Address::repeat_byte(0x99))
                .value(U256::from(1_000_000)),
            tempo: None,
        },
        None,
        None,
    )
    .await
    .expect("transfer from an automatic-yield account");
    assert!(gas >= U256::from(21_000), "{gas}");

    handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}
