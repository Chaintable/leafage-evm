//! Historical Ethermint replay before the Cronos v1 upgrade.
//! Receipt agreement alone does not certify the Cosmos hooks or internal frames.
use super::{
    build::{pipeline_block, pipeline_transaction},
    convert::append_tx_traces,
    inspector::PipelineInspector,
    types::BlockFile,
};
use alloy::{
    consensus::Transaction as _,
    primitives::{
        map::{AddressMap, HashSet},
        U256,
    },
    rpc::types::{Block, BlockTransactions, TransactionReceipt},
};
use alloy_evm::{eth::EthEvmFactory, EvmEnv, EvmFactory, IntoTxEnv};
use anyhow::{anyhow, ensure, Context as _, Result};
use revm::{
    bytecode::opcode,
    context::{
        result::{EVMError, HaltReason},
        BlockEnv, CfgEnv, ContextTr, JournalTr,
    },
    context_interface::cfg::GasId,
    database::CacheDB,
    handler::{EthFrame, EvmTr, FrameResult, Handler},
    inspector::{InspectorEvmTr, InspectorHandler},
    interpreter::interpreter::EthInterpreter,
    primitives::hardfork::SpecId,
    state::{EvmState, TransactionId},
    Database, DatabaseCommit, DatabaseRef, Inspector,
};
use std::{collections::BTreeSet, fmt::Debug, marker::PhantomData};

type Warm = AddressMap<HashSet<U256>>;

// Ethermint supplies gas fees to the Cosmos fee collector. The proposer
// address remains visible to COINBASE but earns no EVM miner reward.
struct EthermintHandler<E> {
    warm: Warm,
    marker: PhantomData<E>,
}

impl<E> Handler for EthermintHandler<E>
where
    E: EvmTr<
        Context: ContextTr<Journal: JournalTr<State = EvmState>>,
        Frame = EthFrame<EthInterpreter>,
    >,
{
    type Evm = E;
    type Error = EVMError<<<<E as EvmTr>::Context as ContextTr>::Db as Database>::Error>;
    type HaltReason = HaltReason;

    fn load_accounts(&self, evm: &mut E) -> Result<(), Self::Error> {
        revm::handler::pre_execution::load_accounts::<E, Self::Error>(evm)?;
        evm.ctx().journal_mut().warm_access_list(self.warm.clone());
        Ok(())
    }

    fn reward_beneficiary(&self, _: &mut E, _: &mut FrameResult) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<E> InspectorHandler for EthermintHandler<E>
where
    E: InspectorEvmTr<
        Context: ContextTr<Journal: JournalTr<State = EvmState>>,
        Frame = EthFrame<EthInterpreter>,
        Inspector: Inspector<<E as EvmTr>::Context, EthInterpreter>,
    >,
{
    type IT = EthInterpreter;
}

// Applying the genesis extra_eips in their stored order (2929, 2200,
// 1884, 1344) replaces Berlin SSTORE with EIP-2200. An SSTORE by
// itself therefore does not add the slot to Ethermint's access list.
fn ethermint_sstore<D: Database>(
    context: revm::interpreter::InstructionContext<
        '_,
        alloy_evm::eth::EthEvmContext<D>,
        EthInterpreter,
    >,
) -> revm::interpreter::InstructionExecResult {
    use revm::interpreter::instructions::host::{
        sstore_default_gas_accounting, sstore_with_gas_accounting,
    };
    let slot = context.interpreter.stack.peek(0)?;
    sstore_with_gas_accounting(context, |ctx, address, change| {
        if change.is_cold {
            ctx.host
                .journaled_state
                .state
                .get_mut(&address)
                .expect("sstore loaded the account")
                .storage
                .get_mut(&slot)
                .expect("sstore loaded the slot")
                .mark_cold();
        }
        sstore_default_gas_accounting(ctx, address, change)
    })
}

mod releases;
pub use releases::CronosRelease;

/// Cronos mainnet genesis sets london_block to 2^63. The v0.7.0
/// upgrade handler sets it to zero and enables the fee market at Huygen.
/// v1 and later require independently verified execution rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CronosEra {
    BeforeHuygen,
    Huygen,
    HuygenHotfix,
}

impl CronosEra {
    pub fn at_height(height: u64) -> Option<Self> {
        CronosRelease::at_height(height)?.rules
    }

