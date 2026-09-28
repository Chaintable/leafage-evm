use alloy::primitives::keccak256;
use auto_impl::auto_impl;
use leafage_evm_types::{
    Address, BlockId, BlockInfo, BlockStateUpdate, Bytecode, StoredAccount, H256, U256,
};
use revm::database_interface::DBErrorMarker;
use std::fmt::Debug;
use std::sync::Arc;

/// [`StateDB`] is a trait that provides access to the state of the EVM at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait StateDB {
    type Error: std::error::Error + DBErrorMarker + Send + Sync + 'static;
    /// Get basic account information.
    fn basic(&self, address: H256) -> Result<Option<StoredAccount>, Self::Error>;
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error>;
    /// Get storage value of address at index.
    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error>;
    // History related
    fn block_hash(&self, number: u64) -> Result<H256, Self::Error>;

    /// Batched [`StateDB::basic`]: one result per input, same order.
    /// The scalar default keeps every implementation correct; backends
    /// that can serve point reads in one storage round trip override it.
    fn basic_many(&self, addresses: &[H256]) -> Result<Vec<Option<StoredAccount>>, Self::Error> {
        addresses
            .iter()
            .map(|address| self.basic(*address))
            .collect()
    }

    /// Batched [`StateDB::code_by_hash`]: one result per input, same order.
    fn code_by_hash_many(&self, code_hashes: &[H256]) -> Result<Vec<Bytecode>, Self::Error> {
        code_hashes
            .iter()
            .map(|hash| self.code_by_hash(*hash))
            .collect()
    }

    /// Batched [`StateDB::storage`] over `(address, index)` pairs: one
    /// result per input, same order.
    fn storage_many(&self, keys: &[(H256, H256)]) -> Result<Vec<U256>, Self::Error> {
        keys.iter()
            .map(|(address, index)| self.storage(*address, *index))
            .collect()
    }

    /// Whether the `*_many` reads above are served by a real batched
    /// storage primitive instead of the scalar defaults. A performance
    /// hint for callers deciding whether eager batched reads are worth
    /// issuing; correctness never depends on it.
    fn supports_batched_reads(&self) -> bool {
        false
    }
}

/// [`BlockContext`] is a trait that provides access to the block information at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait BlockContext {
    type Error: std::error::Error + Send + Sync + 'static;
    // Block ctx related
    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        Ok(self.block_info_arc()?.as_ref().clone())
    }

    fn block_info_arc(&self) -> Result<Arc<BlockInfo>, Self::Error> {
        Ok(Arc::new(self.block_info()?))
    }

    fn state_diff(&self) -> Result<BlockStateUpdate, Self::Error> {
        Ok(self.state_diff_arc()?.as_ref().clone())
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStateUpdate>, Self::Error> {
        Ok(Arc::new(self.state_diff()?))
    }
}

#[derive(Clone, Debug)]
pub struct TxContext {
    pub block_hash: H256,
    pub block_number: u64,
    pub transaction_index: u64,
    pub transaction_hash: H256,
}

/// [`BlockIndex`] is a trait that provides access to the block information at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait BlockIndex {
    type Error: std::error::Error + Send + Sync + 'static;

    fn get_block_by_id(&self, block_id: BlockId) -> Result<Option<BlockInfo>, Self::Error> {
        self.get_block_by_id_arc(block_id)
            .map(|b| b.map(|b| b.as_ref().clone()))
    }

    fn get_block_by_id_arc(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<BlockInfo>>, Self::Error> {
        self.get_block_by_id(block_id)
            .map(|b| b.map(|b| Arc::new(b)))
    }
}

/// [`EvmStorageWrapper`] is a wrapper for [`StateDB`] that maps plain
/// addresses and storage slots to state keys. Accounts are returned as
/// stored; building revm's account view is up to the caller.
#[derive(Clone, Debug)]
pub struct EvmStorageWrapper<T> {
    pub db: T,
    pub normalize_state_key: bool,
}

impl<T: StateDB> EvmStorageWrapper<T> {
    pub fn basic_ref(&self, address: Address) -> Result<Option<StoredAccount>, T::Error> {
        self.db.basic(keccak256(address.as_slice()))
    }
    pub fn code_by_hash_ref(&self, code_hash: H256) -> Result<Bytecode, T::Error> {
        self.db.code_by_hash(code_hash.0.into())
    }
    pub fn storage_ref(&self, address: Address, index: U256) -> Result<U256, T::Error> {
        let address = keccak256(address.as_slice());
        let index = keccak256::<[u8; 32]>(if self.normalize_state_key {
            to_normalize_state_key(index)
        } else {
            index.to_be_bytes()
        });

        self.db
            .storage(address.into(), index.into())
            .map(|n| n.into())
    }
    pub fn block_hash_ref(&self, number: u64) -> Result<H256, T::Error> {
        self.db.block_hash(number).map(|h| h.0.into())
    }
}

impl<T: StateDB> EvmStorageWrapper<T> {
    /// Batched [`Self::basic_ref`]: one result per input, same order.
    pub fn basic_many_ref(
        &self,
        addresses: &[Address],
    ) -> Result<Vec<Option<StoredAccount>>, T::Error> {
        let hashed: Vec<H256> = addresses
            .iter()
            .map(|address| keccak256(address.as_slice()))
            .collect();
        self.db.basic_many(&hashed)
    }

