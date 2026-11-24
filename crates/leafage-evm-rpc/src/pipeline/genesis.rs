//! Bundle 0, the genesis block. It has no transactions to replay: like the
//! pipeline's `OnGenesisBlock`, its block file and state diff are made from
//! the genesis alloc, read here from the archive state of block 0. The
//! archive keys accounts by hashed address, so the chain names the alloc's
//! addresses.

use super::build::{pipeline_block, pipeline_header};
use super::chain::{ReplayOptions, ReplayOutput, ReplayTimings};
use super::types::{addr_hex, to_hash, BlockFile, PipelineTrace, PipelineTransaction};
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::rpc::types::Block as RpcBlock;
use anyhow::{anyhow, bail, ensure, Result};
use leafage_evm_types::{
    AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use revm::primitives::KECCAK_EMPTY;
use revm::DatabaseRef;
use std::collections::BTreeMap;

pub(crate) struct GenesisOptions<'a> {
    /// Original storage keys; None emits legacy empty entries for every account.
    pub storage_keys: Option<&'a BTreeMap<Address, Vec<B256>>>,
    /// Require every alloc account and verify its code hash.
    pub strict: bool,
    /// ETH/BSC deduplicate code; Arc Writer emits a code entry per account.
    pub deduplicate_code: bool,
}

impl Default for GenesisOptions<'_> {
    fn default() -> Self {
        Self {
            storage_keys: None,
            strict: false,
            deduplicate_code: true,
        }
    }
}

/// Stand-in contract of the native token (`0xeee…`).
const NATIVE_TOKEN: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// One account of the genesis alloc as stored in the archive.
struct AllocAccount {
    address: Address,
    balance: U256,
    nonce: u64,
    code: Bytes,
    storage: Option<Vec<IndexValuePair>>,
}

