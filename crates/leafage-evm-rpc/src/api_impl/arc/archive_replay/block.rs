//! Mainnet block hooks, matching arc-node 830ad37's ArcBlockExecutor.
// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use alloy::primitives::{address, Address, Bytes, U256};
use alloy::sol_types::{sol, SolCall, SolValue};
use alloy_evm::Database;
use anyhow::{anyhow, ensure, Result};
use leafage_evm_chains::arc::{
    blocklist_storage_slot, is_blocklisted_status, ArcEvm, NATIVE_COIN_CONTROL_ADDRESS,
};
use revm::{context::ContextTr, DatabaseCommit, SystemCallEvm};

const SYSTEM_CALLER: Address = revm::handler::SYSTEM_ADDRESS;
const PROTOCOL_CONFIG: Address = address!("3600000000000000000000000000000000000001");
const ACCOUNTING: Address = address!("1800000000000000000000000000000000000002");

sol! {
    #![sol(alloy_sol_types = alloy::sol_types)]
    struct GasValues { uint64 gasUsed; uint64 gasUsedSmoothed; uint64 nextBaseFee; }
    struct FeeParams {
        uint64 alpha; uint64 kRate; uint64 inverseElasticityMultiplier;
        uint256 minBaseFee; uint256 maxBaseFee; uint256 blockGasLimit;
    }
    function feeParams() external view returns (FeeParams);
    function getGasValues(uint64 blockNumber) external view returns (GasValues);
    function storeGasValues(uint64 blockNumber, GasValues gasValues) external returns (bool);
}

/// Read-only system calls intentionally discard their returned state, as the Writer does.
fn call<DB: Database, I>(evm: &mut ArcEvm<DB, I>, to: Address, data: Bytes) -> Result<Bytes> {
    let outcome = evm
        .system_call_with_caller(SYSTEM_CALLER, to, data)
        .map_err(|e| anyhow!("system call to {to}: {e:?}"))?;
    ensure!(
        outcome.result.is_success(),
        "system call to {to} failed: {:?}",
        outcome.result
    );
    outcome
        .result
        .output()
        .cloned()
        .ok_or_else(|| anyhow!("system call to {to} has no output"))
}

fn fee_params<DB: Database, I>(evm: &mut ArcEvm<DB, I>) -> Result<Option<FeeParams>> {
    // Contract failures retain the Writer's fallback; database/EVM errors abort replay.
    let outcome = evm
        .system_call_with_caller(
            SYSTEM_CALLER,
            PROTOCOL_CONFIG,
            feeParamsCall {}.abi_encode().into(),
        )
        .map_err(|e| anyhow!("ProtocolConfig feeParams: {e}"))?;
    if !outcome.result.is_success() {
        tracing::warn!(result = ?outcome.result, "feeParams failed; using defaults");
        return Ok(None);
    }
    let Some(output) = outcome.result.output() else {
        tracing::warn!("feeParams has no output; using defaults");
        return Ok(None);
    };
    match feeParamsCall::abi_decode_returns(output) {
        Ok(params) => Ok(Some(params)),
        Err(err) => {
            tracing::warn!(%err, "invalid feeParams output; using defaults");
            Ok(None)
        }
    }
}

pub(crate) fn before<DB: Database + DatabaseCommit, I>(
    evm: &mut ArcEvm<DB, I>,
    header: &alloy::rpc::types::Header,
) -> Result<()> {
    // EIP-2935 is active from genesis; replay never executes block zero.
    let outcome = evm
        .system_call_with_caller(
            SYSTEM_CALLER,
            alloy::eips::eip2935::HISTORY_STORAGE_ADDRESS,
            header.parent_hash.0.to_vec().into(),
        )
        .map_err(|e| anyhow!("history storage call: {e:?}"))?;
    // SystemCaller does not require a successful result for this system call.
    let state = outcome.state;
    evm.ctx_mut().db_mut().commit(state);

    let blocked = evm
        .ctx_mut()
        .db_mut()
        .storage(
            NATIVE_COIN_CONTROL_ADDRESS,
            blocklist_storage_slot(header.beneficiary),
        )
        .map_err(|e| anyhow!("beneficiary blocklist: {e:?}"))?;
    ensure!(
        !is_blocklisted_status(blocked),
        "beneficiary {} is blocklisted",
        header.beneficiary
    );
    let params = fee_params(evm)?;
    let gas_limit = params
        .and_then(|p| u64::try_from(p.blockGasLimit).ok())
        .filter(|v| (10_000_000..=200_000_000).contains(v))
        .unwrap_or(30_000_000);
    ensure!(
        header.gas_limit == gas_limit,
        "block {} gas limit {} differs from ProtocolConfig {gas_limit}",
        header.number,
        header.gas_limit
    );
    Ok(())
}

