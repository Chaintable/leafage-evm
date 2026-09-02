use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, SimulationExecutionOutput};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy::primitives::{Address, Log};
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arc::{ArcChainConfig, ArcContext, ArcEvmFactory};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, MainnetSpecId, U256};
use revm::{
    bytecode::OpCode,
    context::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
        ContextTr, JournalTr, TxEnv,
    },
    database::WrapDatabaseRef,
    inspector::{Inspector, NoOpInspector},
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm,
};
use revm_inspectors::tracing::{OpcodeFilter, TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type ArcApiImpl<DB> = ApiImpl<DB, MainnetSpecId, ArcChainConfig>;

/// Adds callbacks for Arc logs written directly to the journal while keeping
/// the normal `TracingInspector` callbacks exactly once.
// TODO: Remove this Arc-local journal cursor and emitter sidecar after upgrading to
// `revm-inspector >= 42.0.0` and a compatible `revm-inspectors >= 0.37.0`. Upstream
// `bluealloy/revm#3816` forwards journal-log deltas, and
// `paradigmxyz/revm-inspectors#413` preserves `CallLog.address`; run cross-chain trace
// regressions before deleting this compatibility layer.
struct ArcTracingInspector {
    inner: TracingInspector,
    journal_log_count: usize,
    // Indexed by the global CallLog::index assigned by TracingInspector.
    log_emitters: Vec<Address>,
}

impl ArcTracingInspector {
    fn new(config: TracingInspectorConfig) -> Self {
        Self {
            inner: TracingInspector::new(config),
            journal_log_count: 0,
            log_emitters: Vec::new(),
        }
    }

    fn into_parts(self) -> (TracingInspector, Vec<Address>) {
        (self.inner, self.log_emitters)
    }

    fn record_log<DB>(&mut self, context: &mut ArcContext<DB>, log: Log)
    where
        DB: revm::Database,
    {
        let emitter = log.address;
        self.inner.log(context, log);
        self.log_emitters.push(emitter);
    }

    fn sync_journal<DB>(&mut self, context: &mut ArcContext<DB>)
    where
        DB: revm::Database,
    {
        let logs_len = context.journal_ref().logs().len();
        if logs_len < self.journal_log_count {
            self.journal_log_count = logs_len;
            return;
        }

        let new_logs = context.journal_ref().logs()[self.journal_log_count..].to_vec();
        self.journal_log_count = logs_len;
        for log in new_logs {
            self.record_log(context, log);
        }
    }

    fn capture_callback_log<DB>(&mut self, context: &mut ArcContext<DB>, log: Log)
    where
        DB: revm::Database,
    {
        self.record_log(context, log);
        self.journal_log_count = context.journal_ref().logs().len();
    }
}

impl<DB> Inspector<ArcContext<DB>> for ArcTracingInspector
where
    DB: revm::Database,
{
    fn initialize_interp(&mut self, interpreter: &mut Interpreter, context: &mut ArcContext<DB>) {
        self.sync_journal(context);
        self.inner.initialize_interp(interpreter, context);
    }

    fn step(&mut self, interpreter: &mut Interpreter, context: &mut ArcContext<DB>) {
        // A failed child is reverted after its call_end callback. Observe that
        // shorter journal before the parent executes another instruction, so
        // a direct Arc log from that instruction is not mistaken for an old
        // child log and skipped by step_end.
        self.sync_journal(context);
        self.inner.step(interpreter, context);
    }

    fn step_end(&mut self, interpreter: &mut Interpreter, context: &mut ArcContext<DB>) {
        self.inner.step_end(interpreter, context);
        self.sync_journal(context);
    }

    fn log(&mut self, context: &mut ArcContext<DB>, log: Log) {
        self.capture_callback_log(context, log);
    }

    fn log_full(&mut self, interpreter: &mut Interpreter, context: &mut ArcContext<DB>, log: Log) {
        let emitter = log.address;
        self.inner.log_full(interpreter, context, log);
        self.log_emitters.push(emitter);
        self.journal_log_count = context.journal_ref().logs().len();
    }

    fn call(
        &mut self,
        context: &mut ArcContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.sync_journal(context);
        self.inner.call(context, inputs)
    }

    fn call_end(
        &mut self,
        context: &mut ArcContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.sync_journal(context);
        self.inner.call_end(context, inputs, outcome);
    }

    fn create(
        &mut self,
        context: &mut ArcContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.sync_journal(context);
        self.inner.create(context, inputs)
    }

    fn create_end(
        &mut self,
        context: &mut ArcContext<DB>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.sync_journal(context);
        self.inner.create_end(context, inputs, outcome);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        <TracingInspector as Inspector<ArcContext<DB>>>::selfdestruct(
            &mut self.inner,
            contract,
            target,
            value,
        );
    }
}

impl<DB> ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    fn arc_factory(&self) -> Result<ArcEvmFactory, String> {
        self.evm_cfg
            .custom_cfg
            .map(ArcEvmFactory::new)
            .ok_or_else(|| "Arc EVM chain configuration is missing".to_string())
    }
}

impl<DB> GasFeeHandler for ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
}

impl<DB> EvmExecutor for ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
    }

    fn transact<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), NoOpInspector {})
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        evm.transact(tx).map(|result| result.result)
    }

    fn inspect_tx_commit<StateDB, R, F>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
        F: FnOnce(TracingInspector) -> R,
    {
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), &mut inspector)
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        let result = evm.inspect_tx_commit(tx)?;
        drop(evm);
        Ok((result, inspector_collect(inspector)))
    }

    fn execute_simulation<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx_hash: leafage_evm_types::H256,
        tx: Self::Tx,
    ) -> Result<
        SimulationExecutionOutput<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let mut inspector_cfg = TracingInspectorConfig::default_parity()
            .set_record_logs(true)
            .set_steps(true)
            .set_exclude_precompile_calls(false);
        inspector_cfg.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
        let env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let mut inspector = ArcTracingInspector::new(inspector_cfg);
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), &mut inspector)
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        let result = evm.inspect_tx_commit(tx)?;
        drop(evm);
        let (inspector, log_emitters) = inspector.into_parts();
        let (traces, mut events) =
            super::simulation::build_debank_traces(tx_hash, inspector.into_traces(), &log_emitters);
        if !result.is_success() {
            events.clear();
        }

        Ok(SimulationExecutionOutput {
            result,
            traces,
            events,
        })
    }
}

impl<DB> ApiCore for ArcApiImpl<DB> where DB: Sync + Send + 'static {}
