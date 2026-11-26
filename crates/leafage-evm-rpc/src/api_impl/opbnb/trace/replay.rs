//! Re-execute historical opBNB mainnet blocks on the archive state of their
//! parent, collecting the block files the opBNB writer's live tracer makes.
//!
//! The order of state changes mirrors op-geth's `StateProcessor.Process`
//! (`bnb-chain/op-geth` v0.5.9, Regolith from genesis):
//! 1. the create2 deployer code in the Canyon activation block, then the
//!    EIP-4788 beacon root write from Cancun on;
//! 2. the transactions, deposits first as the block orders them, under
//!    op-revm with the opBNB precompiles of the block.
//!
//! The opBNB differences from op-revm's OP rules are applied here: the
//! precompiles ([`OpBnbPrecompiles`]), and from Wright on a transaction with
//! a zero effective gas price pays no L1 data fee, which op-revm would charge
//! (its L1 parameters are zeroed for that transaction). op-revm also reads a
//! zero L1 fee scalar after Ecotone while the scalars are still empty, where
//! op-geth's Bedrock fallback reads slot 6: the replay loads the block's L1
//! info itself and fixes that.
//!
//! The block file follows the opBNB writer (see [`TraceStyle::OpBnb`] and
//! [`opbnb_pipeline_transaction`]): the gas price folds in the L1 fee spread
//! over the gas used (`core.pipelineGasPrice`), a deposit's nonce is the
//! sender's nonce before it, and a deposit rejected before it ran gets one
//! `failed deposit: ...` error trace.
//!
//! The result is checked before it is returned: gas used and receipts root
//! must match the header and, optionally, every account and slot the replay
//! changed must equal the archive state of the block. The state diff is made
//! from the replay's state changes, like the BSC and Ethereum replays.

use crate::pipeline::build::{
    op_receipt_envelope, opbnb_pipeline_transaction, pipeline_block, pipeline_header_of,
};
use crate::pipeline::chain::{ReplayOptions, ReplayOutput, ReplayTimings};
use crate::pipeline::changes::{
    check, delete_touched_empty, AccountChange, ChangeSet, StorageSlots,
};
use crate::pipeline::convert::{append_styled_tx_traces, TraceStyle};
use crate::pipeline::inspector::PipelineInspector;
use crate::pipeline::types::{addr_hex, hash_hex, to_hash, BlockFile, PipelineTrace};
use alloy::consensus::{proofs::calculate_receipt_root, Transaction as _, TxEnvelope};
use alloy::eips::eip7840::BlobParams;
use alloy::primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_evm::FromRecoveredTx;
use anyhow::{anyhow, bail, Context as _, Result};
use leafage_evm_chains::opbnb::{
    is_canyon_activation_block, is_gasless, opbnb_mainnet_spec_at, OpBnbPrecompiles,
    OPBNB_MAINNET_CANYON_TIMESTAMP, OPBNB_MAINNET_CHAIN_ID, OPBNB_MAINNET_ECOTONE_TIMESTAMP,
};
use op_alloy_consensus::{OpReceiptEnvelope, OpTxEnvelope};
use op_revm::transaction::deposit::DepositTransactionParts;
use op_revm::{DefaultOp, L1BlockInfo, OpBuilder, OpHaltReason, OpSpecId, OpTransaction};
use revm::bytecode::Bytecode;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::context_interface::result::ExecutionResult;
use revm::database::CacheDB;
use revm::inspector::NoOpInspector;
use revm::primitives::KECCAK_EMPTY;
use revm::state::{AccountInfo, EvmState};
use revm::{Context, Database, DatabaseCommit, DatabaseRef, ExecuteEvm, InspectEvm};
use std::collections::BTreeSet;
use std::fmt::Debug;

