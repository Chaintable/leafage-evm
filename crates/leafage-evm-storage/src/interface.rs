use alloy::primitives::{b256, keccak256};
use auto_impl::auto_impl;
use leafage_evm_types::{
    AccountInfo, Address, BalanceState, BlockId, BlockInfo, BlockStateUpdate, Bytecode,
    StoredAccount, H256, U256,
};
use revm::database_interface::DBErrorMarker;
use revm::DatabaseRef;
use std::fmt::Debug;
use std::sync::Arc;

/// [`StateDB`] is a trait that provides access to the state of the EVM at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait StateDB {
    type Error: std::error::Error + DBErrorMarker + Send + Sync + 'static;
    /// Get the raw internal account (no balance materialization).
    fn raw_account(&self, address: H256) -> Result<Option<StoredAccount>, Self::Error>;
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error>;
    /// Get storage value of address at index.
    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error>;
    // History related
    fn block_hash(&self, number: u64) -> Result<H256, Self::Error>;
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

/// Trie key of the Blast Shares predeploy: `keccak256(0x4300…0000)`.
pub const BLAST_SHARES_HASH: H256 =
    b256!("34ef019b82232cdd8dce3115c0c0787debeb839c4848a6e1df77393f6625ed82");
/// Trie key of the Shares predeploy's storage slot 1 (sharePrice):
/// `keccak256(uint256_be(1))`.
pub const SHARE_PRICE_SLOT_HASH: H256 =
    b256!("b10e2d527612073b26eecdfd717e6a320cf44b4afac2b0732d9fcbe2b7fa0cf6");

/// Error of [`EvmStorageWrapper`]: a backend error, or a Blast balance
/// derivation overflow (which must be an error, never wrapping arithmetic).
#[derive(Debug, thiserror::Error)]
pub enum EvmStorageError<E> {
    #[error(transparent)]
    Backend(#[from] E),
    #[error("blast balance overflow: shares * sharePrice + remainder exceeds U256")]
    BlastBalanceOverflow,
}

impl<E: std::error::Error + Send + Sync + 'static> DBErrorMarker for EvmStorageError<E> {}

/// [`EvmStorageWrapper`] is a wrapper for [`StateDB`] to implement [`DatabaseRef`].
/// This is the single place where a raw internal account is materialized into
/// a revm [`AccountInfo`] — including deriving the balance of Blast accounts
/// from the sharePrice of the same state view.
#[derive(Clone, Debug)]
pub struct EvmStorageWrapper<T> {
    pub db: T,
    pub ovm_address: Option<H256>,
    pub normalize_state_key: bool,
}

impl<T: StateDB> EvmStorageWrapper<T> {
    /// Materialize a raw account. Blast balance semantics mirror blast-geth
    /// `stateObject.Balance()` (state_object.go:650): only YieldAutomatic (0)
    /// derives `shares * sharePrice + remainder`; every other flags value
    /// (Disabled, Claimable, unknown) returns `fixed`.
    fn materialize(
        &self,
        account: StoredAccount,
    ) -> Result<AccountInfo, EvmStorageError<T::Error>> {
        let balance = match account.balance_state {
            BalanceState::Standard { balance } => balance,
            BalanceState::Blast {
                flags: 0,
                shares,
                remainder,
                ..
            } => {
                let price = self.db.storage(BLAST_SHARES_HASH, SHARE_PRICE_SLOT_HASH)?;
                shares
                    .checked_mul(price)
                    .and_then(|value| value.checked_add(remainder))
                    .ok_or(EvmStorageError::BlastBalanceOverflow)?
            }
            BalanceState::Blast { flags, fixed, .. } => {
                if flags > 2 {
                    tracing::warn!(target: "storage", "unknown blast yield flags {flags}, using fixed balance");
                }
                fixed
            }
        };
        Ok(AccountInfo {
            balance,
            nonce: account.nonce,
            code_hash: account.code_hash.0.into(),
            code: None,
            account_id: Default::default(),
        })
    }
}

impl<T: StateDB> DatabaseRef for EvmStorageWrapper<T> {
    type Error = EvmStorageError<T::Error>;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let account = match self.db.raw_account(keccak256(address.as_slice()))? {
            Some(raw_account) => Some(self.materialize(raw_account)?),
            None => None,
        };
        if let Some(ovm_address) = self.ovm_address {
            let balance = self
                .db
                .storage(ovm_address, keccak256(get_ovm_balance_key(address)))?;

            if let Some(mut account) = account {
                account.balance = balance;
                return Ok(Some(account));
            }

            if balance != U256::ZERO {
                let mut account = AccountInfo::default();
                account.balance = balance;
                return Ok(Some(account));
            }
        }
        Ok(account)
    }
    fn code_by_hash_ref(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        Ok(self.db.code_by_hash(code_hash.0.into())?)
    }
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let address = keccak256(address.as_slice());
        let index = keccak256::<[u8; 32]>(if self.normalize_state_key {
            to_normalize_state_key(index)
        } else {
            index.to_be_bytes()
        });

