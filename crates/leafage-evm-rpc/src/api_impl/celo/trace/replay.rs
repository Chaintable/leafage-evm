//! Replay Celo L1 transactions and finalization, sharing BSC's state-change
//! collection and encoding. Consensus calls affect StateDiff, not tx traces.
use super::super::l1::ArchiveChain;
use super::{
    fees::{abi, word, CeloTransactionHandler, System},
    input::BlockInput,
};
use crate::pipeline::{
    build::{pipeline_block, pipeline_header},
    chain::ReplayOptions,
    changes::RecordingDb,
    convert::append_celo_tx_traces,
    inspector::PipelineInspector,
    types::{addr_hex, hash_hex, BlockFile, DebankBlockOutput, PipelineTransaction},
};
use alloy::{
    primitives::{keccak256, Address, U256},
    rpc::types::Block,
};
use anyhow::{ensure, Context as _, Result};
use leafage_evm_chains::celo::l1::{consensus::CeloHeader, evm::create_celo_l1_evm, L1Fork};
use leafage_evm_storage::{BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockInfo, MainnetSpecId, NewAccount};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::JournalTr,
    database::CacheDB,
    inspector::{InspectorHandler, NoOpInspector},
    primitives::TxKind,
    DatabaseCommit, DatabaseRef, ExecuteEvm,
};
use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap},
    marker::PhantomData,
};

pub(super) fn environment(h: &alloy::rpc::types::Header) -> BlockEnv {
    BlockEnv {
        number: U256::from(h.number),
        beneficiary: h.beneficiary,
        timestamp: U256::from(h.timestamp),
        gas_limit: h.gas_limit,
        basefee: h.base_fee_per_gas.unwrap_or_default(),
        difficulty: h.difficulty,
        prevrandao: None,
        blob_excess_gas_and_price: None,
        ..Default::default()
    }
}
pub(super) fn config() -> CfgEnv<MainnetSpecId> {
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::LONDON);
    cfg.chain_id = 42220;
    cfg.disable_eip3607 = true;
    cfg.disable_fee_charge = true;
    cfg.disable_base_fee = true;
    cfg.tx_gas_limit_cap = None;
    cfg
}