/// `predeploys.Create2DeployerAddr`.
const CREATE2_DEPLOYER: Address = address!("13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");
/// EIP-4788 `BEACON_ROOTS_ADDRESS`.
const BEACON_ROOTS: Address = address!("000F3df6D732807Ef1319fB7B8bB8522d0Beac02");
/// EIP-4788 `HISTORY_BUFFER_LENGTH`.
const BEACON_ROOTS_BUFFER: u64 = 8191;
/// `L1Block.l1FeeScalar`, read by op-geth's Bedrock fallback after Ecotone.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");
const L1_SCALAR_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
/// `params.MaxInitCodeSize`.
const MAX_INIT_CODE_SIZE: usize = 49_152;
const DEPOSIT_RECEIPT_VERSION: u64 = 1;

/// One transaction of an [`OpBnbBlock`].
pub(crate) struct OpBnbTx {
    pub envelope: OpTxEnvelope,
    /// The deposit's `from`, or the signer.
    pub sender: Address,
    /// The EIP-2718 encoding, which the L1 data fee is computed from.
    pub encoded: Bytes,
}

/// An opBNB block from the upstream.
pub(crate) struct OpBnbBlock {
    pub header: alloy::rpc::types::Header,
    pub transactions: Vec<OpBnbTx>,
}

/// An opBNB block from its RLP, checked against `hash`, its senders recovered.
pub(crate) fn decode_block(hash: B256, raw: &[u8]) -> Result<OpBnbBlock> {
    use alloy::consensus::transaction::SignerRecoverable;
    use alloy::eips::Encodable2718;
    use alloy::rlp::Decodable;

    let block = alloy::consensus::Block::<OpTxEnvelope>::decode(&mut &raw[..])
        .map_err(|err| anyhow!("decode block {hash}: {err}"))?;
    let decoded = block.header.hash_slow();
    anyhow::ensure!(
        decoded == hash,
        "upstream returned block {decoded} for {hash}"
    );
    let alloy::consensus::Block { header, body } = block;
    let number = header.number;
    let mut transactions = Vec::with_capacity(body.transactions.len());
    for (index, envelope) in body.transactions.into_iter().enumerate() {
        let sender = match &envelope {
            OpTxEnvelope::Deposit(deposit) => deposit.from,
            OpTxEnvelope::PostExec(_) => {
                bail!("block {number} tx {index}: post-exec transactions are not opBNB's")
            }
            _ => envelope
                .clone()
                .try_into_eth_envelope()
                .map_err(|_| anyhow!("block {number} tx {index}: not an Ethereum envelope"))?
                .recover_signer_unchecked()
                .map_err(|err| anyhow!("block {number} tx {index}: {err}"))?,
        };
        let encoded = envelope.encoded_2718().into();
        transactions.push(OpBnbTx {
            envelope,
            sender,
            encoded,
        });
    }
    let header = alloy::rpc::types::Header::from_consensus(
        alloy::consensus::Sealed::new_unchecked(header, hash),
        None,
        Some(U256::from(raw.len())),
    );
    Ok(OpBnbBlock {
        header,
        transactions,
    })
}

/// One block for an [`OpBnbReplayer`].
pub(crate) struct OpBnbReplayBlock<D> {
    /// State after the block, for the state check and the create2 deployer.
    pub post_state: D,
    /// State before the block, for the storage of the accounts it destroys.
    pub parent_state: Option<Box<dyn StorageSlots + Send>>,
    pub parent_state_root: B256,
    pub block: OpBnbBlock,
}

/// Replays consecutive blocks on one state. The state the replay builds
/// carries over from one block to the next, so a key is read from the
/// archive only the first time the replay touches it.
pub(crate) struct OpBnbReplayer<D: DatabaseRef> {
    /// `None` only while a block is being replayed.
    db: Option<CacheDB<D>>,
    options: ReplayOptions,
}