    /// Batched [`Self::storage_ref`] over `(address, index)`
    /// pairs: one result per input, same order. Applies the same
    /// address keccak and normalize-state-key rules as the scalar path.
    pub fn storage_many_ref(&self, keys: &[(Address, U256)]) -> Result<Vec<U256>, T::Error> {
        let hashed: Vec<(H256, H256)> = keys
            .iter()
            .map(|(address, index)| {
                let address = keccak256(address.as_slice());
                let index = keccak256::<[u8; 32]>(if self.normalize_state_key {
                    to_normalize_state_key(*index)
                } else {
                    index.to_be_bytes()
                });
                (address, index)
            })
            .collect();
        self.db.storage_many(&hashed)
    }

    /// Batched [`Self::code_by_hash_ref`]: one result per input,
    /// same order.
    pub fn code_by_hash_many_ref(&self, code_hashes: &[H256]) -> Result<Vec<Bytecode>, T::Error> {
        self.db.code_by_hash_many(code_hashes)
    }

    /// Whether the `*_many_ref` reads above actually batch at the
    /// storage layer: needs a backend with real batched point reads.
    pub fn supports_batched_reads(&self) -> bool {
        self.db.supports_batched_reads()
    }
}

/// NormalizeStateKey ANDs the 0th bit of the first byte in `key`,
/// which ensures this bit will be 0 and all other bits are left the same.
/// This partitions normal state storage from multicoin storage.
pub fn to_normalize_state_key(index: U256) -> [u8; 32] {
    let mut res = index.to_be_bytes();
    res[0] &= 0xfe;
    res
}

/// [`EvmStorageRead`] is a trait that provides specific [`StateDB`] at specific block height.
#[auto_impl(&, Box, Arc)]
pub trait EvmStorageRead {
    type Error: std::error::Error + Send + Sync + 'static;
    type StateDB: StateDB
        + BlockContext<Error = <Self::StateDB as StateDB>::Error>
        + Send
        + Sync
        + Clone
        + Debug
        + 'static;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error>;
}

/// [`EvmStorageWrite`] is a trait that provides write access to the undering storage.
#[auto_impl(&, Box, Arc)]
pub trait EvmStorageWrite {
    type Error: std::error::Error + Send + Sync + 'static;
    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockStateUpdate,
    ) -> Result<(), Self::Error>;

    fn last_committed_block(&self) -> Result<Option<BlockInfo>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::str::FromStr;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("mock error")]
    struct MockErr;
    impl DBErrorMarker for MockErr {}

    /// Records every key reaching the underlying [`StateDB`], so the
    /// tests can assert the batched reads derive exactly the same keys as
    /// the scalar ones.
    #[derive(Debug, Default)]
    struct RecordingDB {
        basic_keys: RefCell<Vec<H256>>,
        storage_keys: RefCell<Vec<(H256, H256)>>,
    }

    impl StateDB for RecordingDB {
        type Error = MockErr;
        fn basic(&self, address: H256) -> Result<Option<StoredAccount>, MockErr> {
            self.basic_keys.borrow_mut().push(address);
            Ok(Some(StoredAccount::standard(U256::ZERO, 7, H256::ZERO)))
        }
        fn code_by_hash(&self, _code_hash: H256) -> Result<Bytecode, MockErr> {
            Ok(Bytecode::default())
        }
        fn storage(&self, address: H256, index: H256) -> Result<U256, MockErr> {
            self.storage_keys.borrow_mut().push((address, index));
            Ok(U256::from(42u64))
        }
        fn block_hash(&self, _number: u64) -> Result<H256, MockErr> {
            Ok(H256::ZERO)
        }
    }

    #[test]
    fn batched_wrapper_reads_derive_same_keys_as_scalar() {
        for normalize in [false, true] {
            let wrapper = EvmStorageWrapper {
                db: RecordingDB::default(),
                normalize_state_key: normalize,
            };
            let address = Address::repeat_byte(0x11);
            let index = U256::from(0x8000_0001u64) << 248usize;

            let scalar = wrapper.storage_ref(address, index).unwrap();
            let batched = wrapper.storage_many_ref(&[(address, index)]).unwrap();
            assert_eq!(batched, vec![scalar]);
            let keys = wrapper.db.storage_keys.borrow();
            assert_eq!(keys[0], keys[1], "normalize={normalize}");

            let scalar = wrapper.basic_ref(address).unwrap();
            let batched = wrapper.basic_many_ref(&[address]).unwrap();
            assert_eq!(batched, vec![scalar]);
            let keys = wrapper.db.basic_keys.borrow();
            assert_eq!(keys[0], keys[1]);
        }
    }

    #[test]
    fn test_normalize_state_key() {
        let key =
            H256::from_str("0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103")
                .unwrap();

        let key2 =
            H256::from_str("0xb43127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103")
                .unwrap();
        assert_eq!(to_normalize_state_key(key.into()), key2);
    }
}