    pub fn version(self) -> &'static str {
        match self {
            Self::BeforeHuygen => "ethermint-transient-v3",
            Self::Huygen => "ethermint-huygen-v3",
            Self::HuygenHotfix => "ethermint-huygen-hotfix-v3",
        }
    }

    pub fn spec(self, base_fee: Option<u64>) -> Result<SpecId> {
        let spec = match self {
            Self::BeforeHuygen => SpecId::BERLIN,
            Self::Huygen | Self::HuygenHotfix => SpecId::LONDON,
        };
        ensure!(
            base_fee.is_some() == (spec == SpecId::LONDON),
            "baseFeePerGas inconsistent with Cronos mainnet {self:?}"
        );
        Ok(spec)
    }
}

/// v0.6 marks suicide in transient state; v0.7 deletes code by shared hash.
/// Neither lifecycle is represented by the standard CacheDB account deletion.
#[derive(Debug)]
pub struct LegacySelfdestruct;
impl std::fmt::Display for LegacySelfdestruct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("historical SELFDESTRUCT state transition is not implemented")
    }
}

/// Identifies storage failures even when they occur inside an EVM instruction.
#[derive(Debug)]
pub struct CronosArchiveRead;

impl std::fmt::Display for CronosArchiveRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reading Cronos archive state")
    }
}