/// The outputs of the genesis `block`, from the accounts at `alloc` in
/// `state`, the archive state of block 0.
pub(crate) fn genesis_output<D>(
    alloc: &[Address],
    state: &D,
    block: &RpcBlock,
    options: ReplayOptions,
    genesis: GenesisOptions<'_>,
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
        let info = state.basic_ref(address)?;
        let info = if genesis.strict {
            info.ok_or_else(|| anyhow!("genesis account {address} missing from archive"))?
        } else {
            info.unwrap_or_default()
        };
        let code = if info.code_hash == KECCAK_EMPTY || info.code_hash.is_zero() {
            Bytes::new()
        } else {
            state.code_by_hash_ref(info.code_hash)?.original_bytes()
        };
        if genesis.strict {
            ensure!(
                keccak256(&code) == info.code_hash || (code.is_empty() && info.code_hash.is_zero()),
                "genesis code hash mismatch for {address}"
            );
        }
        let storage = match genesis.storage_keys {
            None => Some(Vec::new()),
            Some(keys) => match keys.get(&address).filter(|keys| !keys.is_empty()) {
                None => None,
                Some(keys) => {
                    let mut diffs = Vec::with_capacity(keys.len());
                    for slot in keys {
                        let value = state
                            .storage_ref(address, U256::from_be_bytes(slot.0))
                            .map_err(|err| anyhow!("genesis storage {address}[{slot}]: {err:?}"))?;
                        diffs.push(IndexValuePair {
                            index: keccak256(slot),
                            value,
                        });
                    }
                    diffs.sort_by_key(|diff| diff.index);
                    Some(diffs)
                }
            },
        };
        accounts.push(AllocAccount {
            address,
            balance: info.balance,
            nonce: info.nonce,
            code,
            storage,
        });
    }
    let state_diff = options.state_diff.then(|| {
        alloy::rlp::encode(genesis_state_diff(
            &accounts,
            block,
            genesis.deduplicate_code,
        ))
        .into()
    });
    Ok(ReplayOutput {
        block_file: genesis_block_file(accounts, block, options.traces),
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
fn genesis_block_file(
    mut accounts: Vec<AllocAccount>,
    block: &RpcBlock,
    traces: bool,
) -> BlockFile {
    accounts.sort_by_cached_key(|account| account.address.to_checksum(None));
    let mut block_file = BlockFile {
        block: pipeline_block(&block.header),
        ..Default::default()
    };
    let mut push = |(tx, trace)| {
        block_file.txs.push(tx);
        if traces {
            block_file.traces.push(trace);
        }
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
    if traces {
        block_file.storage_contracts = accounts
            .iter()
            .filter(|account| {
                account
                    .storage
                    .as_ref()
                    .is_some_and(|slots| !slots.is_empty())
            })
            .map(|account| addr_hex(&account.address))
            .collect();
        block_file.storage_contracts.sort();
    }
    block_file
}

/// `GenesisAllocToStateDiff`: every account of the alloc is new. The archive
/// cannot list a contract's slots without their original keys.
fn genesis_state_diff(
    accounts: &[AllocAccount],
    block: &RpcBlock,
    deduplicate_code: bool,
) -> BlockStorageDiff {
    let mut new_accounts = BTreeMap::new();
    let mut new_codes = Vec::new();
    let mut storage_diffs = BTreeMap::new();
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
            new_codes.push(NewCode {
                code_hash,
                code: account.code.clone(),
            });
        }
        if let Some(diffs) = &account.storage {
            storage_diffs.insert(
                hashed,
                AccountStorageDiff {
                    address: hashed,
                    diffs: diffs.clone(),
                },
            );
        }
    }
    new_codes.sort_by_key(|code| code.code_hash);
    if deduplicate_code {
        new_codes.dedup_by_key(|code| code.code_hash);
    }
    BlockStorageDiff {
        hash: block.header.state_root,
        // The genesis block has no parent.
        parent_hash: alloy::consensus::constants::EMPTY_ROOT_HASH,
        new_accounts: new_accounts.into_values().collect(),
        storage_diffs: storage_diffs.into_values().collect(),
        new_codes,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    fn options(traces: bool) -> ReplayOptions {
        ReplayOptions {
            strict: true,
            check_state: false,
            state_diff: true,
            traces,
        }
    }

    #[test]
    fn storage_keys_and_code_deduplication_preserve_chain_formats() {
        let addresses = [Address::repeat_byte(1), Address::repeat_byte(2)];
        let code = Bytecode::new_raw(Bytes::from_static(&[0]));
        let mut db = InMemoryDB::default();
        for address in addresses {
            db.insert_account_info(
                address,
                AccountInfo {
                    code_hash: code.hash_slow(),
                    code: Some(code.clone()),
                    ..Default::default()
                },
            );
        }
        let slot = B256::repeat_byte(3);
        db.insert_account_storage(addresses[0], U256::from_be_bytes(slot.0), U256::from(7))
            .unwrap();
        let keys = BTreeMap::from([(addresses[0], vec![slot])]);
        let block = RpcBlock::default();
        let arc = genesis_output(
            &addresses,
            &db,
            &block,
            options(true),
            GenesisOptions {
                storage_keys: Some(&keys),
                strict: true,
                deduplicate_code: false,
            },
        )
        .unwrap();
        let diff: BlockStorageDiff =
            alloy::rlp::decode_exact(arc.state_diff.unwrap().as_ref()).unwrap();
        assert_eq!(diff.new_codes.len(), 2);
        assert_eq!(diff.storage_diffs.len(), 1);
        assert_eq!(diff.storage_diffs[0].address, keccak256(addresses[0]));
        assert_eq!(diff.storage_diffs[0].diffs[0].index, keccak256(slot));
        assert_eq!(diff.storage_diffs[0].diffs[0].value, U256::from(7));
        assert_eq!(
            arc.block_file.storage_contracts,
            vec![addr_hex(&addresses[0])]
        );

        let legacy = genesis_output(
            &addresses,
            &db,
            &block,
            options(true),
            GenesisOptions::default(),
        )
        .unwrap();
        let diff: BlockStorageDiff =
            alloy::rlp::decode_exact(legacy.state_diff.unwrap().as_ref()).unwrap();
        assert_eq!(diff.new_codes.len(), 1);
        assert_eq!(diff.storage_diffs.len(), 2);
        assert!(diff
            .storage_diffs
            .iter()
            .all(|entry| entry.diffs.is_empty()));
        assert!(legacy.block_file.storage_contracts.is_empty());
    }

    #[test]
    fn strict_genesis_rejects_missing_accounts_and_wrong_code() {
        let address = Address::repeat_byte(1);
        let mut db = InMemoryDB::default();
        let block = RpcBlock::default();
        let strict = || GenesisOptions {
            strict: true,
            ..Default::default()
        };
        let missing = genesis_output(&[address], &db, &block, options(true), strict())
            .err()
            .unwrap();
        assert_eq!(
            missing.to_string(),
            format!("genesis account {address} missing from archive")
        );
        assert!(genesis_output(
            &[address],
            &db,
            &block,
            options(true),
            GenesisOptions::default()
        )
        .is_ok());

        db.insert_account_info(
            address,
            AccountInfo {
                code_hash: B256::repeat_byte(9),
                code: Some(Bytecode::new_raw(Bytes::from_static(&[0]))),
                ..Default::default()
            },
        );
        let wrong_code = genesis_output(&[address], &db, &block, options(true), strict())
            .err()
            .unwrap();
        assert_eq!(
            wrong_code.to_string(),
            format!("genesis code hash mismatch for {address}")
        );
    }

    #[test]
    fn shared_genesis_respects_traces_without_changing_transactions_or_state() {
        let address = Address::repeat_byte(1);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            address,
            AccountInfo {
                balance: U256::from(7),
                ..Default::default()
            },
        );
        let block = RpcBlock::default();
        let traced = genesis_output(
            &[address],
            &db,
            &block,
            options(true),
            GenesisOptions::default(),
        )
        .unwrap();
        let plain = genesis_output(
            &[address],
            &db,
            &block,
            options(false),
            GenesisOptions::default(),
        )
        .unwrap();
        assert_eq!(traced.block_file.traces.len(), 2);
        assert!(plain.block_file.traces.is_empty());
        assert!(plain.block_file.events.is_empty());
        assert!(plain.block_file.storage_contracts.is_empty());
        assert_eq!(
            serde_json::to_value(traced.block_file.txs).unwrap(),
            serde_json::to_value(plain.block_file.txs).unwrap()
        );
        assert_eq!(traced.state_diff, plain.state_diff);
    }

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
