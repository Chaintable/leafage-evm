//! revm's account view over Leafage's stored accounts.
//!
//! Every [`AccountInfo`] served to RPC handlers and to revm is built here,
//! for single and batched reads alike. Chain-specific balance semantics live
//! in this one place: the OP legacy OVM balance mapping and Blast's
//! share-based balances.

use alloy::primitives::keccak256;
use leafage_evm_chains::blast::{derive_balance, BLAST_SHARES_HASH, SHARE_PRICE_SLOT_HASH};
use leafage_evm_storage::{EvmStorageWrapper, StateDB};
use leafage_evm_types::{AccountInfo, Address, BalanceView, Bytecode, StoredAccount, H256, U256};
use revm::database_interface::DBErrorMarker;
use revm::DatabaseRef;

#[derive(Clone, Debug)]
pub struct AccountResolver<T> {
    inner: EvmStorageWrapper<T>,
    /// keccak256 of the OVM ETH contract address. When set, account balances
    /// are read from that contract's balance mapping.
    ovm_address: Option<H256>,
}

#[derive(thiserror::Error)]
pub enum ResolveError<E> {
    #[error(transparent)]
    Backend(#[from] E),
    #[error("Blast balance of {0} overflows U256")]
    BlastBalanceOverflow(Address),
}

// Backend errors print as the backend error itself, so RPC messages built
// with `{:?}` are the same as before the resolver existed.
impl<E: std::fmt::Debug> std::fmt::Debug for ResolveError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(err) => err.fmt(f),
            Self::BlastBalanceOverflow(address) => f
                .debug_tuple("BlastBalanceOverflow")
                .field(address)
                .finish(),
        }
    }
}

impl<E: std::error::Error + Send + Sync + 'static> DBErrorMarker for ResolveError<E> {}

impl<T> AccountResolver<T> {
    pub fn new(db: T, ovm_address: Option<H256>, normalize_state_key: bool) -> Self {
        Self {
            inner: EvmStorageWrapper {
                db,
                normalize_state_key,
            },
            ovm_address,
        }
    }
}

impl<T: StateDB> AccountResolver<T> {
    /// Batched [`DatabaseRef::basic_ref`]: one result per input, same order.
    /// OVM chains take the scalar path, so the OVM override stays in
    /// [`Self::resolve`] only.
    pub fn basic_many_ref(
        &self,
        addresses: &[Address],
    ) -> Result<Vec<Option<AccountInfo>>, ResolveError<T::Error>> {
        if self.ovm_address.is_some() {
            return addresses
                .iter()
                .map(|address| self.basic_ref(*address))
                .collect();
        }
        let accounts = self.inner.basic_many_ref(addresses)?;
        let mut share_price = None;
        addresses
            .iter()
            .zip(accounts)
            .map(|(address, account)| self.resolve(*address, account, &mut share_price))
            .collect()
    }

    /// Batched [`DatabaseRef::storage_ref`]: one result per input, same order.
    pub fn storage_many_ref(
        &self,
        keys: &[(Address, U256)],
    ) -> Result<Vec<U256>, ResolveError<T::Error>> {
        Ok(self.inner.storage_many_ref(keys)?)
    }

    /// Batched [`DatabaseRef::code_by_hash_ref`]: one result per input, same order.
    pub fn code_by_hash_many_ref(
        &self,
        code_hashes: &[H256],
    ) -> Result<Vec<Bytecode>, ResolveError<T::Error>> {
        Ok(self.inner.code_by_hash_many_ref(code_hashes)?)
    }

    /// Whether the `*_many_ref` reads batch at the storage layer. OVM chains
    /// resolve accounts one by one, see [`Self::basic_many_ref`].
    pub fn supports_batched_reads(&self) -> bool {
        self.ovm_address.is_none() && self.inner.supports_batched_reads()
    }

    /// Builds the revm view of a stored account. `share_price` caches the Blast
    /// sharePrice across one batch; every read goes through the same state
    /// view, so the account and the price always belong to the same block.
    fn resolve(
        &self,
        address: Address,
        account: Option<StoredAccount>,
        share_price: &mut Option<U256>,
    ) -> Result<Option<AccountInfo>, ResolveError<T::Error>> {
        if let Some(ovm_address) = self.ovm_address {
            let balance = self
                .inner
                .db
                .storage(ovm_address, keccak256(get_ovm_balance_key(address)))?;
            return Ok(match account {
                Some(account) => Some(account.to_account_info(balance)),
                None if balance != U256::ZERO => {
                    let mut info = AccountInfo::default();
                    info.balance = balance;
                    Some(info)
                }
                None => None,
            });
        }
        let Some(account) = account else {
            return Ok(None);
        };
        let balance = match account.balance_view() {
            BalanceView::Standard(balance) => balance,
            BalanceView::Blast(ext) => derive_balance(ext, || self.share_price(share_price))?
                .ok_or(ResolveError::BlastBalanceOverflow(address))?,
        };
        Ok(Some(account.to_account_info(balance)))
    }