pub fn replay_cronos_ethermint<D: DatabaseRef + Debug>(
    parent: D,
    block: &Block,
    receipts: &[TransactionReceipt],
    gas_limit: u64,
    message_gas: &[u64],
    begin_transfers: &[(alloy::primitives::Address, bool, U256)],
) -> Result<BlockFile> {
    let BlockTransactions::Full(txs) = &block.transactions else {
        anyhow::bail!("missing signed transaction bodies");
    };
    ensure!(
        txs.len() == receipts.len() && message_gas.len() == txs.len(),
        "receipt count mismatch"
    );
    let header = &block.header;
    let era = CronosEra::at_height(header.number).context("unsupported Cronos execution era")?;
    let transient = era == CronosEra::BeforeHuygen;
    let spec = era.spec(header.base_fee_per_gas)?;
    let mut cfg = CfgEnv::new_with_spec(spec).with_chain_id(25);
    // Ethermint exposes zero to GASLIMIT for an unbounded block meter.
    cfg.disable_block_gas_limit = gas_limit == 0;
    cfg.gas_params.override_gas([
        (GasId::sstore_static(), 800),
        (GasId::sstore_set_without_load_cost(), 19_200),
        (GasId::sstore_reset_without_cold_load_cost(), 4_200),
        (GasId::sstore_set_refund(), 19_200),
        (GasId::sstore_reset_refund(), 4_200),
        (GasId::cold_storage_cost(), 0),
        (GasId::sstore_clearing_slot_refund(), 15_000),
    ]);
    let env = EvmEnv::new(
        cfg,
        BlockEnv {
            number: U256::from(header.number),
            timestamp: U256::from(header.timestamp),
            beneficiary: header.beneficiary,
            gas_limit,
            difficulty: U256::ZERO,
            basefee: header.base_fee_per_gas.unwrap_or_default(),
            prevrandao: None,
            ..Default::default()
        },
    );
    let mut db = CacheDB::new(parent);
    for &(address, credit, amount) in begin_transfers {
        let balance = &mut db
            .load_account(address)
            .context(CronosArchiveRead)?
            .info
            .balance;
        *balance = if credit {
            balance.checked_add(amount)
        } else {
            balance.checked_sub(amount)
        }
        .ok_or_else(|| anyhow!("BeginBlock balance unavailable for {address}"))?;
    }
    let mut warm: AddressMap<HashSet<U256>> = Default::default();
    let mut file = BlockFile {
        block: pipeline_block(header),
        ..Default::default()
    };
    let mut storage = BTreeSet::new();
    let mut log_index = 0;
    for (index, (tx, receipt)) in txs.iter().zip(receipts).enumerate() {
        let envelope = tx.inner.inner();
        let hash = envelope.tx_hash();
        if !transient {
            warm.clear();
        }
        if let Some(list) = envelope.access_list() {
            for item in &list.0 {
                warm.entry(item.address).or_default().extend(
                    item.storage_keys
                        .iter()
                        .map(|key| U256::from_be_bytes(key.0)),
                );
            }
        }
        warm.entry(tx.inner.signer()).or_default();
        if let Some(to) = envelope.to() {
            warm.entry(to).or_default();
        }
        let collector = alloy::primitives::address!("f1829676db577682e944fc3493d451b67ff3e29f");
        let escrow = U256::from(envelope.gas_limit())
            * U256::from(envelope.effective_gas_price(header.base_fee_per_gas));
        db.load_account(collector)
            .context(CronosArchiveRead)?
            .info
            .balance += escrow;
        let mut evm = EthEvmFactory::default()
            .create_evm_with_inspector(&mut db, env.clone(), PipelineInspector::new())
            .into_inner();
        // revm combines the warm dynamic SLOAD charge with its base gas.
        evm.instruction.insert_gas(opcode::SLOAD, 900);
        evm.instruction.insert_gas(opcode::BALANCE, 700);
        evm.instruction.insert_gas(opcode::EXTCODEHASH, 700);
        evm.instruction.insert_instruction(
            opcode::SSTORE,
            revm::interpreter::Instruction::new(ethermint_sstore),
            0,
        );
        evm.ctx.tx = (&tx.inner).into_tx_env();
        let mut handler = EthermintHandler {
            warm,
            marker: PhantomData,
        };
        let result = handler.inspect_run(&mut evm).map_err(|e| match e {
            EVMError::Database(source) => anyhow::Error::new(source).context(CronosArchiveRead),
            other => anyhow!("tx {index} {hash}: {other:?}"),
        })?;
        warm = handler.warm;
        let state = evm.ctx.journaled_state.finalize();
        if era != CronosEra::HuygenHotfix
            && state.values().any(|account| account.is_selfdestructed())
        {
            return Err(anyhow!("tx {index} {hash}").context(LegacySelfdestruct));
        }
        ensure!(
            result.is_success() == receipt.status(),
            "tx {index} {hash} status: replay {}, receipt {}",
            result.is_success(),
            receipt.status()
        );
        ensure!(
            result.tx_gas_used() == message_gas[index],
            "tx {index} {hash} execution gas: replay {}, Comet {}",
            result.tx_gas_used(),
            message_gas[index]
        );
        // Before Huygen a revert also discarded the unused-gas refund.
        // At H124760 tx 4 the sender paid 822893 gas; DeliverTx recorded 810418.
        let charged = if transient && !result.is_success() {
            envelope.gas_limit()
        } else {
            result.tx_gas_used()
        };
        ensure!(
            charged == receipt.gas_used,
            "tx {index} {hash} receipt gas: replay {charged}, receipt {}",
            receipt.gas_used
        );
        ensure!(
            result
                .logs()
                .iter()
                .eq(receipt.logs().iter().map(|log| &log.inner)),
            "tx {index} {hash} committed logs differ"
        );
        let (arena, stores) = evm.inspector.into_traces();
        append_tx_traces(
            hash,
            &arena,
            &stores,
            &mut log_index,
            &mut storage,
            &mut file,
        );
        file.txs.push(pipeline_transaction(
            envelope,
            tx.inner.signer(),
            &result,
            envelope.effective_gas_price(header.base_fee_per_gas),
            index,
        ));
        for (address, account) in &state {
            if !account.is_cold_transaction_id(TransactionId::ZERO) {
                let slots = warm.entry(*address).or_default();
                slots.extend(
                    account
                        .storage
                        .iter()
                        .filter(|(_, slot)| !slot.is_cold_transaction_id(TransactionId::ZERO))
                        .map(|(key, _)| *key),
                );
            }
        }
        db.commit(state);
        let refund = U256::from(envelope.gas_limit() - result.tx_gas_used())
            * U256::from(envelope.effective_gas_price(header.base_fee_per_gas));
        if charged != result.tx_gas_used() {
            let balance = &mut db
                .load_account(tx.inner.signer())
                .context(CronosArchiveRead)?
                .info
                .balance;
            *balance = balance
                .checked_sub(refund)
                .ok_or_else(|| anyhow!("sender cannot settle failed tx {hash}"))?;
            continue;
        }
        let balance = &mut db
            .load_account(collector)
            .context(CronosArchiveRead)?
            .info
            .balance;
        *balance = balance
            .checked_sub(refund)
            .ok_or_else(|| anyhow!("fee collector cannot refund {hash}"))?;
    }
    file.storage_contracts = storage.iter().map(super::types::addr_hex).collect();
    check_block_file(&file, receipts)?;
    Ok(file)
}

