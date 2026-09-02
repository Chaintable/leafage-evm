use crate::api::{DebankApiServer, EthApiServer};
use crate::api_impl::core::{Api, EvmExecutor, TxSetter};
use crate::api_impl::debank::MIN_TRANSACTION_GAS;
use crate::api_impl::{utils, ApiImpl};
use alloy::eips::eip7702::Authorization;
use alloy::primitives::{hex, keccak256};
use alloy::rpc::types::state::{AccountOverride, StateOverride};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::signers::{local::PrivateKeySigner, SignerSync};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arc::{ArcChainConfig, ARC_MAINNET_CHAIN_ID};
use leafage_evm_storage::{
    BlockContext, EvmStorageWrapper, EvmStorageWrite, MultiStorage, StateDBProvider,
    StateDBWrapper, StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{
    block_env_from_block, AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag,
    BlockOverrides, BlockStorageDiff, BlockType, Bytes, CallRequest, CfgEnv, DebankBlockContext,
    DebankErrorCode, DebankID, DebankSingleSimulateResult, IndexValuePair, MainnetSpecId,
    NewAccount, NewCode, H256, U256,
};
use revm::context::result::{ExecutionResult, HaltReason};
use revm::database::CacheDB;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

type ArcTestApi = Api<ApiImpl<Arc<StateTree<MultiStorage>>, MainnetSpecId, ArcChainConfig>>;

const ARC_RPC_GAS_CAP: u64 = 25_000_000;
const BLOCK_GAS_LIMIT: u64 = 30_000_000;
const ANCHOR_NUMBER: u64 = 1;
const ANCHOR_BASE_FEE: u64 = 3;
const ENCODED_NEXT_BASE_FEE: u64 = 7;
const OVERRIDDEN_BASE_FEE: u64 = 11;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct TestAddresses {
    funded: Address,
    native_fiat_token: Address,
    observer: Address,
    empty: Address,
    blocked: Address,
    gas_guard: Address,
    revert: Address,
    environment: Address,
    counter: Address,
    logger: Address,
    logger_revert: Address,
    delegate_proxy: Address,
    delegate_implementation: Address,
    balance_reader: Address,
    beneficiary: Address,
    selfdestruct_revert_parent: Address,
    selfdestruct_nonzero: Address,
    selfdestruct_zero: Address,
    selfdestruct_beneficiary: Address,
    reverted_child_then_selfdestruct: Address,
}

struct ArcFixture {
    api: ArcTestApi,
    addresses: TestAddresses,
    path: PathBuf,
}

impl ArcFixture {
    fn close(self) {
        let path = self.path.clone();
        drop(self);
        let _ = std::fs::remove_dir_all(path);
    }
}

fn test_block(number: u64, hash: H256, parent_hash: H256, encoded_next_base_fee: u64) -> BlockInfo {
    let mut block = BlockInfo {
        inner: Block::empty(Default::default()),
        other: Default::default(),
    };
    block.inner.header.hash = hash;
    block.inner.header.inner.number = number;
    block.inner.header.inner.parent_hash = parent_hash;
    block.inner.header.inner.timestamp = 1_000 + number;
    block.inner.header.inner.gas_limit = BLOCK_GAS_LIMIT;
    block.inner.header.inner.base_fee_per_gas = Some(ANCHOR_BASE_FEE);
    block.inner.header.inner.extra_data =
        Bytes::copy_from_slice(&encoded_next_base_fee.to_be_bytes());
    block.inner.header.inner.beneficiary = Address::repeat_byte(0x77);
    block.inner.header.inner.mix_hash = H256::repeat_byte(0x44);
    block.inner.header.inner.excess_blob_gas = Some(0);
    block.inner.header.inner.blob_gas_used = Some(0);
    block
}

fn gas_guard_code(threshold: u32) -> Bytes {
    let threshold = threshold.to_be_bytes();
    Bytes::from(vec![
        // Revert when GAS at contract entry is below `threshold`.
        0x5a,
        0x63,
        threshold[0],
        threshold[1],
        threshold[2],
        threshold[3],
        0x90,
        0x10,
        0x60,
        0x0c,
        0x57,
        0x00,
        0x5b,
        0x5f,
        0x5f,
        0xfd,
    ])
}

fn environment_code() -> Bytes {
    Bytes::from_static(&[
        // Return NUMBER, BASEFEE, BLOCKHASH(0), and BLOCKHASH(1).
        0x43, 0x5f, 0x52, 0x48, 0x60, 0x20, 0x52, 0x5f, 0x40, 0x60, 0x40, 0x52, 0x60, 0x01, 0x40,
        0x60, 0x60, 0x52, 0x60, 0x80, 0x5f, 0xf3,
    ])
}

fn counter_code() -> Bytes {
    Bytes::from_static(&[
        // storage[0] += 1; return storage[0].
        0x5f, 0x54, 0x60, 0x01, 0x01, 0x80, 0x5f, 0x55, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
    ])
}

fn logger_code() -> Bytes {
    // Emit one ordinary LOG0 from the execution address.
    Bytes::from_static(&[0x5f, 0x5f, 0xa0, 0x00])
}

fn logger_revert_code() -> Bytes {
    // Emit LOG0 and then revert, so both this log and Arc's frame-init
    // EIP-7708 log must disappear from the simulated result.
    Bytes::from_static(&[0x5f, 0x5f, 0xa0, 0x5f, 0x5f, 0xfd])
}

fn delegate_proxy_code(implementation: Address) -> Bytes {
    let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x73];
    code.extend_from_slice(implementation.as_slice());
    code.extend_from_slice(&[0x5a, 0xf4, 0x50, 0x00]);
    code.into()
}

