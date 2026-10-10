//! Read hints for the two verified OP batch-claim implementations observed in
//! slow estimates. Hints only fill a backing-state cache: EVM execution, access
//! lists, journal warming, overrides and gas estimation remain authoritative.
//! Unknown code/calldata/backends take the ordinary lazy path.

use alloy::primitives::{address, b256, hex, keccak256, Address, B256, U256};
use leafage_evm_storage::{EvmStorageWrapper, StateDB};
use leafage_evm_types::CallRequest;
use revm::{bytecode::Bytecode, state::AccountInfo, DatabaseRef};
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

// Match both deployment and runtime code hash, not just a four-byte selector.
// Verified source: https://optimism.blockscout.com/address/<address>?tab=contract
const COINTOOL: Address = address!("9ec1c3dcf667f2035fb4cd2eb42a1566fd54d2b7");
const TORRENT: Address = address!("af18644083151cf57f914cccc23c42a1892c218e");
const XEN: Address = address!("eb585163debb1e637c6d617de3bef99347cd75c8");
const COINTOOL_CODE: B256 =
    b256!("e6946f4dd3acf5858e0467f664bae6283add8133441b7a7ce572461ab112a607");
const TORRENT_CODE: B256 =
    b256!("b8bfbd46140b4f2b8a8b2660a91b4c5bc231aa01e99b3e55e58725ed1337459d");
const XEN_CODE: B256 = b256!("67c5a59e6d9b3aa748ea52f017726687b9cdd03e1df16608b0dcc071e466e7d6");
const MAX_PROXIES: usize = 256;
const MAX_INPUT: usize = 16 * 1024;
const BATCH_SIZE: usize = 32;
const WORKERS: usize = 4;
const MAX_WORKERS: usize = 8;
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct PrefetchedDb<DB> {
    db: DB,
    accounts: HashMap<Address, Option<AccountInfo>>,
    storage: HashMap<(Address, U256), U256>,
}

impl<DB> PrefetchedDb<DB> {
    fn new(db: DB) -> Self {
        Self {
            db,
            accounts: HashMap::new(),
            storage: HashMap::new(),
        }
    }
}

impl<DB: DatabaseRef> DatabaseRef for PrefetchedDb<DB> {
    type Error = DB::Error;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        match self.accounts.get(&address) {
            Some(value) => Ok(value.clone()),
            None => self.db.basic_ref(address),
        }
    }
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self.storage.get(&(address, index)) {
            Some(value) => Ok(*value),
            None => self.db.storage_ref(address, index),
        }
    }
    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash_ref(hash)
    }
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number)
    }
}

enum Claim {
    CoinTool {
        proxies: Vec<Address>,
        recipient: Address,
    },
    Torrent {
        token: U256,
        recipient: Address,
    },
}

fn word(data: &[u8], offset: usize) -> Option<U256> {
    Some(U256::from_be_slice(
        data.get(offset..offset.checked_add(32)?)?,
    ))
}

fn address_word(data: &[u8], offset: usize) -> Option<Address> {
    let value = word(data, offset)?.to_be_bytes::<32>();
    value[..12]
        .iter()
        .all(|b| *b == 0)
        .then(|| Address::from_slice(&value[12..]))
}

fn dynamic_bytes(data: &[u8], head: usize, head_size: usize) -> Option<&[u8]> {
    let offset: usize = word(data, head)?.try_into().ok()?;
    if offset < head_size || offset % 32 != 0 {
        return None;
    }
    let length: usize = word(data, offset)?.try_into().ok()?;
    let start = offset.checked_add(32)?;
    data.get(start..start.checked_add(length)?)
}

fn init_hash(factory: Address) -> B256 {
    let mut code = Vec::with_capacity(55);
    code.extend_from_slice(&hex!("3d602d80600a3d3981f3363d3d373d3d3d363d73"));
    code.extend_from_slice(factory.as_slice());
    code.extend_from_slice(&hex!("5af43d82803e903d91602b57fd5bf3"));
    keccak256(code)
}

fn proxy(factory: Address, salt: B256, code: B256) -> Address {
    let mut data = [0u8; 85];
    data[0] = 0xff;
    data[1..21].copy_from_slice(factory.as_slice());
    data[21..53].copy_from_slice(salt.as_slice());
    data[53..].copy_from_slice(code.as_slice());
    Address::from_slice(&keccak256(data).as_slice()[12..])
}

