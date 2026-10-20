//! Re-execute historical Ethereum mainnet blocks on the archive state of
//! their parent, collecting pipeline traces and events per transaction.
//!
//! The block is executed by alloy-evm's [`EthBlockExecutor`], reth's
//! consensus executor: the EIP-2935 and EIP-4788 system calls before the
//! transactions; the withdrawal and consolidation request system calls,
//! block and ommer rewards, withdrawals and the DAO fork drain after them.
//! Like go-ethereum, the pipeline tracer only sees the transactions: system
//! calls have no transaction, so they leave no traces or events, while their
//! state changes are part of the state diff.
//!
//! Every commit of the executor goes through a [`RecordingDb`], which records
//! the block's state changes and applies EIP-161 from Spurious Dragon on.
//!
//! The result is checked before it is returned: gas used, receipts root
//! (the logs bloom before Byzantium, whose receipts carry intermediate state
//! roots the replay cannot compute) and, from Prague on, the requests hash
//! must match the header; optionally every account and slot the replay
//! changed must equal the archive state of the block.

use crate::pipeline::blocks::FetchedBlock;
use crate::pipeline::build::{pipeline_block, pipeline_header, pipeline_transaction};
use crate::pipeline::chain::{ReplayOptions, ReplayOutput, ReplayTimings};
use crate::pipeline::changes::{check, RecordingDb, StorageSlots};
use crate::pipeline::convert::append_tx_traces;
use crate::pipeline::inspector::PipelineInspector;
use crate::pipeline::types::{addr_hex, BlockFile};
use alloy::consensus::{
    proofs::calculate_receipt_root, transaction::Recovered, Transaction as _, TxReceipt,
};
use alloy::eips::BlobScheduleBlobParams;
use alloy::primitives::{Bloom, Bytes, B256};
use alloy::rpc::types::BlockTransactions;
use alloy_evm::block::BlockExecutor;
use alloy_evm::eth::receipt_builder::AlloyReceiptBuilder;
use alloy_evm::eth::spec::EthSpec;
use alloy_evm::eth::{EthBlockExecutionCtx, EthBlockExecutor, EthEvmFactory};
use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_hardforks::EthereumHardforks;
use anyhow::{anyhow, bail, Context as _, Result};
use leafage_evm_chains::eth::mainnet::{
    eth_mainnet_blob_params_at, eth_mainnet_blob_schedule, ETH_MAINNET_CHAIN_ID,
};
use revm::database::CacheDB;
use revm::DatabaseRef;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Debug;

/// One block for an [`EthReplayer`].
pub(crate) struct EthBlock<D> {
    /// State after the block, for the state check.
    pub post_state: D,
    /// State before the block, for the storage of the accounts it destroys.
    pub parent_state: Option<Box<dyn StorageSlots + Send>>,
    pub parent_state_root: B256,
    pub block: FetchedBlock,
}

/// Replays consecutive blocks on one state. The state the replay builds
/// carries over from one block to the next, so a key is read from the
/// archive only the first time the replay touches it: until then no
/// replayed block changed it, so its value in the starting state is still
/// current.
pub(crate) struct EthReplayer<D: DatabaseRef> {
    /// `None` only while a block is being replayed.
    db: Option<CacheDB<D>>,
    options: ReplayOptions,
    spec: EthSpec,
    blobs: BlobScheduleBlobParams,
}

