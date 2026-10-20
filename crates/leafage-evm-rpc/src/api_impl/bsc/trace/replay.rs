//! Re-execute a historical BSC mainnet block on top of the archive state of
//! its parent, collecting pipeline traces and events per transaction.
//!
//! The order of state changes mirrors go-bsc `StateProcessor.Process` and
//! `Parlia.Finalize`:
//! 1. block begin: built-in system contract upgrades (before Feynman), the
//!    EIP-2935 history contract on the Prague transition, then the parent
//!    hash write into it once Prague is active;
//! 2. ordinary transactions (with the HertzFix storage patches);
//! 3. finalize: system contract upgrades (from Feynman on), the fees
//!    collected on `0xff..fe` moved to the coinbase, then the system
//!    transactions in block order.
//!
//! Upgraded contract code is taken from the archive state of the block itself
//! instead of embedding go-bsc's bytecode: a system contract only changes code
//! through an upgrade.
//!
//! The result is checked before it is returned: gas used and receipts root
//! must match the header, and every account and slot the replay changed must
//! equal the archive state of the block.

use super::patches::{self, StoragePatch};
use crate::pipeline::build::{
    pipeline_block, pipeline_header, pipeline_transaction, receipt_envelope,
};
use crate::pipeline::chain::{ReplayOptions, ReplayOutput, ReplayTimings};
use crate::pipeline::changes::{
    check, delete_touched_empty, normalize_code_hash, AccountChange, ChangeSet, StorageSlots,
};
use crate::pipeline::convert::append_tx_traces;
use crate::pipeline::inspector::PipelineInspector;
use crate::pipeline::types::{addr_hex, BlockFile};
use alloy::consensus::{
    proofs::calculate_receipt_root, ReceiptEnvelope, Transaction as _, TxEnvelope,
};
use alloy::eips::{
    eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS},
    eip7840::BlobParams,
};
use alloy::primitives::{address, Address, Bytes, B256, U256};
use alloy::rpc::types::{Block as RpcBlock, BlockTransactions};
use alloy_evm::{EvmEnv, FromRecoveredTx};
use anyhow::{anyhow, bail, Context as _, Result};
use leafage_evm_chains::bsc::mainnet::{
    bsc_mainnet_fork_transition, bsc_mainnet_spec_at, BscMainnetHardforks, BSC_MAINNET_CHAIN_ID,
    BSC_MAINNET_UPGRADABLE_CONTRACTS,
};
use leafage_evm_chains::bsc::{BscEvm, BscHardfork, BscHardforks, BscTxEnv};
use revm::bytecode::Bytecode;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::database::CacheDB;
use revm::inspector::NoOpInspector;
use revm::primitives::{hardfork::SpecId, KECCAK_EMPTY};
use revm::state::{AccountInfo, EvmState};
use revm::{Database, DatabaseCommit, DatabaseRef, ExecuteEvm, InspectEvm};
use std::collections::BTreeSet;
use std::fmt::Debug;

/// go-bsc `consensus.SystemAddress`: collects the fees of a block until
/// `distributeIncoming` hands them to the coinbase.
const SYSTEM_ADDRESS: Address = address!("fffffffffffffffffffffffffffffffffffffffe");

/// Targets that make a zero-price transaction from the coinbase a system
/// transaction (go-bsc `parlia.systemContracts`).
const SYSTEM_TX_CONTRACTS: [Address; 14] = [
    address!("0000000000000000000000000000000000001000"),
    address!("0000000000000000000000000000000000001001"),
    address!("0000000000000000000000000000000000001002"),
    address!("0000000000000000000000000000000000001003"),
    address!("0000000000000000000000000000000000001006"),
    address!("0000000000000000000000000000000000001007"),
    address!("0000000000000000000000000000000000001004"),
    address!("0000000000000000000000000000000000001005"),
    address!("0000000000000000000000000000000000002000"),
    address!("0000000000000000000000000000000000002002"),
    address!("0000000000000000000000000000000000002004"),
    address!("0000000000000000000000000000000000002005"),
    address!("0000000000000000000000000000000000002006"),
    address!("0000000000000000000000000000000000003000"),
];

