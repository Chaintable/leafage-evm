use super::*;
use alloy::{
    primitives::Bytes,
    rpc::types::{TransactionInput, TransactionRequest},
    sol_types::SolCall,
};
use revm::{
    context::{result::ResultAndState, BlockEnv, CfgEnv, TxEnv},
    database::CacheDB,
    database_interface::DBErrorMarker,
    primitives::hardfork::SpecId,
    Context, ExecuteEvm, MainBuilder, MainContext,
};
use serde_json::Value;
use std::{
    str::FromStr,
    sync::{Arc, LazyLock},
};

alloy::sol! {
    function f(uint256[] a, bytes data, bytes _salt);
    function c(address a, bytes data);
    function claimMintRewardAndShare(address other, uint256 pct);
    function bulkClaimMintReward(uint256 tokenId, address to);
}

static FIXTURE: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../tests/fixtures/op_xen_claim.json")).unwrap()
});
const MATH: Address = address!("b4609b8453bb07d540ec95d491b92bdecb794531");

#[derive(Debug, thiserror::Error)]
#[error("injected batch failure")]
struct Error;
impl DBErrorMarker for Error {}

#[derive(Clone, Debug, Default)]
struct Db {
    accounts: HashMap<B256, AccountInfo>,
    codes: HashMap<B256, Bytecode>,
    storage: HashMap<(B256, B256), U256>,
    batched: bool,
    fail_batches: bool,
    panic_batches: bool,
    cancel_on_batch: Option<CancellationToken>,
    batch_calls: Arc<AtomicUsize>,
    scalar_storage: Arc<AtomicUsize>,
}

impl Db {
    fn account(&mut self, address: Address, code: Option<&str>) {
        let mut info = AccountInfo::default();
        if let Some(code) = code {
            let code = Bytecode::new_raw(Bytes::from_str(code).unwrap());
            info.code_hash = code.hash_slow();
            info.nonce = 1;
            self.codes.insert(info.code_hash, code);
        }
        info.code = None;
        info.balance = U256::from(10u128.pow(18));
        self.accounts.insert(keccak256(address), info);
    }
    fn slot(&mut self, address: Address, index: U256, value: U256) {
        self.storage.insert(
            (keccak256(address), keccak256(index.to_be_bytes::<32>())),
            value,
        );
    }
    fn wrapper(self) -> EvmStorageWrapper<Self> {
        EvmStorageWrapper {
            db: self,
            ovm_address: None,
            normalize_state_key: false,
        }
    }
    fn batch(&self, size: usize) -> Result<(), Error> {
        assert!(size <= BATCH_SIZE);
        self.batch_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(cancel) = &self.cancel_on_batch {
            cancel.cancel();
        }
        assert!(!self.panic_batches, "injected batch panic");
        if self.fail_batches {
            Err(Error)
        } else {
            Ok(())
        }
    }
}

impl StateDB for Db {
    type Error = Error;
    fn basic(&self, address: B256) -> Result<Option<AccountInfo>, Error> {
        Ok(self.accounts.get(&address).cloned())
    }
    fn code_by_hash(&self, hash: B256) -> Result<Bytecode, Error> {
        Ok(self.codes.get(&hash).cloned().unwrap_or_default())
    }
    fn storage(&self, address: B256, index: B256) -> Result<U256, Error> {
        self.scalar_storage.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .storage
            .get(&(address, index))
            .copied()
            .unwrap_or_default())
    }
    fn block_hash(&self, _: u64) -> Result<B256, Error> {
        Ok(B256::ZERO)
    }
    fn supports_batched_reads(&self) -> bool {
        self.batched
    }
    fn basic_many(&self, addresses: &[B256]) -> Result<Vec<Option<AccountInfo>>, Error> {
        self.batch(addresses.len())?;
        Ok(addresses
            .iter()
            .map(|address| self.accounts.get(address).cloned())
            .collect())
    }
    fn storage_many(&self, keys: &[(B256, B256)]) -> Result<Vec<U256>, Error> {
        self.batch(keys.len())?;
        Ok(keys
            .iter()
            .map(|key| self.storage.get(key).copied().unwrap_or_default())
            .collect())
    }
}

