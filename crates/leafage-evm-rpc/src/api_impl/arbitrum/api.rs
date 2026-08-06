//! Arbitrum Orbit (Nitro) RPC impl.
//!
//! Transaction environment creation reuses mainnet's `create_mainnet_txn_env`
//! because Nitro's normal L2 execution is an Ethereum EVM transaction. Execution
//! uses an Arbitrum-specific EVM builder so ArbOS precompile addresses are
//! available in `eth_call` / gas simulation. The pre-execution hook stays at its
//! no-op default even on Prague: Arbitrum skips the EIP-2935 parent-blockhash
//! system call (go-ethereum-arb gates it on `!IsArbitrum`; block hashes come
//! from the per-block internal tx instead). Nitro's L1 data-posting cost is
//! charged by the Arbitrum EVM handler so RPC gas estimation does not add a
//! second `estimate_l1_overhead`.

use crate::api_impl::arbitrum::evm::create_arbitrum_evm_from_state;
use crate::api_impl::arbitrum::node_interface::{
    configured_legacy_zero_base_fee_until, header_l1_and_arbos_version, NodeInterfaceAction,
};
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use crate::error::rpc_error_with_code;
use alloy::primitives::{Bytes, B256, Log, U256};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::evm::ArbitrumExecutionContext;
use leafage_evm_chains::arbitrum::precompile::ArbitrumPrecompileEnv;
use leafage_evm_chains::arbitrum::tx::{ArbitrumTxContext, ArbitrumTxEnv, ArbitrumRetryTx};
use leafage_evm_chains::arbitrum::{ArbitrumEvmConfig, ArbitrumHardfork};
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, CfgEnv, DebankErrorCode};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction, Output};
use revm::context::ContextTr;
use revm::context::Transaction as _;
use revm::database::CacheDB;
use revm::inspector::NoOpInspector;
use revm::interpreter::InstructionResult;
use revm::primitives::{Address, TxKind};
use revm::{DatabaseCommit, DatabaseRef, ExecuteCommitEvm, InspectCommitEvm};
use revm_inspectors::tracing::types::{CallKind, CallLog, CallTrace, TraceMemberOrder};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::collections::VecDeque;
use std::fmt::Debug;

pub(super) type ArbitrumApiImpl<DB> = ApiImpl<DB, ArbitrumHardfork, ArbitrumEvmConfig>;

fn precompile_env(
    tx: &ArbitrumTxEnv,
    custom_cfg: Option<&ArbitrumEvmConfig>,
) -> ArbitrumPrecompileEnv {
    let retryable = tx.retryable_redeem_tx();
    ArbitrumPrecompileEnv {
        current_tx_l1_gas_fees: U256::ZERO,
        current_tx_l1_gas_units: 0,
        current_l1_block_number: tx.context.current_l1_block_number,
        current_retryable_ticket: retryable
            .map(|retryable| retryable.ticket_id)
            .filter(|ticket_id| *ticket_id != B256::ZERO),
        current_refund_to: retryable.map(|retryable| retryable.refund_to),
        allow_debug_precompiles: custom_cfg.is_some_and(|cfg| cfg.allow_debug_precompiles),
        current_chain_config: custom_cfg
            .and_then(|cfg| cfg.chain_config.as_ref())
            .map(|chain_config| Bytes::copy_from_slice(chain_config.get().as_bytes())),
        ..Default::default()
    }
}

impl<DB> ArbitrumApiImpl<DB> {
    fn cfg_for_tx(&self, tx: &ArbitrumTxEnv) -> CfgEnv<ArbitrumHardfork> {
        let mut cfg = self.evm_cfg.cfg.clone();
        cfg.spec = self
            .evm_cfg
            .custom_cfg
            .as_ref()
            .and_then(|custom_cfg| custom_cfg.hardfork_override)
            .unwrap_or_else(|| {
                ArbitrumHardfork::from_arbos_version(tx.context.current_arbos_version)
            });
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
        // Nitro lowers the basefee to 0 only for unpriced messages
        // (`internal/ethapi/api.go` doCall); priced simulations keep the real
        // basefee so the BASEFEE / GASPRICE opcodes observe it.
        if tx.effective_gas_price(block_env.basefee as u128) == 0 {
            evm_block_env.basefee = 0;
        }
        evm_block_env.difficulty = U256::ONE;
        evm_block_env.prevrandao = Some(B256::with_last_byte(1));
        (evm_block_env, execution_context)
    }

