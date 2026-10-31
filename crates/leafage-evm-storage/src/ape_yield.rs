//! ApeChain balances for the read path.
//!
//! The Chaintable writer stores ApeChain accounts with balance 0 and keeps the
//! raw yield fields as storage of [`APE_YIELD_ADDRESS`] (see
//! [`leafage_evm_types::ape_yield`]). [`ApeYieldStorage`] wraps an
//! [`EvmStorageRead`] so every account read resolves the balance from those
//! slots and the ArbOS share price of the same block. Storage, code and block
//! reads pass through unchanged.

use crate::interface::{BlockContext, BlockIndex, EvmStorageRead, StateDB};
use alloy::primitives::keccak256;
use leafage_evm_types::ape_yield::{
    ape_arbos_root_slot, ape_balance_from_slots, ape_yield_slot, APE_ARBOS_STATE_ADDRESS,
    APE_SHARE_PRICE_OFFSET, APE_YIELD_ADDRESS_HASH, APE_YIELD_SLOT_COUNT,
};
use leafage_evm_types::{AccountInfo, BlockId, BlockInfo, BlockStorageDiff, Bytecode, H256, U256};
use std::sync::{Arc, OnceLock};

/// [`EvmStorageRead`] whose states resolve ApeChain balances.
#[derive(Clone, Debug)]
pub struct ApeYieldStorage<T>(pub T);

impl<T: EvmStorageRead> EvmStorageRead for ApeYieldStorage<T> {
    type Error = T::Error;
    type StateDB = ApeYieldStateDB<T::StateDB>;

    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error> {
        Ok(self.0.state_at(block_arg)?.map(ApeYieldStateDB::new))
    }
}

impl<T: BlockIndex> BlockIndex for ApeYieldStorage<T> {
    type Error = T::Error;

    fn get_block_by_id(&self, block_id: BlockId) -> Result<Option<BlockInfo>, Self::Error> {
        self.0.get_block_by_id(block_id)
    }

    fn get_block_by_id_arc(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<BlockInfo>>, Self::Error> {
        self.0.get_block_by_id_arc(block_id)
    }
}

/// One block's state with ApeChain balances. The share price is read once.
#[derive(Clone, Debug)]
pub struct ApeYieldStateDB<S> {
    inner: S,
    share_price: OnceLock<U256>,
}

/// StateDB keys of the reserved-account slots of `addr_hash`.
fn yield_keys(addr_hash: H256) -> [(H256, H256); APE_YIELD_SLOT_COUNT] {
    std::array::from_fn(|i| {
        let slot = ape_yield_slot(addr_hash, i);
        (APE_YIELD_ADDRESS_HASH, keccak256(slot.to_be_bytes::<32>()))
    })
}

fn share_price_key() -> (H256, H256) {
    let slot = ape_arbos_root_slot(APE_SHARE_PRICE_OFFSET);
    (
        keccak256(APE_ARBOS_STATE_ADDRESS),
        keccak256(slot.to_be_bytes::<32>()),
    )
}

/// The reserved account itself: nonce 1 so revm never clears it as empty.
fn yield_account() -> AccountInfo {
    AccountInfo {
        nonce: 1,
        ..Default::default()
    }
}

impl<S: StateDB> ApeYieldStateDB<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            share_price: OnceLock::new(),
        }
    }

    fn share_price(&self) -> Result<U256, S::Error> {
        if let Some(price) = self.share_price.get() {
            return Ok(*price);
        }
        let (address, index) = share_price_key();
        let price = self.inner.storage(address, index)?;
        Ok(*self.share_price.get_or_init(|| price))
    }
}

