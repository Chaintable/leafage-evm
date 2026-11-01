//! Mainnet allocation addresses from celo-blockchain core/genesis_alloc.go.
//! Values and storage are read from the local genesis archive, like BSC.
use crate::pipeline::changes::StorageSlots;
use alloy::primitives::{address, keccak256, Address};
use anyhow::{Context as _, Result};
use leafage_evm_types::{
    AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use revm::{primitives::KECCAK_EMPTY, DatabaseRef};
use std::collections::BTreeMap;

pub(super) const ALLOC: [Address; 32] = [
    address!("000000000000000000000000000000000000ce10"),
    address!("08960ce6b58be32fbc6ac1489d04364b4f7dc216"),
    address!("11901cf7eeae1e2644995fb2e47ce46bc7f33246"),
    address!("19992ae48914a178bf138665cffdd8cd79b99513"),
    address!("1b6c64779f42ba6b54c853ab70171acd81b072f7"),
    address!("3fa7c646599f3174380bd9a7b6efcde90b5d129d"),
    address!("417fe63186c388812e342c85ff87187dc584c630"),
    address!("5f857c501b73ddfa804234f1f1418d6f75554076"),
    address!("743d80810fe10c5c3346d2940997cc9647035b13"),
    address!("75bb69c002c43f5a26a2a620518775795fd45ecf"),
    address!("77b68b2e7091d4f242a8af89f200af941433c6d8"),
    address!("7cf091c954ed7e9304452d31fd59999505ddcb7a"),
    address!("7fa26b50b3e9a2ec8ad1850a4c4fbbf94d806e95"),
    address!("8a07541c2ef161f4e3f8de7c7894718da26626b2"),
    address!("8d485780e84e23437f8f6938d96b964645529127"),
    address!("8e1c4355307f1a59e7ed4ae057c51368b9338c38"),
    address!("9033ff75af27222c8f36a148800c7331581933f3"),
    address!("989e1a3b344a43911e02ccc609d469fbc15ab1f1"),
    address!("a5d2944c32a8d7b284ff0b84c20fdcc46937cf64"),
    address!("a5d40d93b01afbafec84e20018aff427628f645e"),
    address!("aa9064f57f8d7de4b3e08c35561e21afd6341390"),
    address!("ae1d640648009dbe0aa4485d3bfbb68c37710924"),
    address!("b2fe7afe178335cec3564d7671eebd7634c626b0"),
    address!("b80d1e7f9cebe4b5e1b1acf037d3a44871105041"),
    address!("c1cda18694f5b86cfb80c1b4f8cc046b0d7e6326"),
    address!("c471776ea02705004c451959129bf09423b56526"),
    address!("de22679dca843b424fd0bbd70a22d5f5a4b94fe4"),
    address!("e23a4c6615669526ab58e9c37088bee4ed2b2dee"),
    address!("ef283eca68de87e051d427b4be152a7403110647"),
    address!("f5720c180a6fa14ecce82fb1bb060a39e93a263c"),
    address!("f8ed78a113cd2a34df451ba3d540ffae66829aa0"),
    address!("fc89c17525f08f2bc9ba8cb77bcf05055b1f7059"),
];

pub(super) fn state_diff<D>(
    state: &D,
    header: &alloy::rpc::types::Header,
) -> Result<BlockStorageDiff>
where
    D: DatabaseRef + StorageSlots,
    D::Error: Send + Sync + 'static,
{
    let mut diff = BlockStorageDiff {
        hash: header.state_root,
        // Native Celo uses the zero hash for the genesis predecessor.
        parent_hash: Default::default(),
        ..Default::default()
    };
    let mut codes = BTreeMap::new();
    for address in ALLOC {
        let account = state
            .basic_ref(address)?
            .context("Celo genesis account missing")?;
        let hashed = keccak256(address);
        diff.new_accounts.push(NewAccount {
            address: hashed,
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
        });
        let slots = state
            .storage_slots(address)?
            .context("genesis storage iteration unavailable")?;
        diff.storage_diffs.push(AccountStorageDiff {
            address: hashed,
            diffs: slots
                .into_iter()
                .map(|(index, value)| IndexValuePair { index, value })
                .collect(),
        });
        if account.code_hash != KECCAK_EMPTY && !account.code_hash.is_zero() {
            codes.insert(
                account.code_hash,
                NewCode {
                    code_hash: account.code_hash,
                    code: state.code_by_hash_ref(account.code_hash)?.original_bytes(),
                },
            );
        }
    }
    diff.new_accounts.sort_by_key(|a| a.address);
    diff.storage_diffs.sort_by_key(|s| s.address);
    for storage in &mut diff.storage_diffs {
        storage.diffs.sort_by_key(|s| s.index);
    }
    diff.new_codes = codes.into_values().collect();
    Ok(diff)
}
