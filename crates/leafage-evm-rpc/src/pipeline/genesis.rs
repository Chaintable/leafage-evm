//! Bundle 0, the genesis block. It has no transactions to replay: like the
//! pipeline's `OnGenesisBlock`, its block file and state diff are made from
//! the genesis alloc, read here from the archive state of block 0. The
//! archive keys accounts by hashed address, so the chain names the alloc's
//! addresses.

use super::build::{pipeline_block, pipeline_header};
use super::chain::{ReplayOptions, ReplayOutput, ReplayTimings};
use super::types::{addr_hex, to_hash, BlockFile, PipelineTrace, PipelineTransaction};
use alloy::primitives::{keccak256, Address, Bytes, U256};
use alloy::rpc::types::Block as RpcBlock;
use anyhow::{bail, Result};
use leafage_evm_types::{BlockStorageDiff, NewAccount, NewCode};
use revm::primitives::KECCAK_EMPTY;
use revm::DatabaseRef;
use std::collections::BTreeMap;

/// Stand-in contract of the native token (`0xeee…`).
const NATIVE_TOKEN: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// One account of the genesis alloc as stored in the archive.
struct AllocAccount {
    address: Address,
    balance: U256,
    nonce: u64,
    code: Bytes,
}

/// The outputs of the genesis `block`, from the accounts at `alloc` in
/// `state`, the archive state of block 0.
pub(crate) fn genesis_output<D>(
    alloc: &[Address],
    state: &D,
    block: &RpcBlock,
    options: ReplayOptions,
) -> Result<ReplayOutput>
where
    D: DatabaseRef,
    D::Error: Send + Sync + 'static,
{
    if block.header.number != 0 {
        bail!("block {} is not the genesis block", block.header.number);
    }
    let mut accounts = Vec::with_capacity(alloc.len());
    for &address in alloc {
        let info = state.basic_ref(address)?.unwrap_or_default();
        let code = if info.code_hash == KECCAK_EMPTY || info.code_hash.is_zero() {
            Bytes::new()
        } else {
            state.code_by_hash_ref(info.code_hash)?.original_bytes()
        };
        accounts.push(AllocAccount {
            address,
            balance: info.balance,
            nonce: info.nonce,
            code,
        });
    }
    let state_diff = options
        .state_diff
        .then(|| alloy::rlp::encode(genesis_state_diff(&accounts, block)).into());
    Ok(ReplayOutput {
        block_file: genesis_block_file(accounts, block),
        header: pipeline_header(block),
        state_diff,
        timings: ReplayTimings::default(),
    })
}

/// `genesisTxID`: 0x, the kind (1 balance, 2 code, 3 native token), 22
/// zeros and the address, as long as a transaction hash.
fn genesis_tx_id(kind: u8, address: &str) -> String {
    format!("0x{kind:02}{:022}{}", 0, address.trim_start_matches("0x"))
}

/// A synthetic transaction of the genesis block and its single trace.
fn genesis_tx(
    kind: u8,
    to: &str,
    value: U256,
    input: Bytes,
    output: Bytes,
    idx: i64,
) -> (PipelineTransaction, PipelineTrace) {
    let id = genesis_tx_id(kind, to);
    let create = kind != 1;
    let trace = PipelineTrace {
        id: to_hash(&[&id, "", "0"]),
        from_addr: ZERO_ADDRESS.to_string(),
        gas_limit: 0,
        input: input.clone(),
        to_addr: to.to_string(),
        value,
        gas_used: 0,
        output,
        call_create_type: if create { "create" } else { "call" }.to_string(),
        call_type: if create { "" } else { "call" }.to_string(),
        tx_id: id.clone(),
        parent_trace_id: String::new(),
        pos_in_parent_trace: 0,
        self_storage_change: false,
        storage_change: false,
        subtraces: 0,
        trace_address: Vec::new(),
        error: String::new(),
    };
    let tx = PipelineTransaction {
        id,
        from_addr: ZERO_ADDRESS.to_string(),
        to_addr: to.to_string(),
        gas_limit: 0,
        gas_price: 0,
        gas_used: 0,
        status: true,
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        input,
        nonce: 0,
        idx,
        value,
    };
    (tx, trace)
}

/// `OnGenesisBlock`'s block file: a transfer for every account with a
/// balance, a create for every account with code, in the order of the
/// checksummed addresses, then the create of the native token.
fn genesis_block_file(mut accounts: Vec<AllocAccount>, block: &RpcBlock) -> BlockFile {
    accounts.sort_by_cached_key(|account| account.address.to_checksum(None));
    let mut block_file = BlockFile {
        block: pipeline_block(&block.header),
        ..Default::default()
    };
    let mut push = |(tx, trace)| {
        block_file.txs.push(tx);
        block_file.traces.push(trace);
    };
    let mut idx = 0;
    for account in &accounts {
        let to = addr_hex(&account.address);
        if !account.balance.is_zero() {
            push(genesis_tx(
                1,
                &to,
                account.balance,
                Bytes::new(),
                Bytes::new(),
                idx,
            ));
            idx += 1;
        }
        if !account.code.is_empty() {
            push(genesis_tx(
                2,
                &to,
                U256::ZERO,
                account.code.clone(),
                account.code.clone(),
                idx,
            ));
            idx += 1;
        }
    }
    push(genesis_tx(
        3,
        NATIVE_TOKEN,
        U256::ZERO,
        Bytes::new(),
        Bytes::new(),
        idx,
    ));
    block_file
}

/// `GenesisAllocToStateDiff`: every account of the alloc is new. The archive
/// cannot list a contract's slots without their keys, so genesis storage is
/// left out (none of the supported chains has any).
fn genesis_state_diff(accounts: &[AllocAccount], block: &RpcBlock) -> BlockStorageDiff {
    let mut new_accounts = BTreeMap::new();
    let mut new_codes = BTreeMap::new();
    for account in accounts {
        let hashed = keccak256(account.address);
        let code_hash = keccak256(&account.code);
        new_accounts.insert(
            hashed,
            NewAccount {
                address: hashed,
                balance: account.balance,
                nonce: account.nonce,
                code_hash,
            },
        );
        if !account.code.is_empty() {
            new_codes.insert(
                code_hash,
                NewCode {
                    code_hash,
                    code: account.code.clone(),
                },
            );
        }
    }
    BlockStorageDiff {
        hash: block.header.state_root,
        // The genesis block has no parent.
        parent_hash: alloy::consensus::constants::EMPTY_ROOT_HASH,
        new_accounts: new_accounts.into_values().collect(),
        new_codes: new_codes.into_values().collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_tx_id_matches_go() {
        // fmt.Sprintf("0x%02d%022d%s", 1, 0, "b005…b632")
        assert_eq!(
            genesis_tx_id(1, "0xb005741528b86f5952469d80a8614591e3c5b632"),
            "0x010000000000000000000000b005741528b86f5952469d80a8614591e3c5b632"
        );
        assert_eq!(genesis_tx_id(3, NATIVE_TOKEN).len(), 66);
    }
}