/// One block of a [`replay_range`].
pub(crate) struct RangeBlock<D> {
    /// State after the block, used for upgrades and checks.
    pub post_state: D,
    /// State before the block, for the storage of the accounts it destroys.
    pub parent_state: Option<Box<dyn StorageSlots + Send>>,
    pub parent_timestamp: u64,
    pub parent_state_root: B256,
    pub block: RpcBlock,
}

/// Replay consecutive blocks on top of `parent_state`, the state before the
/// first one. The state the replay builds carries over from one block to the
/// next, so a key is read from `parent_state` only the first time the range
/// touches it: until then no block of the range changed it, so its value
/// before the first block is still current.
#[cfg(test)]
pub(crate) fn replay_range<D>(
    parent_state: D,
    blocks: Vec<RangeBlock<D>>,
    options: ReplayOptions,
) -> Result<Vec<ReplayOutput>>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    let mut replayer = RangeReplayer::new(parent_state, options);
    blocks
        .into_iter()
        .map(|block| replayer.replay(block))
        .collect()
}

/// [`replay_range`] fed one block at a time.
pub(crate) struct RangeReplayer<D: DatabaseRef> {
    /// `None` only while a block is being replayed.
    db: Option<CacheDB<D>>,
    options: ReplayOptions,
}

impl<D> RangeReplayer<D>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    pub(crate) fn new(parent_state: D, options: ReplayOptions) -> Self {
        Self {
            db: Some(CacheDB::new(parent_state)),
            options,
        }
    }

    /// Replay the block after the last one replayed (or after
    /// `parent_state`).
    pub(crate) fn replay(&mut self, block: RangeBlock<D>) -> Result<ReplayOutput> {
        let db = self.db.take().context("an earlier replay failed")?;
        let (output, db) = replay_one(db, block, self.options)?;
        self.db = Some(db);
        Ok(output)
    }

    /// Continue on `state`, the state after the last block replayed, instead
    /// of the state built so far, dropping the cached accounts and slots.
    /// Contract code, immutable for its hash, is kept.
    pub(crate) fn rebase(&mut self, state: D) {
        let mut db = CacheDB::new(state);
        if let Some(old) = self.db.take() {
            db.cache.contracts = old.cache.contracts;
        }
        self.db = Some(db);
    }
}