fn sender() -> Address {
    Address::from_str(FIXTURE["sender"].as_str().unwrap()).unwrap()
}
fn number(name: &str) -> U256 {
    if let Some(n) = FIXTURE[name].as_u64() {
        U256::from(n)
    } else {
        U256::from_str_radix(FIXTURE[name].as_str().unwrap(), 10).unwrap()
    }
}
fn input(indices: &[u64]) -> Vec<u8> {
    fCall {
        a: indices.iter().map(|n| U256::from(*n)).collect(),
        data: cCall {
            a: XEN,
            data: claimMintRewardAndShareCall {
                other: sender(),
                pct: U256::from(100u64),
            }
            .abi_encode()
            .into(),
        }
        .abi_encode()
        .into(),
        _salt: vec![1].into(),
    }
    .abi_encode()
}
fn request(to: Address, input: Vec<u8>) -> CallRequest {
    CallRequest {
        inner: TransactionRequest::default()
            .from(sender())
            .to(to)
            .input(TransactionInput::new(input.into())),
        tempo: None,
    }
}
fn fixture(to: Address, indices: &[u64], redeemed: bool) -> (Db, CallRequest) {
    let mut db = Db {
        batched: true,
        ..Default::default()
    };
    for (address, name) in [
        (COINTOOL, "cointool"),
        (TORRENT, "torrent"),
        (XEN, "xen"),
        (MATH, "math"),
    ] {
        db.account(address, FIXTURE["codes"][name].as_str());
    }
    db.account(sender(), None);
    let token = U256::from(42u64);
    let (req, proxies) = if to == COINTOOL {
        let bytes = input(indices);
        let Some(Claim::CoinTool { proxies, .. }) = decode_claim(to, sender(), &bytes) else {
            panic!()
        };
        (request(to, bytes), proxies)
    } else {
        let bytes = bulkClaimMintRewardCall {
            tokenId: token,
            to: sender(),
        }
        .abi_encode();
        db.slot(
            TORRENT,
            mapping(token, 3),
            U256::from_be_slice(sender().as_slice()),
        );
        db.slot(TORRENT, mapping(token, 11), U256::from(indices.len()));
        db.slot(TORRENT, mapping(token, 13), U256::from(u64::from(redeemed)));
        db.slot(TORRENT, U256::from(14u64), U256::MAX); // nonReentrant guard
        let proxies = (1..=indices.len())
            .map(|i| {
                let mut salt = [0u8; 64];
                salt[..32].copy_from_slice(&U256::from(i).to_be_bytes::<32>());
                salt[32..].copy_from_slice(&token.to_be_bytes::<32>());
                proxy(TORRENT, keccak256(salt), init_hash(TORRENT))
            })
            .collect();
        (request(to, bytes), proxies)
    };
    let clone_code = format!(
        "0x363d3d373d3d3d363d73{}5af43d82803e903d91602b57fd5bf3",
        hex::encode(to)
    );
    for (i, proxy) in proxies.iter().enumerate() {
        db.account(*proxy, Some(&clone_code));
        if !redeemed {
            let slot = mapping(U256::from_be_slice(proxy.as_slice()), 9);
            let rank_offset = if to == COINTOOL {
                indices[i] - 351301
            } else {
                i as u64
            };
            let values = [
                U256::from_be_slice(proxy.as_slice()),
                number("term"),
                number("maturity"),
                number("first_rank") + U256::from(rank_offset),
                number("amplifier"),
                U256::ZERO,
            ];
            for (field, value) in values.into_iter().enumerate() {
                db.slot(XEN, slot + U256::from(field), value);
            }
        }
    }
    for (slot, name) in [
        (2u64, "total_supply"),
        (5, "global_rank"),
        (6, "active_minters"),
    ] {
        db.slot(XEN, U256::from(slot), number(name));
    }
    db.slot(
        XEN,
        mapping(U256::from_be_slice(sender().as_slice()), 0),
        number("recipient_balance"),
    );
    (db, req)
}

fn warmed(db: Db, req: &CallRequest) -> (PrefetchedDb<EvmStorageWrapper<Db>>, PrefetchStats) {
    let budget = AtomicUsize::new(0);
    let result = prefetch_with_budget(db.wrapper(), 10, req, &CancellationToken::new(), &budget);
    assert_eq!(budget.load(Ordering::Relaxed), 0);
    result
}

