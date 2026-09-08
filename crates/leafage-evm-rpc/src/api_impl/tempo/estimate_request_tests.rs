use super::*;
use crate::{api_impl::core::Api, DebankApiServer};
use alloy::primitives::{address, keccak256, Address, Signature, B256, U256};
use leafage_evm_chains::tempo::{fee_payer as fp, precompile::storage_types::StorageKey};
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, StateDB};
use leafage_evm_types::{BlockId, BlockStorageDiff, Bytecode};
use serde_json::json;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[derive(Clone, Debug, Default)]
struct EstimateState {
    block: BlockInfo,
    accounts: HashMap<B256, revm::state::AccountInfo>,
    storage: HashMap<(B256, B256), U256>,
}

impl StateDB for EstimateState {
    type Error = Infallible;
    fn basic(&self, address: B256) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        Ok(self.accounts.get(&address).cloned())
    }
    fn code_by_hash(&self, _: B256) -> Result<Bytecode, Self::Error> {
        Ok(Bytecode::default())
    }
    fn storage(&self, address: B256, key: B256) -> Result<U256, Self::Error> {
        Ok(self
            .storage
            .get(&(address, key))
            .copied()
            .unwrap_or_default())
    }
    fn block_hash(&self, _: u64) -> Result<B256, Self::Error> {
        Ok(self.block.header.hash)
    }
}
impl BlockContext for EstimateState {
    type Error = Infallible;
    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        Ok(self.block.clone())
    }
    fn state_diff(&self) -> Result<BlockStorageDiff, Self::Error> {
        Ok(Default::default())
    }
}
impl BlockIndex for EstimateState {
    type Error = Infallible;
    fn get_block_by_id(&self, _: BlockId) -> Result<Option<BlockInfo>, Self::Error> {
        Ok(Some(self.block.clone()))
    }
}
impl EvmStorageRead for EstimateState {
    type Error = Infallible;
    type StateDB = Self;
    fn state_at(&self, _: BlockId) -> Result<Option<Self>, Self::Error> {
        Ok(Some(self.clone()))
    }
}

