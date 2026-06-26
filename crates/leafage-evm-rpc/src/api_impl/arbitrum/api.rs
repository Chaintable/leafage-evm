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

use crate::api_impl::arbitrum::evm::create_arbitrum_evm_from_state;
use crate::api_impl::arbitrum::node_interface::{
    configured_legacy_zero_base_fee_until, header_l1_block_num,
};
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use crate::error::rpc_error_with_code;
use alloy::primitives::{Bytes, B256, U256};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::arbos_state::ArbStateReader;
use leafage_evm_chains::arbitrum::context::ArbitrumExecutionContext;
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
    let l1_fee_basefee = if tx.is_zero_gas_price_retryable() {
        0
    } else {
        block_env.basefee
    };
    ArbitrumPrecompileEnv {
        current_arbos_version: state.arbos_version(),
        current_tx_l1_gas_fees: state.current_tx_l1_gas_fee(&tx.base, l1_fee_basefee),
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

    fn execution_env_for_tx(
        block_env: &BlockEnv,
        tx: &ArbitrumTxEnv,
    ) -> (BlockEnv, ArbitrumExecutionContext) {
        let mut evm_block_env = block_env.clone();
        let mut execution_context = ArbitrumExecutionContext::default();
        execution_context.set_current_l2_context(block_env.number, block_env.basefee);

        if tx.context.current_l1_block_number != 0 {
            evm_block_env.number = U256::from(tx.context.current_l1_block_number);
        }
        evm_block_env.basefee = 0;
        evm_block_env.difficulty = U256::ONE;
        evm_block_env.prevrandao = Some(B256::with_last_byte(1));
        (evm_block_env, execution_context)
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
        let (evm_block_env, execution_context) = Self::execution_env_for_tx(block_env, &tx);
        let precompile_env =
            precompile_env(block_env, &state, &tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state(
            evm_block_env,
            self.cfg_for_tx(&tx),
            state,
            NoOpInspector {},
            precompile_env,
            execution_context,
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
        Ok(ArbitrumTxEnv::new(base, self.tx_context_for_block(block)))
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
        if let Some(result) = self.try_execute_node_interface(&block_env, &state, &tx)? {
            return Ok((result, inspector_collect(inspector)));
        }

        let (evm_block_env, execution_context) = Self::execution_env_for_tx(block_env, &tx);
        let precompile_env =
            precompile_env(&block_env, &state, &tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state(
            evm_block_env,
            self.cfg_for_tx(&tx),
            state,
            &mut inspector,
            precompile_env,
            execution_context,
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
            .unwrap_or(u64::MAX))
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

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::arbitrum::arbos_state::ARBOS_STATE_ADDRESS;
    use revm::context::TxEnv;
    use revm::primitives::{Address, StorageKey, StorageValue};
    use revm::state::{AccountInfo, Bytecode};
    use revm::DatabaseRef;
    use std::convert::Infallible;

    fn arbos_slot(hex: &str) -> U256 {
        U256::from_str_radix(hex, 16).expect("valid ArbOS slot")
    }

    #[derive(Clone, Copy, Debug)]
    struct PricingState;

    impl DatabaseRef for PricingState {
        type Error = Infallible;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage_ref(
            &self,
            address: Address,
            index: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            if address != ARBOS_STATE_ADDRESS {
                return Ok(U256::ZERO);
            }
            if index
                == arbos_slot("a9f6f085d78d1d37c5819e5c16c9e03198bd14e08cd1f6f8191bc6207b9e9707")
            {
                return Ok(U256::from(1_000_000_000u64));
            }
            if index
                == arbos_slot("e54de2a4cdacc0a0059d2b6e16348103df8c4aff409c31e40ec73d11926c8203")
            {
                return Ok(U256::ONE);
            }
            if index
                == arbos_slot("15fed0451499512d95f3ec5a41c878b9de55f21878b5b4e190d4667ec709b407")
            {
                return Ok(U256::from(1));
            }
            Ok(U256::ZERO)
        }

        fn storage_by_account_id_ref(
            &self,
            address: Address,
            _account_id: usize,
            storage_key: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            self.storage_ref(address, storage_key)
        }

        fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[test]
    fn execution_env_for_tx_matches_arbitrum_contexts() {
        let mut block_env = BlockEnv {
            number: U256::from(123_456u64),
            basefee: 10_000_000,
            difficulty: U256::ZERO,
            prevrandao: Some(B256::ZERO),
            ..Default::default()
        };
        block_env.timestamp = U256::from(1_700_000_000u64);

        let tx = ArbitrumTxEnv::new(
            TxEnv::default(),
            ArbitrumTxContext {
                current_l1_block_number: 99_999,
            },
        );

        let (evm_block_env, execution_context) =
            ArbitrumApiImpl::<()>::execution_env_for_tx(&block_env, &tx);
        assert_eq!(evm_block_env.number, U256::from(99_999u64));
        assert_eq!(evm_block_env.basefee, 0);
        assert_eq!(evm_block_env.difficulty, U256::ONE);
        assert_eq!(evm_block_env.prevrandao, Some(B256::with_last_byte(1)));

        assert_eq!(
            execution_context.current_l2_block_number(),
            Some(U256::from(123_456u64))
        );
        assert_eq!(execution_context.current_l2_basefee(), Some(10_000_000));
    }

    #[test]
    fn execution_env_for_tx_keeps_l2_number_when_l1_number_is_unknown() {
        let block_env = BlockEnv {
            number: U256::from(123_456u64),
            basefee: 10_000_000,
            ..Default::default()
        };
        let tx = ArbitrumTxEnv::new(
            TxEnv::default(),
            ArbitrumTxContext {
                current_l1_block_number: 0,
            },
        );

        let (evm_block_env, _) = ArbitrumApiImpl::<()>::execution_env_for_tx(&block_env, &tx);
        assert_eq!(evm_block_env.number, U256::from(123_456u64));
        assert_eq!(evm_block_env.basefee, 0);
        assert_eq!(evm_block_env.prevrandao, Some(B256::with_last_byte(1)));
    }

    #[test]
    fn precompile_env_keeps_zero_l1_fee_for_zero_gas_retryable() {
        let block_env = BlockEnv {
            basefee: 10_000_000,
            ..Default::default()
        };
        let state = PricingState;
        let normal_tx = ArbitrumTxEnv::new(
            TxEnv {
                gas_price: 10_000_000,
                gas_limit: 100_000,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let normal_env = precompile_env(&block_env, &state, &normal_tx, None);
        assert!(normal_env.current_tx_l1_gas_fees > U256::ZERO);

        let retryable = ArbitrumTxEnv::retryable_redeem(
            TxEnv {
                gas_price: 0,
                gas_limit: 100_000,
                ..Default::default()
            },
            None,
            Address::ZERO,
            ArbitrumTxContext::default(),
        );
        let retryable_env = precompile_env(&block_env, &state, &retryable, None);
        assert_eq!(retryable_env.current_tx_l1_gas_fees, U256::ZERO);
    }
}