impl<D> OpBnbReplayer<D>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    /// Start after the block whose state is `parent_state`.
    pub(crate) fn new(parent_state: D, options: ReplayOptions) -> Self {
        Self {
            db: Some(CacheDB::new(parent_state)),
            options,
        }
    }

    /// Replay the block after the last one replayed.
    pub(crate) fn replay(&mut self, block: OpBnbReplayBlock<D>) -> Result<ReplayOutput> {
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

fn replay_one<D>(
    db: CacheDB<D>,
    input: OpBnbReplayBlock<D>,
    options: ReplayOptions,
) -> Result<(ReplayOutput, CacheDB<D>)>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    let OpBnbReplayBlock {
        post_state,
        parent_state,
        parent_state_root,
        block,
    } = input;
    let header = &block.header;
    let number = header.number;
    let timestamp = header.timestamp;
    let spec = opbnb_mainnet_spec_at(timestamp);
    let started = std::time::Instant::now();
    let mut timings = ReplayTimings::default();
    let mut replay = Replayer {
        cfg: {
            let mut cfg = CfgEnv::new_with_spec(spec);
            cfg.chain_id = OPBNB_MAINNET_CHAIN_ID;
            cfg
        },
        block_env: block_env(header),
        spec,
        number,
        timestamp,
        db,
        track: options.check_state || options.state_diff,
        traces: options.traces,
        changes: ChangeSet::default(),
        l1_info: None,
        log_index: 0,
        gas_used: 0,
        receipts: Vec::new(),
        storage_contracts: BTreeSet::new(),
        block_file: BlockFile {
            block: pipeline_block(header),
            ..Default::default()
        },
    };

    // 1. before the transactions
    if is_canyon_activation_block(timestamp) {
        let deployer = post_state
            .basic_ref(CREATE2_DEPLOYER)?
            .context("create2 deployer missing after the Canyon activation")?;
        let code = post_state.code_by_hash_ref(deployer.code_hash)?;
        replay.set_code(CREATE2_DEPLOYER, code)?;
    }
    if let Some(root) = header.parent_beacon_block_root {
        replay.beacon_root(root)?;
    }

    // 2. the transactions
    for (index, tx) in block.transactions.iter().enumerate() {
        replay.execute(tx, index)?;
    }

    timings.execute = started.elapsed();
    let started = std::time::Instant::now();
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
        check(replay.changes.verify(&post_state))?;
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
        header: pipeline_header_of(header),
        state_diff,
        timings,
    };
    Ok((output, replay.db))
}

fn block_env(header: &alloy::rpc::types::Header) -> BlockEnv {
    let cancun = header.timestamp >= OPBNB_MAINNET_ECOTONE_TIMESTAMP;
    BlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        gas_limit: header.gas_limit,
        basefee: header.base_fee_per_gas.unwrap_or_default(),
        difficulty: U256::ZERO,
        // op-geth's PREVRANDAO is the raw mix hash, milliseconds included.
        prevrandao: Some(header.mix_hash),
        blob_excess_gas_and_price: cancun.then(|| {
            let excess_blob_gas = header.excess_blob_gas.unwrap_or_default();
            BlobExcessGasAndPrice {
                excess_blob_gas,
                blob_gasprice: BlobParams::cancun().calc_blob_fee(excess_blob_gas),
            }
        }),
        slot_num: 0,
    }
}

/// op-geth's `IntrinsicGas` of a deposit (no access list).
fn intrinsic_gas(data: &[u8], create: bool, shanghai: bool) -> u64 {
    let zeros = data.iter().filter(|byte| **byte == 0).count() as u64;
    let mut gas = if create { 53_000 } else { 21_000 };
    gas += zeros * 4 + (data.len() as u64 - zeros) * 16;
    if create && shanghai {
        gas += 2 * (data.len() as u64).div_ceil(32);
    }
    gas
}

struct Replayer<D: DatabaseRef> {
    cfg: CfgEnv<OpSpecId>,
    block_env: BlockEnv,
    spec: OpSpecId,
    number: u64,
    timestamp: u64,
    db: CacheDB<D>,
    /// Record `changes`; only the state check and the state diff use them.
    track: bool,
    /// Run the transactions under [`PipelineInspector`].
    traces: bool,
    changes: ChangeSet,
    /// The block's L1 info, loaded before its first non-deposit transaction,
    /// after the L1 info deposit wrote it.
    l1_info: Option<L1BlockInfo>,
    /// Block-wide index of the next surviving log.
    log_index: u64,
    gas_used: u64,
    receipts: Vec<OpReceiptEnvelope>,
    storage_contracts: BTreeSet<Address>,
    block_file: BlockFile,
}