fn bounded(value: u64, max: u64, default: u64) -> u64 {
    if (1..=max).contains(&value) {
        value
    } else {
        default
    }
}

fn next_fee(used: u64, limit: u64, base: u64, rate: u64, inverse: u64) -> u64 {
    let target = u64::try_from(limit as u128 * inverse as u128 / 10_000).unwrap_or(u64::MAX);
    if target == 0 || rate == 0 {
        return base;
    }
    let denominator = target as u128 * 10_000 / rate as u128;
    if denominator == 0 {
        return base;
    }
    let delta = base as u128 * used.abs_diff(target) as u128 / denominator;
    let delta = u64::try_from(delta).unwrap_or(u64::MAX);
    match used.cmp(&target) {
        std::cmp::Ordering::Greater => base.saturating_add(delta.max(1)),
        std::cmp::Ordering::Less => base.saturating_sub(delta),
        std::cmp::Ordering::Equal => base,
    }
}

pub(crate) fn after<DB: Database + DatabaseCommit, I>(
    evm: &mut ArcEvm<DB, I>,
    header: &alloy::rpc::types::Header,
    gas_used: u64,
) -> Result<()> {
    let params = fee_params(evm)?;
    let (alpha, rate, inverse) = params
        .as_ref()
        .map(|p| {
            (
                bounded(p.alpha, 100, 20),
                bounded(p.kRate, 1000, 200),
                bounded(p.inverseElasticityMultiplier, 9000, 5000),
            )
        })
        .unwrap_or((20, 200, 5000));
    let output = call(
        evm,
        ACCOUNTING,
        getGasValuesCall {
            blockNumber: header.number.saturating_sub(1),
        }
        .abi_encode()
        .into(),
    )?;
    let parent = getGasValuesCall::abi_decode_returns(&output)?;
    let smoothed = (((100 - alpha) as u128 * parent.gasUsedSmoothed as u128
        + alpha as u128 * gas_used as u128)
        / 100) as u64;
    let mut next = next_fee(
        smoothed,
        header.gas_limit,
        header.base_fee_per_gas.unwrap_or_default(),
        rate,
        inverse,
    );
    if let Some(p) = params {
        let min = u64::try_from(p.minBaseFee).unwrap_or(u64::MAX);
        let max = u64::try_from(p.maxBaseFee).unwrap_or(u64::MAX);
        if max != 0 && max >= min {
            next = next.clamp(min, max);
        }
    }
    next = next.clamp(1, 20_000_000_000_000);
    ensure!(
        header.extra_data.as_ref() == next.to_be_bytes(),
        "block {} extraData differs from computed next base fee {next}",
        header.number
    );
    let outcome = evm
        .system_call_with_caller(
            SYSTEM_CALLER,
            ACCOUNTING,
            storeGasValuesCall {
                blockNumber: header.number,
                gasValues: GasValues {
                    gasUsed: gas_used,
                    gasUsedSmoothed: smoothed,
                    nextBaseFee: next,
                },
            }
            .abi_encode()
            .into(),
        )
        .map_err(|e| anyhow!("store gas values: {e:?}"))?;
    ensure!(
        outcome.result.is_success(),
        "store gas values failed: {:?}",
        outcome.result
    );
    ensure!(
        storeGasValuesCall::abi_decode_returns(
            outcome
                .result
                .output()
                .ok_or_else(|| anyhow!("store gas values has no output"))?
        )?,
        "store gas values returned false"
    );
    evm.ctx_mut().db_mut().commit(outcome.state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use alloy_evm::EvmEnv;
    use leafage_evm_chains::arc::{ArcChainConfig, ArcEvmFactory};
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode, EvmState},
    };

    #[derive(Debug, thiserror::Error)]
    #[error("forced ProtocolConfig database read failure")]
    struct ReadError;
    impl revm::database_interface::DBErrorMarker for ReadError {}

    #[derive(Debug, Default)]
    struct FailingDb(InMemoryDB);

    impl revm::Database for FailingDb {
        type Error = ReadError;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, ReadError> {
            if address == PROTOCOL_CONFIG {
                return Err(ReadError);
            }
            Ok(self.0.basic(address).unwrap())
        }

        fn code_by_hash(&mut self, hash: B256) -> Result<Bytecode, ReadError> {
            Ok(self.0.code_by_hash(hash).unwrap())
        }

        fn storage(&mut self, address: Address, slot: U256) -> Result<U256, ReadError> {
            Ok(self.0.storage(address, slot).unwrap())
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, ReadError> {
            Ok(self.0.block_hash(number).unwrap())
        }
    }

    impl DatabaseCommit for FailingDb {
        fn commit(&mut self, changes: EvmState) {
            self.0.commit(changes);
        }
    }

    fn env() -> EvmEnv {
        let config = ArcChainConfig::mainnet();
        let mut cfg = CfgEnv::new_with_spec(config.ethereum_spec());
        cfg.chain_id = config.chain_id();
        EvmEnv::new(
            cfg,
            BlockEnv {
                gas_limit: 30_000_000,
                ..Default::default()
            },
        )
    }

    #[test]
    fn database_errors_abort_both_block_hooks() {
        let header = alloy::rpc::types::Header::default();
        for pre_block in [true, false] {
            let mut db = FailingDb::default();
            let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
            let mut evm = factory
                .create(env(), &mut db, revm::inspector::NoOpInspector {})
                .unwrap();
            let err = if pre_block {
                before(&mut evm, &header)
            } else {
                after(&mut evm, &header, 0)
            }
            .unwrap_err();
            let message = err.to_string();
            assert!(message.contains("ProtocolConfig feeParams"), "{message}");
            assert!(
                message.contains("forced ProtocolConfig database read failure"),
                "{message}"
            );
        }
    }

    fn config_db(code: Bytes) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        let code = Bytecode::new_raw(code);
        db.insert_account_info(
            PROTOCOL_CONFIG,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
        db
    }

    #[test]
    fn contract_and_abi_failures_keep_writer_defaults() {
        // REVERT, exceptional halt, empty return, and a one-byte malformed return.
        for code in ["60006000fd", "fe", "00", "600160005360016000f3"] {
            let mut db = config_db(alloy::hex::decode(code).unwrap().into());
            let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
            let mut evm = factory
                .create(env(), &mut db, revm::inspector::NoOpInspector {})
                .unwrap();
            assert!(fee_params(&mut evm).unwrap().is_none(), "{code}");
        }
    }

    #[test]
    fn valid_contract_fee_params_are_preserved() {
        let params = FeeParams {
            alpha: 37,
            kRate: 123,
            inverseElasticityMultiplier: 6789,
            minBaseFee: U256::from(10),
            maxBaseFee: U256::from(1000),
            blockGasLimit: U256::from(42_000_000),
        };
        let output = params.abi_encode();
        let mut code = alloy::hex::decode("6100c061000f6000396100c06000f3").unwrap();
        assert_eq!(code.len(), 15);
        assert_eq!(output.len(), 192);
        code.extend_from_slice(&output);
        let mut db = config_db(code.into());
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
        let mut evm = factory
            .create(env(), &mut db, revm::inspector::NoOpInspector {})
            .unwrap();
        assert_eq!(fee_params(&mut evm).unwrap().unwrap().abi_encode(), output);
    }
}