/// Check the serialized frame relationships after conversion. The receipt
/// anchors the root outcome and the ordered logs of the successful frames.
fn check_block_file(file: &BlockFile, receipts: &[TransactionReceipt]) -> Result<()> {
    use super::types::{addr_hex, hash_hex, to_hash};
    use std::collections::{HashMap, HashSet};
    let mut nodes = HashMap::new();
    let mut paths = HashSet::new();
    for (traces, failed) in [(&file.traces, false), (&file.error_traces, true)] {
        for t in traces {
            ensure!(
                nodes.insert(t.id.as_str(), (t, failed)).is_none(),
                "duplicate frame {}",
                t.id
            );
            ensure!(
                paths.insert((&t.tx_id, &t.trace_address)),
                "duplicate call path {}",
                t.id
            );
            ensure!(
                failed != t.error.is_empty(),
                "incorrect failure class {}",
                t.id
            );
        }
    }
    for (t, failed) in nodes.values() {
        ensure!(
            t.id == to_hash(&[
                &t.tx_id,
                &t.parent_trace_id,
                &t.pos_in_parent_trace.to_string()
            ]),
            "invalid frame id {}",
            t.id
        );
        if t.trace_address.is_empty() {
            ensure!(
                t.parent_trace_id.is_empty() && t.pos_in_parent_trace == 0,
                "invalid root {}",
                t.id
            );
        } else {
            let (parent, parent_failed) = nodes
                .get(t.parent_trace_id.as_str())
                .context("missing parent frame")?;
            let (index, prefix) = t.trace_address.split_last().expect("nonempty path");
            ensure!(
                parent.tx_id == t.tx_id
                    && parent.trace_address == prefix
                    && (0..parent.subtraces).contains(index),
                "invalid call path {}",
                t.id
            );
            ensure!(
                !parent_failed || *failed,
                "failed parent has successful child {}",
                t.id
            );
        }
    }
    let mut logs = file.events.iter().collect::<Vec<_>>();
    logs.sort_unstable_by_key(|e| e.idx);
    let mut log_index = 0;
    for r in receipts {
        let tx = hash_hex(&r.transaction_hash);
        let root_id = to_hash(&[&tx, "", "0"]);
        let (_, failed) = nodes
            .get(root_id.as_str())
            .context("missing transaction root")?;
        ensure!(*failed != r.status(), "root outcome differs for {tx}");
        for log in r.logs() {
            let e = logs.get(log_index).context("missing committed event")?;
            let (parent, failed) = nodes
                .get(e.parent_trace_id.as_str())
                .context("missing event parent")?;
            let topics = log.inner.data.topics();
            ensure!(
                !failed
                    && parent.tx_id == tx
                    && e.idx == log_index as i64
                    && e.contract_id == addr_hex(&log.inner.address)
                    && e.data == log.inner.data.data
                    && e.selector == topics.first().map(hash_hex).unwrap_or_default()
                    && e.topics.iter().flatten().eq(topics
                        .iter()
                        .skip(1)
                        .map(hash_hex)
                        .collect::<Vec<_>>()
                        .iter())
                    && e.id == to_hash(&[&e.parent_trace_id, &e.pos_in_parent_trace.to_string()]),
                "committed event {log_index} differs for {tx}"
            );
            log_index += 1;
        }
    }
    ensure!(logs.len() == log_index, "extra committed events");
    for e in &file.error_events {
        ensure!(
            nodes
                .get(e.parent_trace_id.as_str())
                .is_some_and(|(_, failed)| *failed)
                && e.id == to_hash(&[&e.parent_trace_id, &e.pos_in_parent_trace.to_string()]),
            "invalid error event {}",
            e.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod era_tests {
    use super::*;

    #[test]
    fn mainnet_upgrade_boundaries_and_header_consistency() {
        for (height, era) in [
            (0, None),
            (1, Some(CronosEra::BeforeHuygen)),
            (2_693_799, Some(CronosEra::BeforeHuygen)),
            (2_693_800, Some(CronosEra::Huygen)),
            (3_982_499, Some(CronosEra::Huygen)),
            (3_982_500, Some(CronosEra::HuygenHotfix)),
            (6_542_799, Some(CronosEra::HuygenHotfix)),
            (6_542_800, None),
        ] {
            assert_eq!(CronosEra::at_height(height), era);
        }
        assert_eq!(CronosEra::BeforeHuygen.spec(None).unwrap(), SpecId::BERLIN);
        assert!(CronosEra::BeforeHuygen.spec(Some(0)).is_err());
        assert_eq!(CronosEra::Huygen.spec(Some(0)).unwrap(), SpecId::LONDON);
        assert!(CronosEra::Huygen.spec(None).is_err());
    }
}