impl<D> Replayer<D>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    fn execute(&mut self, tx: &OpBnbTx, index: usize) -> Result<()> {
        let tx_hash = tx.envelope.tx_hash();
        let failed = |err| anyhow!("transaction {tx_hash} failed to execute: {err:?}");
        let canyon = self.timestamp >= OPBNB_MAINNET_CANYON_TIMESTAMP;
        let sender_before = self.db.basic(tx.sender)?.unwrap_or_default();

        let (tx_env, chain, l1_fee) = match &tx.envelope {
            OpTxEnvelope::Deposit(deposit) => {
                let base = TxEnv {
                    tx_type: 0x7E,
                    caller: deposit.from,
                    gas_limit: deposit.gas_limit,
                    gas_price: 0,
                    kind: deposit.to,
                    value: deposit.value,
                    data: deposit.input.clone(),
                    nonce: 0,
                    chain_id: None,
                    ..Default::default()
                };
                let parts = DepositTransactionParts {
                    source_hash: deposit.source_hash,
                    mint: Some(deposit.mint),
                    is_system_transaction: deposit.is_system_transaction,
                };
                let tx_env = OpTransaction {
                    base,
                    enveloped_tx: Some(tx.encoded.clone()),
                    deposit: parts,
                };
                (tx_env, self.l1_info.clone().unwrap_or_default(), 0)
            }
            envelope => {
                let eth: TxEnvelope = envelope
                    .clone()
                    .try_into_eth_envelope()
                    .map_err(|_| anyhow!("transaction {tx_hash} is not an Ethereum envelope"))?;
                let tx_env = OpTransaction {
                    base: TxEnv::from_recovered_tx(&eth, tx.sender),
                    enveloped_tx: Some(tx.encoded.clone()),
                    deposit: Default::default(),
                };
                let l1_info = self.l1_info()?;
                let price = revm::context::Transaction::effective_gas_price(
                    &tx_env.base,
                    self.block_env.basefee as u128,
                );
                if is_gasless(self.timestamp, price) {
                    let zero = L1BlockInfo {
                        l2_block: Some(U256::from(self.number)),
                        ..Default::default()
                    };
                    (tx_env, zero, 0)
                } else {
                    let mut info = l1_info.clone();
                    let fee = info.calculate_tx_l1_cost(&tx.encoded, self.spec);
                    (tx_env, l1_info, u128::try_from(fee).unwrap_or(u128::MAX))
                }
            }
        };
        let gas_limit = tx_env.base.gas_limit;
        let effective_price = revm::context::Transaction::effective_gas_price(
            &tx_env.base,
            self.block_env.basefee as u128,
        );

        let ctx = Context::op()
            .with_block(self.block_env.clone())
            .with_cfg(self.cfg.clone())
            .with_chain(chain)
            .with_db(&mut self.db);
        let precompiles = OpBnbPrecompiles::new(self.number, self.timestamp);
        let (outcome, traces) = if self.traces {
            let mut inspector = PipelineInspector::new();
            let outcome = ctx
                .build_op_with_inspector(&mut inspector)
                .with_precompiles(precompiles)
                .inspect_tx(tx_env)
                .map_err(failed)?;
            (outcome, Some(inspector.into_traces()))
        } else {
            let outcome = ctx
                .build_op_with_inspector(NoOpInspector)
                .with_precompiles(precompiles)
                .transact(tx_env)
                .map_err(failed)?;
            (outcome, None)
        };
        let (result, mut state) = (outcome.result, outcome.state);
        self.record(&state)?;
        delete_touched_empty(&mut state);
        self.db.commit(state);