#[tokio::test]
async fn formal_t11_rpc_uses_requested_timestamp_for_strict_abi() {
    use alloy::sol_types::SolCall;
    use leafage_evm_chains::tempo::precompile::{
        address_registry::IAddressRegistry, ADDRESS_REGISTRY_ADDRESS,
    };

    let canonical = IAddressRegistry::isVirtualAddressCall {
        addr: Address::repeat_byte(0x11),
    }
    .abi_encode();
    let mut trailing = canonical.clone();
    trailing.extend([0; 32]);
    // Deliberately return to a historical timestamp after testing the active fork.
    for timestamp in [
        1_789_048_799u64,
        1_789_048_800,
        1_789_048_801,
        1_789_048_799,
    ] {
        for use_override in [false, true] {
            let mut db = EstimateState::default();
            db.block.header.number = 100;
            db.block.header.timestamp = if use_override {
                1_789_048_801
            } else {
                timestamp
            };
            db.block.header.gas_limit = 100_000_000;
            let core = ApiImpl {
                db,
                evm_cfg: tests::review_api().evm_cfg,
                historical_client: None,
                historical_height: None,
                token_collector: None,
            };
            let api = Api::new(core);
            let mut module = crate::EthApiServer::into_rpc(api.clone());
            module.merge(DebankApiServer::into_rpc(api)).unwrap();
            let overrides = use_override.then(|| json!({"time": format!("0x{timestamp:x}")}));
            for (data, malformed) in [(&canonical, false), (&trailing, true)] {
                let request = json!({
                    "from": Address::repeat_byte(0x11), "to": ADDRESS_REGISTRY_ADDRESS,
                    "gas": "0xf4240", "data": alloy::primitives::Bytes::copy_from_slice(data),
                });
                for (method, params) in [
                    ("eth_call", json!([request, "0x64", null, overrides])),
                    (
                        "estimateGas",
                        json!([request, {"block_id":"0x64","type":"Equals"}, overrides]),
                    ),
                ] {
                    let raw = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params})
                        .to_string();
                    let (response, _) = module.raw_json_request(&raw, 1).await.unwrap();
                    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
                    let must_revert = malformed && timestamp >= 1_789_048_800;
                    assert_eq!(
                        response.get("error").is_some(),
                        must_revert,
                        "{method} timestamp={timestamp} override={use_override}: {response}"
                    );
                    if !must_revert && method == "eth_call" {
                        assert_eq!(response["result"], json!(format!("0x{}", "00".repeat(32))));
                    }
                    if must_revert {
                        // These RPCs intentionally expose different legacy error formats.
                        if method == "estimateGas" {
                            assert_eq!(response["error"], json!({"code": -39000, "message": ""}));
                        } else {
                            assert_eq!(
                                response["error"],
                                json!({"code": -32603, "message": "Reverted: \"\""})
                            );
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn review_signed_sponsor_estimate_dispatch_preserves_nonce_without_fallback() {
    let caller = address!("1111111111111111111111111111111111111111");
    let to = Address::repeat_byte(0x22);
    let token = address!("20c0000000000000000000000000000000000000");
    let signature = Signature::new(U256::ONE, U256::ONE, false);
    let payer = fp::recover_fee_payer(
        &signature,
        4217,
        0,
        1_000_000_000_000,
        1_000_000,
        &[fp::Call {
            to: Some(to),
            value: U256::ZERO,
            input: Default::default(),
        }],
        &Default::default(),
        U256::ZERO,
        0,
        None,
        None,
        Some(token),
        caller,
        &[],
        None,
    )
    .unwrap();
    let request = json!({
        "from":caller,"to":to,"gas":"0xf4240","nonce":"0x0","chainId":"0x1079",
        "maxFeePerGas":"0xe8d4a51000","maxPriorityFeePerGas":"0x0",
        "feeToken":token,"feePayerSignature":signature
    });
    let mut db = EstimateState::default();
    db.block.header.number = 100;
    db.block.header.timestamp = 1_788_743_086;
    db.block.header.gas_limit = 100_000_000;
    db.accounts.insert(
        keccak256(caller),
        revm::state::AccountInfo {
            nonce: 7,
            ..Default::default()
        },
    );
    let marker = Bytecode::new_legacy(alloy::primitives::bytes!("ef"));
    db.accounts.insert(
        keccak256(token),
        revm::state::AccountInfo::new(U256::ZERO, 1, marker.hash_slow(), marker),
    );
    db.storage.insert(
        (
            keccak256(token),
            keccak256(payer.mapping_slot(U256::from(9)).to_be_bytes::<32>()),
        ),
        U256::from(2_000_000),
    );

    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let server = jsonrpsee::server::ServerBuilder::default()
        .build("127.0.0.1:0")
        .await
        .unwrap();
    let url = format!("http://{}", server.local_addr().unwrap());
    let mut fallback = jsonrpsee::RpcModule::new(fallback_calls.clone());
    fallback
        .register_method("estimateGas", |_, calls, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, jsonrpsee::types::ErrorObjectOwned>(U256::from(999_999))
        })
        .unwrap();
    let handle = server.start(fallback);

    for with_fallback in [false, true] {
        let core = ApiImpl {
            db: db.clone(),
            evm_cfg: tests::review_api().evm_cfg,
            historical_client: with_fallback.then(|| {
                jsonrpsee::http_client::HttpClientBuilder::default()
                    .build(&url)
                    .unwrap()
            }),
            historical_height: Some(101),
            token_collector: None,
        };
        let module = DebankApiServer::into_rpc(Api::new(core));
        let raw = json!({"jsonrpc":"2.0","id":1,"method":"estimateGas", "params":[request,{"block_id":"0x64","type":"Equals"}]}).to_string();
        let (response, _) = module.raw_json_request(&raw, 1).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        let gas: U256 = serde_json::from_value(response["result"].clone()).unwrap();
        // Execution uses the state nonce (7), not the signed nonce (0): no 250k creation cost.
        assert!(
            gas >= U256::from(21_000) && gas < U256::from(22_000),
            "{gas}"
        );
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);

        let mut missing_nonce = request.clone();
        missing_nonce.as_object_mut().unwrap().remove("nonce");
        let raw = json!({"jsonrpc":"2.0","id":2,"method":"estimateGas", "params":[missing_nonce,{"block_id":"0x64","type":"Equals"}]}).to_string();
        let (response, _) = module.raw_json_request(&raw, 1).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        if with_fallback {
            // Prove the historical client is eligible and works: only the bad
            // local request takes this path, never the complete signed request.
            assert_eq!(
                serde_json::from_value::<U256>(response["result"].clone()).unwrap(),
                U256::from(999_999)
            );
            assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        } else {
            assert!(response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("fee payer signature recovery failed"));
        }
    }
    handle.stop().unwrap();
    handle.stopped().await;
}

#[test]
fn review_estimate_nonce_policy_preserves_other_chains_and_unsigned_requests() {
    use crate::api_impl::api_impl::NoneEvmCustomConfig;
    let mainnet = ApiImpl::<_, leafage_evm_types::MainnetSpecId, NoneEvmCustomConfig>::new(
        (),
        revm::context::CfgEnv::new_with_spec(leafage_evm_types::MainnetSpecId::CANCUN),
        None,
        None,
        None,
        None,
        true,
        false,
        String::new(),
        0,
        None,
        None,
        None,
    );
    for signed in [false, true] {
        let mut json = json!({"to":Address::repeat_byte(0x22),"nonce":"0x7"});
        if signed {
            json["feePayerSignature"] = json!({"r":"0x1","s":"0x1","yParity":"0x0"});
        }
        let mut request: CallRequest = serde_json::from_value(json).unwrap();
        let mut ordinary = request.clone();
        mainnet.prepare_estimate_request(&mut ordinary);
        assert_eq!(ordinary.nonce, None);
        tests::review_api().prepare_estimate_request(&mut request);
        assert_eq!(request.nonce, if signed { Some(7) } else { None });
    }
}