        Ok(self.db.storage(address.into(), index.into())?)
    }
    fn block_hash_ref(&self, number: u64) -> Result<H256, Self::Error> {
        Ok(self.db.block_hash(number).map(|h| h.0.into())?)
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
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_get_ovm_balance_key() {
        let address = Address::from_str("0x455875815af7E846317D9E73e9Ea65d19EC58A82").unwrap();
        let expected_key =
            H256::from_str("0x0f3a88bb217e688cf0fede2f015e98298b832dcc3e2e4aa014ec244f1c785da6")
                .unwrap();
        assert_eq!(get_ovm_balance_key(address), expected_key);
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

    #[derive(Debug, thiserror::Error)]
    #[error("mock error")]
    struct MockErr;
    impl DBErrorMarker for MockErr {}

    /// Mock keyed the same way the real backends are: accounts and storage by
    /// trie key hash.
    #[derive(Debug, Default, Clone)]
    struct MockStateDB {
        accounts: std::collections::HashMap<H256, StoredAccount>,
        storage: std::collections::HashMap<(H256, H256), U256>,
    }

    impl StateDB for MockStateDB {
        type Error = MockErr;
        fn raw_account(&self, address: H256) -> Result<Option<StoredAccount>, MockErr> {
            Ok(self.accounts.get(&address).cloned())
        }
        fn code_by_hash(&self, _code_hash: H256) -> Result<Bytecode, MockErr> {
            Ok(Bytecode::default())
        }
        fn storage(&self, address: H256, index: H256) -> Result<U256, MockErr> {
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

    fn blast_account(flags: u8, fixed: u64, shares: U256, remainder: u64) -> StoredAccount {
        StoredAccount {
            nonce: 9,
            code_hash: H256::repeat_byte(0x33),
            balance_state: BalanceState::Blast {
                flags,
                fixed: U256::from(fixed),
                shares,
                remainder: U256::from(remainder),
            },
        }
    }

    fn wrapper_with(account: StoredAccount, price: Option<U256>) -> EvmStorageWrapper<MockStateDB> {
        let address = Address::from_str("0x455875815af7E846317D9E73e9Ea65d19EC58A82").unwrap();
        let mut db = MockStateDB::default();
        db.accounts.insert(keccak256(address.as_slice()), account);
        if let Some(price) = price {
            db.storage
                .insert((BLAST_SHARES_HASH, SHARE_PRICE_SLOT_HASH), price);
        }
        EvmStorageWrapper {
            db,
            ovm_address: None,
            normalize_state_key: false,
        }
    }

    fn balance_of(
        wrapper: &EvmStorageWrapper<MockStateDB>,
    ) -> Result<U256, EvmStorageError<MockErr>> {
        let address = Address::from_str("0x455875815af7E846317D9E73e9Ea65d19EC58A82").unwrap();
        Ok(wrapper.basic_ref(address)?.unwrap().balance)
    }

    /// Mirrors blast-geth `stateObject.Balance()`: only flags == 0 derives
    /// `shares * sharePrice + remainder`; 1, 2 and unknown values return fixed.
    #[test]
    fn test_blast_balance_derivation() {
        // Automatic: shares * price + remainder.
        let w = wrapper_with(
            blast_account(0, 11, U256::from(13), 17),
            Some(U256::from(1_019_184_352u64)),
        );
        assert_eq!(
            balance_of(&w).unwrap(),
            U256::from(13u64 * 1_019_184_352 + 17)
        );

        // price == 0 (slot missing / zero): balance == remainder.
        let w = wrapper_with(blast_account(0, 11, U256::from(13), 17), None);
        assert_eq!(balance_of(&w).unwrap(), U256::from(17));

        // Disabled / Claimable / unknown flags: fixed.
        for flags in [1u8, 2, 3, 255] {
            let w = wrapper_with(
                blast_account(flags, 11, U256::from(13), 17),
                Some(U256::from(1_019_184_352u64)),
            );
            assert_eq!(balance_of(&w).unwrap(), U256::from(11), "flags {flags}");
        }

        // Overflow must be an error, never wrapping arithmetic.
        let w = wrapper_with(blast_account(0, 0, U256::MAX, 0), Some(U256::from(2)));
        assert!(matches!(
            balance_of(&w),
            Err(EvmStorageError::BlastBalanceOverflow)
        ));

        // Standard accounts pass their balance through unchanged.
        let standard = StoredAccount {
            nonce: 3,
            code_hash: H256::repeat_byte(0x44),
            balance_state: BalanceState::Standard {
                balance: U256::from(12345),
            },
        };
        let w = wrapper_with(standard, None);
        let address = Address::from_str("0x455875815af7E846317D9E73e9Ea65d19EC58A82").unwrap();
        let info = w.basic_ref(address).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(12345));
        assert_eq!(info.nonce, 3);
        assert_eq!(info.code_hash, H256::repeat_byte(0x44));
    }
}