pub(super) fn replay<DB>(
    db: &DB,
    info: &BlockInfo,
    parent: Option<&BlockInfo>,
    input: BlockInput,
    options: ReplayOptions,
) -> Result<DebankBlockOutput>
where
    DB: BlockIndex + EvmStorageRead,
{
    let number = input.header.number;
    let fork = L1Fork::at(number).map_err(anyhow::Error::msg)?;
    let mut block_file = BlockFile {
        block: pipeline_block(&input.header),
        ..Default::default()
    };
    block_file.block.process_start_timestamp = 0;
    let rpc_block = Block {
        header: input.header.clone(),
        ..Default::default()
    };
    let header = pipeline_header(&rpc_block);
    let parent = parent.unwrap_or(info);
    ensure!(
        number == 0 || info.header.parent_hash == parent.header.hash,
        "archived parent mismatch"
    );
    let state = db
        .state_at(BlockId::Hash(parent.header.hash.into()))?
        .context("parent state is not in the archive")?;
    let parent_state = EvmStorageWrapper {
        db: state,
        ovm_address: None,
        normalize_state_key: false,
    };
    let mut cache = RecordingDb::new(
        CacheDB::new(parent_state.clone()),
        options.state_diff || options.check_state,
        true,
    );
    let chain = ArchiveChain(db);
    let system = System {
        fork,
        chain: &chain,
    };
    let mut block = environment(&input.header);

    // Read pre-Gingerbread metadata on the parent state, as the native
    // blockfile producer does. It was not part of the legacy header.
    if fork < L1Fork::Gingerbread {
        let parent_fork = L1Fork::at(parent.header.number).map_err(anyhow::Error::msg)?;
        let mut evm = create_celo_l1_evm(
            environment(&parent.header),
            config(),
            &mut cache,
            NoOpInspector,
            &chain,
        )
        .map_err(anyhow::Error::msg)?;
        evm.inner
            .ctx
            .journaled_state
            .set_spec_id(parent_fork.spec());
        let parent_system = System {
            fork: parent_fork,
            chain: &chain,
        };
        block_file.block.base_fee_per_gas = parent_system
            .minimum(&mut evm.inner.ctx, None)?
            .try_into()
            .context("base fee exceeds u128")?;
        block.gas_limit = parent_system
            .scalar(
                &mut evm.inner.ctx,
                "BlockchainParameters",
                "blockGasLimit()",
                &[],
                40_000,
                U256::from(20_000_000),
            )?
            .try_into()
            .context("block gas limit exceeds u64")?;
        // Native RPC uses its historical mainnet table, including the period
        // before BlockchainParameters was deployed.
        block_file.block.gas_limit = match number {
            0..3317 => 20_000_000,
            3317..3_251_772 => 10_000_000,
            3_251_772..6_137_285 => 13_000_000,
            6_137_285..13_562_578 => 20_000_000,
            13_562_578..14_137_511 => 50_000_000,
            14_137_511..21_355_415 => 20_000_000,
            _ => 32_000_000,
        };
    }
    if number == 0 {
        // Celo mainnet's genesis allocation has storage only at Registry.
        // Native blockfiles expose that allocation without synthetic txs.
        if number == 0 {
            block_file.storage_contracts =
                vec!["0x000000000000000000000000000000000000ce10".into()];
        }
        let validation_hash = block_file.validation_hash();
        return Ok(DebankBlockOutput {
            block_file,
            header,
            state_diff: options
                .state_diff
                .then(|| super::genesis::state_diff(&parent_state, &input.header))
                .transpose()?
                .map(|diff| alloy::rlp::encode(diff).into()),
            validation_hash,
        });
    }
    // ApplyBlockRandomnessTx precedes every primary transaction.
    {
        let mut evm =
            create_celo_l1_evm(block.clone(), config(), &mut cache, NoOpInspector, &chain)
                .map_err(anyhow::Error::msg)?;
        evm.inner.ctx.journaled_state.set_spec_id(fork.spec());
        if let Some(random) = system.registered(&mut evm.inner.ctx, "Random")? {
            let proposer = CeloHeader::from_header(&input.header)
                .proposer()
                .map_err(anyhow::Error::msg)?;
            let result = system.call(
                &mut evm.inner.ctx,
                random,
                abi(
                    "revealAndCommit(bytes32,bytes32,address)",
                    &[
                        U256::from_be_bytes(input.randomness.revealed.0),
                        U256::from_be_bytes(input.randomness.committed.0),
                        word(proposer),
                    ],
                ),
                2_000_000,
                false,
                false,
            )?;
            ensure!(
                result.result.is_ok(),
                "Random.revealAndCommit failed: {:?}",
                result.result
            );
            let state = evm.finalize();
            evm.inner.ctx.journaled_state.database.commit(state);
        }
    }
    // Espresso freezes fee minima and intrinsic surcharge at block start.
    let mut minima = HashMap::new();
    let mut alternative_intrinsic = 0;
    if fork >= L1Fork::Espresso {
        let mut evm =
            create_celo_l1_evm(block.clone(), config(), &mut cache, NoOpInspector, &chain)
                .map_err(anyhow::Error::msg)?;
        evm.inner.ctx.journaled_state.set_spec_id(fork.spec());
        for currency in input.transactions.iter().map(|tx| tx.fee_currency) {
            if let std::collections::hash_map::Entry::Vacant(entry) = minima.entry(currency) {
                entry.insert(system.minimum(&mut evm.inner.ctx, currency)?);
            }
        }
        if input
            .transactions
            .iter()
            .any(|tx| tx.fee_currency.is_some())
        {
            alternative_intrinsic = system.alternative_intrinsic(&mut evm.inner.ctx)?;
        }
    }
    let mut storage = BTreeSet::new();
    let mut cumulative = 0u64;
    for (index, tx) in input.transactions.iter().enumerate() {
        let receipt = &input.receipts[index];
        let mut evm = create_celo_l1_evm(
            block.clone(),
            config(),
            &mut cache,
            PipelineInspector::celo(),
            &chain,
        )
        .map_err(anyhow::Error::msg)?;
        evm.inner.ctx.journaled_state.set_spec_id(fork.spec());
        let minimum = if fork >= L1Fork::Espresso {
            minima[&tx.fee_currency]
        } else {
            system.minimum(&mut evm.inner.ctx, tx.fee_currency)?
        };
        let surcharge = if tx.fee_currency.is_none() {
            0
        } else if fork >= L1Fork::Espresso {
            alternative_intrinsic
        } else {
            system.alternative_intrinsic(&mut evm.inner.ctx)?
        };
        let price = tx.price(minimum)?;
        ensure!(
            tx.gas.to::<u64>() <= block.gas_limit.saturating_sub(cumulative),
            "transaction exceeds remaining block gas"
        );
        evm.inner.ctx.tx = TxEnv {
            caller: tx.from,
            kind: tx.to.map(TxKind::Call).unwrap_or(TxKind::Create),
            gas_limit: tx.gas.to(),
            gas_price: price,
            gas_priority_fee: None,
            nonce: tx.nonce.to(),
            value: tx.value,
            data: tx.input.clone(),
            chain_id: Some(42220),
            tx_type: if tx.kind.is_zero() { 0 } else { 1 },
            access_list: tx.access_list.clone(),
            ..Default::default()
        };
        let mut handler = CeloTransactionHandler {
            system: System {
                fork,
                chain: &chain,
            },
            tx,
            minimum,
            price,
            alternative_intrinsic: surcharge,
            debit_refund: Cell::new(0),
            marker: PhantomData,
        };
        let result = handler
            .inspect_run(&mut evm)
            .with_context(|| format!("transaction {index} {}", tx.hash))?;
        let used = result.tx_gas_used();
        if used != receipt.gas_used.to::<u64>() {
            tracing::warn!(target: "trace", tx = %tx.hash, replay_gas = used,
                receipt_gas = receipt.gas_used.to::<u64>(), calls = ?evm.inner.inspector.call_gas(),
                "Celo transaction gas mismatch");
        }
        cumulative = cumulative
            .checked_add(used)
            .context("cumulative gas overflow")?;
        ensure!(
            used == receipt.gas_used.to::<u64>()
                && cumulative == receipt.cumulative_gas_used.to::<u64>(),
            "transaction {index} {} gas mismatch: replay {used}/{cumulative}, receipt {}/{}",
            tx.hash,
            receipt.gas_used,
            receipt.cumulative_gas_used
        );
        ensure!(
            result.is_success() == (receipt.status.to::<u64>() == 1),
            "transaction {index} status mismatch"
        );
        let expected_logs: Vec<_> = receipt.logs.iter().map(|l| l.inner.clone()).collect();
        ensure!(
            result.logs() == expected_logs,
            "transaction {index} {} receipt logs mismatch",
            tx.hash
        );
        let state = evm.finalize();
        evm.inner.ctx.journaled_state.database.commit(state);
        let (arena, sstore, positions) = evm.inner.inspector.into_celo_traces();
        append_celo_tx_traces(
            &tx.hash,
            &arena,
            &sstore,
            &positions,
            &receipt.logs,
            &mut storage,
            &mut block_file,
        )?;
        block_file.txs.push(PipelineTransaction {
            id: hash_hex(&tx.hash),
            from_addr: addr_hex(&tx.from),
            to_addr: addr_hex(&tx.to.or(receipt.contract_address).unwrap_or(Address::ZERO)),
            gas_limit: tx.gas.to::<u64>() as i64,
            gas_price: price,
            gas_used: used as i64,
            status: result.is_success(),
            max_fee_per_gas: tx
                .max_fee_per_gas
                .unwrap_or_default()
                .try_into()
                .context("gas cap exceeds u128")?,
            max_priority_fee_per_gas: tx
                .max_priority_fee_per_gas
                .unwrap_or_default()
                .try_into()
                .context("tip cap exceeds u128")?,
            input: tx.input.clone(),
            nonce: tx.nonce.to::<u64>() as i64,
            idx: index as i64,
            value: tx.value,
        });
    }
    ensure!(cumulative == input.header.gas_used, "block gas mismatch");
    if options.state_diff || options.check_state {
        super::finalize::finalize(
            &mut cache,
            &chain,
            &input,
            &parent_state,
            &parent.header,
            block,
        )?;
    }
    if let Some(error) = cache.error.take() {
        return Err(error.into());
    }
    let state_diff = if let Some(changes) = &cache.changes {
        let post = EvmStorageWrapper {
            db: db
                .state_at(BlockId::Hash(info.header.hash.into()))?
                .context("post state is not in the archive")?,
            ovm_address: None,
            normalize_state_key: false,
        };
        if options.check_state {
            changes.verify(&post)?;
        }
        if options.state_diff {
            let mut diff = changes.to_storage_diff(
                &cache.db,
                Some(&parent_state),
                info.header.state_root,
                parent.header.state_root,
            )?;
            // Native Celo's archive diff also lists account leaves whose only
            // change was the storage root. Keep the shared BSC net-change
            // collector and adapt only this serialization convention.
            let addresses: HashMap<_, _> = cache
                .db
                .cache
                .accounts
                .keys()
                .map(|address| (keccak256(address), *address))
                .collect();
            diff.storage_diffs
                .retain(|s| !diff.deleted_accounts.contains(&s.address));
            for storage in &diff.storage_diffs {
                if diff
                    .new_accounts
                    .iter()
                    .any(|a| a.address == storage.address)
                {
                    continue;
                }
                let address = addresses
                    .get(&storage.address)
                    .context("changed account preimage missing")?;
                let account = cache
                    .db
                    .basic_ref(*address)?
                    .context("changed account missing")?;
                diff.new_accounts.push(NewAccount {
                    address: storage.address,
                    balance: account.balance,
                    nonce: account.nonce,
                    code_hash: account.code_hash,
                });
            }
            diff.new_accounts.sort_by_key(|a| a.address);
            Some(alloy::rlp::encode(diff).into())
        } else {
            None
        }
    } else {
        None
    };
    block_file.storage_contracts = storage.iter().map(addr_hex).collect();
    let validation_hash = block_file.validation_hash();
    Ok(DebankBlockOutput {
        block_file,
        header,
        state_diff,
        validation_hash,
    })
}