fn execute<DB: DatabaseRef<Error = Error>>(db: DB, req: &CallRequest, gas: u64) -> ResultAndState {
    let mut cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
    cfg.chain_id = 10;
    let mut evm = Context::mainnet()
        .with_cfg(cfg)
        .with_block(BlockEnv {
            number: number("block"),
            timestamp: number("timestamp"),
            gas_limit: 40_000_000,
            basefee: 0,
            ..Default::default()
        })
        .with_db(CacheDB::new(db))
        .build_mainnet();
    evm.transact(TxEnv {
        caller: sender(),
        kind: req.to.unwrap(),
        data: req.input.input.clone().unwrap(),
        gas_limit: gas,
        gas_price: 0,
        chain_id: Some(10),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn historical_cointool_plan_matches_independently_traced_keys() {
    let indices: Vec<_> = (351301..351401).collect();
    let Some(Claim::CoinTool { proxies, recipient }) =
        decode_claim(COINTOOL, sender(), &input(&indices))
    else {
        panic!()
    };
    assert_eq!(
        proxies[0],
        address!("29c99d4fbecb2756980292a9a52eaea46a19ce33")
    );
    assert_eq!(
        proxies[1],
        address!("d2bbe4dc47bf20d547f803c2cd0af434813a629c")
    );
    let keys = xen_slots(&proxies, recipient);
    assert_eq!(keys.len(), 704); // excludes the four OP fee slots, read normally
    assert_eq!(
        keys[0].1,
        U256::from_be_bytes(hex!(
            "3b2a369c6f6e2db39789e53591c5e99f5489f2d7bf68a4cd5905ef659370cd14"
        ))
    );
}

#[test]
fn real_batch_claims_preserve_gas_logs_and_state_at_multiple_gas_limits() {
    let indices: Vec<_> = (351301..351401).collect();
    for to in [COINTOOL, TORRENT] {
        let (db, req) = fixture(to, &indices, false);
        let (prefetched, stats) = warmed(db.clone(), &req);
        assert_eq!(stats.outcome, "complete");
        assert!(stats.storage_slots >= 704);
        for gas in [16_777_216, 5_318_407, 2_000_000] {
            db.scalar_storage.store(0, Ordering::Relaxed);
            let plain = execute(db.clone().wrapper(), &req, gas);
            let plain_reads = db.scalar_storage.swap(0, Ordering::Relaxed);
            let warm = execute(&prefetched, &req, gas);
            let warm_reads = db.scalar_storage.load(Ordering::Relaxed);
            assert_eq!(
                plain.result, warm.result,
                "gas/result mismatch at {to} / {gas}"
            );
            assert_eq!(plain.state, warm.state, "state mismatch at {to} / {gas}");
            if gas == 16_777_216 {
                assert!(warm.result.is_success());
                assert!(
                    plain_reads >= 704,
                    "fixture must exercise the cold state path"
                );
                assert!(
                    warm_reads <= 4,
                    "bulk state reads must leave the serial EVM path"
                );
                assert!(
                    warm.result.logs().len() >= 300,
                    "must execute successful claims, not swallowed reverts"
                );
                let balance_slot = mapping(U256::from_be_slice(sender().as_slice()), 0);
                let balance = warm.state[&XEN].storage[&balance_slot].present_value();
                assert_eq!(
                    balance - number("recipient_balance"),
                    U256::from(14_100u64) * U256::from(10u128.pow(18))
                );
            }
        }
    }
}

#[test]
fn swallowed_reverts_and_duplicate_proxies_keep_original_semantics() {
    for (indices, redeemed) in [
        (vec![351301, 351302, 351303], true),
        (vec![351301, 351301, 351302], false),
    ] {
        let (db, req) = fixture(COINTOOL, &indices, redeemed);
        let (prefetched, stats) = warmed(db.clone(), &req);
        assert_eq!(stats.outcome, "complete");
        let plain = execute(db.wrapper(), &req, 16_777_216);
        let warm = execute(prefetched, &req, 16_777_216);
        assert_eq!(plain.result, warm.result);
        assert_eq!(plain.state, warm.state);
        assert!(warm.result.is_success());
        if redeemed {
            assert!(warm.result.logs().is_empty());
        } else {
            assert_eq!(warm.result.logs().len(), 6);
        }
    }
}

#[test]
fn already_redeemed_torrent_only_reads_metadata() {
    let (db, req) = fixture(TORRENT, &[351301, 351302], true);
    let (prefetched, stats) = warmed(db.clone(), &req);
    assert_eq!(stats.outcome, "ineligible_claim");
    assert_eq!(stats.storage_slots, 3);
    assert_eq!(stats.batches, 0);
    let plain = execute(db.wrapper(), &req, 16_777_216);
    let warm = execute(prefetched, &req, 16_777_216);
    assert_eq!(plain.result, warm.result);
    assert!(!warm.result.is_success());
}

#[test]
fn failed_or_panicked_batches_fall_back_and_release_budget() {
    for panic_batches in [false, true] {
        let (mut db, req) = fixture(COINTOOL, &[351301, 351302], false);
        db.fail_batches = !panic_batches;
        db.panic_batches = panic_batches;
        let (prefetched, stats) = warmed(db.clone(), &req);
        assert_eq!(stats.outcome, "partial");
        assert_eq!(stats.storage_slots, 0);
        let plain = execute(db.wrapper(), &req, 16_777_216);
        let warm = execute(prefetched, &req, 16_777_216);
        assert_eq!(plain.result, warm.result);
        assert_eq!(plain.state, warm.state);
    }
}

#[test]
fn guards_skip_unsupported_code_backends_chains_and_busy_workers() {
    let (db, req) = fixture(COINTOOL, &[351301, 351302], false);
    for scenario in 0..5 {
        let mut db = db.clone();
        if scenario == 0 {
            db.accounts.get_mut(&keccak256(COINTOOL)).unwrap().code_hash = B256::ZERO;
        }
        if scenario == 1 {
            db.batched = false;
        }
        let cancel = CancellationToken::new();
        if scenario == 2 {
            cancel.cancel();
        }
        let initial = if scenario == 3 { MAX_WORKERS } else { 0 };
        let budget = AtomicUsize::new(initial);
        let chain = if scenario == 4 { 8453 } else { 10 };
        let (prefetched, stats) = prefetch_with_budget(db.wrapper(), chain, &req, &cancel, &budget);
        assert_eq!(stats.batches, 0);
        assert!(prefetched.storage.is_empty());
        assert_eq!(budget.load(Ordering::Relaxed), initial);
    }
}

#[test]
fn malformed_and_oversized_calldata_never_creates_an_unbounded_plan() {
    let valid = input(&[351301, 351302]);
    for length in 0..valid.len() {
        let _ = decode_claim(COINTOOL, sender(), &valid[..length]); // no panics on any truncation
    }
    let mut bad = valid.clone();
    bad[4..36].fill(0xff);
    assert!(decode_claim(COINTOOL, sender(), &bad).is_none());
    bad = valid.clone();
    bad[100..132].fill(0xff); // dynamic array count, checked before allocation
    assert!(decode_claim(COINTOOL, sender(), &bad).is_none());
    assert!(decode_claim(COINTOOL, sender(), &vec![0; MAX_INPUT + 1]).is_none());
    assert!(decode_claim(
        COINTOOL,
        sender(),
        &input(&(351301..351558).collect::<Vec<_>>())
    )
    .is_none());
    assert!(decode_claim(Address::ZERO, sender(), &valid).is_none());
}

#[test]
fn cancellation_stops_issuing_batches_and_budget_has_a_process_bound() {
    let (mut db, req) = fixture(COINTOOL, &(351301..351401).collect::<Vec<_>>(), false);
    let cancel = CancellationToken::new();
    db.cancel_on_batch = Some(cancel.clone());
    let budget = AtomicUsize::new(0);
    let (_, stats) = prefetch_with_budget(db.wrapper(), 10, &req, &cancel, &budget);
    assert_eq!(stats.outcome, "cancelled");
    assert!(stats.batches <= WORKERS);
    assert_eq!(budget.load(Ordering::Relaxed), 0);
    let a = WorkerBudget::try_acquire(&budget).unwrap();
    let b = WorkerBudget::try_acquire(&budget).unwrap();
    assert!(WorkerBudget::try_acquire(&budget).is_none());
    drop(a);
    drop(b);
    assert_eq!(budget.load(Ordering::Relaxed), 0);
}

#[test]
fn prefetched_state_is_request_local_and_outer_overrides_win() {
    let (db, req) = fixture(COINTOOL, &[351301, 351302], false);
    let (prefetched, _) = warmed(db.clone(), &req);
    let mut changed = db;
    changed.slot(XEN, U256::from(5u64), U256::from(123u64));
    let (other, _) = warmed(changed, &req);
    assert_eq!(
        prefetched.storage_ref(XEN, U256::from(5u64)).unwrap(),
        number("global_rank")
    );
    assert_eq!(
        other.storage_ref(XEN, U256::from(5u64)).unwrap(),
        U256::from(123u64)
    );
    let mut overlay = CacheDB::new(prefetched);
    overlay
        .insert_account_storage(XEN, U256::from(5u64), U256::from(456u64))
        .unwrap();
    assert_eq!(
        revm::Database::storage(&mut overlay, XEN, U256::from(5u64)).unwrap(),
        U256::from(456u64)
    );
}

#[test]
fn nested_and_unwound_rocksdb_profiles_leave_later_requests_usable() {
    let (((), nested), outer) = profile_rocksdb_reads(|| profile_rocksdb_reads(|| ()));
    assert!(nested.is_empty());
    assert!(outer.contains_key("block_reads"));
    let _ = std::panic::catch_unwind(|| profile_rocksdb_reads(|| panic!("injected")));
    let (_, next) = profile_rocksdb_reads(|| ());
    assert!(next.contains_key("block_reads"));
}
