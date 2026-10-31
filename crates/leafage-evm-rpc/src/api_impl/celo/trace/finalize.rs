//! Native Istanbul Finalize after transaction replay. The state commits to
//! BSC's RecordingDb; these calls never enter the transaction blockfile.
use super::{
    fees::{word, ContractFailure, System},
    input::BlockInput,
    replay::{config, environment},
};
use crate::pipeline::changes::RecordingDb;
use alloy::primitives::{Address, U256};
use anyhow::{ensure, Context as _, Result};
use leafage_evm_chains::celo::l1::{
    consensus::{epoch_uptimes, CeloChainData},
    evm::{create_celo_l1_evm, CeloL1Context},
    L1Fork, EPOCH_SIZE,
};
use revm::{
    context::BlockEnv, context_interface::JournalTr, inspector::NoOpInspector, Database,
    DatabaseCommit, DatabaseRef, ExecuteEvm,
};
use std::collections::HashMap;

fn words(data: &[u8]) -> Result<Vec<U256>> {
    ensure!(
        data.len() % 32 == 0,
        ContractFailure("invalid system ABI result length".into())
    );
    Ok(data.chunks_exact(32).map(U256::from_be_slice).collect())
}
fn scalar(data: &[u8]) -> Result<U256> {
    ensure!(
        data.len() >= 32,
        ContractFailure("missing system ABI result".into())
    );
    Ok(U256::from_be_slice(&data[..32]))
}
fn address(data: &[u8]) -> Result<Address> {
    let value = scalar(data)?;
    ensure!(
        value.bit_len() <= 160,
        ContractFailure("invalid system ABI address".into())
    );
    Ok(Address::from_slice(&value.to_be_bytes::<32>()[12..]))
}
fn array(data: &[U256], index: usize) -> Result<Vec<U256>> {
    let offset: usize = data
        .get(index)
        .context("missing ABI array offset")?
        .try_into()
        .context("ABI offset overflow")?;
    ensure!(offset % 32 == 0, "invalid ABI array offset");
    let count: usize = data
        .get(offset / 32)
        .context("missing ABI array length")?
        .try_into()
        .context("ABI array length overflow")?;
    let start = offset / 32 + 1;
    let end = start
        .checked_add(count)
        .context("ABI array length overflow")?;
    Ok(data
        .get(start..end)
        .context("truncated ABI array")?
        .to_vec())
}
fn to_address(value: U256) -> Address {
    Address::from_slice(&value.to_be_bytes::<32>()[12..])
}

fn attempt<DB: Database>(
    ctx: &mut CeloL1Context<DB>,
    name: &str,
    f: impl FnOnce(&mut CeloL1Context<DB>) -> Result<()>,
) -> Result<()> {
    let checkpoint = ctx.journaled_state.checkpoint();
    match f(ctx) {
        Ok(()) => ctx.journaled_state.checkpoint_commit(),
        Err(error) => {
            ctx.journaled_state.checkpoint_revert(checkpoint);
            if !error.is::<ContractFailure>() {
                return Err(error.context(name.to_string()));
            }
            tracing::debug!(target: "trace", name, %error, "Celo finalization reverted");
        }
    }
    Ok(())
}

pub(super) fn finalize<D>(
    db: &mut RecordingDb<D>,
    chain: &dyn CeloChainData,
    input: &BlockInput,
    parent: &D,
    parent_header: &alloy::rpc::types::Header,
    block: BlockEnv,
) -> Result<()>
where
    D: DatabaseRef,
    D::Error: Send + Sync + 'static,
{
    let fork = L1Fork::at(input.header.number).map_err(anyhow::Error::msg)?;
    let system = System { fork, chain };
    let mut evm = create_celo_l1_evm(block, config(), db, NoOpInspector, chain)
        .map_err(anyhow::Error::msg)?;
    evm.inner.ctx.journaled_state.set_spec_id(fork.spec());
    let ctx = &mut evm.inner.ctx;
    attempt(ctx, "initial supply", |ctx| {
        let supply = scalar(&system.required(ctx, "GoldToken", "totalSupply()", &[], 50_000)?)?;
        if supply.is_zero() {
            // Sum of the 32 accounts in native mainnetAllocJSON.
            let supply = U256::from(600_000_000u64) * U256::from(10).pow(U256::from(18));
            system.execute(
                ctx,
                "GoldToken",
                "increaseSupply(uint256)",
                &[supply],
                50_000,
            )?;
        }
        Ok(())
    })?;
    if fork < L1Fork::Gingerbread {
        attempt(ctx, "gas price minimum", |ctx| {
            let gas_limit = system.scalar(
                ctx,
                "BlockchainParameters",
                "blockGasLimit()",
                &[],
                40_000,
                U256::from(20_000_000),
            )?;
            system.execute(
                ctx,
                "GasPriceMinimum",
                "updateGasPriceMinimum(uint256,uint256)",
                &[U256::from(input.header.gas_used), gas_limit],
                2_000_000,
            )?;
            Ok(())
        })?;
    }
    if input.header.number % EPOCH_SIZE == 0 {
        attempt(ctx, "epoch rewards", |ctx| {
            epoch(ctx, &system, input.header.number, parent, parent_header)
        })?;
    }
    let expected: Vec<_> = input
        .receipts
        .get(input.transactions.len())
        .into_iter()
        .flat_map(|receipt| receipt.logs.iter().map(|log| log.inner.clone()))
        .collect();
    ensure!(
        ctx.journaled_state.logs == expected,
        "Celo finalization receipt logs mismatch"
    );
    let state = evm.finalize();
    evm.inner.ctx.journaled_state.database.commit(state);
    Ok(())
}

