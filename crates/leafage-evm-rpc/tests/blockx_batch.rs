//! End-to-end tests for `blockx_stateReadBatch` (BSRB/1 wire) over a
//! real RocksDB-backed StateTree and jsonrpsee HTTP server: batch
//! results must be byte-identical to the single getAddressCode /
//! getStorageAt responses, including error code/message text, strict
//! decode rejects and the per-item historical fallback.

use alloy::primitives::keccak256;
use jsonrpsee::core::ClientError;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{ApiBuilder, BlockxApiClient, DebankApiClient, MultiChainCfgEnv};
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
    StorageKind,
};
use leafage_evm_types::{
    AccountStorageDiff, Address, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff,
    BlockType, BsrbContext, BsrbOutcome, BsrbRead, BsrbRequest, BsrbResponse, Bytes, CfgEnv,
    DebankBlockContext, IndexValuePair, JsonStorageKey, MainnetSpecId, NewAccount, NewCode, H256,
    U256,
};
use std::sync::Arc;
use std::time::Duration;

const CONTRACT: Address = Address::repeat_byte(0x33);
const PROXY: Address = Address::repeat_byte(0x44);
const ALICE: Address = Address::repeat_byte(0x11);
const MISSING: Address = Address::repeat_byte(0x77);

fn h(n: u8) -> H256 {
    H256::repeat_byte(n)
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

fn contract_code() -> Bytes {
    Bytes::from(vec![0x60u8, 0x80, 0x60, 0x40, 0x52])
}

fn pos(n: u8) -> H256 {
    H256::with_last_byte(n)
}

/// DB storage key for a position under normalize_state_key=false.
fn slot_key(position: H256) -> H256 {
    keccak256(position.as_slice())
}

fn genesis_diff() -> BlockStorageDiff {
    let code = contract_code();
    let code_hash: H256 = keccak256(&code);
    let mut diff = BlockStorageDiff::default();
    diff.new_accounts.push(NewAccount {
        address: keccak256(ALICE.as_slice()),
        balance: U256::from(1u64),
        nonce: 0,
        code_hash: H256::ZERO,
    });
    for contract in [CONTRACT, PROXY] {
        diff.new_accounts.push(NewAccount {
            address: keccak256(contract.as_slice()),
            balance: U256::ZERO,
            nonce: 1,
            code_hash,
        });
    }
    diff.new_codes.push(NewCode { code_hash, code });
    diff
}

fn storage_diff_block1() -> BlockStorageDiff {
    let mut diff = BlockStorageDiff::default();
    diff.storage_diffs.push(AccountStorageDiff {
        address: keccak256(CONTRACT.as_slice()),
        diffs: vec![IndexValuePair {
            index: slot_key(pos(1)),
            value: U256::from(0xabcdu64),
        }],
    });
    diff
}

/// Full three-block chain: genesis on disk, storage diff and an empty
/// diff as in-memory layers, so batch reads cross diff layers, the
/// shared cache and RocksDB.
fn build_full_tree(db_path: &std::path::Path) -> Arc<StateTree<MultiStorage>> {
    let db = MultiStorage::open(db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(block_info(0, h(0xaa), H256::ZERO), genesis_diff())
    .unwrap();
    let tree =
        Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
    tree.update_block(block_info(1, h(0xbb), h(0xaa)), storage_diff_block1())
        .unwrap();
    tree.update_block(block_info(2, h(0xcc), h(0xbb)), BlockStorageDiff::default())
        .unwrap();
    tree
}

fn test_cfg() -> MultiChainCfgEnv {
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM);
    cfg.chain_id = 1;
    MultiChainCfgEnv::Mainnet(cfg)
}

/// The single-method block context equivalent to a BSRB context.
fn debank_ctx(context: BsrbContext) -> DebankBlockContext {
    DebankBlockContext {
        block_id: match context {
            BsrbContext::Hash(hash) => BlockId::Hash(hash.into()),
            BsrbContext::Number(number) => BlockId::Number(BlockNumberOrTag::Number(number)),
        },
        block_type: BlockType::Equals,
    }
}

fn code_read(address: Address) -> BsrbRead {
    BsrbRead::AddressCode { address }
}

fn storage_read(address: Address, slot: H256) -> BsrbRead {
    BsrbRead::StorageAt { address, slot }
}

async fn send(client: &HttpClient, request: &BsrbRequest) -> Result<BsrbResponse, ClientError> {
    let payload = BlockxApiClient::state_read_batch(client, Bytes::from(request.encode())).await?;
    Ok(BsrbResponse::decode(&payload).expect("server must return a decodable BSRB response"))
}

fn value(outcome: &BsrbOutcome) -> &Bytes {
    match outcome {
        BsrbOutcome::Value(value) => value,
        BsrbOutcome::Error { code, message } => {
            panic!("expected value, got error {code}: {message}")
        }
    }
}

fn error(outcome: &BsrbOutcome) -> (i32, &str) {
    match outcome {
        BsrbOutcome::Error { code, message } => (*code, message.as_str()),
        BsrbOutcome::Value(value) => panic!("expected error, got value {value}"),
    }
}

fn word_bytes(word: H256) -> Bytes {
    Bytes::copy_from_slice(word.as_slice())
}

fn call_error(err: ClientError) -> jsonrpsee::types::ErrorObjectOwned {
    match err {
        ClientError::Call(err) => err,
        other => panic!("expected call error, got {other:?}"),
    }
}

async fn expect_invalid_params(client: &HttpClient, payload: Vec<u8>, what: &str) {
    let err = call_error(
        BlockxApiClient::state_read_batch(client, Bytes::from(payload))
            .await
            .expect_err(what),
    );
    assert_eq!(err.code(), -32602, "{what}: {}", err.message());
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_matches_single_methods_byte_for_byte() {
    let db_path = std::env::temp_dir().join(format!(
        "leafage-blockx-batch-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&db_path);
    std::fs::create_dir_all(&db_path).unwrap();
    let tree = build_full_tree(&db_path);

    let addr = "127.0.0.1:18561";
    let handle = ApiBuilder::new(tree.clone(), test_cfg())
        .with_state_read_concurrency(2)
        .build_and_run(
            addr,
            100,
            Duration::from_secs(10),
            false,
            false,
            "blockx-batch-test".to_string(),
            100,
            1024,
        )
        .await
        .unwrap();
    let client = HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();

    for context in [BsrbContext::Number(2), BsrbContext::Hash(h(0xcc))] {
        let ctx = debank_ctx(context);
        let request = BsrbRequest {
            context,
            reads: vec![
                code_read(CONTRACT),
                code_read(PROXY),
                code_read(ALICE),
                code_read(MISSING),
                storage_read(CONTRACT, pos(1)),
                storage_read(CONTRACT, pos(2)),
                // Duplicate key is legal and must resolve to the same
                // value; order is the correlation.
                storage_read(CONTRACT, pos(1)),
            ],
        };
        let resp = send(&client, &request).await.unwrap();
        assert_eq!(resp.results.len(), request.reads.len());

        for (read, outcome) in request.reads.iter().zip(&resp.results) {
            let batch_value = value(outcome).clone();
            let single_value = match read {
                BsrbRead::AddressCode { address } => {
                    DebankApiClient::get_address_code(&client, *address, Some(ctx.clone()))
                        .await
                        .unwrap()
                }
                BsrbRead::StorageAt { address, slot } => word_bytes(
                    DebankApiClient::get_storage_at(
                        &client,
                        *address,
                        JsonStorageKey::from(*slot),
                        Some(ctx.clone()),
                    )
                    .await
                    .unwrap(),
                ),
            };
            assert_eq!(batch_value, single_value, "read {:?}", read);
        }

        // Spot-check the actual values, not only equality.
        assert_eq!(value(&resp.results[0]), &contract_code());
        assert_eq!(value(&resp.results[3]), &Bytes::new());
        let word: [u8; 32] = U256::from(0xabcdu64).to_be_bytes();
        assert_eq!(value(&resp.results[4]), &word_bytes(word.into()));
        assert_eq!(value(&resp.results[5]), &word_bytes(H256::ZERO));
        assert_eq!(value(&resp.results[6]), value(&resp.results[4]));
    }

    // A fixed block the state node does not have: the batch-level error
    // must match the single-method error byte-for-byte.
    let stale = BsrbContext::Number(999);
    let batch_err = call_error(
        send(
            &client,
            &BsrbRequest {
                context: stale,
                reads: vec![storage_read(CONTRACT, pos(1))],
            },
        )
        .await
        .expect_err("unknown block must fail"),
    );
    let single_err = call_error(
        DebankApiClient::get_storage_at(
            &client,
            CONTRACT,
            JsonStorageKey::from(pos(1)),
            Some(debank_ctx(stale)),
        )
        .await
        .expect_err("unknown block must fail"),
    );
    assert_eq!(batch_err.code(), single_err.code());
    assert_eq!(batch_err.message(), single_err.message());
    assert_eq!(batch_err.code(), -39006);

    // Strict decode rejects: every malformed payload fails with -32602
    // before any state work.
    let good = BsrbRequest {
        context: BsrbContext::Number(2),
        reads: vec![code_read(CONTRACT)],
    }
    .encode();

    expect_invalid_params(
        &client,
        BsrbRequest {
            context: BsrbContext::Number(2),
            reads: vec![],
        }
        .encode(),
        "empty reads",
    )
    .await;
    expect_invalid_params(
        &client,
        BsrbRequest {
            context: BsrbContext::Number(2),
            reads: (0..65).map(|_| code_read(CONTRACT)).collect(),
        }
        .encode(),
        "over hard cap",
    )
    .await;
    let mut bad_version = good.clone();
    bad_version[0] = 9;
    expect_invalid_params(&client, bad_version, "unknown version").await;
    let mut bad_ctx_kind = good.clone();
    bad_ctx_kind[1] = 7;
    expect_invalid_params(&client, bad_ctx_kind, "unknown context kind").await;
    let mut bad_read_kind = good.clone();
    // version(1) + ctx_kind(1) + height(8) + count(2) => first item kind.
    bad_read_kind[12] = 9;
    expect_invalid_params(&client, bad_read_kind, "unknown read kind").await;
    let mut trailing = good.clone();
    trailing.push(0);
    expect_invalid_params(&client, trailing, "trailing bytes").await;
    let truncated = good[..good.len() - 1].to_vec();
    expect_invalid_params(&client, truncated, "truncated payload").await;

    handle.stop().unwrap();
    let _ = std::fs::remove_dir_all(&db_path);
}

/// Primary node only has genesis; the historical node has the full
/// chain. Items failing locally are retried per item against the
/// historical client, and items failing on both sides carry the
/// combined error — identical to the single-method fallback text.
#[tokio::test(flavor = "multi_thread")]
async fn batch_falls_back_to_historical_per_item() {
    let unique = format!("{}-{:?}", std::process::id(), std::thread::current().id());
    let hist_path = std::env::temp_dir().join(format!("leafage-blockx-hist-{unique}"));
    let primary_path = std::env::temp_dir().join(format!("leafage-blockx-primary-{unique}"));
    for p in [&hist_path, &primary_path] {
        let _ = std::fs::remove_dir_all(p);
        std::fs::create_dir_all(p).unwrap();
    }

    // Historical node: full chain.
    let hist_tree = build_full_tree(&hist_path);
    let hist_addr = "127.0.0.1:18562";
    let hist_handle = ApiBuilder::new(hist_tree.clone(), test_cfg())
        .build_and_run(
            hist_addr,
            100,
            Duration::from_secs(10),
            false,
            false,
            "blockx-hist-test".to_string(),
            100,
            1024,
        )
        .await
        .unwrap();

    // Primary node: genesis only, historical fallback configured.
    let primary_db =
        MultiStorage::open(&primary_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
    StateDBWrapper(
        primary_db
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
    .update_block(block_info(0, h(0xaa), H256::ZERO), genesis_diff())
    .unwrap();
    let primary_tree = Arc::new(
        StateTree::new(primary_db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap(),
    );
    let primary_addr = "127.0.0.1:18563";
    let primary_handle = ApiBuilder::new(primary_tree.clone(), test_cfg())
        .with_historical_config(Some(format!("http://{hist_addr}")), Some(1000))
        .with_state_read_concurrency(1)
        .build_and_run(
            primary_addr,
            100,
            Duration::from_secs(10),
            false,
            false,
            "blockx-primary-test".to_string(),
            100,
            1024,
        )
        .await
        .unwrap();

    let primary = HttpClientBuilder::default()
        .build(format!("http://{primary_addr}"))
        .unwrap();
    let hist = HttpClientBuilder::default()
        .build(format!("http://{hist_addr}"))
        .unwrap();

    // Block 2 exists only on the historical node.
    let remote = BsrbContext::Number(2);
    let resp = send(
        &primary,
        &BsrbRequest {
            context: remote,
            reads: vec![
                code_read(CONTRACT),
                storage_read(CONTRACT, pos(1)),
                storage_read(CONTRACT, pos(2)),
            ],
        },
    )
    .await
    .unwrap();
    assert!(resp
        .results
        .iter()
        .all(|r| matches!(r, BsrbOutcome::Value(_))));
    let hist_code = DebankApiClient::get_address_code(&hist, CONTRACT, Some(debank_ctx(remote)))
        .await
        .unwrap();
    assert_eq!(value(&resp.results[0]), &hist_code);
    let hist_storage = DebankApiClient::get_storage_at(
        &hist,
        CONTRACT,
        JsonStorageKey::from(pos(1)),
        Some(debank_ctx(remote)),
    )
    .await
    .unwrap();
    assert_eq!(value(&resp.results[1]), &word_bytes(hist_storage));

    // Block 7 exists nowhere: per-item combined errors, identical to
    // the single-method fallback error.
    let nowhere = BsrbContext::Number(7);
    let resp = send(
        &primary,
        &BsrbRequest {
            context: nowhere,
            reads: vec![storage_read(CONTRACT, pos(1))],
        },
    )
    .await
    .unwrap();
    let (item_code, item_message) = error(&resp.results[0]);
    let single_err = call_error(
        DebankApiClient::get_storage_at(
            &primary,
            CONTRACT,
            JsonStorageKey::from(pos(1)),
            Some(debank_ctx(nowhere)),
        )
        .await
        .expect_err("block exists nowhere"),
    );
    assert_eq!(item_code, single_err.code());
    assert_eq!(item_message, single_err.message());
    assert_eq!(item_code, -39006);
    assert!(item_message.starts_with("Local error: "));
    assert!(item_message.contains("Historical RPC error: "));

    primary_handle.stop().unwrap();
    hist_handle.stop().unwrap();
    for p in [&hist_path, &primary_path] {
        let _ = std::fs::remove_dir_all(p);
    }
}
