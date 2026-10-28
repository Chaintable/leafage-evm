//! Arc mainnet genesis keys from arc-node 830ad37 assets/mainnet/genesis.json.
//! Account values, code and storage are read by the shared genesis builder.

use crate::pipeline::{
    chain::{ReplayOptions, ReplayOutput},
    genesis::{self, GenesisOptions},
};
use alloy::{
    primitives::{Address, B256},
    rpc::types::Block,
};
use anyhow::Result;
use revm::DatabaseRef;
use std::{collections::BTreeMap, sync::LazyLock};

static ALLOC: LazyLock<BTreeMap<Address, Vec<B256>>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("mainnet_genesis_keys.json"))
        .expect("valid pinned Arc genesis keys")
});

pub(crate) fn genesis_output<D>(
    state: &D,
    block: &Block,
    options: ReplayOptions,
) -> Result<ReplayOutput>
where
    D: DatabaseRef,
    D::Error: Send + Sync + 'static,
{
    let addresses: Vec<_> = ALLOC.keys().copied().collect();
    genesis::genesis_output(
        &addresses,
        state,
        block,
        options,
        GenesisOptions {
            storage_keys: Some(&ALLOC),
            strict: true,
            deduplicate_code: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;
    use revm::{database::InMemoryDB, state::AccountInfo};

    #[test]
    fn no_traces_preserves_genesis_transactions_and_storage_diff() {
        let mut db = InMemoryDB::default();
        for address in ALLOC.keys() {
            db.insert_account_info(
                *address,
                AccountInfo {
                    balance: U256::from(1),
                    ..Default::default()
                },
            );
        }
        let block = Block::default();
        let mut options = ReplayOptions {
            strict: true,
            check_state: false,
            state_diff: true,
            traces: true,
        };
        let traced = genesis_output(&db, &block, options).unwrap();
        options.traces = false;
        let plain = genesis_output(&db, &block, options).unwrap();
        assert!(!traced.block_file.traces.is_empty());
        assert!(!traced.block_file.storage_contracts.is_empty());
        assert!(plain.block_file.traces.is_empty());
        assert!(plain.block_file.storage_contracts.is_empty());
        assert_eq!(plain.block_file.txs.len(), ALLOC.len() + 1);
        assert_eq!(
            serde_json::to_value(plain.block_file.txs).unwrap(),
            serde_json::to_value(traced.block_file.txs).unwrap()
        );
        assert_eq!(plain.state_diff, traced.state_diff);
    }
}
