//! Historical Ethermint replay across Cronos mainnet binary upgrades.
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
        Address, U256,
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

    fn first_frame_input(
        &mut self,
        evm: &mut E,
        gas: &mut revm::interpreter::GasTracker,
    ) -> Result<Option<revm::interpreter::interpreter_action::FrameInit>, Self::Error> {
        modern::first_frame_input(evm, gas)
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

mod galileo;
mod modern;
mod pallene;
mod releases;
pub use releases::CronosRelease;

/// Cronos mainnet genesis sets london_block to 2^63. The v0.7.0
/// upgrade handler sets it to zero and enables the fee market at Huygen.
/// Subsequent revisions follow the mainnet EVM module migrations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CronosEra {
    BeforeHuygen,
    Huygen,
    HuygenHotfix,
    Galileo,
    FeeMarket,
    Titan,
    V12,
    V13,
    Pallene,
    V15,
    V16,
    V17,
}

impl CronosEra {
    pub fn at_height(height: u64) -> Option<Self> {
        CronosRelease::at_height(height)?.rules
    }

    pub fn version(self) -> &'static str {
        match self {
            Self::Galileo => "ethermint-galileo-v1",
            Self::FeeMarket => "ethermint-feemarket-v1",
            Self::Titan => "ethermint-titan-v1",
            Self::V12 => "ethermint-v12-v1",
            Self::V13 => "ethermint-v13-v1",
            Self::Pallene => "ethermint-pallene-v1",
            Self::V15 => "ethermint-v15-v1",
            Self::V16 => "ethermint-v16-v1",
            Self::V17 => "ethermint-v17-v1",
            Self::BeforeHuygen => "ethermint-transient-v4",
            Self::Huygen => "ethermint-huygen-v5",
            Self::HuygenHotfix => "ethermint-huygen-hotfix-v5",
        }
    }

    fn block_gas_limit(self, height: u64, consensus_limit: u64) -> u64 {
        // The SDK rebuilds the meter after Titan's BeginBlock upgrade. Comet
        // exposes the updated consensus parameter starting at the next height.
        if self == Self::Titan && Self::at_height(height - 1) != Some(self) {
            return 60_000_000;
        }
        // The SDK used by Galileo through v1.3 returns MaxUint64 from
        // InfiniteGasMeter.Limit(). Pallene reads the consensus params directly.
        if consensus_limit == 0
            && matches!(
                self,
                Self::Galileo | Self::FeeMarket | Self::Titan | Self::V12 | Self::V13
            )
        {
            return u64::MAX;
        }
        consensus_limit
    }

    pub fn spec(self, base_fee: Option<u64>) -> Result<SpecId> {
        let spec = match self {
            Self::BeforeHuygen => SpecId::BERLIN,
            Self::Huygen | Self::HuygenHotfix | Self::Galileo | Self::FeeMarket => SpecId::LONDON,
            Self::Titan | Self::V12 | Self::V13 | Self::Pallene => SpecId::SHANGHAI,
            Self::V15 | Self::V16 | Self::V17 => SpecId::PRAGUE,
        };
        ensure!(
            base_fee.is_some() == (spec >= SpecId::LONDON),
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

#[derive(Debug)]
pub struct CronosRuleGap(pub &'static str);
impl std::fmt::Display for CronosRuleGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[derive(Debug)]
pub enum CronosNativeCall {
    Precompile(alloy::primitives::Address),
    Hook(alloy::primitives::B256),
}
impl std::fmt::Display for CronosNativeCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Precompile(addr) => write!(f, "Cosmos state required by precompile {addr}"),
            Self::Hook(topic) => write!(f, "Cosmos state required by post-execution hook {topic}"),
        }
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

#[derive(Debug, Default)]
pub struct CronosExecutionParams {
    pub min_gas_multiplier_atomics: u64,
    pub history_serve_window: u64,
}

fn adjust_balance<D: DatabaseRef>(
    db: &mut CacheDB<D>,
    address: Address,
    amount: U256,
    credit: bool,
) -> Result<()> {
    if amount.is_zero() {
        return Ok(());
    }
    let mut info = db
        .load_account(address)
        .context(CronosArchiveRead)?
        .info
        .clone();
    info.balance = if credit {
        info.balance.checked_add(amount)
    } else {
        info.balance.checked_sub(amount)
    }
    .with_context(|| format!("cannot settle {amount} basecro at {address}"))?;
    // insert_account_info also makes a newly funded address visible to the EVM.
    db.insert_account_info(address, info);
    Ok(())
}

pub fn replay_cronos_ethermint<D: DatabaseRef + Debug>(
    parent: D,
    block: &Block,
    receipts: &[TransactionReceipt],
    gas_limit: u64,
    message_gas: &[u64],
    parameters: &CronosExecutionParams,
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
    let gas_limit = era.block_gas_limit(header.number, gas_limit);
    let transient = era == CronosEra::BeforeHuygen;
    let spec = era.spec(header.base_fee_per_gas)?;
    let mut cfg = CfgEnv::new_with_spec(spec).with_chain_id(25);
    // A zero GASLIMIT can still represent an unbounded SDK meter.
    cfg.disable_block_gas_limit = gas_limit == 0;
    let genesis_eips = matches!(
        era,
        CronosEra::BeforeHuygen | CronosEra::Huygen | CronosEra::HuygenHotfix
    );
    // DeliverTx escrows the effective gas price; CheckTx separately checks the cap.
    cfg.disable_balance_check = !genesis_eips;
    cfg.disable_eip7623 = spec == SpecId::PRAGUE;
    if genesis_eips {
        cfg.gas_params.override_gas([
            // The genesis extra EIP-2929 reinstalls geth's refund-enabled
            // SELFDESTRUCT gas function after the London jump table is selected.
            (GasId::selfdestruct_refund(), 24_000),
            (GasId::sstore_static(), 800),
            (GasId::sstore_set_without_load_cost(), 19_200),
            (GasId::sstore_reset_without_cold_load_cost(), 4_200),
            (GasId::sstore_set_refund(), 19_200),
            (GasId::sstore_reset_refund(), 4_200),
            (GasId::cold_storage_cost(), 0),
            (GasId::sstore_clearing_slot_refund(), 15_000),
        ]);
    }
    let env = EvmEnv::new(
        cfg,
        BlockEnv {
            number: U256::from(header.number),
            timestamp: U256::from(header.timestamp),
            beneficiary: header.beneficiary,
            gas_limit,
            difficulty: U256::ZERO,
            basefee: header.base_fee_per_gas.unwrap_or_default(),
            prevrandao: (spec >= SpecId::SHANGHAI).then_some(Default::default()),
            ..Default::default()
        },
    );
    let mut db = CacheDB::new(parent);
    modern::prepare_block_hash(&mut db, header.number);
    if era == CronosEra::V17 {
        modern::prepare_history(
            &mut db,
            header.number,
            header.hash,
            parameters.history_serve_window,
        )?;
    }
    pallene::apply_upgrade(header.number, &mut db)?;
    for &(address, credit, amount) in begin_transfers {
        adjust_balance(&mut db, address, amount, credit).context("BeginBlock transfer")?;
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
        if matches!(era, CronosEra::V15 | CronosEra::V16)
            && envelope.to().is_none()
            && !db
                .load_account(tx.inner.signer())
                .context(CronosArchiveRead)?
                .info
                .is_empty_code_hash()
        {
            return Err(anyhow!("tx {index} {hash}")
                .context(CronosRuleGap("delegated sender creation nonce")));
        }
        let escrow = U256::from(envelope.gas_limit())
            * U256::from(envelope.effective_gas_price(header.base_fee_per_gas));
        if !genesis_eips {
            let balance = db
                .load_account(tx.inner.signer())
                .context(CronosArchiveRead)?
                .info
                .balance;
            ensure!(
                balance >= escrow && balance >= envelope.value(),
                "insufficient funds at ante for tx {hash}"
            );
            if balance - escrow < envelope.value() {
                return Err(anyhow!("tx {index} {hash}").context(CronosRuleGap(
                    "value transfer underfunded after effective fee escrow",
                )));
            }
        }
        adjust_balance(&mut db, collector, escrow, true)?;
        let mut evm = EthEvmFactory::default()
            .create_evm_with_inspector(
                &mut db,
                env.clone(),
                PipelineInspector::<true>::configured(),
            )
            .into_inner();
        // revm combines the warm dynamic SLOAD charge with its base gas.
        if genesis_eips {
            evm.instruction.insert_gas(opcode::SLOAD, 900);
            evm.instruction.insert_gas(opcode::BALANCE, 700);
            evm.instruction.insert_gas(opcode::EXTCODEHASH, 700);
            evm.instruction.insert_instruction(
                opcode::SSTORE,
                revm::interpreter::Instruction::new(ethermint_sstore),
                0,
            );
        }
        evm.ctx.tx = (&tx.inner).into_tx_env();
        let mut handler = EthermintHandler {
            warm,
            marker: PhantomData,
        };
        let result = handler.inspect_run(&mut evm).map_err(|e| match e {
            EVMError::Database(source) => anyhow::Error::new(source).context(CronosArchiveRead),
            other => anyhow!("tx {index} {hash}: {other:?}"),
        })?;
        if era == CronosEra::Titan && evm.inspector.ran_prevrandao {
            return Err(anyhow!("tx {index} {hash}")
                .context(CronosRuleGap("Titan PREVRANDAO with nil Random")));
        }
        if spec >= SpecId::SHANGHAI {
            if let Some(target) = evm.inspector.cronos_native_call {
                return Err(
                    anyhow!("tx {index} {hash}").context(CronosNativeCall::Precompile(target))
                );
            }
        }
        if spec == SpecId::PRAGUE {
            if evm.inspector.ran_blobbasefee {
                return Err(anyhow!("tx {index} {hash}").context(CronosRuleGap(
                    "BLOBBASEFEE with an unset Ethermint blob base fee",
                )));
            }
            if envelope.authorization_list().is_some() && !result.is_success() {
                return Err(anyhow!("tx {index} {hash}").context(CronosRuleGap(
                    "failed authorization transaction state rollback",
                )));
            }
        }
        if let Some(topic) = result
            .logs()
            .iter()
            .map(|log| &log.data)
            .chain(receipt.logs().iter().map(|log| &log.inner.data))
            .filter_map(|data| data.topics().first())
            .find(|topic| pallene::has_native_hook(era, **topic))
        {
            return Err(anyhow!("tx {index} {hash}").context(CronosNativeCall::Hook(*topic)));
        }
        warm = handler.warm;
        let state = evm.ctx.journaled_state.finalize();
        if matches!(era, CronosEra::BeforeHuygen | CronosEra::Huygen)
            && state.values().any(|account| account.is_selfdestructed())
        {
            return Err(anyhow!("tx {index} {hash}").context(LegacySelfdestruct));
        }
        if state
            .values()
            .any(|account| account.is_selfdestructed() && !account.info.balance.is_zero())
        {
            return Err(anyhow!("tx {index} {hash}")
                .context(CronosRuleGap("balance received after SELFDESTRUCT")));
        }
        ensure!(
            result.is_success() == receipt.status(),
            "tx {index} {hash} status: replay {}, receipt {}",
            result.is_success(),
            receipt.status()
        );
        let net = result.tx_gas_used();
        let settled = if genesis_eips {
            // v0.6 discards the unused-gas refund when execution reverts.
            let charged = if transient && !result.is_success() {
                envelope.gas_limit()
            } else {
                net
            };
            galileo::SettledGas {
                cosmos: net,
                charged,
                receipt: charged,
            }
        } else {
            let basis = if matches!(era, CronosEra::Galileo | CronosEra::FeeMarket) {
                galileo::GasBasis::AfterRefund
            } else {
                galileo::GasBasis::BeforeRefund
            };
            basis.settle(
                envelope.gas_limit(),
                result.gas().total_gas_spent(),
                net,
                parameters.min_gas_multiplier_atomics,
            )?
        };
        ensure!(
            settled.cosmos == message_gas[index],
            "tx {index} {hash} execution gas: replay {}, Comet {}",
            settled.cosmos,
            message_gas[index]
        );
        ensure!(
            settled.receipt == receipt.gas_used,
            "tx {index} {hash} receipt gas: replay {}, receipt {}",
            settled.receipt,
            receipt.gas_used
        );
        ensure!(
            result
                .logs()
                .iter()
                .eq(receipt.logs().iter().map(|log| &log.inner)),
            "tx {index} {hash} committed logs differ"
        );
        let (mut arena, stores) = evm.inspector.into_traces();
        if let Some(root) = arena.nodes_mut().first_mut() {
            root.trace.gas_used = settled.cosmos;
            root.trace.gas_limit = envelope.gas_limit();
        }
        append_tx_traces(
            hash,
            &arena,
            &stores,
            &mut log_index,
            &mut BTreeSet::new(),
            &mut file,
        );
        let mut transaction = pipeline_transaction(
            envelope,
            tx.inner.signer(),
            &result,
            envelope.effective_gas_price(header.base_fee_per_gas),
            index,
        );
        transaction.gas_used = settled.cosmos.try_into()?;
        if envelope.to().is_none() {
            transaction.to_addr = super::types::addr_hex(&Address::ZERO);
        }
        // The Cosmos writer only populates cap fields for a dynamic-fee (type 2) tx.
        if envelope.authorization_list().is_some() {
            transaction.gas_price = envelope.max_fee_per_gas();
            transaction.max_fee_per_gas = 0;
            transaction.max_priority_fee_per_gas = 0;
        }
        file.txs.push(transaction);
        for (address, account) in &state {
            if account.changed_storage_slots().next().is_some() {
                storage.insert(*address);
            }
            if transient && !account.is_cold_transaction_id(TransactionId::ZERO) {
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
        let price = U256::from(envelope.effective_gas_price(header.base_fee_per_gas));
        let refund = U256::from(envelope.gas_limit() - settled.charged) * price;
        adjust_balance(
            &mut db,
            tx.inner.signer(),
            U256::from(settled.charged - net) * price,
            false,
        )
        .with_context(|| format!("sender settlement for {hash}"))?;
        adjust_balance(&mut db, collector, refund, false)
            .with_context(|| format!("fee collector settlement for {hash}"))?;
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
    fn sdk_block_meter_and_titan_upgrade() {
        for (height, input, output) in [
            (1, 0, 0),
            (6_542_800, 0, u64::MAX),
            (13_184_000, 40_000_000, 60_000_000),
            (13_184_001, 0, u64::MAX),
            (17_154_999, 0, u64::MAX),
            (17_155_000, 0, 0),
        ] {
            assert_eq!(
                CronosEra::at_height(height)
                    .unwrap()
                    .block_gas_limit(height, input),
                output
            );
        }
    }

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
            (6_542_800, Some(CronosEra::Galileo)),
        ] {
            assert_eq!(CronosEra::at_height(height), era);
        }
        assert_eq!(CronosEra::BeforeHuygen.spec(None).unwrap(), SpecId::BERLIN);
        assert!(CronosEra::BeforeHuygen.spec(Some(0)).is_err());
        assert_eq!(CronosEra::Huygen.spec(Some(0)).unwrap(), SpecId::LONDON);
        assert!(CronosEra::Huygen.spec(None).is_err());
    }
}
