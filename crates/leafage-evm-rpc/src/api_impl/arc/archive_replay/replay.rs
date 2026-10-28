//! Re-execute canonical Arc blocks on archive parent state.

use super::{
    block, convert,
    inspector::{record_subcall_trace_completion, ArcEventInspector},
    state_diff,
};
use crate::pipeline::{
    blocks::FetchedBlock,
    build::{pipeline_block, pipeline_header, pipeline_transaction, receipt_envelope},
    chain::{ReplayOptions, ReplayOutput, ReplayTimings},
    changes::check,
    types::{addr_hex, BlockFile},
};
use alloy::{
    consensus::{proofs::calculate_receipt_root, Transaction as _},
    primitives::{Bytes, B256, U256},
    rpc::types::BlockTransactions,
};
use alloy_evm::{EvmEnv, FromRecoveredTx};
use anyhow::{anyhow, bail, ensure, Context as _, Result};
use leafage_evm_chains::arc::{ArcChainConfig, ArcEvmFactory};
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::{
    bytecode::opcode::OpCode,
    context::TxEnv,
    database::{states::bundle_state::BundleRetention, State, WrapDatabaseRef},
    DatabaseCommit, DatabaseRef, ExecuteEvm, InspectEvm,
};
use revm_inspectors::tracing::OpcodeFilter;
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::{fmt::Debug, time::Instant};

pub(crate) struct ArcBlock<D> {
    pub post_state: Option<D>,
    pub parent_state_root: B256,
    pub block: FetchedBlock,
}

pub(crate) struct ArcReplayer<D: DatabaseRef> {
    db: Option<State<WrapDatabaseRef<D>>>,
    options: ReplayOptions,
}

fn inspector() -> (TracingInspector, ArcEventInspector) {
    let mut config = TracingInspectorConfig::default_parity()
        .set_steps(true)
        .set_record_logs(true)
        .set_exclude_precompile_calls(false);
    config.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
    (TracingInspector::new(config), ArcEventInspector::default())
}