fn balance_reader_code() -> Bytes {
    Bytes::from_static(&[
        // return BALANCE(address(calldataload(0))).
        0x5f, 0x35, 0x31, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
    ])
}

fn init_code(runtime: &[u8]) -> Bytes {
    assert!(runtime.len() <= u8::MAX as usize);
    let mut init = vec![
        0x60,
        runtime.len() as u8,
        0x60,
        0x0c,
        0x60,
        0x00,
        0x39,
        0x60,
        runtime.len() as u8,
        0x60,
        0x00,
        0xf3,
    ];
    init.extend_from_slice(runtime);
    init.into()
}

fn native_coin_mint_input(to: Address, amount: U256) -> Bytes {
    let mut input = selector("mint(address,uint256)").to_vec();
    input.extend_from_slice(&address_word(to));
    input.extend_from_slice(&amount.to_be_bytes::<32>());
    input.into()
}

fn native_fiat_token_code(account: Address) -> Bytes {
    let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
        .parse()
        .unwrap();
    let mut input_word = [0u8; 32];
    input_word[..4].copy_from_slice(&selector("blocklist(address)"));

    let mut code = vec![0x7f];
    code.extend_from_slice(&input_word);
    code.extend_from_slice(&[0x5f, 0x52, 0x73]);
    code.extend_from_slice(account.as_slice());
    code.extend_from_slice(&[
        0x60, 0x04, 0x52, 0x60, 0x20, 0x5f, 0x60, 0x24, 0x5f, 0x5f, 0x73,
    ]);
    code.extend_from_slice(native_coin_control.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x60, 0x20, 0x5f, 0xf3]);
    code.into()
}

fn selfdestruct_code(beneficiary: Address) -> Bytes {
    let mut code = vec![0x73];
    code.extend_from_slice(beneficiary.as_slice());
    code.push(0xff);
    code.into()
}

fn call_child_code(child: Address, revert: bool) -> Bytes {
    // CALL the child with zero value/input/output and consume the success
    // word. The reverting variant then rolls the successful child frame
    // back from its parent.
    let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x5f, 0x73];
    code.extend_from_slice(child.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50]);
    if revert {
        code.extend_from_slice(&[0x5f, 0x5f, 0xfd]);
    } else {
        code.push(0x00);
    }
    code.into()
}

fn reverted_value_child_then_selfdestruct_code(child: Address, beneficiary: Address) -> Bytes {
    // The child receives value (Arc EIP-7708), emits LOG0, and reverts.
    // After its checkpoint is rolled back, the parent SELFDESTRUCT writes
    // a direct Arc EIP-7708 log that must still reach the inspector.
    let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x60, 0x05, 0x73];
    code.extend_from_slice(child.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x73]);
    code.extend_from_slice(beneficiary.as_slice());
    code.push(0xff);
    code.into()
}

fn blocklist_storage_index(address: Address) -> H256 {
    let mut mapping_input = [0u8; 64];
    mapping_input[12..32].copy_from_slice(address.as_slice());
    mapping_input[63] = 2;
    keccak256(keccak256(mapping_input))
}

fn build_arc_fixture(estimate_gas_buffer: u64) -> ArcFixture {
    build_arc_fixture_with_rpc_gas_cap(estimate_gas_buffer, ARC_RPC_GAS_CAP)
}