/// Replay one block on `db`, the state after its parent, and hand `db`
/// back with the block applied.
fn replay_one<D>(
    db: CacheDB<D>,
    input: RangeBlock<D>,
    options: ReplayOptions,
) -> Result<(ReplayOutput, CacheDB<D>)>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    let RangeBlock {
        post_state,
        parent_state,
        parent_timestamp,
        parent_state_root,
        block,
    } = input;
    let header = &block.header;
    let number = header.number;
    let timestamp = header.timestamp;
    let BlockTransactions::Full(transactions) = &block.transactions else {
        bail!("block {number} was returned without transaction bodies");
    };

    let forks = BscMainnetHardforks;
    let spec = bsc_mainnet_spec_at(number, timestamp);
    let feynman_at_parent = forks.is_feynman_active_at_timestamp(number, parent_timestamp);
    let prague = forks.is_pascal_active_at_timestamp(number, timestamp);
    let prague_transition =
        prague && !forks.is_pascal_active_at_timestamp(number, parent_timestamp);

    let post = post_state;
    let started = std::time::Instant::now();
    let mut timings = ReplayTimings::default();
    let mut replay = Replayer {
        env: evm_env(header, spec),
        db,
        track: options.check_state || options.state_diff,
        traces: options.traces,
        changes: ChangeSet::default(),
        log_index: 0,
        gas_used: 0,
        receipts: Vec::new(),
        storage_contracts: BTreeSet::new(),
        block_file: BlockFile {
            block: pipeline_block(header),
            ..Default::default()
        },
    };

    // 1. block begin
    let upgrades = if bsc_mainnet_fork_transition(number, parent_timestamp, timestamp) {
        changed_system_code(&replay.db, &post)?
    } else {
        Vec::new()
    };
    if !feynman_at_parent {
        replay.apply_code(&upgrades)?;
    }
    if prague_transition {
        let history = post
            .basic_ref(HISTORY_STORAGE_ADDRESS)?
            .context("history storage contract missing after the Prague transition")?;
        let code = post.code_by_hash_ref(history.code_hash)?;
        replay.set_code(HISTORY_STORAGE_ADDRESS, code, Some(history.nonce))?;
    }
    if prague {
        // EIP-2935 system call: the contract stores the parent hash at
        // `(number - 1) % HISTORY_SERVE_WINDOW`.
        let slot = U256::from((number - 1) % HISTORY_SERVE_WINDOW as u64);
        replay.set_storage(
            HISTORY_STORAGE_ADDRESS,
            slot,
            U256::from_be_bytes(header.parent_hash.0),
        )?;
    }

    // 2. ordinary transactions
    let coinbase = header.beneficiary;
    let mut system_txs = Vec::new();
    for (index, tx) in transactions.iter().enumerate() {
        let sender = tx.inner.signer();
        let envelope = tx.inner.inner();
        if is_system_transaction(envelope, sender, coinbase) {
            system_txs.push((envelope, sender));
            continue;
        }
        let tx_hash = *envelope.tx_hash();
        if let Some(patch) = patches::find(patches::PATCHES_BEFORE_TX, &tx_hash) {
            replay.apply_patch(patch)?;
        }
        let tx_env = BscTxEnv::new(TxEnv::from_recovered_tx(envelope, sender));
        let gas_price = revm::context::Transaction::effective_gas_price(
            &tx_env,
            header.base_fee_per_gas.unwrap_or_default() as u128,
        );
        replay.execute(envelope, sender, tx_env, gas_price, index)?;
        if let Some(patch) = patches::find(patches::PATCHES_AFTER_TX, &tx_hash) {
            replay.apply_patch(patch)?;
        }
    }

    // 3. finalize
    if feynman_at_parent {
        replay.apply_code(&upgrades)?;
    }
    replay.distribute_incoming(coinbase)?;
    for (envelope, sender) in system_txs {
        let to = envelope
            .to()
            .context("system transaction without recipient")?;
        let tx_env = BscTxEnv {
            base: TxEnv {
                tx_type: 0,
                caller: sender,
                gas_limit: envelope.gas_limit(),
                gas_price: 0,
                kind: revm::primitives::TxKind::Call(to),
                value: envelope.value(),
                data: envelope.input().clone(),
                nonce: envelope.nonce(),
                chain_id: envelope.chain_id(),
                ..Default::default()
            },
            is_system_transaction: true,
        };
        let index = replay.receipts.len();
        replay.execute(envelope, sender, tx_env, 0, index)?;
    }

    timings.execute = started.elapsed();
    let started = std::time::Instant::now();
    // checks
    let check = |result: Result<()>| check(options.strict, result);
    check(if replay.gas_used != header.gas_used {
        Err(anyhow!(
            "block {number}: replay used {} gas, header says {}",
            replay.gas_used,
            header.gas_used
        ))
    } else {
        Ok(())
    })?;
    let receipts_root = calculate_receipt_root(&replay.receipts);
    check(if receipts_root != header.receipts_root {
        Err(anyhow!(
            "block {number}: replay receipts root {receipts_root}, header says {}",
            header.receipts_root
        ))
    } else {
        Ok(())
    })?;
    if options.check_state {
        check(replay.changes.verify(&post))?;
    }

    timings.verify = started.elapsed();
    let started = std::time::Instant::now();
    let state_diff = if !options.state_diff {
        None
    } else if header.state_root == parent_state_root {
        Some(Bytes::new())
    } else {
        let parent_state = parent_state
            .as_deref()
            .map(|state| state as &dyn StorageSlots);
        let diff = replay.changes.to_storage_diff(
            &replay.db,
            parent_state,
            header.state_root,
            parent_state_root,
        )?;
        Some(alloy::rlp::encode(diff).into())
    };

    let mut block_file = replay.block_file;
    block_file.storage_contracts = replay.storage_contracts.iter().map(addr_hex).collect();
    timings.diff = started.elapsed();
    let output = ReplayOutput {
        block_file,
        header: pipeline_header(&block),
        state_diff,
        timings,
    };
    Ok((output, replay.db))
}