impl<D> ArcReplayer<D>
where
    D: DatabaseRef + Debug,
    D::Error: Send + Sync + 'static,
{
    pub(crate) fn new(state: D, options: ReplayOptions) -> Self {
        Self {
            db: Some(
                State::builder()
                    .with_database(WrapDatabaseRef(state))
                    .with_bundle_update()
                    .build(),
            ),
            options,
        }
    }

    pub(crate) fn rebase(&mut self, state: D) {
        // Code is immutable for its hash; preserve its cache, not account/slot values.
        let mut db = State::builder()
            .with_database(WrapDatabaseRef(state))
            .with_bundle_update()
            .build();
        if let Some(old) = self.db.take() {
            db.cache.contracts = old.cache.contracts;
        }
        self.db = Some(db);
    }

    pub(crate) fn replay(&mut self, input: ArcBlock<D>) -> Result<ReplayOutput> {
        let mut db = self.db.take().context("an earlier Arc replay failed")?;
        let FetchedBlock {
            block: rpc_block,
            ommers,
        } = input.block;
        let header = &rpc_block.header;
        ensure!(ommers.is_empty(), "Arc blocks cannot contain ommers");
        let BlockTransactions::Full(txs) = &rpc_block.transactions else {
            bail!("block {} has no transaction bodies", header.number);
        };
        let chain_config = ArcChainConfig::mainnet();
        let mut cfg = CfgEnv::new_with_spec(chain_config.ethereum_spec());
        cfg.chain_id = chain_config.chain_id();
        let env = EvmEnv::new(
            cfg,
            BlockEnv {
                number: U256::from(header.number),
                beneficiary: header.beneficiary,
                timestamp: U256::from(header.timestamp),
                gas_limit: header.gas_limit,
                basefee: header.base_fee_per_gas.unwrap_or_default(),
                difficulty: header.difficulty,
                prevrandao: Some(header.mix_hash),
                blob_excess_gas_and_price: header.excess_blob_gas.map(|excess_blob_gas| {
                    revm::context_interface::block::BlobExcessGasAndPrice::new_with_spec(
                        excess_blob_gas,
                        chain_config.ethereum_spec(),
                    )
                }),
                ..Default::default()
            },
        );
        let factory = ArcEvmFactory::new(chain_config);
        let started = Instant::now();
        // System calls must not add nodes to a transaction's inspector.
        {
            let mut evm =
                factory.create(env.clone(), &mut db, revm::inspector::NoOpInspector {})?;
            block::before(&mut evm, header)?;
        }
        let mut file = BlockFile {
            block: pipeline_block(header),
            ..Default::default()
        };
        let mut receipts = Vec::with_capacity(txs.len());
        let mut log_index = 0;
        let mut gas_used = 0u64;
        for (index, tx) in txs.iter().enumerate() {
            let sender = tx.inner.signer();
            let envelope = tx.inner.inner();
            ensure!(
                envelope.gas_limit() <= header.gas_limit.saturating_sub(gas_used),
                "transaction {} gas limit exceeds remaining block gas",
                envelope.tx_hash()
            );
            let tx_env = TxEnv::from_recovered_tx(envelope, sender);
            let outcome = if self.options.traces {
                let mut inspectors = inspector();
                let outcome = {
                    let mut evm = factory.create(env.clone(), &mut db, &mut inspectors)?;
                    evm.set_subcall_trace_completion_hook(record_subcall_trace_completion);
                    evm.inspect_tx(tx_env)
                        .map_err(|e| anyhow!("transaction {}: {e:?}", envelope.tx_hash()))?
                };
                let (tracer, events) = inspectors;
                let captured = events.into_captured();
                ensure!(
                    captured.successful_logs() == outcome.result.logs(),
                    "transaction {} captured logs differ from execution",
                    envelope.tx_hash()
                );
                let converted = convert::build_debank_traces(
                    *envelope.tx_hash(),
                    tracer.into_traces(),
                    captured,
                    &mut log_index,
                )
                .map_err(anyhow::Error::msg)?;
                convert::validate_transaction_classification(
                    outcome.result.is_success(),
                    &converted.0,
                    &converted.1,
                    &converted.2,
                    &converted.3,
                )
                .map_err(anyhow::Error::msg)?;
                convert::append(&mut file, converted);
                outcome
            } else {
                factory
                    .create(env.clone(), &mut db, revm::inspector::NoOpInspector {})?
                    .transact(tx_env)
                    .map_err(|e| anyhow!("transaction {}: {e:?}", envelope.tx_hash()))?
            };
            gas_used = gas_used
                .checked_add(outcome.result.tx_gas_used())
                .context("cumulative gas overflow")?;
            receipts.push(receipt_envelope(
                envelope.tx_type(),
                &outcome.result,
                gas_used,
            ));
            let mut transaction = pipeline_transaction(
                envelope,
                sender,
                &outcome.result,
                envelope.effective_gas_price(header.base_fee_per_gas),
                index,
            );
            // Writer uses the transaction trait accessors for every transaction type.
            transaction.max_fee_per_gas = envelope.max_fee_per_gas();
            transaction.max_priority_fee_per_gas =
                envelope.max_priority_fee_per_gas().unwrap_or_default();
            file.txs.push(transaction);
            db.commit(outcome.state);
        }
        let mut timings = ReplayTimings {
            execute: started.elapsed(),
            ..Default::default()
        };
        let started = Instant::now();
        check(
            self.options.strict,
            if gas_used == header.gas_used {
                Ok(())
            } else {
                Err(anyhow!(
                    "block {} gasUsed: replay {gas_used}, header {}",
                    header.number,
                    header.gas_used
                ))
            },
        )?;
        let root = calculate_receipt_root(&receipts);
        check(
            self.options.strict,
            if root == header.receipts_root {
                Ok(())
            } else {
                Err(anyhow!(
                    "block {} receiptsRoot: replay {root}, header {}",
                    header.number,
                    header.receipts_root
                ))
            },
        )?;
        timings.verify = started.elapsed();
        let started = Instant::now();
        {
            let mut evm = factory.create(env, &mut db, revm::inspector::NoOpInspector {})?;
            block::after(&mut evm, header, gas_used)?;
        }
        db.merge_transitions(BundleRetention::PlainState);
        let bundle = db.take_bundle();
        timings.execute += started.elapsed();
        let started = Instant::now();
        if self.options.check_state {
            check(
                self.options.strict,
                state_diff::verify(
                    &bundle,
                    input
                        .post_state
                        .as_ref()
                        .context("missing Arc post-state")?,
                ),
            )?;
        }
        timings.verify += started.elapsed();
        let started = Instant::now();
        if self.options.traces {
            file.storage_contracts = state_diff::get_storage_contracts_from_bundle(&bundle)
                .iter()
                .map(addr_hex)
                .collect();
        }
        let diff = if !self.options.state_diff {
            None
        } else if header.state_root == input.parent_state_root {
            Some(Bytes::new())
        } else {
            // original_info belongs to this block; the range's base archive can be older.
            let mut diff = state_diff::get_storage_diffs_from_bundle(bundle);
            diff.hash = header.state_root;
            diff.parent_hash = input.parent_state_root;
            Some(alloy::rlp::encode(diff).into())
        };
        timings.diff = started.elapsed();
        self.db = Some(db);
        Ok(ReplayOutput {
            block_file: file,
            header: pipeline_header(&rpc_block),
            state_diff: diff,
            timings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{database::InMemoryDB, state::AccountInfo};

    fn state() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            alloy::primitives::address!("1800000000000000000000000000000000000002"),
            AccountInfo {
                nonce: 1,
                ..Default::default()
            },
        );
        db
    }

    fn options(traces: bool) -> ReplayOptions {
        ReplayOptions {
            strict: true,
            check_state: false,
            state_diff: true,
            traces,
        }
    }

    fn empty_block() -> ArcBlock<InMemoryDB> {
        let mut block: alloy::rpc::types::Block = Default::default();
        block.header.number = 1;
        block.header.gas_limit = 30_000_000;
        block.header.base_fee_per_gas = Some(1);
        block.header.extra_data = 1u64.to_be_bytes().to_vec().into();
        block.header.receipts_root = alloy::consensus::constants::EMPTY_ROOT_HASH;
        block.header.state_root = B256::repeat_byte(1);
        block.transactions = BlockTransactions::Full(vec![]);
        ArcBlock {
            post_state: None,
            parent_state_root: B256::repeat_byte(2),
            block: FetchedBlock {
                block,
                ommers: vec![],
            },
        }
    }

    #[test]
    fn gas_and_receipt_errors_precede_extra_data_errors() {
        for gas_mismatch in [true, false] {
            let mut block = empty_block();
            block.block.block.header.extra_data = Bytes::new();
            if gas_mismatch {
                block.block.block.header.gas_used = 1;
            } else {
                block.block.block.header.receipts_root = B256::ZERO;
            }
            let mut replay = ArcReplayer::new(state(), options(true));
            let message = replay.replay(block).err().unwrap().to_string();
            assert!(
                message.contains(if gas_mismatch {
                    "gasUsed: replay"
                } else {
                    "receiptsRoot: replay"
                }),
                "{message}"
            );
            assert!(!message.contains("extraData"), "{message}");
        }
    }

    #[test]
    fn no_traces_omits_storage_contracts_but_keeps_system_state_diff() {
        let mut replay = ArcReplayer::new(state(), options(true));
        let traced = replay.replay(empty_block()).unwrap();
        let mut replay = ArcReplayer::new(state(), options(false));
        let plain = replay.replay(empty_block()).unwrap();
        assert!(!traced.block_file.storage_contracts.is_empty());
        assert!(plain.block_file.storage_contracts.is_empty());
        assert!(plain.block_file.traces.is_empty());
        assert!(plain.block_file.events.is_empty());
        assert_eq!(plain.state_diff, traced.state_diff);
        assert!(plain
            .state_diff
            .as_ref()
            .is_some_and(|diff| !diff.is_empty()));
        assert_eq!(
            plain.block_file.validation_hash(),
            traced.block_file.validation_hash()
        );
    }

    #[test]
    fn post_state_is_required_only_for_state_verification() {
        let mut replay = ArcReplayer::new(state(), options(false));
        assert!(replay.replay(empty_block()).is_ok());
        let mut checked = options(false);
        checked.check_state = true;
        let mut replay = ArcReplayer::new(state(), checked);
        let err = replay.replay(empty_block()).err().unwrap();
        assert_eq!(err.to_string(), "missing Arc post-state");
    }
}
