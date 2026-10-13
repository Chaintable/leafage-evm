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
use alloy::primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy::rpc::types::{Block as RpcBlock, BlockTransactions};
use alloy_evm::{EvmEnv, FromRecoveredTx};
use anyhow::{anyhow, bail, Context as _, Result};
use leafage_evm_chains::bsc::mainnet::{
    bsc_mainnet_fork_transition, bsc_mainnet_spec_at, BscMainnetHardforks, BSC_MAINNET_CHAIN_ID,
};
use leafage_evm_chains::bsc::{BscEvm, BscHardfork, BscHardforks, BscTxEnv};
use leafage_evm_types::{
    AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use revm::bytecode::Bytecode;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::database::CacheDB;
use revm::inspector::NoOpInspector;
use revm::primitives::{hardfork::SpecId, KECCAK_EMPTY};
use revm::state::{AccountInfo, EvmState};
use revm::{Database, DatabaseCommit, DatabaseRef, ExecuteEvm, InspectEvm};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use tracing::warn;

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

/// Every contract a go-bsc hardfork upgrade may replace
/// (`systemcontracts.TryUpdateBuildInSystemContract`).
pub(crate) const UPGRADABLE_CONTRACTS: [Address; 17] = [
    address!("0000000000000000000000000000000000001000"),
    address!("0000000000000000000000000000000000001001"),
    address!("0000000000000000000000000000000000001002"),
    address!("0000000000000000000000000000000000001003"),
    address!("0000000000000000000000000000000000001004"),
    address!("0000000000000000000000000000000000001005"),
    address!("0000000000000000000000000000000000001006"),
    address!("0000000000000000000000000000000000001007"),
    address!("0000000000000000000000000000000000001008"),
    address!("0000000000000000000000000000000000002000"),
    address!("0000000000000000000000000000000000002001"),
    address!("0000000000000000000000000000000000002002"),
    address!("0000000000000000000000000000000000002003"),
    address!("0000000000000000000000000000000000002004"),
    address!("0000000000000000000000000000000000002005"),
    address!("0000000000000000000000000000000000002006"),
    address!("0000000000000000000000000000000000003000"),
];

/// Mismatches listed in a verification error before it is cut short.
const MAX_REPORTED_MISMATCHES: usize = 20;

/// One block of a [`replay_range`].
pub(crate) struct RangeBlock<D> {
    /// State after the block, used for upgrades and checks.
    pub post_state: D,
    pub parent_timestamp: u64,
    pub parent_state_root: B256,
    pub block: RpcBlock,
}

/// Replay consecutive blocks on top of `parent_state`, the state before the
/// first one. The state the replay builds carries over from one block to the
/// next, so a key is read from `parent_state` only the first time the range
/// touches it: until then no block of the range changed it, so its value
/// before the first block is still current.
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
    let check = |result: Result<()>| match result {
        Err(err) if !options.strict => {
            warn!("ignoring failed check: {err:#}");
            Ok(())
        }
        result => result,
    };
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
        let diff =
            replay
                .changes
                .to_storage_diff(&replay.db, header.state_root, parent_state_root)?;
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
    for address in UPGRADABLE_CONTRACTS {
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
        let (result, state) = (outcome.result, outcome.state);
        self.record(&state)?;
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

    /// The change entry of `address`, created with the value the account
    /// had before the block. Called before the first write to it, so the
    /// read hits the account the execution already cached.
    fn track(&mut self, address: Address) -> Result<&mut AccountChange> {
        if !self.changes.accounts.contains_key(&address) {
            let original = self.db.basic(address)?.filter(|info| !is_empty(info));
            self.changes.accounts.insert(
                address,
                AccountChange {
                    info: original.clone(),
                    original,
                    storage: HashMap::new(),
                },
            );
        }
        Ok(self
            .changes
            .accounts
            .get_mut(&address)
            .expect("inserted above"))
    }

    /// Fold a transaction's state changes into the block's, before they are
    /// committed to `db`.
    fn record(&mut self, state: &EvmState) -> Result<()> {
        if !self.track {
            return Ok(());
        }
        for (address, account) in state {
            if !account.is_touched() {
                continue;
            }
            let change = self.track(*address)?;
            if account.is_selfdestructed() || account.is_created() {
                for slot in change.storage.values_mut() {
                    slot.present = U256::ZERO;
                }
            }
            if account.is_selfdestructed() {
                change.info = None;
                continue;
            }
            change.info = Some(account.info.clone());
            for (key, value) in &account.storage {
                if value.is_changed() {
                    change
                        .storage
                        .entry(*key)
                        .or_insert(SlotChange {
                            original: value.original_value(),
                            present: U256::ZERO,
                        })
                        .present = value.present_value();
                }
            }
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
        self.changes
            .accounts
            .get_mut(&address)
            .expect("tracked")
            .info = Some(info);
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
        change
            .storage
            .entry(slot)
            .or_insert(SlotChange {
                original,
                present: U256::ZERO,
            })
            .present = value;
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

/// Accounts and slots written by the replay, with their values before and
/// after the block.
#[derive(Default)]
struct ChangeSet {
    accounts: HashMap<Address, AccountChange>,
}

struct AccountChange {
    /// Before the block; `None` if the account did not exist (or was empty).
    original: Option<AccountInfo>,
    /// `None` once the account was destroyed.
    info: Option<AccountInfo>,
    storage: HashMap<U256, SlotChange>,
}

struct SlotChange {
    original: U256,
    present: U256,
}

impl ChangeSet {
    /// Compare every changed account and slot with the archive state of the
    /// block.
    fn verify<D>(&self, post: &D) -> Result<()>
    where
        D: DatabaseRef,
        D::Error: Send + Sync + 'static,
    {
        let mut mismatches = Vec::new();
        let mut addresses: Vec<_> = self.accounts.keys().collect();
        addresses.sort();
        for address in addresses {
            let change = &self.accounts[address];
            let ours = change.info.as_ref().filter(|info| !is_empty(info));
            let stored = post.basic_ref(*address)?;
            let stored = stored.as_ref().filter(|info| !is_empty(info));
            match (ours, stored) {
                (None, None) => {}
                (Some(ours), Some(stored))
                    if ours.balance == stored.balance
                        && ours.nonce == stored.nonce
                        && normalize_code_hash(ours.code_hash)
                            == normalize_code_hash(stored.code_hash) => {}
                (ours, stored) => mismatches.push(format!(
                    "account {address}: replay {}, archive {}",
                    describe(ours),
                    describe(stored)
                )),
            }
            if ours.is_none() {
                continue;
            }
            let mut slots: Vec<_> = change
                .storage
                .iter()
                .map(|(slot, value)| (slot, &value.present))
                .collect();
            slots.sort();
            for (slot, value) in slots {
                let stored = post.storage_ref(*address, *slot)?;
                if stored != *value {
                    mismatches.push(format!(
                        "storage {address}[{slot:#x}]: replay {value:#x}, archive {stored:#x}"
                    ));
                }
            }
        }
        if mismatches.is_empty() {
            return Ok(());
        }
        let total = mismatches.len();
        mismatches.truncate(MAX_REPORTED_MISMATCHES);
        bail!(
            "replayed state differs from the archive in {total} places: {}",
            mismatches.join("; ")
        )
    }

    /// Best-effort `BlockStorageDiff` in the leafage / pipeline encoding.
    fn to_storage_diff<D>(
        &self,
        db: &CacheDB<D>,
        state_root: B256,
        parent_state_root: B256,
    ) -> Result<BlockStorageDiff>
    where
        D: DatabaseRef,
        D::Error: Send + Sync + 'static,
    {
        let mut new_accounts = BTreeMap::new();
        let mut deleted_accounts = BTreeSet::new();
        let mut storage_diffs = BTreeMap::new();
        let mut new_codes = BTreeMap::new();
        // Net changes against the state before the block, from the values
        // recorded during execution (no reads of the parent state).
        for (address, change) in &self.accounts {
            let hashed = keccak256(address);
            let before = change.original.as_ref();
            let Some(info) = change.info.as_ref().filter(|info| !is_empty(info)) else {
                if before.is_some() {
                    deleted_accounts.insert(hashed);
                }
                continue;
            };
            let unchanged = before.is_some_and(|before| {
                before.balance == info.balance
                    && before.nonce == info.nonce
                    && normalize_code_hash(before.code_hash) == normalize_code_hash(info.code_hash)
            });
            if !unchanged {
                new_accounts.insert(
                    hashed,
                    NewAccount {
                        address: hashed,
                        balance: info.balance,
                        nonce: info.nonce,
                        code_hash: info.code_hash,
                    },
                );
            }
            let code_hash = normalize_code_hash(info.code_hash);
            let code_before = before.map(|before| normalize_code_hash(before.code_hash));
            if code_hash != KECCAK_EMPTY && code_before != Some(code_hash) {
                let code = db.code_by_hash_ref(info.code_hash)?;
                new_codes.insert(
                    code_hash,
                    NewCode {
                        code_hash,
                        code: code.original_bytes(),
                    },
                );
            }
            let mut diffs: Vec<_> = change
                .storage
                .iter()
                .filter(|(_, slot)| slot.present != slot.original)
                .map(|(key, slot)| IndexValuePair {
                    index: keccak256(key.to_be_bytes::<32>()),
                    value: slot.present,
                })
                .collect();
            if !diffs.is_empty() {
                diffs.sort_by_key(|pair| pair.index);
                storage_diffs.insert(
                    hashed,
                    AccountStorageDiff {
                        address: hashed,
                        diffs,
                    },
                );
            }
        }
        Ok(BlockStorageDiff {
            hash: state_root,
            parent_hash: parent_state_root,
            new_accounts: new_accounts.into_values().collect(),
            deleted_accounts: deleted_accounts.into_iter().collect(),
            storage_diffs: storage_diffs.into_values().collect(),
            new_codes: new_codes.into_values().collect(),
        })
    }
}

/// EIP-161 empty: such accounts are removed from the trie.
fn is_empty(info: &AccountInfo) -> bool {
    info.balance.is_zero() && info.nonce == 0 && normalize_code_hash(info.code_hash) == KECCAK_EMPTY
}

fn normalize_code_hash(code_hash: B256) -> B256 {
    if code_hash.is_zero() {
        KECCAK_EMPTY
    } else {
        code_hash
    }
}

fn describe(info: Option<&AccountInfo>) -> String {
    match info {
        None => "absent".to_string(),
        Some(info) => format!(
            "balance {} nonce {} code {}",
            info.balance, info.nonce, info.code_hash
        ),
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