fn evm_env(header: &alloy::rpc::types::Header, spec: BscHardfork) -> EvmEnv<BscHardfork> {
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.chain_id = BSC_MAINNET_CHAIN_ID;
    let post_merge = SpecId::from(spec).is_enabled_in(SpecId::MERGE);
    let cancun = BscMainnetHardforks.is_cancun_active_at_timestamp(header.number, header.timestamp);
    let block = BlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        gas_limit: header.gas_limit,
        basefee: header.base_fee_per_gas.unwrap_or_default(),
        // BSC keeps DIFFICULTY semantics after the merge specs, so feed the
        // difficulty where revm reads prevrandao (as reth-bsc does).
        difficulty: if post_merge {
            U256::ZERO
        } else {
            header.difficulty
        },
        prevrandao: post_merge.then(|| header.difficulty.into()),
        blob_excess_gas_and_price: cancun.then(|| {
            let excess_blob_gas = header.excess_blob_gas.unwrap_or_default();
            BlobExcessGasAndPrice {
                excess_blob_gas,
                blob_gasprice: BlobParams::cancun().calc_blob_fee(excess_blob_gas),
            }
        }),
        slot_num: 0,
    };
    EvmEnv::new(cfg, block)
}

/// go-bsc `Parlia.IsSystemTransaction`.
fn is_system_transaction(tx: &TxEnvelope, sender: Address, coinbase: Address) -> bool {
    sender == coinbase
        && tx.max_fee_per_gas() == 0
        && tx.to().is_some_and(|to| SYSTEM_TX_CONTRACTS.contains(&to))
}

/// System contracts whose code differs between the parent and the block.
fn changed_system_code<P, D>(parent: &P, post: &D) -> Result<Vec<(Address, Bytecode)>>
where
    P: DatabaseRef<Error = D::Error>,
    D: DatabaseRef,
    D::Error: Send + Sync + 'static,
{
    let mut changed = Vec::new();
    for address in BSC_MAINNET_UPGRADABLE_CONTRACTS {
        let before = parent.basic_ref(address)?.map(|info| info.code_hash);
        let Some(after) = post.basic_ref(address)? else {
            continue;
        };
        if normalize_code_hash(before.unwrap_or(KECCAK_EMPTY))
            == normalize_code_hash(after.code_hash)
        {
            continue;
        }
        changed.push((address, post.code_by_hash_ref(after.code_hash)?));
    }
    Ok(changed)
}

struct Replayer<D: DatabaseRef> {
    env: EvmEnv<BscHardfork>,
    db: CacheDB<D>,
    /// Record `changes`; only the state check and the state diff use them.
    track: bool,
    /// Run the transactions under [`PipelineInspector`] for the block file's
    /// traces and events.
    ///
    traces: bool,
    changes: ChangeSet,
    /// Block-wide index of the next surviving log.
    log_index: u64,
    gas_used: u64,
    receipts: Vec<ReceiptEnvelope>,
    storage_contracts: BTreeSet<Address>,
    block_file: BlockFile,
}

