//! Arbitrum Orbit (Nitro) RPC impl.
//!
//! Transaction environment creation reuses mainnet's `create_mainnet_txn_env`
//! because Nitro's normal L2 execution is an Ethereum EVM transaction. Execution
//! uses an Arbitrum-specific EVM builder so ArbOS precompile addresses are
//! available in `eth_call` / gas simulation. The pre-execution hook stays at its
//! no-op default even on Prague: Arbitrum skips the EIP-2935 parent-blockhash
//! system call (go-ethereum-arb gates it on `!IsArbitrum`; block hashes come
//! from the per-block internal tx instead). Gas estimation still overrides
//! [`GasFeeHandler::estimate_l1_overhead`] to add Nitro's L1 data-posting cost
//! (posterGas), gated by the per-chain `enable_l1_gas` switch (off by default).

use crate::api_impl::arbitrum::evm::create_arbitrum_evm_from_state_with_env;
use crate::api_impl::arbitrum::node_interface::{
    configured_legacy_zero_base_fee_until, header_l1_block_num,
};
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use crate::error::rpc_error_with_code;
use alloy::primitives::{Bytes, U256};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::arbos_state::ArbStateReader;
use leafage_evm_chains::arbitrum::precompile::ArbitrumPrecompileEnv;
use leafage_evm_chains::arbitrum::tx::{ArbitrumTxContext, ArbitrumTxEnv};
use leafage_evm_chains::arbitrum::{ArbitrumEvmConfig, ArbitrumHardfork};
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, CfgEnv, DebankErrorCode};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::Transaction as _;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

pub(super) type ArbitrumApiImpl<DB> = ApiImpl<DB, ArbitrumHardfork, ArbitrumEvmConfig>;

fn precompile_env<StateDB: DatabaseRef>(
    block_env: &BlockEnv,
    state: &StateDB,
    tx: &ArbitrumTxEnv,
    custom_cfg: Option<&ArbitrumEvmConfig>,
) -> ArbitrumPrecompileEnv {
    ArbitrumPrecompileEnv {
        current_arbos_version: state.arbos_version(),
        current_tx_l1_gas_fees: state.current_tx_l1_gas_fee(&tx.base, block_env.basefee),
        current_l1_block_number: tx.context.current_l1_block_number,
        current_retryable_ticket: tx.retryable.as_ref().and_then(|ctx| ctx.ticket_id),
        current_refund_to: tx.retryable.as_ref().map(|ctx| ctx.refund_to),
        allow_debug_precompiles: custom_cfg.is_some_and(|cfg| cfg.allow_debug_precompiles),
        current_chain_config: custom_cfg
            .and_then(|cfg| cfg.chain_config.as_ref())
            .map(|chain_config| Bytes::copy_from_slice(chain_config.get().as_bytes())),
    }
}

impl<DB> ArbitrumApiImpl<DB> {
    fn cfg_for_tx(&self, tx: &ArbitrumTxEnv) -> CfgEnv<ArbitrumHardfork> {
        let mut cfg = self.evm_cfg.cfg.clone();
        if tx.is_retryable_redeem() {
            cfg.disable_balance_check = true;
            cfg.disable_nonce_check = true;
            cfg.disable_eip3607 = true;
            if tx.is_zero_gas_price_retryable() {
                cfg.disable_base_fee = true;
            }
        }
        cfg
    }

    fn block_env_for_tx(&self, block_env: &BlockEnv, tx: &ArbitrumTxEnv) -> BlockEnv {
        let mut block_env = block_env.clone();
        if tx.is_zero_gas_price_retryable() {
            block_env.basefee = 0;
        }
        block_env
    }

    fn tx_context_for_block(&self, block: &BlockInfo) -> ArbitrumTxContext {
        ArbitrumTxContext {
            current_l1_block_number: header_l1_block_num(
                block,
                configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref()),
            ),
        }
    }

    pub(super) fn transact_evm<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: ArbitrumTxEnv,
    ) -> Result<ExecutionResult<HaltReason>, EVMError<StateDB::Error, InvalidTransaction>>
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let precompile_env =
            precompile_env(block_env, &state, &tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state_with_env(
            block_env.clone(),
            self.cfg_for_tx(&tx),
            state,
            NoOpInspector {},
            precompile_env,
        );

        evm.transact(tx).map(|res| res.result.into())
    }
}

// create_txn_env reuses mainnet's free function; execution uses Arbitrum
// precompiles. apply_pre_execution_changes keeps the trait default (no-op) —
// see module doc.
impl<DB> EvmExecutor for ArbitrumApiImpl<DB>
where
    DB: BlockIndex + Sync + Send + 'static,
{
    type Tx = ArbitrumTxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let base =
            create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)?;
        Ok(ArbitrumTxEnv::new(base).with_context(self.tx_context_for_block(block)))
    }

    fn transact<StateDB: DatabaseRef + Debug>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let block_env = self.block_env_for_tx(block_env, &tx);
        if let Some(result) = self.try_execute_node_interface(&block_env, &state, &tx)? {
            return Ok(result);
        }

        self.transact_evm(&block_env, state, tx)
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit + Debug,
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
        let block_env = self.block_env_for_tx(block_env, &tx);
        if let Some(result) = self.try_execute_node_interface(&block_env, &state, &tx)? {
            return Ok((result, inspector_collect(inspector)));
        }

        let precompile_env =
            precompile_env(&block_env, &state, &tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state_with_env(
            block_env,
            self.cfg_for_tx(&tx),
            state,
            &mut inspector,
            precompile_env,
        );

        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for ArbitrumApiImpl<DB> where DB: BlockIndex + Sync + Send + 'static {}

impl TxSetter for ArbitrumTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl<DB> GasFeeHandler for ArbitrumApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = ArbitrumTxEnv;

    fn gas_allowance<StateDB: DatabaseRef>(
        &self,
        _request: &CallRequest,
        tx: &Self::Tx,
        state: &StateDB,
        _block_env: &BlockEnv,
    ) -> RpcResult<u64> {
        if tx.is_retryable_redeem() {
            return Ok(u64::MAX);
        }

        let caller = state.basic_ref(tx.caller()).map_err(|err| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, err.to_string())
        })?;
        let balance = caller
            .map(|account| account.balance)
            .unwrap_or_default()
            .checked_sub(tx.value())
            .ok_or_else(|| {
                rpc_error_with_code(
                    DebankErrorCode::BalanceExhausted as i32,
                    "Insufficient funds".to_string(),
                )
            })?;
        Ok(balance
            .checked_div(U256::from(tx.gas_price()))
            .unwrap_or_default()
            .try_into()
            .unwrap())
    }

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
        // Per-chain opt-in: off (other arb chains / no config) -> behave like mainnet.
        if !self
            .evm_cfg
            .custom_cfg
            .as_ref()
            .is_some_and(|c| c.enable_l1_gas)
        {
            return 0;
        }

        // Pricing read straight from ArbOS state; missing / pre-pricing -> 0 (safe degrade).
        let pricing = match state.read_pricing() {
            Some(p) => p,
            None => return 0,
        };

        pricing.poster_gas(&tx.base, block_env.basefee)
    }
}
