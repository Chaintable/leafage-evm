use crate::{
    bundle::s3_read_bundle,
    utils::{s3_get_block_diff, s3_get_block_info, s3_get_block_transactions},
};
use alloy::{
    consensus::constants::{EMPTY_ROOT_HASH, KECCAK_EMPTY},
    primitives::{address, keccak256},
};
use alloy_rlp::Decodable;
use anyhow::{ensure, Context, Result};
use aws_sdk_s3::{
    config::{Credentials, Region},
    Client,
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    Router,
};
use flate2::read::GzDecoder;
use leafage_evm_storage::{
    BlockContext, EvmStorageRead, EvmStorageWrite, MultiStorage, StateDB, StateDBProvider,
    StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{
    Address, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, Bytes, DebankOutPut,
    DebankTransaction, NewAccount, H256, U256,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const FIXTURE_CHAIN: &str = "arc-a1b";
const FIXTURE_BUCKET: &str = "fixtures";
const WRITER_BASELINE_COMMIT: &str = "23d38e7d0cbf54e184faf3751c619f2169b3ed79";
const WRITER_RELEASE_COMMIT: &str = "79b6fddf18345732007bb94b4af3add4c2efd12d";
const EXPORTER_COMMIT: &str = "5ab40f925d551f36fda40210bd4f81afd92de1ba";
const TRANSFORMER_BLOB: &str = "9b10987627f4bfac9b0b5b94cf8f3c9643950f14";
const FORMAT_REFERENCE_COMMIT: &str = "7c4e096bfbc132dcb79312e2371c80919b966a52";
const STATE_DIFF_INDEX_BYTES: usize = 8_133;
const HISTORY_STORAGE_ADDRESS: Address = address!("0000f90827f1c53a10cb7a02335b175320002935");
const SYSTEM_ACCOUNTING_ADDRESS: Address = address!("1800000000000000000000000000000000000002");
const NORMALIZED_CAPTURE_SHA256: [(&str, &str); 5] = [
    (
        "genesis",
        "58639c78e6c0ddac56ed00600b84b3af0709ab6d1cffd213716f45bfdaa4f84f",
    ),
    (
        "empty-hooks",
        "bf924bb2f661593e5833855663076161821e58fd5f0d94d366a9db3c0ba117cc",
    ),
    (
        "native-transfer",
        "38853b0678b1f8807a8eaa532bc2579350415bc2d44089aebac4497ebf44b408",
    ),
    (
        "create2",
        "30e04d2165a5d7633769eff003b1ef930c3a2358857d942364e399005ed50b8d",
    ),
    (
        "failed-create",
        "15c8b5fa325f56e60b6d26cada5be95cf28d969501349480056a68f5e76c23be",
    ),
];
static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize)]
struct ManifestProducerBaseline {
    repository: String,
    commit: String,
    release: String,
    release_commit: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestProducer {
    repository: String,
    commit: String,
    baseline_commit: String,
    changes_from_baseline: Vec<String>,
    source_policy: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestCommit {
    repository: String,
    commit: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestFormatSource {
    path: String,
    blob: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestFixtureTransformer {
    repository: String,
    commit: String,
    entrypoint: String,
    entrypoint_blob: String,
    compatibility_contract: ManifestCompatibilityContract,
    encoding: ManifestEncoding,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestCompatibilityContract {
    gzip_container: String,
    gzip_json_payload: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestEncoding {
    bundle0: String,
    outer_block_file: String,
    per_block_header: String,
    per_block_state_diff: String,
    rpc: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestFormatReference {
    repository: String,
    release: String,
    commit: String,
    scope: Vec<String>,
    sources: Vec<ManifestFormatSource>,
    executed_by_fixture_generation: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestCaptureHash {
    label: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestNormalization {
    process_start_timestamp: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestGeneration {
    comparison_normalization: ManifestNormalization,
    independent_capture_verification: Vec<ManifestCaptureHash>,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestBlock {
    label: String,
    number: u64,
    hash: H256,
    parent_hash: H256,
    state_root: H256,
    extra_data: String,
    validation_hash: i64,
    process_start_timestamp: u64,
    rpc: String,
    header: String,
    state_diff: String,
    block_file: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ChainContext {
    chain_id: u64,
    wallet: Address,
    native_recipient: Address,
    create2_probe: Address,
    create2_child: Address,
    failed_create_address: Address,
}

#[derive(Debug, Deserialize)]
struct FixtureManifest {
    schema_version: u64,
    producer_baseline: ManifestProducerBaseline,
    producer: ManifestProducer,
    exporter: ManifestCommit,
    fixture_transformer: ManifestFixtureTransformer,
    format_reference: ManifestFormatReference,
    generation: ManifestGeneration,
    chain: ChainContext,
    blocks: Vec<ManifestBlock>,
    files: Vec<ManifestFile>,
    coverage: Vec<String>,
    excluded: Vec<String>,
}

#[derive(Clone, Debug)]
struct FixtureBlock {
    manifest: ManifestBlock,
    rpc: DebankOutPut,
    rpc_json: Value,
    block_info: BlockInfo,
    diff: BlockStorageDiff,
    outer_block_file: Value,
}

#[derive(Debug)]
struct FixtureSet {
    root: PathBuf,
    manifest: FixtureManifest,
    blocks: Vec<FixtureBlock>,
}

impl FixtureSet {
    fn load() -> Result<Self> {
        let root = fixture_root();
        let manifest: FixtureManifest = serde_json::from_slice(
            &fs::read(root.join("manifest.json"))
                .with_context(|| format!("read fixture manifest from {}", root.display()))?,
        )?;
        let mut blocks = Vec::with_capacity(manifest.blocks.len());
        for block in &manifest.blocks {
            let rpc_bytes = fs::read(root.join(&block.rpc))?;
            let rpc = serde_json::from_slice(&rpc_bytes)?;
            let rpc_json = serde_json::from_slice(&rpc_bytes)?;

            let header_json = decode_gzip(&fs::read(root.join(&block.header))?)?;
            let block_info = serde_json::from_slice(&header_json)?;

            let diff_bytes = fs::read(root.join(&block.state_diff))?;
            let mut diff_slice = diff_bytes.as_slice();
            let diff = BlockStorageDiff::decode(&mut diff_slice)?;
            ensure!(
                diff_slice.is_empty(),
                "{} StateDiff has {} trailing bytes",
                block.label,
                diff_slice.len()
            );

            let outer_block_file =
                serde_json::from_slice(&decode_gzip(&fs::read(root.join(&block.block_file))?)?)?;
            blocks.push(FixtureBlock {
                manifest: block.clone(),
                rpc,
                rpc_json,
                block_info,
                diff,
                outer_block_file,
            });
        }
        Ok(Self {
            root,
            manifest,
            blocks,
        })
    }

    fn object_map(&self) -> Result<HashMap<String, Vec<u8>>> {
        let mut objects = HashMap::new();
        for block in &self.blocks {
            objects.insert(
                format!("{FIXTURE_CHAIN}/{}/block", block.manifest.hash),
                fs::read(self.root.join(&block.manifest.header))?,
            );
            objects.insert(
                format!("{FIXTURE_CHAIN}/{}/stateDiff", block.manifest.state_root),
                fs::read(self.root.join(&block.manifest.state_diff))?,
            );
            objects.insert(
                format!("{FIXTURE_CHAIN}/{}", block.manifest.hash),
                fs::read(self.root.join(&block.manifest.block_file))?,
            );
        }
        for resource in ["block", "stateDiff"] {
            objects.insert(
                format!("{FIXTURE_CHAIN}/0/{resource}"),
                fs::read(self.root.join("bundle/0").join(resource))?,
            );
        }
        Ok(objects)
    }
}

fn fixture_root() -> PathBuf {
    std::env::var_os("LEAFAGE_ARC_A1B_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/arc-a1b"))
}

fn decode_gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;
    Ok(decoded)
}

fn normalize_process_start_timestamp(value: &mut Value) -> Result<u64> {
    let timestamp = value
        .pointer("/block/process_start_timestamp")
        .and_then(Value::as_u64)
        .context("BlockFile process_start_timestamp is missing")?;
    *value
        .pointer_mut("/block/process_start_timestamp")
        .context("BlockFile process_start_timestamp is missing")? = json!(0);
    Ok(timestamp)
}

fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sort_json_keys).collect()),
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_json_keys(&values[key]));
            }
            Value::Object(sorted)
        }
        value => value.clone(),
    }
}

/// Mirrors the producer converter's canonical capture hash: normalize only
/// process_start_timestamp, recursively sort object keys, emit compact JSON,
/// and append one trailing newline before SHA-256.
fn normalized_capture_sha256(capture: &Value) -> Result<String> {
    let mut normalized = capture.clone();
    *normalized
        .pointer_mut("/block_file/block/process_start_timestamp")
        .context("capture process_start_timestamp is missing")? = json!(0);
    let mut bytes = serde_json::to_vec(&sort_json_keys(&normalized))?;
    bytes.push(b'\n');
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[derive(Clone, Default)]
struct ExpectedState {
    accounts: HashMap<H256, NewAccount>,
    known_accounts: HashSet<H256>,
    codes: HashMap<H256, Bytes>,
    storage: HashMap<(H256, H256), U256>,
}

impl ExpectedState {
    fn apply(&mut self, diff: &BlockStorageDiff) {
        for address in &diff.deleted_accounts {
            self.known_accounts.insert(*address);
            self.accounts.remove(address);
            self.storage.retain(|(owner, _), _| owner != address);
        }
        for account in &diff.new_accounts {
            self.known_accounts.insert(account.address);
            self.accounts.insert(account.address, account.clone());
        }
        for account in &diff.storage_diffs {
            for pair in &account.diffs {
                self.storage
                    .insert((account.address, pair.index), pair.value);
            }
        }
        for code in &diff.new_codes {
            self.codes.insert(code.code_hash, code.code.clone());
        }
    }
}

fn expected_history(blocks: &[FixtureBlock]) -> Vec<ExpectedState> {
    let mut current = ExpectedState::default();
    blocks
        .iter()
        .map(|block| {
            current.apply(&block.diff);
            current.clone()
        })
        .collect()
}

fn persisted_block_context_matches(actual: &BlockInfo, expected: &BlockInfo) -> bool {
    // State storage persists the complete Header and WithOtherFields metadata,
    // but intentionally reconstructs the unused block body as Hashes([]).
    // Producer Header JSON decodes the same empty body as Uncle, so body-enum
    // equality is not part of the production state-storage contract.
    actual.header == expected.header && actual.other == expected.other
}

fn assert_state_view_matches<S>(
    state: &S,
    block_id: BlockId,
    expected_block: &BlockInfo,
    expected: &ExpectedState,
) -> Result<()>
where
    S: StateDB + BlockContext<Error = <S as StateDB>::Error>,
{
    let actual_block = state.block_info()?;
    ensure!(
        persisted_block_context_matches(&actual_block, expected_block),
        "persisted header or WithOtherFields metadata differs at {block_id:?}"
    );

    for address in &expected.known_accounts {
        let actual = state.basic(*address)?;
        match expected.accounts.get(address) {
            Some(expected) => {
                let actual = actual.with_context(|| format!("account {address} is missing"))?;
                ensure!(
                    actual.balance == expected.balance
                        && actual.nonce == expected.nonce
                        && actual.code_hash == expected.code_hash,
                    "account {address} differs at {block_id:?}"
                );
            }
            None => ensure!(
                actual.is_none(),
                "deleted account {address} is visible at {block_id:?}"
            ),
        }
    }
    for (code_hash, expected_code) in &expected.codes {
        let actual = state.code_by_hash(*code_hash)?;
        ensure!(
            actual.original_bytes().as_ref() == expected_code.as_ref(),
            "code {code_hash} differs at {block_id:?}"
        );
    }
    for ((address, index), expected_value) in &expected.storage {
        ensure!(
            state.storage(*address, *index)? == *expected_value,
            "storage {address}/{index} differs at {block_id:?}"
        );
    }
    Ok(())
}

fn assert_storage_state_matches(
    storage: &MultiStorage,
    block_id: BlockId,
    expected_block: &BlockInfo,
    expected: &ExpectedState,
) -> Result<()> {
    let state = storage
        .state_at(block_id)?
        .with_context(|| format!("storage state is missing at {block_id:?}"))?;
    assert_state_view_matches(&state, block_id, expected_block, expected)
}

fn assert_tree_state_matches(
    tree: &StateTree<MultiStorage>,
    block_id: BlockId,
    expected_block: &BlockInfo,
    expected: &ExpectedState,
) -> Result<()> {
    let state = tree
        .state_at(block_id)?
        .with_context(|| format!("StateTree state is missing at {block_id:?}"))?;
    assert_state_view_matches(&state, block_id, expected_block, expected)
}

fn assert_tree_state_missing(tree: &StateTree<MultiStorage>, block_id: BlockId) -> Result<()> {
    ensure!(
        tree.state_at(block_id)?.is_none(),
        "StateTree unexpectedly exposes {block_id:?}"
    );
    Ok(())
}

fn assert_scenario_coverage(fixtures: &FixtureSet, history: &[ExpectedState]) -> Result<()> {
    ensure!(
        fixtures.manifest.chain.chain_id == 1_337,
        "unexpected fixture chain"
    );
    ensure!(fixtures.blocks.len() == 5, "expected five fixture blocks");
    ensure!(
        fixtures
            .blocks
            .iter()
            .map(|block| block.manifest.label.as_str())
            .eq([
                "genesis",
                "empty-hooks",
                "native-transfer",
                "create2",
                "failed-create",
            ]),
        "fixture scenario labels changed"
    );
    ensure!(
        fixtures
            .blocks
            .iter()
            .all(|block| block.diff.deleted_accounts.is_empty()),
        "A1b scenarios unexpectedly contain deleted accounts"
    );

    let wallet = keccak256(fixtures.manifest.chain.wallet.as_slice());
    let recipient = keccak256(fixtures.manifest.chain.native_recipient.as_slice());
    let probe = keccak256(fixtures.manifest.chain.create2_probe.as_slice());
    let child = keccak256(fixtures.manifest.chain.create2_child.as_slice());
    let failed = keccak256(fixtures.manifest.chain.failed_create_address.as_slice());
    let account = |height: usize, address| history[height].accounts.get(&address);

    ensure!(
        account(0, wallet).is_some(),
        "producer wallet missing at genesis"
    );
    ensure!(
        account(1, wallet).unwrap().balance != account(2, wallet).unwrap().balance,
        "native-transfer block did not change sender balance through value or fee"
    );
    ensure!(
        account(1, recipient).is_none(),
        "recipient existed before transfer"
    );
    ensure!(
        account(2, recipient).is_some_and(|account| !account.balance.is_zero()),
        "native transfer did not create a funded recipient"
    );
    ensure!(account(2, probe).is_none() && account(2, child).is_none());
    let probe_account = account(3, probe).context("CREATE probe account missing")?;
    ensure!(
        probe_account.code_hash != KECCAK_EMPTY
            && history[3].codes.contains_key(&probe_account.code_hash),
        "CREATE probe runtime code was not captured"
    );
    let child_account = account(3, child).context("CREATE2 child account missing")?;
    ensure!(
        child_account.code_hash == KECCAK_EMPTY && child_account.balance == U256::from(1),
        "CREATE2 child account does not have the expected empty code and balance"
    );
    ensure!(
        history[3]
            .storage
            .keys()
            .any(|(address, _)| *address == probe),
        "CREATE/CALL probe did not write storage"
    );
    ensure!(
        account(4, failed).is_none(),
        "failed CREATE leaked an account"
    );

    let expected_hook_contracts =
        HashSet::from([HISTORY_STORAGE_ADDRESS, SYSTEM_ACCOUNTING_ADDRESS]);
    let expected_hook_storage_accounts: HashSet<H256> = expected_hook_contracts
        .iter()
        .map(|address| keccak256(address.as_slice()))
        .collect();
    let storage_contracts: Vec<Address> = serde_json::from_value(
        fixtures.blocks[1].rpc_json["block_file"]["storage_contracts"].clone(),
    )?;
    ensure!(
        storage_contracts.len() == expected_hook_contracts.len()
            && storage_contracts.iter().copied().collect::<HashSet<_>>() == expected_hook_contracts,
        "empty block storage contracts are not the exact Arc hook set"
    );
    ensure!(
        fixtures.blocks[1].diff.storage_diffs.len() == expected_hook_storage_accounts.len()
            && fixtures.blocks[1]
                .diff
                .storage_diffs
                .iter()
                .all(|diff| !diff.diffs.is_empty())
            && fixtures.blocks[1]
                .diff
                .storage_diffs
                .iter()
                .map(|diff| diff.address)
                .collect::<HashSet<_>>()
                == expected_hook_storage_accounts,
        "empty block StateDiff storage accounts are not the exact Arc hook set"
    );

    let block3_storage_contracts: Vec<Address> = serde_json::from_value(
        fixtures.blocks[3].rpc_json["block_file"]["storage_contracts"].clone(),
    )?;
    let expected_block3_contracts = HashSet::from([
        HISTORY_STORAGE_ADDRESS,
        SYSTEM_ACCOUNTING_ADDRESS,
        fixtures.manifest.chain.create2_probe,
    ]);
    let expected_block3_storage_accounts: HashSet<H256> = expected_block3_contracts
        .iter()
        .map(|address| keccak256(address.as_slice()))
        .collect();
    ensure!(
        block3_storage_contracts.len() == expected_block3_contracts.len()
            && block3_storage_contracts
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                == expected_block3_contracts,
        "block 3 storage contracts are not the exact hooks-plus-probe set"
    );
    ensure!(
        fixtures.blocks[3].diff.storage_diffs.len() == expected_block3_storage_accounts.len()
            && fixtures.blocks[3]
                .diff
                .storage_diffs
                .iter()
                .all(|diff| !diff.diffs.is_empty())
            && fixtures.blocks[3]
                .diff
                .storage_diffs
                .iter()
                .map(|diff| diff.address)
                .collect::<HashSet<_>>()
                == expected_block3_storage_accounts,
        "block 3 StateDiff storage accounts are not the exact hooks-plus-probe set"
    );

    let failed_block = &fixtures.blocks[4].diff;
    ensure!(
        failed_block
            .new_accounts
            .iter()
            .all(|account| account.address != failed),
        "failed CREATE leaked its account into StateDiff"
    );
    ensure!(
        failed_block.new_codes.is_empty(),
        "failed CREATE leaked code into StateDiff"
    );
    ensure!(
        failed_block
            .storage_diffs
            .iter()
            .all(|storage| storage.address != failed),
        "failed CREATE leaked storage into StateDiff"
    );
    Ok(())
}

#[derive(Clone)]
struct MockS3 {
    objects: Arc<HashMap<String, Vec<u8>>>,
}

async fn mock_s3_get(State(state): State<MockS3>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().trim_start_matches('/');
    let (_, key) = path.split_once('/').unwrap_or(("", path));
    let Some(data) = state.objects.get(key) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "application/xml")
            .body(Body::from(
                "<Error><Code>NoSuchKey</Code><Message>missing</Message></Error>",
            ))
            .unwrap();
    };

    let range = request
        .headers()
        .get("range")
        .and_then(|value| value.to_str().ok());
    match range {
        Some(range) => {
            let range = range.strip_prefix("bytes=").unwrap();
            let (start, end) = range.split_once('-').unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap();
            let body = data[start..=end].to_vec();
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("content-length", body.len())
                .header(
                    "content-range",
                    format!("bytes {start}-{end}/{}", data.len()),
                )
                .body(Body::from(body))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::OK)
            .header("content-length", data.len())
            .body(Body::from(data.clone()))
            .unwrap(),
    }
}

async fn mock_s3_client(
    objects: HashMap<String, Vec<u8>>,
) -> (Client, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(mock_s3_get).with_state(MockS3 {
        objects: Arc::new(objects),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("test", "test", None, None, "test"))
        .endpoint_url(format!("http://{address}"))
        .force_path_style(true)
        .build();
    (Client::from_conf(config), server)
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(profile: &str) -> Result<Self> {
        let unique = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "leafage-arc-a1b-{profile}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn arc_producer_objects_have_pipeline_formats_and_locked_digests() -> Result<()> {
    let fixtures = FixtureSet::load()?;
    ensure!(fixtures.manifest.schema_version == 1);
    ensure!(
        fixtures.manifest.producer_baseline.repository == "Chaintable/arc-node"
            && fixtures.manifest.producer_baseline.commit == WRITER_BASELINE_COMMIT
            && fixtures.manifest.producer_baseline.release == "v0.7.3"
            && fixtures.manifest.producer_baseline.release_commit == WRITER_RELEASE_COMMIT
    );
    ensure!(
        fixtures.manifest.producer.repository == "Chaintable/arc-node"
            && fixtures.manifest.producer.commit == EXPORTER_COMMIT
            && fixtures.manifest.producer.baseline_commit == WRITER_BASELINE_COMMIT
            && fixtures.manifest.producer.source_policy
                == "only test harness and fixture scripts may differ"
    );
    ensure!(fixtures
        .manifest
        .producer
        .changes_from_baseline
        .iter()
        .map(String::as_str)
        .eq([
            "crates/execution-e2e/src/setup.rs",
            "crates/execution-e2e/tests/export_leafage_a1b.rs",
            "scripts/fixtures/build_leafage_a1b_fixtures.py",
            "scripts/generate-leafage-a1b-fixtures.sh",
        ]));
    ensure!(
        fixtures.manifest.exporter.repository == "Chaintable/arc-node"
            && fixtures.manifest.exporter.commit == EXPORTER_COMMIT
    );
    ensure!(
        fixtures.manifest.fixture_transformer.repository == "Chaintable/arc-node"
            && fixtures.manifest.fixture_transformer.commit == EXPORTER_COMMIT
            && fixtures.manifest.fixture_transformer.entrypoint
                == "scripts/fixtures/build_leafage_a1b_fixtures.py"
            && fixtures.manifest.fixture_transformer.entrypoint_blob == TRANSFORMER_BLOB
            && fixtures
                .manifest
                .fixture_transformer
                .compatibility_contract
                .gzip_container
                == "deterministic fixture encoding; gzip header and compressed bytes are not a pipeline contract"
            && fixtures
                .manifest
                .fixture_transformer
                .compatibility_contract
                .gzip_json_payload
                == "compact JSON without a trailing newline, matching background-tracer serde_json::to_vec semantics"
            && fixtures.manifest.fixture_transformer.encoding.bundle0
                == "8133-byte index followed by genesis RLP entry"
            && fixtures
                .manifest
                .fixture_transformer
                .encoding
                .outer_block_file
                == "gzip JSON BlockFile"
            && fixtures
                .manifest
                .fixture_transformer
                .encoding
                .per_block_header
                == "gzip JSON Header"
            && fixtures
                .manifest
                .fixture_transformer
                .encoding
                .per_block_state_diff
                == "raw RLP BlockStorageDiff"
            && fixtures.manifest.fixture_transformer.encoding.rpc == "DebankOutPut JSON"
    );
    ensure!(
        fixtures.manifest.format_reference.repository == "Chaintable/background-tracer"
            && fixtures.manifest.format_reference.release == "v0.1.43"
            && fixtures.manifest.format_reference.commit == FORMAT_REFERENCE_COMMIT
            && !fixtures
                .manifest
                .format_reference
                .executed_by_fixture_generation
    );
    ensure!(fixtures
        .manifest
        .format_reference
        .scope
        .iter()
        .map(String::as_str)
        .eq([
            "DebankOutPut JSON input schema",
            "gzip JSON Header and BlockFile objects",
            "raw RLP BlockStorageDiff object",
        ]));
    ensure!(fixtures
        .manifest
        .format_reference
        .sources
        .iter()
        .map(|source| (source.path.as_str(), source.blob.as_str()))
        .eq([
            (
                "bin/background-tracer/src/utils/codec.rs",
                "0fb07bbacddc59cbec352634472895a622fbec65",
            ),
            (
                "bin/background-tracer/src/upload/s3.rs",
                "597fa9ee6de41183f46c1e8b88bb50407094f0a7",
            ),
            (
                "types/src/debank.rs",
                "2926b245378d4b3f54b5584ec36ac7204bca112f",
            ),
        ]));
    ensure!(
        fixtures
            .manifest
            .generation
            .comparison_normalization
            .process_start_timestamp
            == 0
    );
    ensure!(fixtures
        .manifest
        .generation
        .independent_capture_verification
        .iter()
        .map(|capture| (capture.label.as_str(), capture.sha256.as_str()))
        .eq(NORMALIZED_CAPTURE_SHA256));
    ensure!(fixtures.manifest.coverage.iter().map(String::as_str).eq([
        "genesis full alloc",
        "empty block EIP-2935 and SystemAccounting hooks",
        "native transfer",
        "successful CREATE root",
        "CALL root with internal CREATE2",
        "failed CREATE root",
        "new account, code, and storage",
    ]));
    ensure!(fixtures
        .manifest
        .excluded
        .iter()
        .map(String::as_str)
        .eq(["StorageCleared"]));
    ensure!(fixtures.manifest.files.len() == 22);

    for file in &fixtures.manifest.files {
        let bytes = fs::read(fixtures.root.join(&file.path))?;
        ensure!(
            bytes.len() as u64 == file.bytes,
            "{} byte size changed",
            file.path
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == file.sha256,
            "{} SHA-256 changed",
            file.path
        );
    }

    let mut previous_hash = H256::ZERO;
    let mut previous_root = EMPTY_ROOT_HASH;
    for block in &fixtures.blocks {
        let manifest = &block.manifest;
        let expected_capture_sha = NORMALIZED_CAPTURE_SHA256
            .iter()
            .find_map(|(label, sha)| (*label == manifest.label).then_some(*sha))
            .with_context(|| format!("missing locked capture SHA for {}", manifest.label))?;
        let manifest_capture_sha = fixtures
            .manifest
            .generation
            .independent_capture_verification
            .iter()
            .find(|capture| capture.label == manifest.label)
            .with_context(|| format!("missing manifest capture SHA for {}", manifest.label))?;
        let actual_capture_sha = normalized_capture_sha256(&block.rpc_json)?;
        ensure!(manifest_capture_sha.sha256 == expected_capture_sha);
        ensure!(
            actual_capture_sha == expected_capture_sha,
            "{} normalized capture SHA changed",
            manifest.label
        );
        ensure!(block.rpc.header.number == manifest.number);
        ensure!(block.block_info.header.number == manifest.number);
        ensure!(block.block_info.header.hash == manifest.hash);
        ensure!(block.block_info.header.state_root == manifest.state_root);
        ensure!(block.block_info.header.parent_hash == manifest.parent_hash);
        ensure!(manifest.parent_hash == previous_hash);
        ensure!(block.diff.hash == manifest.state_root);
        ensure!(block.diff.parent_hash == previous_root);
        ensure!(
            block.rpc.state_diff.as_ref()
                == fs::read(fixtures.root.join(&manifest.state_diff))?.as_slice()
        );
        ensure!(
            block.rpc_json["header"]["extraData"] == Value::String(manifest.extra_data.clone())
        );
        ensure!(
            block.rpc_json["validation_hash"] == json!(manifest.validation_hash),
            "{} validation_hash was not preserved",
            manifest.label
        );

        let header_json = serde_json::from_slice::<Value>(&decode_gzip(&fs::read(
            fixtures.root.join(&manifest.header),
        )?)?)?;
        ensure!(header_json == block.rpc_json["header"]);

        let mut rpc_block_file = block.rpc_json["block_file"].clone();
        let mut outer_block_file = block.outer_block_file.clone();
        let rpc_timestamp = normalize_process_start_timestamp(&mut rpc_block_file)?;
        let outer_timestamp = normalize_process_start_timestamp(&mut outer_block_file)?;
        ensure!(rpc_timestamp > 0 && rpc_timestamp == manifest.process_start_timestamp);
        ensure!(outer_timestamp == rpc_timestamp);
        ensure!(outer_block_file == rpc_block_file);

        previous_hash = manifest.hash;
        previous_root = manifest.state_root;
    }

    let genesis_block_file = &fixtures.blocks[0].rpc_json["block_file"];
    let genesis_txs = genesis_block_file["txs"]
        .as_array()
        .context("genesis BlockFile transactions are missing")?;
    let genesis_traces = genesis_block_file["traces"]
        .as_array()
        .context("genesis BlockFile traces are missing")?;
    let genesis_tx_ids = genesis_txs
        .iter()
        .map(|tx| {
            tx["id"]
                .as_str()
                .context("genesis transaction ID is missing")
        })
        .collect::<Result<HashSet<_>>>()?;
    let genesis_trace_tx_ids = genesis_traces
        .iter()
        .map(|trace| {
            trace["tx_id"]
                .as_str()
                .context("genesis trace transaction ID is missing")
        })
        .collect::<Result<HashSet<_>>>()?;
    ensure!(genesis_tx_ids.len() == genesis_txs.len());
    ensure!(genesis_traces.len() == genesis_txs.len());
    ensure!(genesis_trace_tx_ids == genesis_tx_ids);
    ensure!(genesis_tx_ids
        .iter()
        .all(|tx_id| tx_id.parse::<H256>().is_ok()));
    ensure!(genesis_tx_ids
        .iter()
        .all(|tx_id| !tx_id.contains("genesis")));

    let bundle_header = decode_gzip(&fs::read(fixtures.root.join("bundle/0/block"))?)?;
    let bundle_headers: Vec<BlockInfo> = serde_json::from_slice(&bundle_header)?;
    ensure!(bundle_headers == vec![fixtures.blocks[0].block_info.clone()]);
    let bundle_diff = fs::read(fixtures.root.join("bundle/0/stateDiff"))?;
    ensure!(bundle_diff.len() > STATE_DIFF_INDEX_BYTES);
    ensure!(bundle_diff[0] == 1 && bundle_diff[1..125].iter().all(|byte| *byte == 0));
    let first_end = u64::from_be_bytes(bundle_diff[133..141].try_into().unwrap()) as usize;
    ensure!(first_end == bundle_diff.len() - STATE_DIFF_INDEX_BYTES);
    ensure!(bundle_diff[141..STATE_DIFF_INDEX_BYTES]
        .iter()
        .all(|byte| *byte == 0));
    ensure!(
        &bundle_diff[STATE_DIFF_INDEX_BYTES..]
            == fs::read(fixtures.root.join(&fixtures.blocks[0].manifest.state_diff))?.as_slice()
    );

    let history = expected_history(&fixtures.blocks);
    assert_scenario_coverage(&fixtures, &history)
}

#[tokio::test]
async fn production_loaders_read_arc_per_block_outer_and_bundle_zero() -> Result<()> {
    let fixtures = FixtureSet::load()?;
    let (client, server) = mock_s3_client(fixtures.object_map()?).await;

    for block in &fixtures.blocks {
        let loaded_info = s3_get_block_info(
            &client,
            FIXTURE_BUCKET,
            FIXTURE_CHAIN,
            "",
            block.manifest.hash,
        )
        .await
        .with_context(|| format!("load {} per-block Header", block.manifest.label))?;
        let loaded_diff = s3_get_block_diff(
            &client,
            FIXTURE_BUCKET,
            FIXTURE_CHAIN,
            "",
            block.manifest.state_root,
        )
        .await
        .with_context(|| format!("load {} per-block StateDiff", block.manifest.label))?;
        ensure!(loaded_info == block.block_info);
        ensure!(loaded_diff == block.diff);
        // Genesis uses canonical bytes32 IDs for synthetic transactions, but
        // they still do not identify signed chain transactions. The warmup
        // transaction loader is never used for genesis; bundle 0 below is the
        // production genesis path.
        if block.manifest.number != 0 {
            let transactions = s3_get_block_transactions(
                &client,
                FIXTURE_BUCKET,
                FIXTURE_CHAIN,
                "",
                block.manifest.hash,
            )
            .await
            .with_context(|| format!("load {} outer BlockFile", block.manifest.label))?;
            let expected_transactions: Vec<DebankTransaction> =
                serde_json::from_value(block.rpc_json["block_file"]["txs"].clone())?;
            ensure!(
                transactions.len() == expected_transactions.len(),
                "{} transaction count changed",
                block.manifest.label
            );
            for (index, (actual, expected)) in
                transactions.iter().zip(&expected_transactions).enumerate()
            {
                ensure!(
                    serde_json::to_value(actual)? == serde_json::to_value(expected)?,
                    "{} transaction {index} changed",
                    block.manifest.label
                );
            }
            match block.manifest.label.as_str() {
                "create2" => ensure!(
                    expected_transactions.len() == 2
                        && expected_transactions[0].status
                        && expected_transactions[0].to == fixtures.manifest.chain.create2_probe
                        && expected_transactions[0].to != Address::ZERO,
                    "block 3 CREATE transaction lost its nonzero created-address target"
                ),
                "failed-create" => ensure!(
                    expected_transactions.len() == 1
                        && !expected_transactions[0].status
                        && expected_transactions[0].to
                            == fixtures.manifest.chain.failed_create_address,
                    "block 4 failed CREATE status or created-address target changed"
                ),
                _ => {}
            }
        }
    }

    let mut loaded_bundle = None;
    let last = s3_read_bundle(
        &client,
        FIXTURE_BUCKET,
        FIXTURE_CHAIN,
        "",
        0,
        0,
        32,
        |block_info, block_diff| {
            loaded_bundle = Some((block_info, block_diff));
            std::future::ready(Ok(()))
        },
    )
    .await
    .context("load bundle-0 through production decoder")?
    .context("bundle zero was not found")?;
    let (bundle_info, bundle_diff) = loaded_bundle.context("bundle zero was not processed")?;
    ensure!(last == fixtures.blocks[0].block_info);
    ensure!(bundle_info == fixtures.blocks[0].block_info);
    ensure!(bundle_diff == fixtures.blocks[0].diff);
    server.abort();
    Ok(())
}

#[test]
fn arc_state_tree_caps_to_rocksdb_and_reopens_snapshot_and_archive_profiles() -> Result<()> {
    let fixtures = FixtureSet::load()?;
    let history = expected_history(&fixtures.blocks);
    assert_scenario_coverage(&fixtures, &history)?;

    for is_archive in [false, true] {
        let profile = if is_archive { "archive" } else { "snapshot" };
        let db_dir = TestDir::new(profile)?;
        let storage = MultiStorage::open(
            &db_dir.0,
            64,
            StorageKind::Rocksdb,
            is_archive,
            false,
            false,
        )?;

        let genesis = &fixtures.blocks[0];
        storage
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .context("latest genesis writer state is missing")?
            .update_block(genesis.block_info.clone(), genesis.diff.clone())?;
        assert_storage_state_matches(
            &storage,
            BlockId::Number(BlockNumberOrTag::Latest),
            &genesis.block_info,
            &history[0],
        )?;

        // This is the production startup path in standalone.rs: initialize the
        // bottom DB, then layer recent blocks in StateTree. A depth of two
        // forces blocks 1 and 2 to be capped while blocks 3 and 4 remain in
        // memory.
        let tree = StateTree::new(storage, StateTreeConfig::new(2, 64, 64, 64, true))?;
        for (index, block) in fixtures.blocks.iter().enumerate().skip(1) {
            tree.update_block(block.block_info.clone(), block.diff.clone())?;
            assert_tree_state_matches(
                &tree,
                BlockId::Number(BlockNumberOrTag::Latest),
                &block.block_info,
                &history[index],
            )?;
        }

        let committed = tree
            .last_committed_block()?
            .context("StateTree committed block is missing after cap")?;
        ensure!(
            persisted_block_context_matches(&committed, &fixtures.blocks[2].block_info),
            "{profile} StateTree capped to block {}, expected block 2",
            committed.header.number
        );

        for (block, expected) in fixtures.blocks.iter().zip(&history).skip(2) {
            assert_tree_state_matches(
                &tree,
                BlockId::Number(BlockNumberOrTag::Number(block.manifest.number)),
                &block.block_info,
                expected,
            )?;
            assert_tree_state_matches(
                &tree,
                BlockId::Hash(block.manifest.hash.into()),
                &block.block_info,
                expected,
            )?;
        }
        if is_archive {
            for (block, expected) in fixtures.blocks.iter().zip(&history).take(2) {
                assert_tree_state_matches(
                    &tree,
                    BlockId::Number(BlockNumberOrTag::Number(block.manifest.number)),
                    &block.block_info,
                    expected,
                )?;
                assert_tree_state_matches(
                    &tree,
                    BlockId::Hash(block.manifest.hash.into()),
                    &block.block_info,
                    expected,
                )?;
            }
        } else {
            for block in &fixtures.blocks[..2] {
                assert_tree_state_missing(
                    &tree,
                    BlockId::Number(BlockNumberOrTag::Number(block.manifest.number)),
                )?;
                assert_tree_state_missing(&tree, BlockId::Hash(block.manifest.hash.into()))?;
            }
        }
        drop(tree);

        let reopened = MultiStorage::open(
            &db_dir.0,
            64,
            StorageKind::Rocksdb,
            is_archive,
            false,
            false,
        )?;
        let capped = &fixtures.blocks[2];
        assert_storage_state_matches(
            &reopened,
            BlockId::Number(BlockNumberOrTag::Latest),
            &capped.block_info,
            &history[2],
        )?;
        if is_archive {
            for (block, expected) in fixtures.blocks.iter().zip(&history).take(3) {
                assert_storage_state_matches(
                    &reopened,
                    BlockId::Number(BlockNumberOrTag::Number(block.manifest.number)),
                    &block.block_info,
                    expected,
                )?;
                assert_storage_state_matches(
                    &reopened,
                    BlockId::Hash(block.manifest.hash.into()),
                    &block.block_info,
                    expected,
                )?;
            }
        }
        for block in &fixtures.blocks[3..] {
            ensure!(
                reopened
                    .state_at(BlockId::Hash(block.manifest.hash.into()))?
                    .is_none(),
                "{profile} reopened DB contains uncapped block {}",
                block.manifest.number
            );
        }

        let rebuilt = StateTree::new(reopened, StateTreeConfig::new(2, 64, 64, 64, true))?;
        assert_tree_state_matches(
            &rebuilt,
            BlockId::Number(BlockNumberOrTag::Latest),
            &capped.block_info,
            &history[2],
        )?;
        assert_tree_state_matches(
            &rebuilt,
            BlockId::Number(BlockNumberOrTag::Number(2)),
            &capped.block_info,
            &history[2],
        )?;
        assert_tree_state_matches(
            &rebuilt,
            BlockId::Hash(capped.manifest.hash.into()),
            &capped.block_info,
            &history[2],
        )?;
        for block in &fixtures.blocks[3..] {
            assert_tree_state_missing(
                &rebuilt,
                BlockId::Number(BlockNumberOrTag::Number(block.manifest.number)),
            )?;
            assert_tree_state_missing(&rebuilt, BlockId::Hash(block.manifest.hash.into()))?;
        }

        // Reapply the in-memory tail after restart, as the production updater
        // would. The latest Arc balance/code/storage view must return to block
        // 4 while the committed RocksDB head remains block 2.
        for (block, expected) in fixtures.blocks.iter().zip(&history).skip(3) {
            rebuilt.update_block(block.block_info.clone(), block.diff.clone())?;
            assert_tree_state_matches(
                &rebuilt,
                BlockId::Number(BlockNumberOrTag::Latest),
                &block.block_info,
                expected,
            )?;
        }
        ensure!(persisted_block_context_matches(
            &rebuilt
                .last_committed_block()?
                .context("rebuilt StateTree committed block is missing")?,
            &capped.block_info,
        ));
        for (block, expected) in fixtures.blocks.iter().zip(&history).skip(2) {
            assert_tree_state_matches(
                &rebuilt,
                BlockId::Number(BlockNumberOrTag::Number(block.manifest.number)),
                &block.block_info,
                expected,
            )?;
            assert_tree_state_matches(
                &rebuilt,
                BlockId::Hash(block.manifest.hash.into()),
                &block.block_info,
                expected,
            )?;
        }
        drop(rebuilt);
    }
    Ok(())
}