impl<S: StateDB> StateDB for ApeYieldStateDB<S> {
    type Error = S::Error;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        if address == APE_YIELD_ADDRESS_HASH {
            return Ok(Some(yield_account()));
        }
        let Some(mut info) = self.inner.basic(address)? else {
            return Ok(None);
        };
        let slots = self.inner.storage_many(&yield_keys(address))?;
        info.balance =
            ape_balance_from_slots(std::array::from_fn(|i| slots[i]), self.share_price()?);
        Ok(Some(info))
    }

    fn basic_many(&self, addresses: &[H256]) -> Result<Vec<Option<AccountInfo>>, Self::Error> {
        let mut accounts = self.inner.basic_many(addresses)?;
        let found: Vec<usize> = (0..addresses.len())
            .filter(|&i| accounts[i].is_some() && addresses[i] != APE_YIELD_ADDRESS_HASH)
            .collect();
        let mut keys: Vec<(H256, H256)> = found
            .iter()
            .flat_map(|&i| yield_keys(addresses[i]))
            .collect();
        let read_price = self.share_price.get().is_none();
        if read_price {
            keys.push(share_price_key());
        }
        let values = if keys.is_empty() {
            Vec::new()
        } else {
            self.inner.storage_many(&keys)?
        };
        let price = if read_price {
            *self.share_price.get_or_init(|| values[values.len() - 1])
        } else {
            self.share_price()?
        };
        for (n, &i) in found.iter().enumerate() {
            let at = n * APE_YIELD_SLOT_COUNT;
            if let Some(info) = accounts[i].as_mut() {
                info.balance =
                    ape_balance_from_slots(std::array::from_fn(|k| values[at + k]), price);
            }
        }
        for (i, address) in addresses.iter().enumerate() {
            if *address == APE_YIELD_ADDRESS_HASH {
                accounts[i] = Some(yield_account());
            }
        }
        Ok(accounts)
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        self.inner.storage(address, index)
    }

    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        self.inner.block_hash(number)
    }

    fn code_by_hash_many(&self, code_hashes: &[H256]) -> Result<Vec<Bytecode>, Self::Error> {
        self.inner.code_by_hash_many(code_hashes)
    }

    fn storage_many(&self, keys: &[(H256, H256)]) -> Result<Vec<U256>, Self::Error> {
        self.inner.storage_many(keys)
    }

    fn supports_batched_reads(&self) -> bool {
        self.inner.supports_batched_reads()
    }

    fn storage_slots(&self, address: H256) -> Result<Option<Vec<(H256, U256)>>, Self::Error> {
        self.inner.storage_slots(address)
    }
}

