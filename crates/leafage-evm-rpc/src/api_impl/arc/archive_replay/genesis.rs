//! Arc mainnet genesis keys from arc-node 830ad37 assets/mainnet/genesis.json.
//! Account values and code are read from archive block zero.

use super::convert::{append, DebankTrace};
use crate::pipeline::{
    build::{pipeline_block, pipeline_header},
    chain::{ReplayOptions, ReplayOutput, ReplayTimings},
    types::{addr_hex, to_hash, BlockFile, PipelineTransaction},
};
use alloy::{
    primitives::{keccak256, Address, Bytes, B256, U256},
    rpc::types::Block,
};
use anyhow::{anyhow, ensure, Result};
use leafage_evm_types::{
    AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use revm::{primitives::KECCAK_EMPTY, DatabaseRef};
use std::{collections::BTreeMap, sync::LazyLock};

static ALLOC: LazyLock<BTreeMap<Address, Vec<B256>>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("mainnet_genesis_keys.json"))
        .expect("valid pinned Arc genesis keys")
});

fn synthetic(file: &mut BlockFile, kind: u8, address: Address, value: U256, code: Bytes) {
    let id = format!(
        "0x{kind:02}{:022}{}",
        0,
        addr_hex(&address).trim_start_matches("0x")
    );
    file.txs.push(PipelineTransaction {
        id: id.clone(),
        from_addr: addr_hex(&Address::ZERO),
        to_addr: addr_hex(&address),
        gas_limit: 0,
        gas_price: 0,
        gas_used: 0,
        status: true,
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        input: code.clone(),
        nonce: 0,
        idx: file.txs.len() as i64,
        value,
    });
    let trace = DebankTrace {
        id: to_hash(&[&id, "", "0"]),
        from_addr: Address::ZERO,
        to_addr: address,
        value,
        input: code.clone(),
        output: code,
        call_create_type: if kind == 1 { "call" } else { "create" }.into(),
        call_type: if kind == 1 { "call" } else { "" }.into(),
        tx_id: id,
        ..Default::default()
    };
    append(file, (vec![trace], vec![], vec![], vec![]));
}

pub(crate) fn genesis_output<D: DatabaseRef>(
    state: &D,
    block: &Block,
    options: ReplayOptions,
) -> Result<ReplayOutput> {
    ensure!(block.header.number == 0, "expected Arc genesis");
    let mut file = BlockFile {
        block: pipeline_block(&block.header),
        ..Default::default()
    };
    let mut diff = BlockStorageDiff {
        hash: block.header.state_root,
        parent_hash: alloy::consensus::constants::EMPTY_ROOT_HASH,
        ..Default::default()
    };
    let mut addresses: Vec<_> = ALLOC.keys().copied().collect();
    addresses.sort_by_cached_key(|a| a.to_checksum(None));
    for address in addresses {
        let info = state
            .basic_ref(address)
            .map_err(|e| anyhow!("genesis {address}: {e:?}"))?
            .ok_or_else(|| anyhow!("genesis account {address} missing from archive"))?;
        let code = if info.code_hash == KECCAK_EMPTY || info.code_hash.is_zero() {
            Bytes::new()
        } else {
            state
                .code_by_hash_ref(info.code_hash)
                .map_err(|e| anyhow!("genesis code {address}: {e:?}"))?
                .original_bytes()
        };
        ensure!(
            keccak256(&code) == info.code_hash || (code.is_empty() && info.code_hash.is_zero()),
            "genesis code hash mismatch for {address}"
        );
        if !info.balance.is_zero() {
            synthetic(&mut file, 1, address, info.balance, Bytes::new());
        }
        if !code.is_empty() {
            synthetic(&mut file, 2, address, U256::ZERO, code.clone());
        }
        let hashed = keccak256(address);
        diff.new_accounts.push(NewAccount {
            address: hashed,
            balance: info.balance,
            nonce: info.nonce,
            code_hash: keccak256(&code),
        });
        if !code.is_empty() {
            diff.new_codes.push(NewCode {
                code_hash: keccak256(&code),
                code,
            });
        }
        let keys = &ALLOC[&address];
        if !keys.is_empty() {
            file.storage_contracts.push(addr_hex(&address));
            let mut diffs = Vec::with_capacity(keys.len());
            for slot in keys {
                let value = state
                    .storage_ref(address, U256::from_be_bytes(slot.0))
                    .map_err(|e| anyhow!("genesis storage {address}[{slot}]: {e:?}"))?;
                diffs.push(IndexValuePair {
                    index: keccak256(slot),
                    value,
                });
            }
            diffs.sort_by_key(|v| v.index);
            diff.storage_diffs.push(AccountStorageDiff {
                address: hashed,
                diffs,
            });
        }
    }
    synthetic(
        &mut file,
        3,
        Address::repeat_byte(0xee),
        U256::ZERO,
        Bytes::new(),
    );
    file.storage_contracts.sort();
    diff.new_accounts.sort_by_key(|a| a.address);
    diff.storage_diffs.sort_by_key(|a| a.address);
    diff.new_codes.sort_by_key(|c| c.code_hash);
    Ok(ReplayOutput {
        block_file: file,
        header: pipeline_header(block),
        state_diff: options.state_diff.then(|| alloy::rlp::encode(diff).into()),
        timings: ReplayTimings::default(),
    })
}
