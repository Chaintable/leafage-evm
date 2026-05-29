//! Arbitrum Orbit (Nitro) RPC impl.
//!
//! EVM execution mirrors mainnet verbatim (Nitro's normal-tx L2 execution is a
//! mainnet EVM), reusing the shared free functions in `mainnet::evm`. The only
//! behavioural addition is overriding [`GasFeeHandler::estimate_l1_overhead`] to
//! add Nitro's L1 data-posting cost (posterGas) to `eth_estimateGas`, gated by
//! the per-chain `enable_l1_gas` switch. With the switch off, this type behaves
//! exactly like mainnet.

use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler};
use crate::api_impl::mainnet::evm::{
    apply_blockhashes_contract_call, create_main_evm_from_state, create_mainnet_txn_env,
};
use crate::api_impl::ApiImpl;
use alloy::consensus::BlockHeader;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::{arbos_state, poster_gas, ArbitrumEvmConfig};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, MainnetSpecId};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type ArbitrumApiImpl<DB> = ApiImpl<DB, MainnetSpecId, ArbitrumEvmConfig>;

// EVM execution: identical to mainnet (mirrors `mainnet::api`), so eth_call /
// trace / replay are byte-for-byte unchanged versus running as `mainnet`.
impl<DB> EvmExecutor for ArbitrumApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut evm = create_main_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );

        evm.transact(tx).map(|res| res.result.into())
    }

    fn apply_pre_execution_changes<StateDB>(
        &self,
        header: impl BlockHeader,
        block_env: &BlockEnv,
        state: &mut StateDB,
    ) -> RpcResult<()>
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        apply_blockhashes_contract_call(&self.evm_cfg.cfg, header.parent_hash(), block_env, state)
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = create_main_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );

        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> GasFeeHandler for ArbitrumApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;

    fn estimate_l1_overhead<StateDB>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        tx: Self::Tx,
        state: &StateDB,
    ) -> u64
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        // Per-chain opt-in: off (other arb chains / no config) → behave like mainnet.
        if !self
            .evm_cfg
            .custom_cfg
            .as_ref()
            .is_some_and(|c| c.enable_l1_gas)
        {
            return 0;
        }

        // Pricing read straight from ArbOS state; missing / pre-pricing → 0 (safe degrade).
        let pricing = match arbos_state::read_pricing(state) {
            Some(p) => p,
            None => return 0,
        };

        poster_gas::compute(&tx, block_env.basefee, &pricing)
    }
}

impl<DB> ApiCore for ArbitrumApiImpl<DB> where DB: Sync + Send + 'static {}