fn build_arc_fixture_with_rpc_gas_cap(estimate_gas_buffer: u64, rpc_gas_cap: u64) -> ArcFixture {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("leafage-arc-estimate-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();

    let addresses = TestAddresses {
        funded: Address::repeat_byte(0x11),
        native_fiat_token: "0x3600000000000000000000000000000000000000"
            .parse()
            .unwrap(),
        observer: Address::repeat_byte(0x13),
        empty: Address::repeat_byte(0x22),
        blocked: Address::repeat_byte(0x33),
        gas_guard: Address::repeat_byte(0x44),
        revert: Address::repeat_byte(0x55),
        environment: Address::repeat_byte(0x66),
        counter: Address::repeat_byte(0x68),
        logger: Address::repeat_byte(0x69),
        logger_revert: Address::repeat_byte(0x6d),
        delegate_proxy: Address::repeat_byte(0x6a),
        delegate_implementation: Address::repeat_byte(0x6b),
        balance_reader: Address::repeat_byte(0x6c),
        beneficiary: Address::repeat_byte(0x77),
        selfdestruct_revert_parent: Address::repeat_byte(0x72),
        selfdestruct_nonzero: Address::repeat_byte(0x73),
        selfdestruct_zero: Address::repeat_byte(0x74),
        selfdestruct_beneficiary: Address::repeat_byte(0x75),
        reverted_child_then_selfdestruct: Address::repeat_byte(0x79),
    };
    let native_fiat_token = native_fiat_token_code(addresses.empty);
    let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
        .parse()
        .unwrap();
    let gas_guard = gas_guard_code(300_000);
    let revert = Bytes::from_static(&[0x5f, 0x5f, 0xfd]);
    let environment = environment_code();
    let counter = counter_code();
    let logger = logger_code();
    let logger_revert = logger_revert_code();
    let delegate_implementation = logger_code();
    let delegate_proxy = delegate_proxy_code(addresses.delegate_implementation);
    let balance_reader = balance_reader_code();
    let selfdestruct = selfdestruct_code(addresses.selfdestruct_beneficiary);
    let selfdestruct_revert_parent = call_child_code(addresses.selfdestruct_nonzero, true);
    let reverted_child_then_selfdestruct = reverted_value_child_then_selfdestruct_code(
        addresses.logger_revert,
        addresses.selfdestruct_beneficiary,
    );
    let mut diff = BlockStorageDiff::default();
    for (address, balance, nonce, code_hash) in [
        (addresses.funded, U256::ONE << 128, 0, H256::ZERO),
        (
            addresses.native_fiat_token,
            U256::ONE << 128,
            0,
            keccak256(&native_fiat_token),
        ),
        (addresses.observer, U256::ONE << 128, 0, H256::ZERO),
        (
            addresses.blocked,
            U256::from(1_000_000_000u64),
            0,
            H256::ZERO,
        ),
        (native_coin_control, U256::ZERO, 1, H256::ZERO),
        (addresses.gas_guard, U256::ZERO, 1, keccak256(&gas_guard)),
        (addresses.revert, U256::ZERO, 1, keccak256(&revert)),
        (
            addresses.environment,
            U256::ZERO,
            1,
            keccak256(&environment),
        ),
        (addresses.counter, U256::ZERO, 1, keccak256(&counter)),
        (addresses.logger, U256::ZERO, 1, keccak256(&logger)),
        (
            addresses.logger_revert,
            U256::ZERO,
            1,
            keccak256(&logger_revert),
        ),
        (
            addresses.delegate_proxy,
            U256::ZERO,
            1,
            keccak256(&delegate_proxy),
        ),
        (
            addresses.delegate_implementation,
            U256::ZERO,
            1,
            keccak256(&delegate_implementation),
        ),
        (
            addresses.balance_reader,
            U256::ZERO,
            1,
            keccak256(&balance_reader),
        ),
        (
            addresses.selfdestruct_revert_parent,
            U256::ZERO,
            1,
            keccak256(&selfdestruct_revert_parent),
        ),
        (
            addresses.selfdestruct_nonzero,
            U256::from(42),
            1,
            keccak256(&selfdestruct),
        ),
        (
            addresses.selfdestruct_zero,
            U256::ZERO,
            1,
            keccak256(&selfdestruct),
        ),
        (
            addresses.reverted_child_then_selfdestruct,
            U256::from(10),
            1,
            keccak256(&reverted_child_then_selfdestruct),
        ),
    ] {
        diff.new_accounts.push(NewAccount {
            address: keccak256(address.as_slice()),
            balance,
            nonce,
            code_hash,
        });
    }
    diff.new_codes.extend([
        NewCode {
            code_hash: keccak256(&native_fiat_token),
            code: native_fiat_token,
        },
        NewCode {
            code_hash: keccak256(&gas_guard),
            code: gas_guard,
        },
        NewCode {
            code_hash: keccak256(&revert),
            code: revert,
        },
        NewCode {
            code_hash: keccak256(&environment),
            code: environment,
        },
        NewCode {
            code_hash: keccak256(&counter),
            code: counter,
        },
        NewCode {
            code_hash: keccak256(&logger),
            code: logger,
        },
        NewCode {
            code_hash: keccak256(&logger_revert),
            code: logger_revert,
        },
        NewCode {
            code_hash: keccak256(&delegate_proxy),
            code: delegate_proxy,
        },
        NewCode {
            code_hash: keccak256(&delegate_implementation),
            code: delegate_implementation,
        },
        NewCode {
            code_hash: keccak256(&balance_reader),
            code: balance_reader,
        },
        NewCode {
            code_hash: keccak256(&selfdestruct),
            code: selfdestruct,
        },
        NewCode {
            code_hash: keccak256(&selfdestruct_revert_parent),
            code: selfdestruct_revert_parent,
        },
        NewCode {
            code_hash: keccak256(&reverted_child_then_selfdestruct),
            code: reverted_child_then_selfdestruct,
        },
    ]);
    diff.storage_diffs.push(AccountStorageDiff {
        address: keccak256(native_coin_control.as_slice()),
        diffs: vec![IndexValuePair {
            index: blocklist_storage_index(addresses.blocked),
            value: U256::ONE,
        }],
    });

    let db = MultiStorage::open(&path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    let genesis = test_block(
        0,
        H256::repeat_byte(0xaa),
        H256::ZERO,
        ENCODED_NEXT_BASE_FEE,
    );
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(genesis, diff)
    .unwrap();

    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    tree.update_block(
        test_block(
            ANCHOR_NUMBER,
            H256::repeat_byte(0xbb),
            H256::repeat_byte(0xaa),
            ENCODED_NEXT_BASE_FEE,
        ),
        BlockStorageDiff::default(),
    )
    .unwrap();

    let arc_config = ArcChainConfig::mainnet();
    let mut cfg = CfgEnv::new_with_spec(arc_config.ethereum_spec());
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_base_fee = true;
    cfg.chain_id = ARC_MAINNET_CHAIN_ID;
    cfg.tx_gas_limit_cap = Some(rpc_gas_cap);
    let api = Api::new(ApiImpl::new(
        tree,
        cfg,
        Some(arc_config),
        None,
        None,
        None,
        false,
        false,
        "arc-estimate-test".to_string(),
        estimate_gas_buffer,
        None,
        None,
        None,
    ));

    ArcFixture {
        api,
        addresses,
        path,
    }
}

fn anchor_context() -> Option<DebankBlockContext> {
    Some(DebankBlockContext {
        block_id: BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER)),
        block_type: BlockType::Equals,
    })
}

fn call_request(from: Address, to: Address) -> CallRequest {
    CallRequest {
        inner: TransactionRequest::default().from(from).to(to),
        tempo: None,
    }
}

async fn estimate(
    api: &ArcTestApi,
    request: CallRequest,
    overrides: Option<BlockOverrides>,
) -> RpcResult<U256> {
    api.estimate_gas(request, anchor_context(), overrides).await
}