    /// The Blast sharePrice of this state view, read at most once per `cache`.
    fn share_price(&self, cache: &mut Option<U256>) -> Result<U256, T::Error> {
        if let Some(price) = *cache {
            return Ok(price);
        }
        let price = self
            .inner
            .db
            .storage(BLAST_SHARES_HASH, SHARE_PRICE_SLOT_HASH)?;
        *cache = Some(price);
        Ok(price)
    }
}

impl<T: StateDB> DatabaseRef for AccountResolver<T> {
    type Error = ResolveError<T::Error>;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let account = self.inner.basic_ref(address)?;
        self.resolve(address, account, &mut None)
    }

    fn code_by_hash_ref(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        Ok(self.inner.code_by_hash_ref(code_hash)?)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.inner.storage_ref(address, index)?)
    }

    fn block_hash_ref(&self, number: u64) -> Result<H256, Self::Error> {
        Ok(self.inner.block_hash_ref(number)?)
    }
}

/// Calculates the OVM storage key for a balance, replicating the logic
/// from the Go function `GetOVMBalanceKey`.
///
/// In the EVM, the storage address for a mapping entry `mapping(key => value)`
/// located at storage slot `p` is computed as `keccak256(padded_key . padded_p)`.
/// This function assumes the storage slot `p` is 0.
///
/// # Arguments
///
/// * `addr` - The H160 (20-byte) address for which to find the balance key.
///
/// # Returns
///
/// * An H256 (32-byte) hash representing the storage key.
pub fn get_ovm_balance_key(addr: Address) -> H256 {
    // 1. Prepare the address. The `key` in the mapping is the user's address.
    //    It must be left-padded with zeros to a full 32 bytes.
    let mut padded_addr = [0u8; 32];
    padded_addr[12..].copy_from_slice(addr.as_slice());

    // 2. Prepare the storage slot position. The Go function uses `common.Big0`,
    //    which is a big integer of value 0. When padded to 32 bytes, this is
    //    simply 32 zero bytes.
    let position_slot = [0u8; 32];

    // 3. Concatenate the padded address and the position slot into a single
    //    64-byte array. The `keccak256` function expects a single byte slice.
    let mut concatenated_data = [0u8; 64];
    concatenated_data[..32].copy_from_slice(&padded_addr);
    concatenated_data[32..].copy_from_slice(&position_slot);

    // 4. Compute the Keccak-256 hash of the concatenated data. This function
    //    returns an alloy_primitives::B256 type.
    keccak256(&concatenated_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::{AccountExt, BlastAccountExt, KECCAK256_EMPTY};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::str::FromStr;

    #[derive(Debug, thiserror::Error)]
    #[error("mock error")]
    struct MockErr;
    impl DBErrorMarker for MockErr {}

    #[derive(Debug, Default)]
    struct MockDB {
        accounts: HashMap<H256, StoredAccount>,
        storage: HashMap<(H256, H256), U256>,
        storage_reads: RefCell<Vec<(H256, H256)>>,
    }

    impl MockDB {
        fn with_share_price(price: u64) -> Self {
            let mut db = Self::default();
            db.storage.insert(
                (BLAST_SHARES_HASH, SHARE_PRICE_SLOT_HASH),
                U256::from(price),
            );
            db
        }

        fn insert(&mut self, address: Address, account: StoredAccount) {
            self.accounts.insert(keccak256(address), account);
        }
    }

    impl StateDB for MockDB {
        type Error = MockErr;
        fn basic(&self, address: H256) -> Result<Option<StoredAccount>, MockErr> {
            Ok(self.accounts.get(&address).cloned())
        }
        fn code_by_hash(&self, _code_hash: H256) -> Result<Bytecode, MockErr> {
            Ok(Bytecode::default())
        }
        fn storage(&self, address: H256, index: H256) -> Result<U256, MockErr> {
            self.storage_reads.borrow_mut().push((address, index));
            Ok(self
                .storage
                .get(&(address, index))
                .copied()
                .unwrap_or_default())
        }
        fn block_hash(&self, _number: u64) -> Result<H256, MockErr> {
            Ok(H256::ZERO)
        }
    }

    fn blast(flags: u8, fixed: u64, shares: u64, remainder: u64) -> StoredAccount {
        StoredAccount::with_ext(
            1,
            KECCAK256_EMPTY,
            AccountExt::Blast(BlastAccountExt {
                flags,
                fixed: U256::from(fixed),
                shares: U256::from(shares),
                remainder: U256::from(remainder),
            }),
        )
    }

    fn balance<T: StateDB>(resolver: &AccountResolver<T>, address: Address) -> U256 {
        resolver.basic_ref(address).unwrap().unwrap().balance
    }

    #[test]
    fn blast_balances_follow_the_share_price_of_the_same_view() {
        let automatic = Address::repeat_byte(1);
        let disabled = Address::repeat_byte(2);
        let claimable = Address::repeat_byte(3);
        let view = |price| {
            let mut db = MockDB::with_share_price(price);
            db.insert(automatic, blast(0, 999, 13, 17));
            db.insert(disabled, blast(1, 42, 13, 17));
            db.insert(claimable, blast(2, 43, 13, 17));
            AccountResolver::new(db, None, false)
        };

        // An idle automatic account's balance changes with the price alone.
        let before = view(5);
        let after = view(6);
        assert_eq!(balance(&before, automatic), U256::from(5 * 13 + 17));
        assert_eq!(balance(&after, automatic), U256::from(6 * 13 + 17));
        for resolver in [&before, &after] {
            assert_eq!(balance(resolver, disabled), U256::from(42));
            assert_eq!(balance(resolver, claimable), U256::from(43));
        }
        let info = before.basic_ref(automatic).unwrap().unwrap();
        assert_eq!(info.nonce, 1);
        assert_eq!(info.code_hash, KECCAK256_EMPTY);
        assert!(before.basic_ref(Address::repeat_byte(9)).unwrap().is_none());
    }

    #[test]
    fn batched_reads_match_scalar_and_read_the_price_once() {
        let mut db = MockDB::with_share_price(5);
        let addresses: Vec<Address> = (1..=4).map(Address::repeat_byte).collect();
        db.insert(addresses[0], blast(0, 0, 13, 17));
        db.insert(addresses[1], blast(0, 0, 2, 1));
        db.insert(
            addresses[2],
            StoredAccount::standard(U256::from(100), 7, KECCAK256_EMPTY),
        );
        let resolver = AccountResolver::new(db, None, false);

        let batched = resolver.basic_many_ref(&addresses).unwrap();
        assert_eq!(resolver.inner.db.storage_reads.borrow().len(), 1);
        for (address, got) in addresses.iter().zip(&batched) {
            assert_eq!(*got, resolver.basic_ref(*address).unwrap());
        }
        assert_eq!(batched[2].as_ref().unwrap().balance, U256::from(100));
        assert!(batched[3].is_none());
    }

    #[test]
    fn blast_balance_overflow_is_an_error() {
        let address = Address::repeat_byte(1);
        let mut db = MockDB::with_share_price(2);
        let account = BlastAccountExt {
            flags: 0,
            fixed: U256::ZERO,
            shares: U256::MAX,
            remainder: U256::ZERO,
        };
        db.insert(
            address,
            StoredAccount::with_ext(1, KECCAK256_EMPTY, AccountExt::Blast(account)),
        );
        let resolver = AccountResolver::new(db, None, false);
        assert!(matches!(
            resolver.basic_ref(address),
            Err(ResolveError::BlastBalanceOverflow(a)) if a == address
        ));
    }

    /// End to end through the state tree's diff layers: the sharePrice comes
    /// from real storage diffs, and an account that no later block touches
    /// still follows each block's price.
    #[test]
    fn idle_blast_account_follows_the_share_price_through_the_state_tree() {
        use leafage_evm_storage::{
            EvmStorageRead, EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper,
            StateTree, StateTreeConfig, StorageKind,
        };
        use leafage_evm_types::{
            AccountStorageDiff, BlastBlockStorageDiff, BlastNewAccount, Block, BlockId, BlockInfo,
            BlockStateUpdate, IndexValuePair,
        };

        fn block(number: u64) -> BlockInfo {
            let mut info = BlockInfo {
                inner: Block::empty(Default::default()),
                other: Default::default(),
            };
            info.inner.header.hash = H256::with_last_byte(number as u8 + 1);
            info.inner.header.inner.number = number;
            info.inner.header.inner.parent_hash = if number == 0 {
                H256::ZERO
            } else {
                H256::with_last_byte(number as u8)
            };
            info
        }

        fn price_diff(price: u64) -> AccountStorageDiff {
            AccountStorageDiff {
                address: BLAST_SHARES_HASH,
                diffs: vec![IndexValuePair {
                    index: SHARE_PRICE_SLOT_HASH,
                    value: U256::from(price),
                }],
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "leafage-resolver-state-tree-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Genesis carries no accounts and every later block stays in the
        // diff layers, so no account record is decoded from disk.
        let db = MultiStorage::open(&dir, 16, StorageKind::Rocksdb, false, false, false).unwrap();
        StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
            .update_block(block(0), BlockStateUpdate::default())
            .unwrap();
        let tree = StateTree::new(db, StateTreeConfig::new(64, 100, 100, 100, true)).unwrap();

        let alice = Address::repeat_byte(0x11);
        let block_1: BlockStateUpdate = BlastBlockStorageDiff {
            new_accounts: vec![BlastNewAccount {
                address: keccak256(alice),
                nonce: 1,
                flags: 0,
                fixed: U256::ZERO,
                shares: U256::from(13),
                remainder: U256::from(17),
                code_hash: KECCAK256_EMPTY,
            }],
            storage_diffs: vec![price_diff(5)],
            ..Default::default()
        }
        .into();
        let block_2: BlockStateUpdate = BlastBlockStorageDiff {
            storage_diffs: vec![price_diff(6)],
            ..Default::default()
        }
        .into();
        tree.update_block(block(1), block_1).unwrap();
        tree.update_block(block(2), block_2).unwrap();

        for (number, price) in [(1u64, 5u64), (2, 6)] {
            let state = tree.state_at(BlockId::number(number)).unwrap().unwrap();
            let resolver = AccountResolver::new(state, None, false);
            assert_eq!(
                balance(&resolver, alice),
                U256::from(price * 13 + 17),
                "block {number}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fixed_blast_balances_do_not_read_the_share_price() {
        let mut db = MockDB::default();
        db.insert(Address::repeat_byte(1), blast(1, 42, 13, 17));
        db.insert(Address::repeat_byte(2), blast(2, 43, 13, 17));
        let resolver = AccountResolver::new(db, None, false);
        assert_eq!(balance(&resolver, Address::repeat_byte(1)), U256::from(42));
        assert_eq!(balance(&resolver, Address::repeat_byte(2)), U256::from(43));
        assert!(resolver.inner.db.storage_reads.borrow().is_empty());
    }

    /// Existing RPC messages format backend errors with `{:?}`.
    #[test]
    fn backend_errors_debug_as_the_backend_error() {
        let err: ResolveError<MockErr> = MockErr.into();
        assert_eq!(format!("{err:?}"), format!("{MockErr:?}"));
        assert_eq!(err.to_string(), MockErr.to_string());
    }

    /// OVM chains fall back to the scalar path so the balance-slot
    /// override (whose key derivation skips normalize-state-key) stays
    /// in one place.
    #[test]
    fn batched_basic_keeps_ovm_balance_override() {
        let ovm_address = H256::repeat_byte(0x42);
        let address = Address::repeat_byte(0x11);
        let mut db = MockDB::default();
        db.insert(address, StoredAccount::standard(U256::ZERO, 7, H256::ZERO));
        db.storage.insert(
            (ovm_address, keccak256(get_ovm_balance_key(address))),
            U256::from(42u64),
        );
        let resolver = AccountResolver::new(db, Some(ovm_address), true);
        assert!(!resolver.supports_batched_reads());

        let batched = resolver.basic_many_ref(&[address]).unwrap();
        let account = batched[0].as_ref().unwrap();
        // Balance overridden from the OVM storage slot, nonce untouched.
        assert_eq!(account.balance, U256::from(42u64));
        assert_eq!(account.nonce, 7);
        // The balance slot is keyed by the raw (non-normalized) OVM key.
        let keys = resolver.inner.db.storage_reads.borrow();
        assert_eq!(
            keys[0],
            (ovm_address, keccak256(get_ovm_balance_key(address)))
        );
    }

    /// Without a stored account, a nonzero OVM balance still yields an
    /// account; a zero one yields none.
    #[test]
    fn ovm_balance_without_account() {
        let ovm_address = H256::repeat_byte(0x42);
        let funded = Address::repeat_byte(0x11);
        let mut db = MockDB::default();
        db.storage.insert(
            (ovm_address, keccak256(get_ovm_balance_key(funded))),
            U256::from(5u64),
        );
        let resolver = AccountResolver::new(db, Some(ovm_address), false);
        let mut expected = AccountInfo::default();
        expected.balance = U256::from(5u64);
        assert_eq!(resolver.basic_ref(funded).unwrap(), Some(expected));
        assert_eq!(
            resolver.basic_ref(Address::repeat_byte(0x12)).unwrap(),
            None
        );
    }

    #[test]
    fn test_get_ovm_balance_key() {
        let address = Address::from_str("0x455875815af7E846317D9E73e9Ea65d19EC58A82").unwrap();
        let expected_key =
            H256::from_str("0x0f3a88bb217e688cf0fede2f015e98298b832dcc3e2e4aa014ec244f1c785da6")
                .unwrap();
        assert_eq!(get_ovm_balance_key(address), expected_key);
    }
}
