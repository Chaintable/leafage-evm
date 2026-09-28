//! Leafage's internal account value.
//!
//! The pipeline carries each chain's consensus account state; revm's
//! [`AccountInfo`] is a view of it. For standard chains the two coincide. For
//! chains whose balance is not stored in the account (see [`AccountExt`]) the
//! raw fields are kept as-is and the balance is derived where `AccountInfo`
//! is built, against the same state view.

use crate::blast::{BlastAccountExt, BlastBlockStorageDiff, BlastNewAccount};
use crate::primitives::{AccountInfo, H256, U256};
use crate::storage::{BlockStorageDiff, NewAccount};

/// Account value carried through the in-memory layers and stored on disk.
///
/// Fields are private: an account with an extension has no materialized
/// balance, and [`StoredAccount::balance_view`] is the only way to reach the
/// balance, so reading one for an extension account is not expressible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    /// Only meaningful while `ext` is `None`.
    balance: U256,
    nonce: u64,
    code_hash: H256,
    /// Boxed so that standard accounts do not pay for the extension's size.
    ext: Option<Box<AccountExt>>,
}

/// Chain-specific account models. Deliberately not `#[non_exhaustive]`: a new
/// variant must be handled explicitly at every match, never fall through a
/// default arm that treats it as a standard account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountExt {
    Blast(BlastAccountExt),
}

/// What a [`StoredAccount`] knows about its balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceView<'a> {
    /// Materialized balance of a standard account.
    Standard(U256),
    /// Raw Blast yield fields; the balance depends on the sharePrice of the
    /// same state view.
    Blast(&'a BlastAccountExt),
}

impl StoredAccount {
    pub fn standard(balance: U256, nonce: u64, code_hash: H256) -> Self {
        Self {
            balance,
            nonce,
            code_hash,
            ext: None,
        }
    }

    pub fn with_ext(nonce: u64, code_hash: H256, ext: AccountExt) -> Self {
        Self {
            balance: U256::ZERO,
            nonce,
            code_hash,
            ext: Some(Box::new(ext)),
        }
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    pub fn code_hash(&self) -> H256 {
        self.code_hash
    }

    pub fn balance_view(&self) -> BalanceView<'_> {
        match self.ext.as_deref() {
            None => BalanceView::Standard(self.balance),
            Some(AccountExt::Blast(ext)) => BalanceView::Blast(ext),
        }
    }

    /// Builds the revm view of this account with a balance the caller has
    /// resolved from [`StoredAccount::balance_view`].
    pub fn to_account_info(&self, balance: U256) -> AccountInfo {
        AccountInfo {
            balance,
            nonce: self.nonce,
            code_hash: self.code_hash.0.into(),
            code: None,
            account_id: Default::default(),
        }
    }
}

impl From<NewAccount> for StoredAccount {
    fn from(account: NewAccount) -> Self {
        Self::standard(account.balance, account.nonce, account.code_hash)
    }
}

impl From<BlastNewAccount> for StoredAccount {
    fn from(account: BlastNewAccount) -> Self {
        let ext = account.ext();
        Self::with_ext(account.nonce, account.code_hash, AccountExt::Blast(ext))
    }
}

/// Account entry of the internal [`BlockStateUpdate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUpdate {
    pub address: H256,
    pub account: StoredAccount,
}

impl From<NewAccount> for AccountUpdate {
    fn from(account: NewAccount) -> Self {
        Self {
            address: account.address,
            account: account.into(),
        }
    }
}

impl From<BlastNewAccount> for AccountUpdate {
    fn from(account: BlastNewAccount) -> Self {
        Self {
            address: account.address,
            account: account.into(),
        }
    }
}

/// Internal per-block state update. [`AccountUpdate`] has no RLP encoding, so
/// this instantiation cannot be put on the wire; it is produced only by
/// decoding a wire diff (see [`crate::decode_state_diff`]).
pub type BlockStateUpdate = BlockStorageDiff<AccountUpdate>;

fn into_state_update<A: Into<AccountUpdate>>(diff: BlockStorageDiff<A>) -> BlockStateUpdate {
    BlockStorageDiff {
        hash: diff.hash,
        parent_hash: diff.parent_hash,
        new_accounts: diff.new_accounts.into_iter().map(Into::into).collect(),
        deleted_accounts: diff.deleted_accounts,
        storage_diffs: diff.storage_diffs,
        new_codes: diff.new_codes,
    }
}

impl From<BlockStorageDiff<NewAccount>> for BlockStateUpdate {
    fn from(diff: BlockStorageDiff<NewAccount>) -> Self {
        into_state_update(diff)
    }
}

impl From<BlastBlockStorageDiff> for BlockStateUpdate {
    fn from(diff: BlastBlockStorageDiff) -> Self {
        into_state_update(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_account_exposes_raw_fields_only() {
        let ext = BlastAccountExt {
            flags: 0,
            fixed: U256::from(1),
            shares: U256::from(2),
            remainder: U256::from(3),
        };
        let account =
            StoredAccount::with_ext(5, H256::repeat_byte(9), AccountExt::Blast(ext.clone()));
        assert_eq!(account.balance_view(), BalanceView::Blast(&ext));
        assert_eq!(account.nonce(), 5);
        assert_eq!(account.code_hash(), H256::repeat_byte(9));

        let standard = StoredAccount::standard(U256::from(100), 1, H256::ZERO);
        assert_eq!(
            standard.balance_view(),
            BalanceView::Standard(U256::from(100))
        );
    }

    #[test]
    fn standard_diff_converts_to_state_update() {
        let diff = BlockStorageDiff {
            hash: H256::repeat_byte(1),
            parent_hash: H256::repeat_byte(2),
            new_accounts: vec![NewAccount {
                address: H256::repeat_byte(3),
                balance: U256::from(10),
                nonce: 4,
                code_hash: H256::repeat_byte(5),
            }],
            deleted_accounts: vec![H256::repeat_byte(6)],
            ..Default::default()
        };
        let update = BlockStateUpdate::from(diff);
        assert_eq!(update.hash, H256::repeat_byte(1));
        assert_eq!(update.parent_hash, H256::repeat_byte(2));
        assert_eq!(update.deleted_accounts, vec![H256::repeat_byte(6)]);
        assert_eq!(
            update.new_accounts,
            vec![AccountUpdate {
                address: H256::repeat_byte(3),
                account: StoredAccount::standard(U256::from(10), 4, H256::repeat_byte(5)),
            }]
        );
    }
}