fn execute_arc_estimate_probe(
    api: &ArcTestApi,
    mut request: CallRequest,
    overrides: Option<BlockOverrides>,
    gas_limit: u64,
) -> ExecutionResult<HaltReason> {
    let state = api.debank_get_state_by_ctx_impl(anchor_context()).unwrap();
    let block = state.block_info_arc().unwrap();
    request.nonce = None;
    request.gas = Some(gas_limit);
    let mut cache_db = CacheDB::new(EvmStorageWrapper {
        db: state,
        ovm_address: None,
        normalize_state_key: false,
    });
    let mut block_env = block_env_from_block(&block);
    if let Some(overrides) = overrides {
        utils::apply_block_overrides(
            overrides,
            &mut cache_db,
            &mut block_env,
            block.header.clone(),
        );
    }
    let memory_db = utils::RequestCacheDB::new(cache_db);
    let mut tx = api
        .inner
        .create_txn_env(
            &block,
            &block_env,
            request,
            &memory_db,
            ARC_MAINNET_CHAIN_ID,
        )
        .unwrap();
    tx.set_gas_estimation();
    tx.set_gas_limit(gas_limit);
    api.inner.transact(&block_env, &memory_db, tx).unwrap()
}

fn success_output(result: ExecutionResult<HaltReason>) -> Bytes {
    match result {
        ExecutionResult::Success { output, .. } => output.into_data(),
        other => panic!("expected success, got {other:?}"),
    }
}

fn output_words(output: &Bytes) -> Vec<U256> {
    output.chunks_exact(32).map(U256::from_be_slice).collect()
}

fn request_with_input(from: Address, to: Address, input: Bytes) -> CallRequest {
    CallRequest {
        inner: TransactionRequest::default()
            .from(from)
            .to(to)
            .input(TransactionInput::new(input)),
        tempo: None,
    }
}

fn selector(signature: &str) -> Bytes {
    Bytes::copy_from_slice(&keccak256(signature.as_bytes())[..4])
}

fn address_word(address: Address) -> Bytes {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    Bytes::copy_from_slice(&word)
}

fn p256_valid_input() -> Bytes {
    // Daimo P256 verifier vector, also checked into revm-precompile 32.1.0.
    hex::decode("4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4da73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff37618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e")
            .unwrap()
            .into()
}

fn root_trace_output(result: &DebankSingleSimulateResult) -> Bytes {
    result
        .traces
        .first()
        .expect("top-level trace")
        .output
        .clone()
}