        let deposit = tx.envelope.is_deposit();
        let success = result.is_success();
        let tx_gas_used = result.tx_gas_used();
        let failed_deposit = matches!(
            &result,
            ExecutionResult::Halt {
                reason: OpHaltReason::FailedDeposit,
                ..
            }
        );
        if let Some((arena, sstore)) = traces {
            // op-revm folds a deposit that halts into a failed deposit, as
            // its end state is the same; op-geth only makes the `failed
            // deposit` trace for one rejected before it ran.
            let root = arena.nodes().first().and_then(|node| node.trace.status);
            let rejected = failed_deposit
                && matches!(
                    root,
                    None | Some(revm::interpreter::InstructionResult::OutOfFunds)
                );
            if rejected {
                let reason = self.deposit_rejection(tx, &sender_before);
                self.block_file.error_traces.push(failed_deposit_trace(
                    tx,
                    sender_before.nonce,
                    &reason,
                ));
            } else {
                append_styled_tx_traces(
                    TraceStyle::OpBnb,
                    &tx_hash,
                    gas_limit,
                    &arena,
                    &sstore,
                    &mut self.log_index,
                    &mut self.storage_contracts,
                    &mut self.block_file,
                );
            }
        }

        self.gas_used += tx_gas_used;
        // `core.pipelineGasPrice`: the L1 fee spread over the gas used.
        let gas_price = if deposit || tx_gas_used == 0 {
            effective_price
        } else {
            effective_price + l1_fee / tx_gas_used as u128
        };
        let nonce = if deposit {
            sender_before.nonce
        } else {
            tx.envelope.nonce()
        };
        self.block_file.txs.push(opbnb_pipeline_transaction(
            &tx.envelope,
            tx.sender,
            success,
            tx_gas_used,
            gas_price,
            nonce,
            index,
        ));
        self.receipts.push(op_receipt_envelope(
            &tx.envelope,
            success,
            result.logs().to_vec(),
            self.gas_used,
            // Before Canyon the deposit nonce is in the receipt but not in its
            // consensus encoding, so it stays out of the receipts root.
            (deposit && canyon).then_some(sender_before.nonce),
            (deposit && canyon).then_some(DEPOSIT_RECEIPT_VERSION),
        ));
        Ok(())
    }

    /// The block's L1 info, as op-geth's L1 cost function reads it.
    fn l1_info(&mut self) -> Result<L1BlockInfo> {
        if let Some(info) = &self.l1_info {
            return Ok(info.clone());
        }
        let number = U256::from(self.number);
        let mut info = L1BlockInfo::try_fetch(&mut self.db, number, self.spec)
            .map_err(|err| anyhow!("L1 info of block {}: {err:?}", self.number))?;
        if info.empty_ecotone_scalars {
            info.l1_base_fee_scalar = self.db.storage(L1_BLOCK, L1_SCALAR_SLOT)?;
        }
        self.l1_info = Some(info.clone());
        Ok(info)
    }

    /// Why op-geth rejected deposit `tx` before running it, in its words.
    fn deposit_rejection(&self, tx: &OpBnbTx, sender_before: &AccountInfo) -> String {
        let OpTxEnvelope::Deposit(deposit) = &tx.envelope else {
            return "not a deposit".to_string();
        };
        let create = deposit.to.is_create();
        let shanghai = self.timestamp >= OPBNB_MAINNET_CANYON_TIMESTAMP;
        let intrinsic = intrinsic_gas(&deposit.input, create, shanghai);
        if deposit.gas_limit < intrinsic {
            return format!(
                "intrinsic gas too low: have {}, want {intrinsic}",
                deposit.gas_limit
            );
        }
        let balance = sender_before.balance + U256::from(deposit.mint);
        if !deposit.value.is_zero() && balance < deposit.value {
            return format!(
                "insufficient funds for transfer: address {}",
                deposit.from.to_checksum(None)
            );
        }
        if shanghai && create && deposit.input.len() > MAX_INIT_CODE_SIZE {
            return format!(
                "max initcode size exceeded: code size {} limit {MAX_INIT_CODE_SIZE}",
                deposit.input.len()
            );
        }
        "unknown deposit failure".to_string()
    }

    /// EIP-4788: the system call stores the timestamp and the parent beacon
    /// root in the beacon roots contract, once it is deployed.
    fn beacon_root(&mut self, root: B256) -> Result<()> {
        let deployed = self
            .db
            .basic(BEACON_ROOTS)?
            .is_some_and(|info| info.code_hash != KECCAK_EMPTY && !info.code_hash.is_zero());
        if !deployed {
            return Ok(());
        }
        let slot = self.timestamp % BEACON_ROOTS_BUFFER;
        self.set_storage(BEACON_ROOTS, U256::from(slot), U256::from(self.timestamp))?;
        self.set_storage(
            BEACON_ROOTS,
            U256::from(slot + BEACON_ROOTS_BUFFER),
            U256::from_be_bytes(root.0),
        )
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        let mut info = self.db.load_account(address)?.info().unwrap_or_default();
        info.code_hash = code.hash_slow();
        info.code = Some(code);
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
}

