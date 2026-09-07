//! Real HTTP OP execution over a local StateTree; no network chain dependencies.
use alloy::primitives::keccak256;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use leafage_evm_rpc::{ApiBuilder, DebankApiClient, EthApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, Bytes, CallRequest,
    CfgEnv, NewAccount, NewCode, OpSpecId, H256, U256,
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn op_config_reaches_call_estimate_multicall_and_trace() {
    let alice = Address::repeat_byte(0x11);
    let contract = Address::repeat_byte(0x22);
    let code: Bytes = "60011e60005260206000f3".parse().unwrap();
    for (name, spec, sizes) in [
        ("default", OpSpecId::OSAKA, None),
        ("rise", OpSpecId::JOVIAN, Some((262144, 524288))),
        ("metis_sizes", OpSpecId::OSAKA, Some((2457600, usize::MAX))),
    ] {
        let path =
            std::env::temp_dir().join(format!("leafage-op-rpc-{}-{name}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let db = MultiStorage::open(&path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
        let mut diff = BlockStorageDiff::default();
        diff.new_accounts.push(NewAccount {
            address: keccak256(alice),
            balance: U256::from(10u128.pow(20)),
            nonce: 0,
            code_hash: H256::ZERO,
        });
        diff.new_accounts.push(NewAccount {
            address: keccak256(contract),
            balance: U256::ZERO,
            nonce: 1,
            code_hash: keccak256(&code),
        });
        diff.new_codes.push(NewCode {
            code_hash: keccak256(&code),
            code: code.clone(),
        });
        let mut block = BlockInfo {
            inner: Block::empty(Default::default()),
            other: Default::default(),
        };
        block.inner.header.hash = H256::repeat_byte(0xaa);
        block.inner.header.inner.gas_limit = 100_000_000;
        StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        )
        .update_block(block, diff)
        .unwrap();
        let tree =
            Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
        let mut cfg = CfgEnv::new_with_spec(spec);
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.disable_block_gas_limit = true;
        cfg.tx_gas_limit_cap = Some(100_000_000);
        if let Some((code, init)) = sizes {
            cfg.limit_contract_code_size = Some(code);
            cfg.limit_contract_initcode_size = Some(init);
        }
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        let handle = ApiBuilder::new(tree.clone(), MultiChainCfgEnv::Op(cfg))
            .build_and_run(
                &addr.to_string(),
                10,
                Duration::from_secs(30),
                false,
                false,
                "test".into(),
                0,
                128,
            )
            .await
            .unwrap();
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();
        let latest = BlockId::Number(BlockNumberOrTag::Latest);
        let req = |data: Bytes| CallRequest {
            inner: TransactionRequest::default()
                .from(alice)
                .gas_limit(10_000_000)
                .input(TransactionInput::new(data)),
            tempo: None,
        };
        let mut clz = req(Bytes::new());
        clz.inner = clz.inner.to(contract);
        let response = EthApiClient::call(&client, clz, latest, None, None).await;
        if spec == OpSpecId::JOVIAN {
            assert!(response.unwrap_err().to_string().contains("NotActivated"));
        } else {
            assert_eq!(U256::from_be_slice(&response.unwrap()), U256::from(255));
        }
        for data in [
            "6160016000f3".parse::<Bytes>().unwrap(),
            vec![0; 49153].into(),
        ] {
            let is_initcode = data.len() > 49152;
            let request = req(data);
            let call = EthApiClient::call(&client, request.clone(), latest, None, None).await;
            let estimate =
                DebankApiClient::estimate_gas(&client, request.clone(), None, None).await;
            let multi = EthApiClient::multi_call(
                &client,
                vec![request.clone()],
                latest,
                None,
                Some(false),
                Some(true),
            )
            .await;
            let traces = client
                .request::<serde_json::Value, _>(
                    "pre_traceMany",
                    rpc_params![vec![request.clone()], latest],
                )
                .await;
            if sizes.is_some() {
                let output = call.unwrap();
                assert_eq!(output.len(), if is_initcode { 0 } else { 24577 });
                assert!(output.iter().all(|b| *b == 0));
                let gas = estimate.unwrap();
                let mut estimated_request = request.clone();
                estimated_request.inner.gas = Some(gas.to::<u64>());
                assert_eq!(
                    EthApiClient::call(&client, estimated_request, latest, None, None)
                        .await
                        .unwrap(),
                    output
                );
                let multi = multi.unwrap();
                let traces = traces.unwrap();
                assert_eq!(multi.results[0].code, 0, "{multi:?}");
                assert_eq!(multi.results[0].result, output);
                assert_eq!(traces[0]["error"]["code"], 0, "{traces}");
                let trace: serde_json::Value = client
                    .request("pre_traceCall", rpc_params![request, latest])
                    .await
                    .unwrap();
                assert_eq!(trace["failed"], false, "{trace}");
            } else {
                let expected = if is_initcode {
                    "create initcode size limit"
                } else {
                    "CreateContractSizeLimit"
                };
                assert!(call.unwrap_err().to_string().contains(expected));
                assert!(estimate.unwrap_err().to_string().contains(expected));
                if is_initcode {
                    assert!(multi.unwrap_err().to_string().contains(expected));
                    assert!(traces.unwrap_err().to_string().contains(expected));
                } else {
                    let multi = multi.unwrap();
                    assert_ne!(multi.results[0].code, 0);
                    assert!(
                        multi.results[0].err.contains("CreateContractSizeLimit"),
                        "{multi:?}"
                    );
                    let traces = traces.unwrap();
                    assert_ne!(traces[0]["error"]["code"], 0);
                    assert!(traces[0]["error"]["msg"]
                        .as_str()
                        .unwrap()
                        .contains("CreateContractSizeLimit"));
                }
            }
        }
        handle.stop().unwrap();
        handle.stopped().await;
        drop(client);
        drop(tree);
        std::fs::remove_dir_all(path).unwrap();
    }
}