fn mint<DB: Database>(
    ctx: &mut CeloL1Context<DB>,
    system: &System<'_>,
    to: Address,
    amount: U256,
) -> Result<()> {
    if !amount.is_zero() {
        system.execute(
            ctx,
            "GoldToken",
            "mint(address,uint256)",
            &[word(to), amount],
            5_000_000,
        )?;
    }
    Ok(())
}

fn epoch<DB, D>(
    ctx: &mut CeloL1Context<DB>,
    system: &System<'_>,
    number: u64,
    parent: &D,
    parent_header: &alloy::rpc::types::Header,
) -> Result<()>
where
    DB: Database,
    D: DatabaseRef,
    D::Error: Send + Sync + 'static,
{
    if system.fork < L1Fork::Gingerbread {
        if let Some(rewards) = system.registered(ctx, "EpochRewards")? {
            if system.scalar(
                ctx,
                "Freezer",
                "isFrozen(address)",
                &[word(rewards)],
                20_000,
                U256::ZERO,
            )? != U256::ZERO
            {
                return Ok(());
            }
        }
    }
    let reserve = system.address(ctx, "Reserve")?;
    let stable = system.address(ctx, "StableToken")?;
    let partner = address(&system.required(
        ctx,
        "EpochRewards",
        "carbonOffsettingPartner()",
        &[],
        20_000,
    )?)?;
    system.execute(
        ctx,
        "EpochRewards",
        "updateTargetVotingYield()",
        &[],
        2_000_000,
    )?;
    let rewards = words(&system.required(
        ctx,
        "EpochRewards",
        "calculateTargetEpochRewards()",
        &[],
        2_000_000,
    )?)?;
    ensure!(
        rewards.len() == 4,
        ContractFailure("invalid epoch rewards result".into())
    );
    let mut lookback = 12;
    if L1Fork::at(number - EPOCH_SIZE + 1).map_err(anyhow::Error::msg)? >= L1Fork::Donut {
        lookback = system
            .scalar(
                ctx,
                "BlockchainParameters",
                "getUptimeLookbackWindow()",
                &[],
                40_000,
                U256::from(12),
            )?
            .try_into()
            .context("uptime window exceeds u64")?;
        lookback = lookback.clamp(3, 720);
    }
    let validators = epoch_uptimes(system.chain, number, lookback).map_err(anyhow::Error::msg)?;
    ensure!(!validators.is_empty(), "empty epoch validator set");
    for (validator, uptime) in &validators {
        system.execute(
            ctx,
            "Validators",
            "updateValidatorScoreFromSigner(address,uint256)",
            &[word(*validator), *uptime],
            1_000_000,
        )?;
    }
    let mut validator_rewards = U256::ZERO;
    for (validator, _) in &validators {
        // Native continues after an individual payment reverts.
        match system.execute(
            ctx,
            "Validators",
            "distributeEpochPaymentsFromSigner(address,uint256)",
            &[word(*validator), rewards[0]],
            1_000_000,
        ) {
            Ok(output) => {
                validator_rewards = validator_rewards
                    .checked_add(scalar(&output)?)
                    .context("validator rewards overflow")?;
            }
            Err(error) if error.is::<ContractFailure>() => {}
            Err(error) => return Err(error),
        }
    }
    // Native NewEVMRunnerForCurrentBlock uses the parent while importing
    // this block. Query the historical parent, never the node's live head.
    let parent_fork = L1Fork::at(parent_header.number).map_err(anyhow::Error::msg)?;
    let mut parent_evm = create_celo_l1_evm(
        environment(parent_header),
        config(),
        revm::database::WrapDatabaseRef(parent),
        NoOpInspector,
        system.chain,
    )
    .map_err(anyhow::Error::msg)?;
    parent_evm
        .inner
        .ctx
        .journaled_state
        .set_spec_id(parent_fork.spec());
    let rate = match (System {
        fork: parent_fork,
        chain: system.chain,
    })
    .query(
        &mut parent_evm.inner.ctx,
        "SortedOracles",
        "medianRate(address)",
        &[word(stable)],
        100_000,
    ) {
        Ok(Some(data)) => words(&data)?,
        // Native currency.GetExchangeRate uses 1:1 when the oracle call
        // or its registry lookup fails; a returned zero rate is an error.
        Ok(None) => vec![U256::from(1), U256::from(1)],
        Err(error) => return Err(error),
    };
    ensure!(
        rate.len() == 2 && !rate[0].is_zero() && !rate[1].is_zero(),
        ContractFailure("invalid stable token exchange rate".into())
    );
    let converted = validator_rewards.widening_mul::<256, 4, 512, 8>(rate[1])
        / rate[0].to::<alloy::primitives::U512>();
    ensure!(
        converted.bit_len() <= 256,
        "converted validator rewards overflow"
    );
    mint(ctx, system, reserve, converted.to::<U256>())?;
    let governance = system.address(ctx, "Governance")?;
    let low = system.fork < L1Fork::Gingerbread
        && scalar(&system.required(ctx, "EpochRewards", "isReserveLow()", &[], 1_000_000)?)?
            != U256::ZERO;
    mint(
        ctx,
        system,
        if low { reserve } else { governance },
        rewards[2],
    )?;
    let locked = system.address(ctx, "LockedGold")?;
    let mut groups = Vec::new();
    let mut uptimes: HashMap<Address, Vec<U256>> = HashMap::new();
    for (validator, uptime) in validators {
        let group = address(&system.required(
            ctx,
            "Validators",
            "getMembershipInLastEpochFromSigner(address)",
            &[word(validator)],
            1_000_000,
        )?)?;
        if !uptimes.contains_key(&group) {
            groups.push(group);
        }
        uptimes.entry(group).or_default().push(uptime);
    }
    let vote_data = words(&system.required(
        ctx,
        "Election",
        "getTotalVotesForEligibleValidatorGroups()",
        &[],
        1_000_000,
    )?)?;
    let vote_groups = array(&vote_data, 0)?;
    let values = array(&vote_data, 1)?;
    ensure!(
        vote_groups.len() == values.len(),
        "vote totals length mismatch"
    );
    let mut votes: Vec<_> = vote_groups
        .into_iter()
        .zip(values)
        .map(|(g, v)| (to_address(g), v))
        .collect();
    let mut group_rewards = Vec::new();
    for group in &groups {
        let scores = &uptimes[group];
        let mut args = vec![
            word(*group),
            rewards[1],
            U256::from(96),
            U256::from(scores.len()),
        ];
        args.extend_from_slice(scores);
        group_rewards.push(scalar(&system.required(
            ctx,
            "Election",
            "getGroupEpochRewards(address,uint256,uint256[])",
            &args,
            500_000,
        )?)?);
    }
    let mut total = U256::ZERO;
    for (group, reward) in groups.into_iter().zip(group_rewards) {
        if let Some((_, value)) = votes.iter_mut().find(|(g, _)| *g == group) {
            *value = value.checked_add(reward).context("vote total overflow")?;
        }
        votes.sort_by(|a, b| b.1.cmp(&a.1));
        let (mut lesser, mut greater) = (Address::ZERO, Address::ZERO);
        if let Some(index) = votes.iter().position(|(g, _)| *g == group) {
            if index > 0 {
                greater = votes[index - 1].0;
            }
            if index + 1 < votes.len() {
                lesser = votes[index + 1].0;
            }
        }
        system.execute(
            ctx,
            "Election",
            "distributeEpochRewards(address,uint256,address,address)",
            &[word(group), reward, word(lesser), word(greater)],
            1_000_000,
        )?;
        total = total
            .checked_add(reward)
            .context("voter rewards overflow")?;
    }
    mint(ctx, system, locked, total)?;
    if partner != Address::ZERO {
        mint(ctx, system, partner, rewards[3])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::celo::l1::consensus::CeloHeader;
    use revm::{
        context_interface::journaled_state::account::JournaledAccountTr, database::EmptyDB,
    };

    struct NoHeaders;
    impl CeloChainData for NoHeaders {
        fn header(&self, _: u64) -> Result<CeloHeader, String> {
            Err("archive unavailable".into())
        }
    }

    #[test]
    fn finalization_rolls_back_contract_failures_and_propagates_node_failures() {
        let mut evm = create_celo_l1_evm(
            BlockEnv {
                number: U256::from(20_000_000),
                ..Default::default()
            },
            config(),
            EmptyDB::default(),
            NoOpInspector,
            &NoHeaders,
        )
        .unwrap();
        let ctx = &mut evm.inner.ctx;
        let target = Address::with_last_byte(42);
        attempt(ctx, "contract failure", |ctx| {
            ctx.journaled_state
                .load_account_mut(target)?
                .incr_balance(U256::from(9));
            Err(ContractFailure("execution reverted".into()).into())
        })
        .unwrap();
        assert_eq!(
            ctx.journaled_state
                .load_account(target)
                .unwrap()
                .info
                .balance,
            U256::ZERO
        );
        let error = attempt(ctx, "node failure", |ctx| {
            ctx.journaled_state
                .load_account_mut(target)?
                .incr_balance(U256::from(13));
            Err(anyhow::anyhow!("missing archive state"))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("missing archive state"));
        assert_eq!(
            ctx.journaled_state
                .load_account(target)
                .unwrap()
                .info
                .balance,
            U256::ZERO
        );
    }
}