/// The trace op-geth's early-exit hook records for a deposit rejected before
/// it ran (`StateTransition.TransitionDb`): a top frame with the deposit's
/// call, all its gas used and the rejection as the error.
fn failed_deposit_trace(tx: &OpBnbTx, nonce_before: u64, reason: &str) -> PipelineTrace {
    let OpTxEnvelope::Deposit(deposit) = &tx.envelope else {
        unreachable!("only deposits fail this way");
    };
    let tx_id = hash_hex(&tx.envelope.tx_hash());
    let (to, call_create_type, call_type) = match deposit.to {
        TxKind::Call(to) => (to, "call", "call"),
        TxKind::Create => (deposit.from.create(nonce_before), "create", ""),
    };
    PipelineTrace {
        id: to_hash(&[&tx_id, "", "0"]),
        from_addr: addr_hex(&deposit.from),
        gas_limit: deposit.gas_limit as i64,
        input: deposit.input.clone(),
        to_addr: addr_hex(&to),
        value: deposit.value,
        gas_used: deposit.gas_limit as i64,
        output: Bytes::new(),
        call_create_type: call_create_type.to_string(),
        call_type: call_type.to_string(),
        tx_id,
        parent_trace_id: String::new(),
        pos_in_parent_trace: 0,
        self_storage_change: false,
        storage_change: false,
        subtraces: 0,
        trace_address: Vec::new(),
        error: format!("failed deposit: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intrinsic_gas_matches_op_geth() {
        assert_eq!(intrinsic_gas(&[], false, true), 21_000);
        assert_eq!(intrinsic_gas(&[0, 1], false, false), 21_000 + 4 + 16);
        // A creation pays 32000 more, and from Shanghai 2 per init code word.
        assert_eq!(intrinsic_gas(&[1; 33], true, false), 53_000 + 33 * 16);
        assert_eq!(intrinsic_gas(&[1; 33], true, true), 53_000 + 33 * 16 + 4);
    }

    #[test]
    fn failed_deposit_trace_of_a_creation_targets_the_new_address() {
        let deposit = op_alloy_consensus::TxDeposit {
            source_hash: B256::repeat_byte(1),
            from: address!("00000000000000000000000000000000000000aa"),
            to: TxKind::Create,
            mint: 0,
            value: U256::from(5),
            gas_limit: 100_000,
            is_system_transaction: false,
            input: Bytes::from_static(&[0x60, 0x00]),
        };
        let envelope = OpTxEnvelope::Deposit(alloy::consensus::Sealed::new(deposit.clone()));
        let tx = OpBnbTx {
            sender: deposit.from,
            encoded: Bytes::new(),
            envelope,
        };
        let trace = failed_deposit_trace(&tx, 7, "insufficient funds for transfer: address 0xAA");
        assert_eq!(trace.call_create_type, "create");
        assert_eq!(trace.call_type, "");
        assert_eq!(trace.to_addr, addr_hex(&deposit.from.create(7)));
        assert_eq!(trace.gas_used, 100_000);
        assert_eq!(
            trace.error,
            "failed deposit: insufficient funds for transfer: address 0xAA"
        );
    }
}