fn decode_claim(to: Address, sender: Address, input: &[u8]) -> Option<Claim> {
    if input.len() > MAX_INPUT {
        return None;
    }
    if to == TORRENT && input.get(..4)? == hex!("f5878b9b") {
        if input.len() != 68 {
            return None;
        } // skip forwarded/unknown formats
        let recipient = address_word(input, 36)?;
        return (recipient != Address::ZERO).then(|| Claim::Torrent {
            token: word(input, 4).unwrap(),
            recipient,
        });
    }
    if to != COINTOOL || input.get(..4)? != hex!("c2580804") {
        return None;
    }
    let args = &input[4..];
    let array: usize = word(args, 0)?.try_into().ok()?;
    if array < 96 || array % 32 != 0 {
        return None;
    }
    let count: usize = word(args, array)?.try_into().ok()?;
    if !(2..=MAX_PROXIES).contains(&count) {
        return None;
    }
    let start = array.checked_add(32)?;
    let indices = args.get(start..start.checked_add(count.checked_mul(32)?)?)?;
    let call = dynamic_bytes(args, 32, 96)?;
    let salt = dynamic_bytes(args, 64, 96)?;
    if salt.len() > 64 || call.get(..4)? != hex!("59635f6f") {
        return None;
    }
    let inner = &call[4..];
    if address_word(inner, 0)? != XEN {
        return None;
    }
    let claim = dynamic_bytes(inner, 32, 64)?;
    if claim.len() != 68 || claim[..4] != hex!("1c560305") {
        return None;
    }
    let recipient = address_word(claim, 4)?;
    if recipient == Address::ZERO || word(claim, 36)? != U256::from(100u64) {
        return None;
    }
    let code = init_hash(COINTOOL);
    let proxies = indices
        .chunks_exact(32)
        .map(|index| {
            let mut data = Vec::with_capacity(salt.len() + 52);
            data.extend_from_slice(salt);
            data.extend_from_slice(index);
            data.extend_from_slice(sender.as_slice());
            proxy(COINTOOL, keccak256(data), code)
        })
        .collect();
    Some(Claim::CoinTool { proxies, recipient })
}

fn mapping(key: U256, slot: u64) -> U256 {
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(&key.to_be_bytes::<32>());
    data[32..].copy_from_slice(&U256::from(slot).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(data).0)
}

fn xen_slots(proxies: &[Address], recipient: Address) -> Vec<(Address, U256)> {
    let mut keys = Vec::with_capacity(proxies.len() * 7 + 4);
    for address in proxies {
        let key = U256::from_be_slice(address.as_slice());
        let base = mapping(key, 9); // userMints: six consecutive MintInfo fields
        for field in 0u64..6 {
            keys.push((XEN, base.wrapping_add(U256::from(field))));
        }
        keys.push((XEN, mapping(key, 0))); // _mint(proxy, 0) still touches balance
    }
    keys.push((XEN, mapping(U256::from_be_slice(recipient.as_slice()), 0)));
    for slot in [2u64, 5, 6] {
        keys.push((XEN, U256::from(slot)));
    }
    let mut seen = HashSet::new();
    keys.retain(|key| seen.insert(*key));
    keys
}

struct WorkerBudget;
impl WorkerBudget {
    fn try_acquire() -> Option<Self> {
        ACTIVE_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |n| {
                (n <= MAX_WORKERS - WORKERS).then_some(n + WORKERS)
            })
            .ok()
            .map(|_| Self)
    }
}
impl Drop for WorkerBudget {
    fn drop(&mut self) {
        ACTIVE_WORKERS.fetch_sub(WORKERS, Ordering::AcqRel);
    }
}