    fn tx_context_for_block(&self, block: &BlockInfo) -> ArbitrumTxContext {
        let legacy_zero_base_fee_until =
            configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref());
        let (current_l1_block_number, current_arbos_version) =
            header_l1_and_arbos_version(block, legacy_zero_base_fee_until);
        ArbitrumTxContext {
            current_l1_block_number,
            current_arbos_version,
            ..Default::default()
        }
    }

    fn transact_evm_commit<StateDB: DatabaseRef + DatabaseCommit>(
        &self,
        block_env: &BlockEnv,
        state: &mut StateDB,
        tx: ArbitrumTxEnv,
    ) -> Result<
        (ExecutionResult<HaltReason>, VecDeque<ArbitrumRetryTx>),
        EVMError<StateDB::Error, InvalidTransaction>,
    >
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let (evm_block_env, execution_context) = Self::execution_env_for_tx(block_env, &tx);
        let precompile_env = precompile_env(&tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state(
            evm_block_env,
            self.cfg_for_tx(&tx),
            &mut *state,
            NoOpInspector {},
            precompile_env,
            execution_context,
        )
        .map_err(EVMError::Database)?;

        let result = evm.transact_commit(tx)?;
        let scheduled = evm.ctx_mut().chain_mut().take_scheduled_retryables();
        Ok((result, scheduled))
    }

    fn inspect_evm_commit<StateDB: DatabaseRef + DatabaseCommit>(
        &self,
        block_env: &BlockEnv,
        state: &mut StateDB,
        tx: ArbitrumTxEnv,
        inspector: &mut TracingInspector,
    ) -> Result<
        (ExecutionResult<HaltReason>, VecDeque<ArbitrumRetryTx>),
        EVMError<StateDB::Error, InvalidTransaction>,
    >
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let (evm_block_env, execution_context) = Self::execution_env_for_tx(block_env, &tx);
        let precompile_env = precompile_env(&tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state(
            evm_block_env,
            self.cfg_for_tx(&tx),
            &mut *state,
            inspector,
            precompile_env,
            execution_context,
        )
        .map_err(EVMError::Database)?;

        let result = evm.inspect_tx_commit(tx)?;
        let scheduled = evm.ctx_mut().chain_mut().take_scheduled_retryables();
        Ok((result, scheduled))
    }

    fn run_scheduled_retryables<E>(
        context: &ArbitrumTxContext,
        root_result: ExecutionResult<HaltReason>,
        mut scheduled: VecDeque<ArbitrumRetryTx>,
        mut execute: impl FnMut(
            ArbitrumTxEnv,
        )
            -> Result<(ExecutionResult<HaltReason>, VecDeque<ArbitrumRetryTx>), E>,
    ) -> Result<(ExecutionResult<HaltReason>, Vec<Log>), E> {
        let (reason, mut gas, mut logs, output) = match root_result {
            ExecutionResult::Success {
                reason,
                gas,
                logs,
                output,
            } => (reason, gas, logs, output),
            result => return Ok((result, Vec::new())),
        };
        let root_logs = logs.clone();

        while let Some(retryable) = scheduled.pop_front() {
            let mut retryable_context = context.clone();
            retryable_context.gas_estimation = true;
            let (result, mut newly_scheduled) =
                execute(ArbitrumTxEnv::from_retryable(retryable, retryable_context))?;
            scheduled.append(&mut newly_scheduled);

            match result {
                ExecutionResult::Success {
                    gas: child_gas,
                    logs: child_logs,
                    ..
                } => {
                    gas = child_gas;
                    logs.extend(child_logs);
                }
                ExecutionResult::Revert {
                    gas,
                    logs: child_logs,
                    output,
                } => {
                    logs.extend(child_logs);
                    return Ok((ExecutionResult::Revert { gas, logs, output }, root_logs));
                }
                ExecutionResult::Halt {
                    reason,
                    gas,
                    logs: child_logs,
                } => {
                    logs.extend(child_logs);
                    return Ok((ExecutionResult::Halt { reason, gas, logs }, root_logs));
                }
            }
        }

        Ok((
            ExecutionResult::Success {
                reason,
                gas,
                logs,
                output,
            },
            root_logs,
        ))
    }

    pub(super) fn transact_arbitrum<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        submit: ArbitrumTxEnv,
    ) -> Result<(ExecutionResult<HaltReason>, Vec<Log>), EVMError<StateDB::Error, InvalidTransaction>>
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let context = submit.context.clone();
        let mut state = CacheDB::new(state);
        let (root_result, scheduled) = self.transact_evm_commit(block_env, &mut state, submit)?;
        Self::run_scheduled_retryables(&context, root_result, scheduled, |retryable| {
            self.transact_evm_commit(block_env, &mut state, retryable)
        })
    }

    fn inspect_arbitrum<StateDB: DatabaseRef + DatabaseCommit>(
        &self,
        block_env: &BlockEnv,
        state: &mut StateDB,
        submit: ArbitrumTxEnv,
        inspector: &mut TracingInspector,
    ) -> Result<(ExecutionResult<HaltReason>, Vec<Log>), EVMError<StateDB::Error, InvalidTransaction>>
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let context = submit.context.clone();
        let (root_result, scheduled) =
            self.inspect_evm_commit(block_env, state, submit, inspector)?;
        Self::run_scheduled_retryables(&context, root_result, scheduled, |retryable| {
            self.inspect_evm_commit(block_env, state, retryable, inspector)
        })
    }

    fn record_virtual_trace(
        inspector: &mut TracingInspector,
        tx: &ArbitrumTxEnv,
        result: &ExecutionResult<HaltReason>,
        root_logs: &[Log],
    ) {
        let (success, status, output, gas_used) = match result {
            ExecutionResult::Success { output, gas, .. } => {
                let output = match output {
                    Output::Call(bytes) => bytes.clone(),
                    Output::Create(bytes, _) => bytes.clone(),
                };
                (true, Some(InstructionResult::Return), output, gas.spent())
            }
            ExecutionResult::Revert { output, gas, .. } => (
                false,
                Some(InstructionResult::Revert),
                output.clone(),
                gas.spent(),
            ),
            ExecutionResult::Halt { gas, .. } => (
                false,
                Some(InstructionResult::FatalExternalError),
                Bytes::new(),
                gas.spent(),
            ),
        };

        let (address, kind) = match tx.kind() {
            TxKind::Call(to) => (to, CallKind::Call),
            TxKind::Create => (Address::ZERO, CallKind::Create),
        };
        let nodes = inspector.traces_mut().nodes_mut();
        let mut root = revm_inspectors::tracing::types::CallTraceNode {
            idx: 0,
            trace: CallTrace {
                depth: 0,
                success,
                caller: tx.caller(),
                address,
                maybe_precompile: Some(true),
                kind,
                value: tx.value(),
                data: tx.input().clone(),
                output,
                gas_used,
                gas_limit: tx.gas_limit(),
                status,
                ..Default::default()
            },
            ..Default::default()
        };
        for (index, log) in root_logs.iter().cloned().enumerate() {
            root.logs
                .push(CallLog::from(log).with_position(0).with_index(index as u64));
            root.ordering.push(TraceMemberOrder::Log(index));
        }

        let has_recorded_child = nodes.iter().any(|node| {
            node.trace.gas_limit != 0
                || node.trace.gas_used != 0
                || !node.trace.data.is_empty()
                || !node.trace.output.is_empty()
                || node.trace.caller != Address::ZERO
                || node.trace.address != Address::ZERO
                || !node.children.is_empty()
                || !node.logs.is_empty()
        });

        if !has_recorded_child {
            if nodes.is_empty() {
                nodes.push(root);
            } else {
                nodes[0] = root;
            }
            return;
        }

        let child_roots = nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.parent.is_none().then_some(index + 1))
            .collect::<Vec<_>>();
        for node in nodes.iter_mut() {
            node.idx += 1;
            node.parent = node.parent.map(|parent| parent + 1).or(Some(0));
            node.children.iter_mut().for_each(|child| *child += 1);
            node.trace.depth += 1;
        }
        root.children.extend(child_roots.iter().copied());
        for index in 0..child_roots.len() {
            root.ordering.push(TraceMemberOrder::Call(index));
        }
        nodes.insert(0, root);
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
        match self.node_interface_action(block_env, &state, &tx)? {
            Some(NodeInterfaceAction::Return(result)) => Ok(result),
            Some(NodeInterfaceAction::Replace(submit)) => self
                .transact_arbitrum(block_env, state, submit)
                .map(|(result, _)| result),
            None => self
                .transact_arbitrum(block_env, state, tx)
                .map(|(result, _)| result),
        }
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit + Debug,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        mut state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut inspector = TracingInspector::new(inspector_cfg);
        if let Some(action) = self.node_interface_action(block_env, &state, &tx)? {
            let (result, root_logs) = match action {
                NodeInterfaceAction::Return(result) => (result, Vec::new()),
                NodeInterfaceAction::Replace(submit) => {
                    self.inspect_arbitrum(block_env, &mut state, submit, &mut inspector)?
                }
            };
            Self::record_virtual_trace(&mut inspector, &tx, &result, &root_logs);
            return Ok((result, inspector_collect(inspector)));
        }
        let (result, _) = self.inspect_arbitrum(block_env, &mut state, tx, &mut inspector)?;
        Ok((result, inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for ArbitrumApiImpl<DB> where DB: BlockIndex + Sync + Send + 'static {}

impl TxSetter for ArbitrumTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }

    fn set_gas_estimation(&mut self) {
        self.context.gas_estimation = true;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::arbitrum::arbos_state::ARBOS_STATE_ADDRESS;
    use leafage_evm_chains::arbitrum::precompile::NODE_INTERFACE_ADDRESS;
    use revm::bytecode::Bytecode;
    use revm::context::result::{ResultGas, SuccessReason};
    use revm::context::TxEnv;
    use revm::database::{CacheDB, EmptyDB};
    use revm::primitives::keccak256;
    use revm::state::AccountInfo;
    use revm::ExecuteEvm;

    fn test_api(hardfork_override: Option<ArbitrumHardfork>) -> ArbitrumApiImpl<()> {
        let custom_cfg = hardfork_override.map(|hardfork_override| ArbitrumEvmConfig {
            hardfork_override: Some(hardfork_override),
            ..Default::default()
        });
        ArbitrumApiImpl::new(
            (),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            custom_cfg,
            None,
            None,
            None,
            true,
            false,
            "test".to_string(),
            100,
            None,
            None,
        )
    }

    fn tx_at_arbos_version(version: u64) -> ArbitrumTxEnv {
        ArbitrumTxEnv::new(
            TxEnv::default(),
            ArbitrumTxContext {
                current_arbos_version: version,
                ..Default::default()
            },
        )
    }

    #[test]
    fn cfg_for_tx_selects_hardfork_from_arbos_version() {
        let api = test_api(None);
        for (version, expected) in [
            (10, ArbitrumHardfork::London),
            (11, ArbitrumHardfork::Shanghai),
            (20, ArbitrumHardfork::Cancun),
            (40, ArbitrumHardfork::Prague),
            (50, ArbitrumHardfork::Osaka),
            (61, ArbitrumHardfork::Osaka),
        ] {
            assert_eq!(api.cfg_for_tx(&tx_at_arbos_version(version)).spec, expected);
        }
    }

    #[test]
    fn explicit_hardfork_override_wins_over_arbos_version() {
        let api = test_api(Some(ArbitrumHardfork::Prague));
        assert_eq!(
            api.cfg_for_tx(&tx_at_arbos_version(61)).spec,
            ArbitrumHardfork::Prague
        );
    }

    fn arbos_version_slot() -> U256 {
        let key = [0u8; 32];
        let hashed = keccak256(&key[..31]);
        let mut slot = [0u8; 32];
        slot[..31].copy_from_slice(&hashed[..31]);
        U256::from_be_bytes(slot)
    }

    fn execute_clz(
        header_arbos_version: u64,
        state_arbos_version: u64,
    ) -> ExecutionResult<HaltReason> {
        let target = Address::with_last_byte(0x22);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            arbos_version_slot(),
            U256::from(state_arbos_version),
        )
        .expect("write ArbOS version");
        db.insert_account_info(
            target,
            AccountInfo {
                code: Some(Bytecode::new_legacy(Bytes::from_static(&[
                    0x60, 0x01, 0x1e, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
                ]))),
                ..Default::default()
            },
        );
        let mut tx = tx_at_arbos_version(header_arbos_version);
        tx.base.kind = TxKind::Call(target);
        tx.base.gas_limit = 2_000_000;
        let cfg = test_api(None).cfg_for_tx(&tx);
        let mut evm = create_arbitrum_evm_from_state(
            BlockEnv {
                gas_limit: 30_000_000,
                ..Default::default()
            },
            cfg,
            db,
            NoOpInspector {},
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        )
        .expect("create Arbitrum EVM");
        evm.transact(tx).expect("execute CLZ probe").result
    }

    #[test]
    fn clz_activates_at_arbos_50() {
        assert!(matches!(execute_clz(49, 49), ExecutionResult::Halt { .. }));
        match execute_clz(50, 50) {
            ExecutionResult::Success {
                output: Output::Call(output),
                ..
            } => {
                let mut expected = [0u8; 32];
                expected[31] = 255;
                assert_eq!(output.as_ref(), expected);
            }
            result => panic!("expected CLZ success, got {result:?}"),
        }
    }

    #[test]
    fn state_arbos_override_does_not_change_header_selected_hardfork() {
        assert!(matches!(execute_clz(49, 50), ExecutionResult::Halt { .. }));
        assert!(matches!(
            execute_clz(50, 49),
            ExecutionResult::Success { .. }
        ));
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
                ..Default::default()
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
    fn execution_env_for_tx_keeps_basefee_for_priced_tx() {
        let block_env = BlockEnv {
            number: U256::from(123_456u64),
            basefee: 10_000_000,
            ..Default::default()
        };
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                gas_price: 20_000_000,
                ..Default::default()
            },
            ArbitrumTxContext {
                current_l1_block_number: 99_999,
                ..Default::default()
            },
        );

        let (evm_block_env, execution_context) =
            ArbitrumApiImpl::<()>::execution_env_for_tx(&block_env, &tx);
        assert_eq!(evm_block_env.basefee, 10_000_000);
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
                ..Default::default()
            },
        );

        let (evm_block_env, _) = ArbitrumApiImpl::<()>::execution_env_for_tx(&block_env, &tx);
        assert_eq!(evm_block_env.number, U256::from(123_456u64));
        assert_eq!(evm_block_env.basefee, 0);
        assert_eq!(evm_block_env.prevrandao, Some(B256::with_last_byte(1)));
    }

    #[test]
    fn virtual_node_interface_result_records_root_trace() {
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                gas_limit: 10_000,
                data: Bytes::from_static(&[0xaa, 0xbb, 0xcc, 0xdd]),
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let output = Bytes::from_static(&[1, 2, 3]);
        let result = ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::new(10_000, 123, 0, 0, 0),
            logs: Vec::new(),
            output: Output::Call(output.clone()),
        };
        let mut inspector = TracingInspector::new(TracingInspectorConfig::default_parity());

        ArbitrumApiImpl::<()>::record_virtual_trace(&mut inspector, &tx, &result, &[]);

        let traces = inspector.into_traces().into_nodes();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trace.caller, tx.caller());
        assert_eq!(traces[0].trace.address, NODE_INTERFACE_ADDRESS);
        assert_eq!(traces[0].trace.data, tx.input().clone());
        assert_eq!(traces[0].trace.output, output);
        assert_eq!(traces[0].trace.gas_used, 123);
        assert!(traces[0].trace.success);
    }

    #[test]
    fn virtual_node_interface_trace_wraps_existing_child_trace() {
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                gas_limit: 10_000,
                data: Bytes::from_static(&[0xaa, 0xbb, 0xcc, 0xdd]),
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let result = ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::new(10_000, 123, 0, 0, 0),
            logs: Vec::new(),
            output: Output::Call(Bytes::from_static(&[1, 2, 3])),
        };
        let mut inspector = TracingInspector::new(TracingInspectorConfig::default_parity());
        inspector.traces_mut().nodes_mut()[0].trace = CallTrace {
            depth: 0,
            caller: Address::with_last_byte(2),
            address: Address::with_last_byte(3),
            gas_limit: 9_000,
            ..Default::default()
        };

        ArbitrumApiImpl::<()>::record_virtual_trace(&mut inspector, &tx, &result, &[]);

        let traces = inspector.into_traces().into_nodes();
        assert_eq!(traces.len(), 2);
        assert_eq!(traces[0].trace.address, NODE_INTERFACE_ADDRESS);
        assert_eq!(traces[0].children, vec![1]);
        assert_eq!(traces[1].parent, Some(0));
        assert_eq!(traces[1].trace.depth, 1);
        assert_eq!(traces[1].trace.address, Address::with_last_byte(3));
    }

    #[test]
    fn virtual_node_interface_trace_wraps_each_scheduled_transaction() {
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                gas_limit: 10_000,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let result = ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::new(10_000, 123, 0, 0, 0),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        };
        let mut inspector = TracingInspector::new(TracingInspectorConfig::default_parity());
        let nodes = inspector.traces_mut().nodes_mut();
        nodes[0].trace.gas_limit = 1;
        nodes.push(revm_inspectors::tracing::types::CallTraceNode {
            idx: 1,
            parent: Some(0),
            trace: CallTrace {
                depth: 1,
                gas_limit: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        nodes[0].children.push(1);
        nodes.push(revm_inspectors::tracing::types::CallTraceNode {
            idx: 2,
            trace: CallTrace {
                gas_limit: 1,
                ..Default::default()
            },
            ..Default::default()
        });

        ArbitrumApiImpl::<()>::record_virtual_trace(&mut inspector, &tx, &result, &[]);

        let traces = inspector.into_traces().into_nodes();
        assert_eq!(traces[0].children, vec![1, 3]);
        assert_eq!(traces[1].parent, Some(0));
        assert_eq!(traces[2].parent, Some(1));
        assert_eq!(traces[3].parent, Some(0));
    }
}