impl<D> Replayer<D>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    fn execute(
        &mut self,
        envelope: &TxEnvelope,
        sender: Address,
        tx_env: BscTxEnv,
        gas_price: u128,
        index: usize,
    ) -> Result<()> {
        let tx_hash = *envelope.tx_hash();
        let failed = |err| anyhow!("transaction {tx_hash} failed to execute: {err:?}");
        let (outcome, traces) = if self.traces {
            let mut inspector = PipelineInspector::new();
            let outcome = BscEvm::new(self.env.clone(), &mut self.db, &mut inspector, true)
                .inspect_tx(tx_env)
                .map_err(failed)?;
            (outcome, Some(inspector.into_traces()))
        } else {
            let outcome = BscEvm::new(self.env.clone(), &mut self.db, NoOpInspector, false)
                .transact(tx_env)
                .map_err(failed)?;
            (outcome, None)
        };
        let (result, mut state) = (outcome.result, outcome.state);
        self.record(&state)?;
        delete_touched_empty(&mut state);
        self.db.commit(state);

        if let Some((arena, sstore)) = traces {
            append_tx_traces(
                &tx_hash,
                &arena,
                &sstore,
                &mut self.log_index,
                &mut self.storage_contracts,
                &mut self.block_file,
            );
        }

        let gas_used = result.tx_gas_used();
        self.gas_used += gas_used;
        self.block_file.txs.push(pipeline_transaction(
            envelope, sender, &result, gas_price, index,
        ));
        self.receipts
            .push(receipt_envelope(envelope.tx_type(), &result, self.gas_used));
        Ok(())
    }

    fn apply_code(&mut self, upgrades: &[(Address, Bytecode)]) -> Result<()> {
        for (address, code) in upgrades {
            self.set_code(*address, code.clone(), None)?;
        }
        Ok(())
    }

    fn set_code(&mut self, address: Address, code: Bytecode, nonce: Option<u64>) -> Result<()> {
        let mut info = self.db.load_account(address)?.info().unwrap_or_default();
        info.code_hash = code.hash_slow();
        info.code = Some(code);
        if let Some(nonce) = nonce {
            info.nonce = nonce;
        }
        self.set_info(address, info)
    }

    fn track(&mut self, address: Address) -> Result<&mut AccountChange> {
        Ok(self.changes.track(&mut self.db, address)?)
    }

    /// Fold a transaction's state changes into the block's, before they are
    /// committed to `db`.
    fn record(&mut self, state: &EvmState) -> Result<()> {
        if self.track {
            self.changes.record(&mut self.db, state)?;
        }
        Ok(())
    }

    fn set_info(&mut self, address: Address, info: AccountInfo) -> Result<()> {
        if !self.track {
            self.db.insert_account_info(address, info);
            return Ok(());
        }
        self.track(address)?;
        self.db.insert_account_info(address, info.clone());
        self.track(address)?.info = Some(info);
        Ok(())
    }

    fn set_storage(&mut self, address: Address, slot: U256, value: U256) -> Result<()> {
        if self.db.load_account(address)?.info().is_none() {
            self.set_info(address, AccountInfo::default())?;
        }
        if !self.track {
            self.db.insert_account_storage(address, slot, value)?;
            return Ok(());
        }
        let original = self.db.storage(address, slot)?;
        self.db.insert_account_storage(address, slot, value)?;
        let info = self.db.load_account(address)?.info();
        let change = self.track(address)?;
        if change.info.is_none() {
            change.info = info;
        }
        ChangeSet::set_slot(change, slot, original, value);
        Ok(())
    }

    fn apply_patch(&mut self, patch: &StoragePatch) -> Result<()> {
        for (slot, value) in patch.slots() {
            self.set_storage(patch.address, slot, value)?;
        }
        Ok(())
    }

    /// The balance part of go-bsc `distributeIncoming`. Moving all of it
    /// before the first system transaction is equivalent: nothing executed
    /// in between reads either balance.
    fn distribute_incoming(&mut self, coinbase: Address) -> Result<()> {
        let Some(mut system) = self.db.load_account(SYSTEM_ADDRESS)?.info() else {
            return Ok(());
        };
        if system.balance.is_zero() {
            return Ok(());
        }
        let fees = std::mem::take(&mut system.balance);
        self.set_info(SYSTEM_ADDRESS, system)?;
        let mut validator = self.db.load_account(coinbase)?.info().unwrap_or_default();
        validator.balance += fees;
        self.set_info(coinbase, validator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_transaction_needs_coinbase_zero_price_and_system_target() {
        use alloy::consensus::{Signed, TxLegacy};
        use alloy::primitives::Signature;
        let coinbase = Address::with_last_byte(7);
        let tx = |to: Address, gas_price: u128| {
            TxEnvelope::Legacy(Signed::new_unhashed(
                TxLegacy {
                    to: to.into(),
                    gas_price,
                    ..Default::default()
                },
                Signature::test_signature(),
            ))
        };
        let validator = SYSTEM_TX_CONTRACTS[0];
        assert!(is_system_transaction(&tx(validator, 0), coinbase, coinbase));
        assert!(!is_system_transaction(
            &tx(validator, 1),
            coinbase,
            coinbase
        ));
        assert!(!is_system_transaction(
            &tx(validator, 0),
            Address::ZERO,
            coinbase
        ));
        // TokenManager is upgradable but not a system transaction target.
        let token_manager = address!("0000000000000000000000000000000000001008");
        assert!(!is_system_transaction(
            &tx(token_manager, 0),
            coinbase,
            coinbase
        ));
    }
}