enum Batch<'a> {
    Accounts(&'a [Address]),
    Storage(&'a [(Address, U256)]),
}
enum Values {
    Accounts(Vec<(Address, Option<AccountInfo>)>),
    Storage(Vec<((Address, U256), U256)>),
}

pub(crate) fn prefetch<DB: StateDB + Sync>(
    db: EvmStorageWrapper<DB>,
    chain_id: u64,
    request: &CallRequest,
    cancel: &CancellationToken,
) -> PrefetchedDb<EvmStorageWrapper<DB>> {
    let mut cached = PrefetchedDb::new(db);
    prepare(&mut cached, chain_id, request, cancel);
    cached
}

fn prepare<DB: StateDB + Sync>(
    cache: &mut PrefetchedDb<EvmStorageWrapper<DB>>,
    chain_id: u64,
    request: &CallRequest,
    cancel: &CancellationToken,
) {
    if chain_id != 10 || !cache.db.supports_batched_reads() || cancel.is_cancelled() {
        return;
    }
    let Some(sender) = request.from else {
        return;
    };
    let Some(to) = request.to.and_then(|kind| kind.to().copied()) else {
        return;
    };
    let Some(input) = request.input.input.as_ref().or(request.input.data.as_ref()) else {
        return;
    };
    let Some(claim) = decode_claim(to, sender, input) else {
        return;
    };
    let Some(_budget) = WorkerBudget::try_acquire() else {
        return;
    };

    for (address, expected) in [
        (
            to,
            if to == COINTOOL {
                COINTOOL_CODE
            } else {
                TORRENT_CODE
            },
        ),
        (XEN, XEN_CODE),
    ] {
        let Ok(Some(info)) = cache.db.basic_ref(address) else {
            return;
        };
        let matches = info.code_hash == expected;
        cache.accounts.insert(address, Some(info));
        if !matches {
            return;
        }
    }
    let (proxies, recipient) = match claim {
        Claim::CoinTool { proxies, recipient } => (proxies, recipient),
        Claim::Torrent { token, recipient } => {
            // This exact deployment has mutable ERC2771Context at slot 0,
            // ERC721 at 1..6, vmuCount at 11 and mintInfo at 13.
            let keys = [
                (to, mapping(token, 3)),
                (to, mapping(token, 11)),
                (to, mapping(token, 13)),
            ];

            let Ok(values) = cache.db.storage_many_ref(&keys) else {
                return;
            };
            if values.len() != keys.len() {
                return;
            }
            cache
                .storage
                .extend(keys.into_iter().zip(values.iter().copied()));

            if values[0] != U256::from_be_slice(sender.as_slice())
                || values[2] & U256::from(1u64) != U256::ZERO
            {
                return;
            }
            let Ok(count) = usize::try_from(values[1]) else {
                return;
            };
            if !(2..=MAX_PROXIES).contains(&count) {
                return;
            }
            let code = init_hash(TORRENT);
            let proxies = (1..=count)
                .map(|index| {
                    let mut salt = [0u8; 64];
                    salt[..32].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
                    salt[32..].copy_from_slice(&token.to_be_bytes::<32>());
                    proxy(TORRENT, keccak256(salt), code)
                })
                .collect();
            (proxies, recipient)
        }
    };
    let slots = xen_slots(&proxies, recipient);
    let mut accounts = proxies;
    accounts.extend([sender, recipient]);
    let mut seen = HashSet::new();
    accounts.retain(|address| !cache.accounts.contains_key(address) && seen.insert(*address));
    let batches: Vec<_> = accounts
        .chunks(BATCH_SIZE)
        .map(Batch::Accounts)
        .chain(slots.chunks(BATCH_SIZE).map(Batch::Storage))
        .collect();
    let next = AtomicUsize::new(0);
    let db = &cache.db;
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(WORKERS);
        for _ in 0..WORKERS {
            let batches = &batches;
            let next = &next;
            if let Ok(handle) = std::thread::Builder::new()
                .name("rpc-state-prefetch".into())
                .spawn_scoped(scope, move || {
                    let mut results = Vec::new();
                    while !cancel.is_cancelled() {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(batch) = batches.get(index) else {
                            break;
                        };
                        let value = match batch {
                            Batch::Accounts(keys) => db
                                .basic_many_ref(keys)
                                .ok()
                                .filter(|values| values.len() == keys.len())
                                .map(|values| {
                                    Values::Accounts(keys.iter().copied().zip(values).collect())
                                }),
                            Batch::Storage(keys) => db
                                .storage_many_ref(keys)
                                .ok()
                                .filter(|values| values.len() == keys.len())
                                .map(|values| {
                                    Values::Storage(keys.iter().copied().zip(values).collect())
                                }),
                        };
                        if let Some(value) = value {
                            results.push(value);
                        }
                    }
                    results
                })
            {
                handles.push(handle);
            }
        }
        // Failed reads or workers leave cache entries absent, so EVM execution
        // retries those keys through the ordinary backing-state path.
        for handle in handles {
            if let Ok(results) = handle.join() {
                for result in results {
                    match result {
                        Values::Accounts(values) => cache.accounts.extend(values),
                        Values::Storage(values) => cache.storage.extend(values),
                    }
                }
            }
        }
    });
}