impl<D> EthReplayer<D>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    /// Start after the block whose state is `parent_state`.
    pub(crate) fn new(parent_state: D, options: ReplayOptions) -> Self {
        Self {
            db: Some(CacheDB::new(parent_state)),
            options,
            spec: EthSpec::mainnet(),
            blobs: eth_mainnet_blob_schedule(),
        }
    }

    /// Replay the block after the last one replayed.
    pub(crate) fn replay(&mut self, block: EthBlock<D>) -> Result<ReplayOutput> {
        let db = self.db.take().context("an earlier replay failed")?;
        let (output, db) = self.replay_one(db, block)?;
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

    fn replay_one(&self, db: CacheDB<D>, input: EthBlock<D>) -> Result<(ReplayOutput, CacheDB<D>)> {
        let options = self.options;
        let EthBlock {
            post_state,
            parent_state,
            parent_state_root,
            block: FetchedBlock { block, ommers },
        } = input;
        let header = &block.header;
        let number = header.number;
        let BlockTransactions::Full(transactions) = &block.transactions else {
            bail!("block {number} was returned without transaction bodies");
        };
        let spec = &self.spec;
        let started = std::time::Instant::now();
        let mut timings = ReplayTimings::default();

        let env = EvmEnv::for_eth_block(
            &header.inner,
            spec,
            ETH_MAINNET_CHAIN_ID,
            eth_mainnet_blob_params_at(spec, &self.blobs, header.timestamp),
        );
        let mut db = RecordingDb::new(
            db,
            options.check_state || options.state_diff,
            spec.is_spurious_dragon_active_at_block(number),
        );
        let mut evm = EthEvmFactory::default().create_evm_with_inspector(
            &mut db,
            env,
            PipelineInspector::new(),
        );
        evm.set_inspector_enabled(options.traces);
        let ctx = EthBlockExecutionCtx {
            parent_hash: header.parent_hash,
            parent_beacon_block_root: header.parent_beacon_block_root,
            ommers: &ommers,
            withdrawals: block
                .withdrawals
                .as_ref()
                .map(|w| Cow::Borrowed(w.as_slice())),
            extra_data: header.extra_data.clone(),
            tx_count_hint: Some(transactions.len()),
            slot_number: None,
        };
        let mut executor = EthBlockExecutor::new(evm, ctx, spec, AlloyReceiptBuilder::default());
        executor
            .apply_pre_execution_changes()
            .map_err(|err| anyhow!("block {number} system calls: {err}"))?;

        let mut block_file = BlockFile {
            block: pipeline_block(header),
            ..Default::default()
        };
        let mut log_index = 0;
        let mut storage_contracts = BTreeSet::new();
        let base_fee = header.base_fee_per_gas;
        for (index, tx) in transactions.iter().enumerate() {
            let sender = tx.inner.signer();
            let envelope = tx.inner.inner();
            let tx_hash = *envelope.tx_hash();
            let output = executor
                .execute_transaction_without_commit(Recovered::new_unchecked(envelope, sender))
                .map_err(|err| anyhow!("transaction {tx_hash} failed to execute: {err}"))?;
            let mut code_deposit_failed = false;
            if options.traces {
                let inspector =
                    std::mem::replace(executor.evm_mut().inspector_mut(), PipelineInspector::new());
                code_deposit_failed = inspector.code_deposit_failed();
                let (arena, sstore) = inspector.into_traces();
                append_tx_traces(
                    &tx_hash,
                    &arena,
                    &sstore,
                    &mut log_index,
                    &mut storage_contracts,
                    &mut block_file,
                );
            }
            let mut tx = pipeline_transaction(
                envelope,
                sender,
                &output.result.result,
                envelope.effective_gas_price(base_fee),
                index,
            );
            // geth fails a creation that could not pay for its code (before
            // Homestead), although the contract stays, without code.
            tx.status &= !code_deposit_failed;
            block_file.txs.push(tx);
            executor.commit_transaction(output);
        }
        let (_, result) = executor
            .finish()
            .map_err(|err| anyhow!("block {number} finalization: {err}"))?;
        let RecordingDb {
            db, changes, error, ..
        } = db;
        if let Some(err) = error {
            return Err(err).context("record the state changes");
        }

        timings.execute = started.elapsed();
        let started = std::time::Instant::now();
        let check = |result: Result<()>| check(options.strict, result);
        check(if result.gas_used != header.gas_used {
            Err(anyhow!(
                "block {number}: replay used {} gas, header says {}",
                result.gas_used,
                header.gas_used
            ))
        } else {
            Ok(())
        })?;
        if spec.is_byzantium_active_at_block(number) {
            let receipts_root = calculate_receipt_root(&result.receipts);
            check(if receipts_root != header.receipts_root {
                Err(anyhow!(
                    "block {number}: replay receipts root {receipts_root}, header says {}",
                    header.receipts_root
                ))
            } else {
                Ok(())
            })?;
        } else {
            let bloom = result
                .receipts
                .iter()
                .fold(Bloom::ZERO, |bloom, receipt| bloom | receipt.bloom());
            check(if bloom != header.logs_bloom {
                Err(anyhow!(
                    "block {number}: replay logs bloom differs from the header"
                ))
            } else {
                Ok(())
            })?;
        }
        if let Some(expected) = header.requests_hash {
            let requests_hash = result.requests.requests_hash();
            check(if requests_hash != expected {
                Err(anyhow!(
                    "block {number}: replay requests hash {requests_hash}, header says {expected}"
                ))
            } else {
                Ok(())
            })?;
        }
        if options.check_state {
            if let Some(changes) = &changes {
                check(changes.verify(&post_state))?;
            }
        }

        timings.verify = started.elapsed();
        let started = std::time::Instant::now();
        let state_diff = match &changes {
            Some(_) if !options.state_diff => None,
            None => None,
            Some(_) if header.state_root == parent_state_root => Some(Bytes::new()),
            Some(changes) => {
                let parent_state = parent_state
                    .as_deref()
                    .map(|state| state as &dyn StorageSlots);
                let diff = changes.to_storage_diff(
                    &db,
                    parent_state,
                    header.state_root,
                    parent_state_root,
                )?;
                Some(alloy::rlp::encode(diff).into())
            }
        };
        block_file.storage_contracts = storage_contracts.iter().map(addr_hex).collect();
        timings.diff = started.elapsed();
        let output = ReplayOutput {
            block_file,
            header: pipeline_header(&block),
            state_diff,
            timings,
        };
        Ok((output, db))
    }
}
