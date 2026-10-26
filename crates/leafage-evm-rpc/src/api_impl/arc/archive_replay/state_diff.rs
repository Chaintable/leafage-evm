// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Writer BundleState extraction, ported from arc-node 830ad37 debank_trace.rs.
use alloy::primitives::{keccak256, Address, B256 as H256};
use leafage_evm_types::{
    AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use revm::{database::BundleState, DatabaseRef};

pub fn get_storage_contracts_from_bundle(bundle: &BundleState) -> Vec<Address> {
    let mut storage_contracts: Vec<_> = bundle
        .state
        .iter()
        .filter(|(_, account)| !account.storage.is_empty())
        .map(|(address, _)| *address)
        .collect();
    storage_contracts.sort_unstable();
    storage_contracts
}

pub fn get_storage_diffs_from_bundle(bundle: BundleState) -> anyhow::Result<BlockStorageDiff> {
    let mut new_accounts = Vec::new();
    let mut deleted_accounts = Vec::new();
    let mut storage_diffs = Vec::new();
    let mut new_codes = Vec::new();

    for (address, account) in bundle.state {
        let Some(info) = account.info else {
            deleted_accounts.push(keccak256(address.0));
            continue;
        };

        new_accounts.push(NewAccount {
            address: keccak256(address.0),
            balance: info.balance,
            nonce: info.nonce,
            code_hash: info.code_hash,
        });

        if !account.storage.is_empty() {
            let diffs: Vec<IndexValuePair> = account
                .storage
                .into_iter()
                .map(|(key, slot)| IndexValuePair {
                    index: keccak256::<[u8; 32]>(key.to_be_bytes()),
                    value: slot.present_value,
                })
                .collect();
            if !diffs.is_empty() {
                storage_diffs.push(AccountStorageDiff {
                    address: keccak256(address.0),
                    diffs,
                });
            }
        }

        if let Some(code) = info.code {
            let code_hash = info.code_hash;
            if account
                .original_info
                .as_ref()
                .is_some_and(|old| old.code_hash == code_hash)
            {
                continue;
            }
            new_codes.push(NewCode {
                code_hash,
                code: code.original_bytes(),
            });
        }
    }

    let mut state_diff = BlockStorageDiff {
        hash: H256::ZERO,
        parent_hash: H256::ZERO,
        new_accounts,
        deleted_accounts,
        storage_diffs,
        new_codes,
    };
    sort_block_storage_diff(&mut state_diff);
    Ok(state_diff)
}

fn sort_block_storage_diff(state_diff: &mut BlockStorageDiff) {
    state_diff
        .new_accounts
        .sort_unstable_by_key(|account| account.address);
    state_diff.deleted_accounts.sort_unstable();
    for account in &mut state_diff.storage_diffs {
        account.diffs.sort_unstable_by_key(|diff| diff.index);
    }
    state_diff
        .storage_diffs
        .sort_unstable_by_key(|account| account.address);
    state_diff
        .new_codes
        .sort_unstable_by_key(|code| code.code_hash);
}

pub(crate) fn verify<D: DatabaseRef>(bundle: &BundleState, post: &D) -> anyhow::Result<()> {
    for (address, account) in &bundle.state {
        let actual = post
            .basic_ref(*address)
            .map_err(|e| anyhow::anyhow!("post account {address}: {e:?}"))?;
        match (&account.info, &actual) {
            (None, None) => {}
            (Some(a), Some(b))
                if a.balance == b.balance && a.nonce == b.nonce && a.code_hash == b.code_hash => {}
            _ => anyhow::bail!(
                "post-state account {address} differs: replay {:?}, archive {:?}",
                account.info,
                actual
            ),
        }
        if account.info.is_some() {
            for (slot, value) in &account.storage {
                let actual = post
                    .storage_ref(*address, *slot)
                    .map_err(|e| anyhow::anyhow!("post storage {address}[{slot}]: {e:?}"))?;
                anyhow::ensure!(
                    actual == value.present_value,
                    "post-state storage {address}[{slot}] differs: replay {}, archive {actual}",
                    value.present_value
                );
            }
        }
    }
    Ok(())
}