impl<S: BlockContext> BlockContext for ApeYieldStateDB<S> {
    type Error = <S as BlockContext>::Error;

    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        self.inner.block_info()
    }

    fn block_info_arc(&self) -> Result<Arc<BlockInfo>, Self::Error> {
        self.inner.block_info_arc()
    }

    fn state_diff(&self) -> Result<BlockStorageDiff, Self::Error> {
        self.inner.state_diff()
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, Self::Error> {
        self.inner.state_diff_arc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::ape_yield::{ApeYieldFields, APE_YIELD_DELEGATED, APE_YIELD_DISABLED};
    use leafage_evm_types::Address;
    use revm::database_interface::DBErrorMarker;
    use std::collections::HashMap;

    #[derive(Debug)]
    struct MockErr;
    impl std::fmt::Display for MockErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("mock")
        }
    }
    impl std::error::Error for MockErr {}
    impl DBErrorMarker for MockErr {}

    #[derive(Default)]
    struct MockDB {
        accounts: HashMap<H256, AccountInfo>,
        storage: HashMap<(H256, H256), U256>,
    }

    impl MockDB {
        fn put(&mut self, address: Address, nonce: u64, fields: ApeYieldFields) {
            let hash = keccak256(address);
            self.accounts.insert(
                hash,
                AccountInfo {
                    nonce,
                    ..Default::default()
                },
            );
            for (key, value) in yield_keys(hash).into_iter().zip(fields.to_slots()) {
                if !value.is_zero() {
                    self.storage.insert(key, value);
                }
            }
        }
    }

    impl StateDB for MockDB {
        type Error = MockErr;
        fn basic(&self, address: H256) -> Result<Option<AccountInfo>, MockErr> {
            Ok(self.accounts.get(&address).cloned())
        }
        fn code_by_hash(&self, _: H256) -> Result<Bytecode, MockErr> {
            Ok(Bytecode::default())
        }
        fn storage(&self, address: H256, index: H256) -> Result<U256, MockErr> {
            Ok(self
                .storage
                .get(&(address, index))
                .copied()
                .unwrap_or_default())
        }
        fn block_hash(&self, _: u64) -> Result<H256, MockErr> {
            Ok(H256::ZERO)
        }
    }

    fn db_with_price(price: u64) -> MockDB {
        let mut db = MockDB::default();
        db.storage.insert(share_price_key(), U256::from(price));
        db
    }

    #[test]
    fn resolves_balance_for_each_yield_mode() {
        let price = 1_000_000_007u64;
        let mut db = db_with_price(price);
        let automatic = Address::repeat_byte(0xa0);
        let disabled = Address::repeat_byte(0xb0);
        let delegated = Address::repeat_byte(0xc0);
        let delegate = Address::repeat_byte(0xd0);
        db.put(
            automatic,
            3,
            ApeYieldFields {
                fixed: U256::from(123),
                shares: U256::from(5),
                ..Default::default()
            },
        );
        db.put(
            disabled,
            0,
            ApeYieldFields {
                flags: APE_YIELD_DISABLED,
                fixed: U256::from(77),
                ..Default::default()
            },
        );
        db.put(
            delegated,
            0,
            ApeYieldFields {
                flags: APE_YIELD_DELEGATED,
                fixed: U256::from(300),
                delegate,
                ..Default::default()
            },
        );
        // The delegate holds 1000 of its own plus 300 delegated, owed back as debt.
        db.put(
            delegate,
            0,
            ApeYieldFields {
                fixed: U256::from(1_300),
                debt: U256::from(300),
                ..Default::default()
            },
        );
        let state = ApeYieldStateDB::new(db);
        let balance = |a: Address| state.basic(keccak256(a)).unwrap().unwrap().balance;

        assert_eq!(balance(automatic), U256::from(5 * price + 123));
        assert_eq!(balance(disabled), U256::from(77));
        assert_eq!(balance(delegated), U256::from(300));
        assert_eq!(balance(delegate), U256::from(1_000));
        assert_eq!(state.basic(keccak256(automatic)).unwrap().unwrap().nonce, 3);
        assert!(state
            .basic(keccak256(Address::repeat_byte(0xee)))
            .unwrap()
            .is_none());
        assert_eq!(
            state.basic(APE_YIELD_ADDRESS_HASH).unwrap().unwrap().nonce,
            1
        );

        let hashes = [automatic, Address::repeat_byte(0xee), delegate, disabled].map(keccak256);
        let batched: Vec<Option<U256>> = state
            .basic_many(&hashes)
            .unwrap()
            .into_iter()
            .map(|a| a.map(|a| a.balance))
            .collect();
        assert_eq!(
            batched,
            vec![
                Some(U256::from(5 * price + 123)),
                None,
                Some(U256::from(1_000)),
                Some(U256::from(77)),
            ]
        );
    }

    #[test]
    fn basic_many_reads_price_once_with_cached_value() {
        let mut db = db_with_price(2);
        let a = Address::repeat_byte(0x01);
        db.put(
            a,
            0,
            ApeYieldFields {
                shares: U256::from(10),
                ..Default::default()
            },
        );
        let state = ApeYieldStateDB::new(db);
        assert_eq!(
            state.basic(keccak256(a)).unwrap().unwrap().balance,
            U256::from(20)
        );
        let batched = state.basic_many(&[keccak256(a)]).unwrap();
        assert_eq!(batched[0].as_ref().unwrap().balance, U256::from(20));
        assert!(state.basic_many(&[]).unwrap().is_empty());
    }

    #[test]
    fn storage_passes_through() {
        let mut db = db_with_price(9);
        let key = (H256::repeat_byte(1), H256::repeat_byte(2));
        db.storage.insert(key, U256::from(42));
        let state = ApeYieldStateDB::new(db);
        assert_eq!(state.storage(key.0, key.1).unwrap(), U256::from(42));
    }
}