fn assert_arc_transfer_event(
    event: &leafage_evm_types::DebankEvent,
    from: Address,
    to: Address,
    amount: U256,
    parent_trace_id: &str,
    pos_in_parent_trace: usize,
) {
    assert_eq!(
        event.contract_id,
        "0xfffffffffffffffffffffffffffffffffffffffe"
            .parse::<Address>()
            .unwrap()
    );
    assert_eq!(
        event.selector,
        keccak256("Transfer(address,address,uint256)").to_string()
    );
    assert_eq!(
        event.topics,
        vec![
            H256::left_padding_from(from.as_slice()).to_string(),
            H256::left_padding_from(to.as_slice()).to_string(),
        ]
    );
    assert_eq!(
        event.data,
        Bytes::copy_from_slice(&amount.to_be_bytes::<32>())
    );
    assert_eq!(event.parent_trace_id, parent_trace_id);
    assert_eq!(event.pos_in_parent_trace, pos_in_parent_trace);
    assert_eq!(event.id, event.debank_id());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_call_uses_shared_environment_and_overrides() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));

    let historical = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.environment)
            .gas_price(ANCHOR_BASE_FEE as u128)
            // call-like semantics ignore an explicitly supplied nonce.
            .nonce(999),
        tempo: None,
    };
    let output = fixture
        .api
        .call(historical.clone(), anchor, None, None)
        .await
        .unwrap();
    let words = output_words(&output);
    assert_eq!(words[0], U256::from(ANCHOR_NUMBER));
    assert_eq!(words[1], U256::from(ANCHOR_BASE_FEE));
    assert_eq!(
        words[2],
        U256::from_be_slice(H256::repeat_byte(0xaa).as_slice())
    );
    assert_eq!(words[3], U256::ZERO);

    let overridden_hash = H256::repeat_byte(0xcc);
    let overrides = BlockOverrides {
        number: Some(U256::from(ANCHOR_NUMBER + 1)),
        base_fee: Some(U256::from(OVERRIDDEN_BASE_FEE)),
        block_hash: Some(BTreeMap::from([(ANCHOR_NUMBER, overridden_hash)])),
        ..Default::default()
    };
    let next_request = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.environment)
            .gas_price(OVERRIDDEN_BASE_FEE as u128),
        tempo: None,
    };
    let output = fixture
        .api
        .call(next_request, anchor, None, Some(overrides))
        .await
        .unwrap();
    let words = output_words(&output);
    assert_eq!(words[0], U256::from(ANCHOR_NUMBER + 1));
    assert_eq!(words[1], U256::from(OVERRIDDEN_BASE_FEE));
    assert_eq!(words[3], U256::from_be_slice(overridden_hash.as_slice()));

    let mut state_override = StateOverride::default();
    state_override.insert(
        addresses.environment,
        AccountOverride::default().with_code(Bytes::from_static(&[
            0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ])),
    );
    let overridden = fixture
        .api
        .call(historical, anchor, Some(state_override), None)
        .await
        .unwrap();
    assert_eq!(output_words(&overridden), vec![U256::from(42)]);

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_multicalls_isolate_subcalls_and_preserve_each_fast_fail_contract() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let requests = vec![
        call_request(addresses.funded, addresses.counter),
        call_request(addresses.funded, addresses.counter),
        call_request(addresses.funded, addresses.revert),
        call_request(addresses.funded, addresses.counter),
    ];
    let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));

    let eth = fixture
        .api
        .multi_call(
            requests.clone(),
            anchor,
            Some(true),
            Some(false),
            Some(false),
        )
        .await
        .unwrap();
    assert_eq!(output_words(&eth.results[0].result), vec![U256::ONE]);
    assert_eq!(output_words(&eth.results[1].result), vec![U256::ONE]);
    assert_eq!(
        eth.results[2].code,
        leafage_evm_types::MultiCallErrorCode::EVMReverted as i32
    );
    assert_eq!(
        eth.results[3].code,
        leafage_evm_types::MultiCallErrorCode::EVMFastFailed as i32
    );
    assert_eq!(eth.results[3].err, eth.results[2].err);
    assert!(!eth.stats.success);

    let contract = fixture
        .api
        .contract_multi_call_impl(
            requests,
            anchor_context(),
            None,
            None,
            Some(true),
            Some(false),
            Some(false),
        )
        .await
        .unwrap();
    assert_eq!(output_words(&contract.results[0].result), vec![U256::ONE]);
    assert_eq!(output_words(&contract.results[1].result), vec![U256::ONE]);
    assert_eq!(contract.results[2].code, DebankErrorCode::EvmRevert as i32);
    assert_eq!(
        serde_json::to_value(&contract.results[3]).unwrap(),
        serde_json::to_value(&contract.results[2]).unwrap()
    );
    assert!(!contract.stats.success);

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_commits_sequential_state_fees_and_exact_fast_stop() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let initial_funded_balance = U256::ONE << 128;
    let transferred = U256::from(123);
    let first = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.counter)
            .value(transferred)
            .gas_limit(500_000)
            .max_fee_per_gas(5)
            .max_priority_fee_per_gas(2),
        tempo: None,
    };
    let second = call_request(addresses.funded, addresses.counter);
    let counter_balance = request_with_input(
        addresses.observer,
        addresses.balance_reader,
        address_word(addresses.counter),
    );
    let funded_balance = request_with_input(
        addresses.observer,
        addresses.balance_reader,
        address_word(addresses.funded),
    );
    let beneficiary_balance = request_with_input(
        addresses.observer,
        addresses.balance_reader,
        address_word(addresses.beneficiary),
    );
    let simulated = fixture
        .api
        .simulate_transactions(
            vec![
                first,
                second,
                counter_balance,
                funded_balance,
                beneficiary_balance,
            ],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(simulated.stats.block_num, ANCHOR_NUMBER);
    assert_eq!(simulated.stats.block_hash, H256::repeat_byte(0xbb));
    assert!(simulated.stats.success);
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[0])),
        vec![U256::ONE]
    );
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[1])),
        vec![U256::from(2)]
    );
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[2])),
        vec![transferred]
    );
    let first_fee = U256::from(simulated.results[0].gas_used) * U256::from(5);
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[3])),
        vec![initial_funded_balance - transferred - first_fee]
    );
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[4])),
        vec![first_fee]
    );

    let stopped = fixture
        .api
        .simulate_transactions(
            vec![
                call_request(addresses.funded, addresses.revert),
                call_request(addresses.funded, addresses.counter),
            ],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    assert!(!stopped.stats.success);
    assert_eq!(stopped.results[0].code, DebankErrorCode::EvmRevert as i32);
    assert_eq!(
        serde_json::to_value(&stopped.results[1]).unwrap(),
        serde_json::to_value(&stopped.results[0]).unwrap()
    );

    let explicit_nonce = vec![
        CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.counter)
                .nonce(0),
            tempo: None,
        },
        CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.counter)
                .nonce(0),
            tempo: None,
        },
    ];
    let explicit_nonce = fixture
        .api
        .simulate_transactions(explicit_nonce, anchor_context(), None)
        .await
        .unwrap();
    assert!(explicit_nonce.stats.success);
    assert_eq!(
        output_words(&root_trace_output(&explicit_nonce.results[0])),
        vec![U256::ONE]
    );
    assert_eq!(
        output_words(&root_trace_output(&explicit_nonce.results[1])),
        vec![U256::from(2)]
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_commits_eip7702_delegation_for_the_next_transaction() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let signer: PrivateKeySigner =
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412d9d780c2350c7d"
            .parse()
            .unwrap();
    let authority = signer.address();
    let authorization = Authorization {
        chain_id: U256::from(ARC_MAINNET_CHAIN_ID),
        address: addresses.counter,
        nonce: 0,
    };
    let signature = signer
        .sign_hash_sync(&authorization.signature_hash())
        .unwrap();
    let mut authorize = TransactionRequest::default()
        .from(addresses.funded)
        .to(addresses.empty)
        .gas_limit(500_000);
    authorize.authorization_list = Some(vec![authorization.into_signed(signature)]);

    let simulated = fixture
        .api
        .simulate_transactions(
            vec![
                CallRequest {
                    inner: authorize,
                    tempo: None,
                },
                call_request(addresses.funded, authority),
            ],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    assert!(simulated.stats.success, "{simulated:#?}");
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[1])),
        vec![U256::ONE]
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_uses_anchor_environment_and_plain_block_overrides() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let request = || call_request(addresses.funded, addresses.environment);

    let at_anchor = fixture
        .api
        .simulate_transactions(vec![request()], anchor_context(), None)
        .await
        .unwrap();
    let words = output_words(&root_trace_output(&at_anchor.results[0]));
    assert_eq!(at_anchor.stats.block_num, ANCHOR_NUMBER);
    assert_eq!(words[0], U256::from(ANCHOR_NUMBER));
    assert_eq!(words[1], U256::from(ANCHOR_BASE_FEE));

    let base_fee_only = fixture
        .api
        .simulate_transactions(
            vec![request()],
            anchor_context(),
            Some(BlockOverrides {
                base_fee: Some(U256::from(99)),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let words = output_words(&root_trace_output(&base_fee_only.results[0]));
    assert_eq!(words[0], U256::from(ANCHOR_NUMBER));
    assert_eq!(words[1], U256::from(99));

    for number in [ANCHOR_NUMBER + 1, ANCHOR_NUMBER + 2] {
        let overridden = fixture
            .api
            .simulate_transactions(
                vec![request()],
                anchor_context(),
                Some(BlockOverrides::default().with_number(U256::from(number))),
            )
            .await
            .unwrap();
        let words = output_words(&root_trace_output(&overridden.results[0]));
        assert_eq!(overridden.stats.block_num, ANCHOR_NUMBER);
        assert_eq!(words[0], U256::from(number));
        assert_eq!(words[1], U256::from(ANCHOR_BASE_FEE));
        assert_ne!(words[1], U256::from(ENCODED_NEXT_BASE_FEE));
    }

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_preserves_system_normal_and_delegatecall_log_emitters() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let value_log = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.logger)
            .value(U256::from(5)),
        tempo: None,
    };
    let result = fixture
        .api
        .simulate_transactions(
            vec![
                value_log,
                call_request(addresses.funded, addresses.delegate_proxy),
            ],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    assert!(result.stats.success);
    assert_eq!(
        result.results[0].events.len(),
        2,
        "events: {:#?}",
        result.results[0].events
    );
    assert_eq!(
        result.results[0].events[0].contract_id,
        "0xfffffffffffffffffffffffffffffffffffffffe"
            .parse::<Address>()
            .unwrap()
    );
    assert_eq!(result.results[0].events[1].contract_id, addresses.logger);
    assert_eq!(result.results[1].events.len(), 1);
    assert_eq!(
        result.results[1].events[0].contract_id,
        addresses.delegate_proxy
    );
    assert_ne!(
        result.results[1].events[0].contract_id,
        addresses.delegate_implementation
    );

    let reverted = fixture
        .api
        .simulate_transactions(
            vec![CallRequest {
                inner: TransactionRequest::default()
                    .from(addresses.funded)
                    .to(addresses.logger_revert)
                    .value(U256::from(5)),
                tempo: None,
            }],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(reverted.results[0].code, DebankErrorCode::EvmRevert as i32);
    assert!(reverted.results[0].events.is_empty());

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_preserves_selfdestruct_event_and_rollback() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let nonzero = fixture
        .api
        .simulate_transactions(
            vec![call_request(
                addresses.funded,
                addresses.selfdestruct_nonzero,
            )],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    let zero = fixture
        .api
        .simulate_transactions(
            vec![call_request(addresses.funded, addresses.selfdestruct_zero)],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    let reverted = fixture
        .api
        .simulate_transactions(
            vec![call_request(
                addresses.funded,
                addresses.selfdestruct_revert_parent,
            )],
            anchor_context(),
            None,
        )
        .await
        .unwrap();

    let nonzero_result = &nonzero.results[0];
    let zero_result = &zero.results[0];
    let reverted_result = &reverted.results[0];
    assert_eq!(nonzero_result.code, 0);
    assert_eq!(nonzero_result.traces.len(), 1);
    let root = &nonzero_result.traces[0];
    assert_eq!(root.call_create_type, "suicide");
    assert!(root.parent_trace_id.is_empty());
    assert_eq!(root.pos_in_parent_trace, 0);
    assert_eq!(root.id, root.debank_id());
    assert_eq!(nonzero_result.events.len(), 1);
    assert_arc_transfer_event(
        &nonzero_result.events[0],
        addresses.selfdestruct_nonzero,
        addresses.selfdestruct_beneficiary,
        U256::from(42),
        &root.id,
        0,
    );

    assert_eq!(zero_result.code, 0);
    assert_eq!(zero_result.traces.len(), 1);
    assert_eq!(zero_result.traces[0].call_create_type, "suicide");
    assert!(zero_result.events.is_empty());

    assert_eq!(reverted_result.code, DebankErrorCode::EvmRevert as i32);
    assert!(
        reverted_result.events.is_empty(),
        "parent REVERT leaked SELFDESTRUCT event: {reverted_result:#?}"
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_inspector_resets_log_cursor_after_reverted_child() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let simulated = fixture
        .api
        .simulate_transactions(
            vec![call_request(
                addresses.funded,
                addresses.reverted_child_then_selfdestruct,
            )],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    let result = &simulated.results[0];

    assert_eq!(result.code, 0);
    // The reverted child and both of its logs are omitted. The parent
    // still exposes its later EIP-7708 SELFDESTRUCT event.
    let root = &result.traces[0];
    assert_eq!(result.events.len(), 1, "{result:#?}");
    assert_arc_transfer_event(
        &result.events[0],
        addresses.reverted_child_then_selfdestruct,
        addresses.selfdestruct_beneficiary,
        U256::from(10),
        &root.id,
        0,
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_preserves_create_value_transfer_event_fields() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let value = U256::from(9);
    let created = addresses.funded.create(0);
    let runtime = counter_code();
    let request = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .value(value)
            .input(TransactionInput::new(init_code(&runtime))),
        tempo: None,
    };
    let simulated = fixture
        .api
        .simulate_transactions(
            vec![request, call_request(addresses.funded, created)],
            anchor_context(),
            None,
        )
        .await
        .unwrap();
    let result = &simulated.results[0];

    assert_eq!(result.code, 0);
    assert_eq!(result.traces.len(), 1);
    let root = &result.traces[0];
    assert_eq!(root.call_create_type, "create");
    assert_eq!(root.to_addr, created);
    assert_eq!(root.value, value);
    assert_eq!(result.events.len(), 1);
    assert_arc_transfer_event(
        &result.events[0],
        addresses.funded,
        created,
        value,
        &root.id,
        0,
    );
    assert_eq!(
        output_words(&root_trace_output(&simulated.results[1])),
        vec![U256::ONE]
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_inspector_orders_value_and_precompile_logs_and_rolls_both_back_on_oog() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let native_coin_authority: Address = "0x1800000000000000000000000000000000000000"
        .parse()
        .unwrap();
    let value = U256::from(7);
    let minted = U256::from(10);
    let input = native_coin_mint_input(addresses.empty, minted);
    let request = |gas_limit| CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.native_fiat_token)
            .to(native_coin_authority)
            .value(value)
            .gas_limit(gas_limit)
            .input(TransactionInput::new(input.clone())),
        tempo: None,
    };

    let successful = fixture
        .api
        .simulate_transactions(vec![request(100_000)], anchor_context(), None)
        .await
        .unwrap();
    assert!(successful.stats.success, "{successful:#?}");
    let success = &successful.results[0];
    assert_eq!(success.code, 0);
    assert_eq!(success.events.len(), 2, "{success:#?}");
    let root = &success.traces[0];
    assert_arc_transfer_event(
        &success.events[0],
        addresses.native_fiat_token,
        native_coin_authority,
        value,
        &root.id,
        0,
    );
    assert_arc_transfer_event(
        &success.events[1],
        Address::ZERO,
        addresses.empty,
        minted,
        &root.id,
        1,
    );

    let failed = fixture
        .api
        .simulate_transactions(vec![request(success.gas_used - 1)], anchor_context(), None)
        .await
        .unwrap();
    assert!(!failed.stats.success, "{failed:#?}");
    assert_eq!(
        failed.results[0].code,
        DebankErrorCode::GasExhausted as i32,
        "{failed:#?}"
    );
    assert!(failed.results[0].events.is_empty(), "{failed:#?}");

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_keeps_nested_zero_value_precompile_trace_and_event() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
        .parse()
        .unwrap();

    let simulated = fixture
        .api
        .simulate_transactions(
            vec![call_request(addresses.funded, addresses.native_fiat_token)],
            anchor_context(),
            None,
        )
        .await
        .unwrap();

    assert!(simulated.stats.success, "{simulated:#?}");
    let result = &simulated.results[0];
    assert_eq!(result.code, 0, "{result:#?}");
    assert_eq!(output_words(&root_trace_output(result)), vec![U256::ONE]);
    assert_eq!(result.traces.len(), 2, "{result:#?}");

    let root = &result.traces[0];
    let precompile = &result.traces[1];
    assert_eq!(precompile.to_addr, native_coin_control);
    assert_eq!(precompile.value, U256::ZERO);
    assert_eq!(precompile.parent_trace_id, root.id);
    assert_eq!(precompile.pos_in_parent_trace, 0);

    assert_eq!(result.events.len(), 1, "{result:#?}");
    let event = &result.events[0];
    assert_eq!(event.contract_id, native_coin_control);
    assert_eq!(
        event.selector,
        keccak256("Blocklisted(address)").to_string()
    );
    assert_eq!(
        event.topics,
        vec![H256::left_padding_from(addresses.empty.as_slice()).to_string()]
    );
    assert!(event.data.is_empty());
    assert_eq!(event.parent_trace_id, precompile.id);
    assert_eq!(event.pos_in_parent_trace, 0);
    assert_eq!(event.id, event.debank_id());

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_simulation_returns_rpc_error_for_top_level_invalid_transaction() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let error = fixture
        .api
        .simulate_transactions(
            vec![call_request(addresses.blocked, addresses.empty)],
            anchor_context(),
            None,
        )
        .await
        .unwrap_err();

    assert_eq!(error.code(), DebankErrorCode::EvmFailed as i32);
    assert!(error.message().contains("Blocked address"));

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_normal_and_inspect_paths_share_nca_pq_and_p256_precompiles() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;
    let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));
    let nca: Address = "0x1800000000000000000000000000000000000000"
        .parse()
        .unwrap();
    let pq: Address = "0x1800000000000000000000000000000000000004"
        .parse()
        .unwrap();
    let p256: Address = "0x0000000000000000000000000000000000000100"
        .parse()
        .unwrap();
    let nca_request = request_with_input(addresses.funded, nca, selector("totalSupply()"));
    let p256_request = request_with_input(addresses.funded, p256, p256_valid_input());

    let nca_call = fixture
        .api
        .call(nca_request.clone(), anchor, None, None)
        .await
        .unwrap();
    let p256_call = fixture
        .api
        .call(p256_request.clone(), anchor, None, None)
        .await
        .unwrap();
    assert_eq!(output_words(&nca_call), vec![U256::ZERO]);
    assert_eq!(output_words(&p256_call), vec![U256::ONE]);

    let inspected = fixture
        .api
        .simulate_transactions(vec![nca_request, p256_request], anchor_context(), None)
        .await
        .unwrap();
    assert_eq!(root_trace_output(&inspected.results[0]), nca_call);
    assert_eq!(root_trace_output(&inspected.results[1]), p256_call);

    let malformed_pq = request_with_input(
        addresses.funded,
        pq,
        selector("verifySlhDsaSha2128s(bytes,bytes,bytes)"),
    );
    assert!(fixture
        .api
        .call(malformed_pq.clone(), anchor, None, None)
        .await
        .is_err());
    let inspected_pq = fixture
        .api
        .simulate_transactions(vec![malformed_pq], anchor_context(), None)
        .await
        .unwrap();
    assert_eq!(
        inspected_pq.results[0].code,
        DebankErrorCode::EvmRevert as i32
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_estimate_uses_shared_transfer_and_validation_policy() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;

    let transfer = call_request(addresses.funded, addresses.empty);
    assert_eq!(
        estimate(&fixture.api, transfer, None).await.unwrap(),
        U256::from(MIN_TRANSACTION_GAS)
    );

    let transfer_with_low_request_gas = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty)
            .gas_limit(20_000),
        tempo: None,
    };
    assert_eq!(
        estimate(&fixture.api, transfer_with_low_request_gas, None)
            .await
            .unwrap(),
        U256::from(MIN_TRANSACTION_GAS)
    );

    let value_without_balance = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.empty)
            .to(addresses.funded)
            .value(U256::ONE),
        tempo: None,
    };
    assert_eq!(
        estimate(&fixture.api, value_without_balance, None)
            .await
            .unwrap(),
        U256::from(MIN_TRANSACTION_GAS)
    );

    let fee_without_balance = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.empty)
            .to(addresses.funded)
            .gas_price(1),
        tempo: None,
    };
    assert_eq!(
        estimate(&fixture.api, fee_without_balance, None)
            .await
            .unwrap(),
        U256::from(MIN_TRANSACTION_GAS)
    );

    let contract_sender = call_request(addresses.environment, addresses.empty);
    assert_eq!(
        estimate(&fixture.api, contract_sender, None).await.unwrap(),
        U256::from(MIN_TRANSACTION_GAS)
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_estimate_handles_gas_dependent_revert_and_returns_executable_gas() {
    let fixture = build_arc_fixture(100);
    let request = call_request(fixture.addresses.funded, fixture.addresses.gas_guard);

    let estimated: u64 = estimate(&fixture.api, request.clone(), None)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert!(estimated < ARC_RPC_GAS_CAP);
    assert!(matches!(
        execute_arc_estimate_probe(&fixture.api, request.clone(), None, estimated),
        ExecutionResult::Success { .. }
    ));
    assert!(matches!(
        execute_arc_estimate_probe(
            &fixture.api,
            request,
            None,
            estimated.saturating_sub(20_000),
        ),
        ExecutionResult::Revert { .. }
    ));

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_estimate_uses_generic_block_overrides() {
    let fixture = build_arc_fixture(100);
    let request = call_request(fixture.addresses.funded, fixture.addresses.environment);

    let h_gas: u64 = estimate(&fixture.api, request.clone(), None)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    let h_output = success_output(execute_arc_estimate_probe(
        &fixture.api,
        request.clone(),
        None,
        h_gas,
    ));
    let h_words = output_words(&h_output);
    assert_eq!(h_words[0], U256::from(ANCHOR_NUMBER));
    assert_eq!(h_words[1], U256::from(ANCHOR_BASE_FEE));
    assert_eq!(
        h_words[2],
        U256::from_be_slice(H256::repeat_byte(0xaa).as_slice())
    );

    let overridden_hash = H256::repeat_byte(0xcc);
    let mut block_hashes = BTreeMap::new();
    block_hashes.insert(ANCHOR_NUMBER, overridden_hash);
    let overridden = BlockOverrides {
        number: Some(U256::from(ANCHOR_NUMBER + 1)),
        base_fee: Some(U256::from(11)),
        block_hash: Some(block_hashes),
        ..Default::default()
    };
    let overridden_gas: u64 = estimate(&fixture.api, request.clone(), Some(overridden.clone()))
        .await
        .unwrap()
        .try_into()
        .unwrap();
    let overridden_output = success_output(execute_arc_estimate_probe(
        &fixture.api,
        request,
        Some(overridden),
        overridden_gas,
    ));
    let overridden_words = output_words(&overridden_output);
    assert_eq!(overridden_words[0], U256::from(ANCHOR_NUMBER + 1));
    assert_eq!(overridden_words[1], U256::from(11));
    assert_eq!(
        overridden_words[3],
        U256::from_be_slice(overridden_hash.as_slice())
    );

    fixture.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_estimate_preserves_revert_and_normal_transaction_errors() {
    let fixture = build_arc_fixture(100);
    let addresses = fixture.addresses;

    let revert = estimate(
        &fixture.api,
        call_request(addresses.funded, addresses.revert),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(revert.code(), DebankErrorCode::EvmRevert as i32);
    assert_eq!(revert.message(), "");

    let blocked = estimate(
        &fixture.api,
        call_request(addresses.blocked, addresses.empty),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(blocked.code(), DebankErrorCode::EvmFailed as i32);
    assert!(blocked.message().contains("Blocked address"));

    let invalid_fee = CallRequest {
        inner: TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty)
            .gas_price(1)
            .max_fee_per_gas(2),
        tempo: None,
    };
    let invalid_fee = estimate(&fixture.api, invalid_fee, None).await.unwrap_err();
    assert_eq!(invalid_fee.code(), DebankErrorCode::InvalidParams as i32);
    assert_eq!(invalid_fee.message(), "Invalid fee parameters");

    let mut empty_authorization = TransactionRequest::default()
        .from(addresses.funded)
        .to(addresses.empty);
    empty_authorization.authorization_list = Some(Vec::new());
    let empty_authorization = estimate(
        &fixture.api,
        CallRequest {
            inner: empty_authorization,
            tempo: None,
        },
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        empty_authorization.code(),
        DebankErrorCode::EvmFailed as i32
    );
    assert!(empty_authorization
        .message()
        .to_ascii_lowercase()
        .contains("authorization"));

    fixture.close();
}
