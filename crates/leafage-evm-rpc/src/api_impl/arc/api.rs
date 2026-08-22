use crate::api_impl::core::{
    ApiCore, ArcEstimateGasPolicy, EstimateGasPolicy, EvmExecutor, GasFeeHandler,
};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use crate::error::{internal_rpc_err, invalid_params_rpc_err, rpc_err, rpc_error_with_code};
use alloy::consensus::BlockHeader;
use alloy::eips::eip2935::HISTORY_STORAGE_ADDRESS;
use alloy::primitives::{Address, Log};
use alloy::signers::Either;
use alloy::sol_types::{ContractError, GenericRevertReason, RevertReason};
use alloy_evm::{
    rpc::{CallFeesError, EthTxEnvError, TryIntoTxEnv},
    EvmEnv,
};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arc::{ArcChainConfig, ArcContext, ArcEvmFactory};
use leafage_evm_types::{
    BlockEnv, BlockInfo, Bytes, CallRequest, DebankErrorCode, MainnetSpecId, U256,
};
use revm::{
    context::{
        result::{
            EVMError, ExecutionResult, HaltReason, InvalidHeader, InvalidTransaction, OutOfGasError,
        },
        ContextTr, JournalTr, TxEnv,
    },
    context_interface::Block as _,
    database::WrapDatabaseRef,
    inspector::{Inspector, NoOpInspector},
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm, SystemCallEvm,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type ArcApiImpl<DB> = ApiImpl<DB, MainnetSpecId, ArcChainConfig>;

const RETH_INVALID_INPUT_CODE: i32 = -32000;
const RETH_TRANSACTION_REJECTED_CODE: i32 = -32003;
const RETH_EXECUTION_ERROR_CODE: i32 = 3;

#[derive(Clone, Copy)]
enum ArcCallPreparationErrorPolicy {
    Debank,
    Reth,
}

fn arc_call_database_error(
    policy: ArcCallPreparationErrorPolicy,
    message: String,
) -> jsonrpsee::types::ErrorObjectOwned {
    match policy {
        ArcCallPreparationErrorPolicy::Debank => {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, message)
        }
        ArcCallPreparationErrorPolicy::Reth => internal_rpc_err(message),
    }
}

fn arc_nonce_database_error(
    policy: ArcCallPreparationErrorPolicy,
    message: String,
) -> jsonrpsee::types::ErrorObjectOwned {
    match policy {
        ArcCallPreparationErrorPolicy::Debank => internal_rpc_err("get nonce failed"),
        ArcCallPreparationErrorPolicy::Reth => internal_rpc_err(message),
    }
}

fn arc_tx_env_error(
    policy: ArcCallPreparationErrorPolicy,
    error: EthTxEnvError,
) -> jsonrpsee::types::ErrorObjectOwned {
    match policy {
        ArcCallPreparationErrorPolicy::Reth => match error {
            EthTxEnvError::CallFees(CallFeesError::FeeCapTooLow) => {
                rpc_error_with_code(RETH_INVALID_INPUT_CODE, error.to_string())
            }
            EthTxEnvError::CallFees(CallFeesError::ConflictingFeeFieldsInRequest)
            | EthTxEnvError::Input(_) => invalid_params_rpc_err(error.to_string()),
            EthTxEnvError::CallFees(_) => {
                rpc_error_with_code(RETH_TRANSACTION_REJECTED_CODE, error.to_string())
            }
        },
        ArcCallPreparationErrorPolicy::Debank => match error {
            EthTxEnvError::CallFees(CallFeesError::ConflictingFeeFieldsInRequest) => {
                invalid_params_rpc_err("Invalid fee parameters")
            }
            EthTxEnvError::Input(_) => invalid_params_rpc_err(error.to_string()),
            EthTxEnvError::CallFees(_) => {
                rpc_error_with_code(DebankErrorCode::EvmFailed as i32, error.to_string())
            }
        },
    }
}

fn arc_raw_fees(
    gas_price: Option<U256>,
    max_fee_per_gas: Option<U256>,
    max_priority_fee_per_gas: Option<U256>,
    block_base_fee: U256,
    blob_versioned_hashes: Option<&[leafage_evm_types::H256]>,
    max_fee_per_blob_gas: Option<U256>,
    block_blob_fee: Option<U256>,
) -> Option<(U256, Option<U256>, Option<U256>)> {
    let has_blob_hashes = blob_versioned_hashes.is_some_and(|hashes| !hashes.is_empty());
    let blob_fee = has_blob_hashes
        .then(|| max_fee_per_blob_gas.or(block_blob_fee))
        .flatten();
    if gas_price.is_some()
        && (max_fee_per_gas.is_some()
            || max_priority_fee_per_gas.is_some()
            || max_fee_per_blob_gas.is_some())
    {
        return None;
    }
    match (gas_price, max_fee_per_gas, max_priority_fee_per_gas) {
        (gas_price, None, None) => Some((gas_price.unwrap_or_default(), None, blob_fee)),
        (None, max_fee_per_gas, max_priority_fee_per_gas) => Some((
            max_fee_per_gas.unwrap_or_else(|| {
                block_base_fee.max(max_priority_fee_per_gas.unwrap_or_default())
            }),
            Some(max_priority_fee_per_gas.unwrap_or_default()),
            blob_fee,
        )),
        _ => None,
    }
}

fn arc_call_transaction_error(error: &InvalidTransaction) -> jsonrpsee::types::ErrorObjectOwned {
    use InvalidTransaction::*;

    let (code, message) = match error {
        InvalidChainId | MissingChainId => {
            (RETH_INVALID_INPUT_CODE, "invalid chain ID".to_string())
        }
        PriorityFeeGreaterThanMaxFee => (
            RETH_TRANSACTION_REJECTED_CODE,
            "max priority fee per gas higher than max fee per gas".to_string(),
        ),
        GasPriceLessThanBasefee => (
            RETH_INVALID_INPUT_CODE,
            "max fee per gas less than block base fee".to_string(),
        ),
        CallerGasLimitMoreThanBlock | TxGasLimitGreaterThanCap { .. } => (
            RETH_INVALID_INPUT_CODE,
            "intrinsic gas too high".to_string(),
        ),
        CallGasCostMoreThanGasLimit { .. } | GasFloorMoreThanGasLimit { .. } => {
            (RETH_INVALID_INPUT_CODE, "intrinsic gas too low".to_string())
        }
        RejectCallerWithCode => (
            RETH_TRANSACTION_REJECTED_CODE,
            "sender is not an EOA".to_string(),
        ),
        LackOfFundForMaxFee { fee, balance } => (
            RETH_TRANSACTION_REJECTED_CODE,
            format!("insufficient funds for gas * price + value: have {balance} want {fee}"),
        ),
        OverflowPaymentInTransaction => (
            RETH_TRANSACTION_REJECTED_CODE,
            "gas uint64 overflow".to_string(),
        ),
        NonceOverflowInTransaction => (
            RETH_TRANSACTION_REJECTED_CODE,
            "nonce has max value".to_string(),
        ),
        NonceTooHigh { .. } => (RETH_INVALID_INPUT_CODE, "nonce too high".to_string()),
        NonceTooLow { tx, state } => (
            RETH_INVALID_INPUT_CODE,
            format!("nonce too low: next nonce {state}, tx nonce {tx}"),
        ),
        CreateInitCodeSizeLimit => (
            RETH_TRANSACTION_REJECTED_CODE,
            "max initcode size exceeded".to_string(),
        ),
        AccessListNotSupported => (
            RETH_TRANSACTION_REJECTED_CODE,
            "transactions before Berlin should not have access list".to_string(),
        ),
        MaxFeePerBlobGasNotSupported => (
            RETH_TRANSACTION_REJECTED_CODE,
            "max_fee_per_blob_gas is not supported for blocks before the Cancun hardfork"
                .to_string(),
        ),
        BlobVersionedHashesNotSupported => (
            RETH_TRANSACTION_REJECTED_CODE,
            "blob_versioned_hashes is not supported for blocks before the Cancun hardfork"
                .to_string(),
        ),
        BlobGasPriceGreaterThanMax { .. } => (
            RETH_TRANSACTION_REJECTED_CODE,
            "max fee per blob gas less than block blob gas fee".to_string(),
        ),
        EmptyBlobs => (
            RETH_TRANSACTION_REJECTED_CODE,
            "blob transaction missing blob hashes".to_string(),
        ),
        BlobCreateTransaction => (
            RETH_TRANSACTION_REJECTED_CODE,
            "blob transaction is a create transaction".to_string(),
        ),
        TooManyBlobs { have, .. } => (
            RETH_TRANSACTION_REJECTED_CODE,
            format!("blob transaction exceeds max blobs per block; got {have}"),
        ),
        BlobVersionNotSupported => (
            RETH_TRANSACTION_REJECTED_CODE,
            "blob hash version mismatch".to_string(),
        ),
        AuthorizationListNotSupported => (
            RETH_TRANSACTION_REJECTED_CODE,
            "EIP-7702 authorization list not supported".to_string(),
        ),
        AuthorizationListInvalidFields | EmptyAuthorizationList => (
            RETH_TRANSACTION_REJECTED_CODE,
            "EIP-7702 authorization list has invalid fields".to_string(),
        ),
        Eip2930NotSupported | Eip1559NotSupported | Eip4844NotSupported | Eip7702NotSupported
        | Eip7873NotSupported => (
            RETH_TRANSACTION_REJECTED_CODE,
            "transaction type not supported".to_string(),
        ),
        Eip7873MissingTarget | Str(_) => return internal_rpc_err(error.to_string()),
    };
    rpc_error_with_code(code, message)
}

fn arc_call_error<DBError>(
    error: &EVMError<DBError, InvalidTransaction>,
) -> jsonrpsee::types::ErrorObjectOwned
where
    DBError: std::error::Error,
{
    match error {
        EVMError::Transaction(error) => arc_call_transaction_error(error),
        EVMError::Database(error) => internal_rpc_err(error.to_string()),
        EVMError::Header(InvalidHeader::PrevrandaoNotSet) => {
            internal_rpc_err("prevrandao not in the EVM's environment after merge")
        }
        EVMError::Header(InvalidHeader::ExcessBlobGasNotSet) => {
            internal_rpc_err("excess blob gas missing in the EVM's environment after Cancun")
        }
        EVMError::Custom(error) => internal_rpc_err(format!("Revm error: {error}")),
    }
}

fn arc_call_result(result: ExecutionResult<HaltReason>) -> RpcResult<Bytes> {
    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data().0.into()),
        ExecutionResult::Revert { output, .. } => {
            let mut message = "execution reverted".to_string();
            if let Some(reason) = GenericRevertReason::decode(&output) {
                let decoded = reason.to_string();
                let decoded = if matches!(
                    reason,
                    RevertReason::ContractError(ContractError::Revert(_))
                ) {
                    decoded.trim_start_matches("revert: ")
                } else {
                    decoded.as_str()
                };
                message.push_str(": ");
                message.push_str(decoded);
            }
            let data = (!output.is_empty()).then_some(output.as_ref());
            Err(rpc_err(RETH_EXECUTION_ERROR_CODE, message, data))
        }
        ExecutionResult::Halt { reason, gas, .. } => {
            let gas_used = gas.used();
            let message = match reason {
                HaltReason::OutOfGas(OutOfGasError::Basic | OutOfGasError::ReentrancySentry) => {
                    format!("out of gas: gas required exceeds: {gas_used}")
                }
                HaltReason::OutOfGas(OutOfGasError::Memory) => {
                    format!("out of gas: gas exhausted during memory expansion: {gas_used}")
                }
                HaltReason::OutOfGas(OutOfGasError::MemoryLimit) => {
                    "out of memory: memory limit exceeded during memory expansion".to_string()
                }
                HaltReason::OutOfGas(OutOfGasError::Precompile) => format!(
                    "out of gas: gas exhausted during precompiled contract execution: {gas_used}"
                ),
                HaltReason::OutOfGas(OutOfGasError::InvalidOperand) => {
                    format!("out of gas: invalid operand to an opcode: {gas_used}")
                }
                HaltReason::NonceOverflow => "nonce has max value".to_string(),
                reason => format!("EVM error: {reason:?}"),
            };
            Err(rpc_error_with_code(RETH_TRANSACTION_REJECTED_CODE, message))
        }
    }
}

/// Adds callbacks for Arc logs written directly to the journal while keeping
/// the normal `TracingInspector` callbacks exactly once.
struct ArcTracingInspector {
    inner: TracingInspector,
    journal_log_count: usize,
    // Indexed by the global CallLog::index assigned by TracingInspector.
    log_emitters: Vec<Address>,
    record_log_emitters: bool,
}

impl ArcTracingInspector {
    fn new(config: TracingInspectorConfig) -> Self {
        let record_log_emitters = config.record_logs;
        Self {
            inner: TracingInspector::new(config),
            journal_log_count: 0,
            log_emitters: Vec::new(),
            record_log_emitters,
        }
    }

    fn into_inner(self) -> TracingInspector {
        self.inner
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
        if self.record_log_emitters {
            self.log_emitters.push(emitter);
        }
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
        if self.record_log_emitters {
            self.log_emitters.push(emitter);
        }
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

    fn prepare_arc_transaction_request<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        mut cfg: leafage_evm_types::CfgEnv<MainnetSpecId>,
        mut request: CallRequest,
        db: &StateDB,
        chain_id: u64,
        error_policy: ArcCallPreparationErrorPolicy,
    ) -> RpcResult<(leafage_evm_types::CfgEnv<MainnetSpecId>, CallRequest)> {
        if self.evm_cfg.custom_cfg.is_none() {
            return Err(internal_rpc_err("Arc EVM chain configuration is missing"));
        }

        let max_gas_limit = cfg
            .tx_gas_limit_cap
            .filter(|cap| *cap != 0)
            .unwrap_or(block_env.gas_limit);
        request.gas = Some(request.gas.unwrap_or(max_gas_limit).min(max_gas_limit));

        if request.nonce.is_none() {
            let caller = request.from.unwrap_or_default();
            let nonce = db
                .basic_ref(caller)
                .map_err(|error| arc_nonce_database_error(error_policy, error.to_string()))?
                .map(|account| account.nonce)
                .unwrap_or_default();
            request.nonce = Some(nonce);
        }

        cfg.chain_id = chain_id;
        Ok((cfg, request))
    }

    fn create_arc_call_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        cfg: leafage_evm_types::CfgEnv<MainnetSpecId>,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
        error_policy: ArcCallPreparationErrorPolicy,
    ) -> RpcResult<TxEnv> {
        let (cfg, request) = self.prepare_arc_transaction_request(
            block_env,
            cfg,
            request,
            &db,
            chain_id,
            error_policy,
        )?;
        request
            .inner
            .try_into_tx_env(&EvmEnv::new(cfg, block_env.clone()))
            .map_err(|error| arc_tx_env_error(error_policy, error))
    }

    fn create_arc_raw_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        cfg: leafage_evm_types::CfgEnv<MainnetSpecId>,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
        error_policy: ArcCallPreparationErrorPolicy,
    ) -> RpcResult<TxEnv> {
        let (_cfg, request) = self.prepare_arc_transaction_request(
            block_env,
            cfg,
            request,
            &db,
            chain_id,
            error_policy,
        )?;
        // Preserve the custom simulation/trace request contract: this path
        // keeps raw max-fee fields and does not adopt call-only strict input
        // validation. The minimal type still follows Alloy's field rules.
        let tx_type = request.inner.minimal_tx_type() as u8;
        let alloy::rpc::types::TransactionRequest {
            from,
            to,
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            max_fee_per_blob_gas,
            gas,
            value,
            input,
            chain_id: request_chain_id,
            access_list,
            blob_versioned_hashes,
            authorization_list,
            nonce,
            ..
        } = request.inner;
        let (gas_price, max_priority_fee_per_gas, max_fee_per_blob_gas) = arc_raw_fees(
            gas_price.map(U256::from),
            max_fee_per_gas.map(U256::from),
            max_priority_fee_per_gas.map(U256::from),
            U256::from(block_env.basefee),
            blob_versioned_hashes.as_deref(),
            max_fee_per_blob_gas.map(U256::from),
            block_env.blob_gasprice().map(U256::from),
        )
        .ok_or_else(|| invalid_params_rpc_err("Invalid fee parameters"))?;

        Ok(TxEnv {
            tx_type,
            gas_limit: gas.unwrap_or(block_env.gas_limit),
            nonce: nonce.unwrap_or_default(),
            caller: from.unwrap_or_default(),
            gas_price: gas_price.saturating_to(),
            gas_priority_fee: max_priority_fee_per_gas.map(|fee| fee.saturating_to()),
            kind: to.unwrap_or(revm::primitives::TxKind::Create),
            value: value.unwrap_or_default(),
            data: input.into_input().unwrap_or_default(),
            chain_id: request_chain_id.or(Some(chain_id)),
            access_list: access_list.unwrap_or_default(),
            blob_hashes: blob_versioned_hashes.unwrap_or_default(),
            max_fee_per_blob_gas: max_fee_per_blob_gas
                .map(|fee| fee.saturating_to())
                .unwrap_or_default(),
            authorization_list: authorization_list
                .unwrap_or_default()
                .into_iter()
                .map(Either::Left)
                .collect(),
        })
    }

    fn prepare_call_tx<StateDB: DatabaseRef>(
        &self,
        mut block_env: BlockEnv,
        mut request: CallRequest,
        db: StateDB,
        chain_id: u64,
        error_policy: ArcCallPreparationErrorPolicy,
    ) -> RpcResult<(BlockEnv, TxEnv)> {
        let request_has_gas_limit = request.gas.is_some();
        let rpc_gas_cap = self
            .evm_cfg
            .cfg
            .tx_gas_limit_cap
            .filter(|cap| *cap != 0)
            .unwrap_or(u64::MAX);
        request.gas = Some(request.gas.unwrap_or(rpc_gas_cap).min(rpc_gas_cap));
        request.nonce = None;

        let mut call_cfg = self.evm_cfg.cfg.clone();
        call_cfg.tx_gas_limit_cap = Some(u64::MAX);
        let mut tx = self.create_arc_call_txn_env(
            &block_env,
            call_cfg,
            request,
            &db,
            chain_id,
            error_policy,
        )?;

        if tx.gas_price == 0 {
            block_env.basefee = 0;
        } else if !request_has_gas_limit {
            let balance = db
                .basic_ref(tx.caller)
                .map_err(|error| arc_call_database_error(error_policy, error.to_string()))?
                .map(|account| account.balance)
                .unwrap_or_default();
            let spendable = balance
                .checked_sub(tx.value)
                .ok_or_else(|| match error_policy {
                    ArcCallPreparationErrorPolicy::Debank => rpc_error_with_code(
                        DebankErrorCode::BalanceExhausted as i32,
                        "Insufficient funds".to_string(),
                    ),
                    ArcCallPreparationErrorPolicy::Reth => rpc_error_with_code(
                        RETH_TRANSACTION_REJECTED_CODE,
                        format!(
                            "insufficient funds for gas * price + value: have {balance} want {}",
                            tx.value
                        ),
                    ),
                })?;
            let allowance = spendable
                .checked_div(U256::from(tx.gas_price))
                .unwrap_or_default()
                .min(U256::from(block_env.gas_limit));
            tx.gas_limit = u64::try_from(allowance)
                .map_err(|_| internal_rpc_err("Arc call gas allowance does not fit in u64"))?;
        }

        Ok((block_env, tx))
    }
}

impl<DB> GasFeeHandler for ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;

    fn estimate_gas_policy(&self) -> EstimateGasPolicy {
        EstimateGasPolicy::Arc(ArcEstimateGasPolicy)
    }
}

impl<DB> EvmExecutor for ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn arc_chain_config(&self) -> Option<ArcChainConfig> {
        self.evm_cfg.custom_cfg
    }

    fn call_error<DBError>(
        &self,
        error: &EVMError<DBError, Self::TransactionError>,
    ) -> jsonrpsee::types::ErrorObjectOwned
    where
        DBError: std::error::Error,
    {
        arc_call_error(error)
    }

    fn call_result(&self, result: ExecutionResult<Self::EvmHaltReason>) -> RpcResult<Bytes> {
        arc_call_result(result)
    }

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        if self.evm_cfg.custom_cfg.is_none() {
            return Err(internal_rpc_err("Arc EVM chain configuration is missing"));
        }
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
    }

    fn create_txn_env_for_call<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<(BlockEnv, Self::Tx)> {
        self.prepare_call_tx(
            block_env,
            request,
            db,
            chain_id,
            ArcCallPreparationErrorPolicy::Debank,
        )
    }

    fn create_txn_env_for_estimate<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        self.create_arc_call_txn_env(
            block_env,
            self.evm_cfg.cfg.clone(),
            request,
            db,
            chain_id,
            ArcCallPreparationErrorPolicy::Debank,
        )
    }

    fn create_txn_env_for_eth_call<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<(BlockEnv, Self::Tx)> {
        self.prepare_call_tx(
            block_env,
            request,
            db,
            chain_id,
            ArcCallPreparationErrorPolicy::Reth,
        )
    }

    fn create_txn_env_for_simulation<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let mut simulation_cfg = self.evm_cfg.cfg.clone();
        simulation_cfg.tx_gas_limit_cap = Some(
            simulation_cfg
                .tx_gas_limit_cap
                .filter(|cap| *cap != 0)
                .unwrap_or(u64::MAX),
        );
        self.create_arc_raw_txn_env(
            block_env,
            simulation_cfg,
            request,
            db,
            chain_id,
            ArcCallPreparationErrorPolicy::Debank,
        )
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
        if block_env.number.is_zero() {
            return Ok(());
        }

        let factory = self.arc_factory().map_err(internal_rpc_err)?;
        let env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let result = factory
            .create(env, WrapDatabaseRef(&*state), NoOpInspector {})
            .map_err(|error| internal_rpc_err(error.to_string()))?
            .system_call(HISTORY_STORAGE_ADDRESS, header.parent_hash().0.into())
            .map_err(|error| {
                internal_rpc_err(format!(
                    "EIP-2935 blockhashes contract call failed: {error}"
                ))
            })?;
        state.commit(result.state);
        Ok(())
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

    fn transact_for_call<StateDB>(
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
        let mut cfg = self.evm_cfg.cfg.clone();
        cfg.disable_block_gas_limit = true;
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(u64::MAX);
        let env = EvmEnv::new(cfg, block_env.clone());
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), NoOpInspector {})
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        evm.transact(tx).map(|result| result.result)
    }

    fn transact_for_estimate<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
        hard_gas_cap: u64,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let mut cfg = self.evm_cfg.cfg.clone();
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(hard_gas_cap);
        let env = EvmEnv::new(cfg, block_env.clone());
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
        let mut inspector = ArcTracingInspector::new(inspector_cfg);
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), &mut inspector)
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        let result = evm.inspect_tx_commit(tx)?;
        drop(evm);
        Ok((result, inspector_collect(inspector.into_inner())))
    }

    fn inspect_tx_commit_for_simulation<StateDB, R, F>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R, Vec<Address>),
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
        F: FnOnce(TracingInspector) -> R,
    {
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let mut cfg = self.evm_cfg.cfg.clone();
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(
            cfg.tx_gas_limit_cap
                .filter(|cap| *cap != 0)
                .unwrap_or(u64::MAX),
        );
        let env = EvmEnv::new(cfg, block_env.clone());
        let mut inspector = ArcTracingInspector::new(inspector_cfg);
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), &mut inspector)
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        let result = evm.inspect_tx_commit(tx)?;
        drop(evm);
        let (inspector, log_emitters) = inspector.into_parts();
        Ok((result, inspector_collect(inspector), log_emitters))
    }
}

impl<DB> ApiCore for ArcApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::TxType;
    use alloy::sol_types::{Revert, SolError};
    use leafage_evm_chains::arc::ARC_MAINNET_CHAIN_ID;
    use revm::{
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        state::{AccountInfo, Bytecode},
    };
    use serde_json::json;

    #[derive(Debug)]
    struct MockDatabaseError;

    impl std::fmt::Display for MockDatabaseError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("injected Arc call database failure")
        }
    }

    impl std::error::Error for MockDatabaseError {}
    impl DBErrorMarker for MockDatabaseError {}

    #[derive(Debug)]
    struct FailingDatabase;

    impl DatabaseRef for FailingDatabase {
        type Error = MockDatabaseError;

        fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(MockDatabaseError)
        }

        fn code_by_hash_ref(&self, _: alloy::primitives::B256) -> Result<Bytecode, Self::Error> {
            Err(MockDatabaseError)
        }

        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(MockDatabaseError)
        }

        fn block_hash_ref(&self, _: u64) -> Result<alloy::primitives::B256, Self::Error> {
            Err(MockDatabaseError)
        }
    }

    fn test_api() -> ArcApiImpl<()> {
        let arc_config = ArcChainConfig::mainnet();
        let mut cfg = leafage_evm_types::CfgEnv::new_with_spec(arc_config.ethereum_spec());
        cfg.chain_id = arc_config.chain_id();
        cfg.tx_gas_limit_cap = Some(30_000_000);
        ApiImpl::new(
            (),
            cfg,
            Some(arc_config),
            None,
            None,
            None,
            false,
            false,
            "arc-call-builder-test".to_string(),
            0,
            None,
            None,
            None,
        )
    }

    fn test_block_env() -> BlockEnv {
        let mut block_env = BlockEnv::default();
        block_env.basefee = 100;
        block_env.gas_limit = 30_000_000;
        block_env
    }

    fn request(value: serde_json::Value) -> CallRequest {
        serde_json::from_value(value).expect("valid call request")
    }

    #[test]
    fn arc_call_internal_errors_follow_reth_messages() {
        let error: EVMError<MockDatabaseError, InvalidTransaction> =
            EVMError::Database(MockDatabaseError);
        let rpc_error = arc_call_error(&error);

        assert_eq!(rpc_error.code(), -32603);
        assert_eq!(rpc_error.message(), "injected Arc call database failure");
        assert!(rpc_error.data().is_none());

        let custom: EVMError<MockDatabaseError, InvalidTransaction> =
            EVMError::Custom("injected custom failure".to_string());
        let custom = arc_call_error(&custom);
        assert_eq!(custom.code(), -32603);
        assert_eq!(custom.message(), "Revm error: injected custom failure");
        assert!(custom.data().is_none());

        for (header, message) in [
            (
                InvalidHeader::PrevrandaoNotSet,
                "prevrandao not in the EVM's environment after merge",
            ),
            (
                InvalidHeader::ExcessBlobGasNotSet,
                "excess blob gas missing in the EVM's environment after Cancun",
            ),
        ] {
            let header: EVMError<MockDatabaseError, InvalidTransaction> = EVMError::Header(header);
            let header = arc_call_error(&header);
            assert_eq!(header.code(), -32603);
            assert_eq!(header.message(), message);
            assert!(header.data().is_none());
        }
    }

    #[test]
    fn arc_call_and_simulation_fee_builders_keep_distinct_prices() {
        let api = test_api();
        let block_env = test_block_env();
        let db = InMemoryDB::default();
        let dynamic = request(json!({
            "from": "0x0000000000000000000000000000000000000001",
            "to": "0x0000000000000000000000000000000000000002",
            "gas": "0x186a0",
            "nonce": "0x0",
            "maxFeePerGas": "0x3e8",
            "maxPriorityFeePerGas": "0x1"
        }));

        let call = api
            .create_arc_call_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                dynamic.clone(),
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Reth,
            )
            .unwrap();
        let simulation = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                dynamic,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap();

        assert_eq!(call.tx_type, TxType::Eip1559 as u8);
        assert_eq!(call.gas_price, 101);
        assert_eq!(call.gas_priority_fee, Some(1));
        assert_eq!(simulation.tx_type, TxType::Eip1559 as u8);
        assert_eq!(simulation.gas_price, 1_000);
        assert_eq!(simulation.gas_priority_fee, Some(1));

        let only_tip = request(json!({
            "nonce": "0x0",
            "maxPriorityFeePerGas": "0x96"
        }));
        let only_tip = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                only_tip,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap();
        assert_eq!(only_tip.gas_price, 150);
        assert_eq!(only_tip.gas_priority_fee, Some(150));

        let only_max_fee = request(json!({
            "nonce": "0x0",
            "maxFeePerGas": "0x3e8"
        }));
        let only_max_fee = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                only_max_fee,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap();
        assert_eq!(only_max_fee.gas_price, 1_000);
        assert_eq!(only_max_fee.gas_priority_fee, Some(0));
    }

    #[test]
    fn arc_default_builder_preserves_pre_trace_nonce_behavior() {
        let api = test_api();
        let block_env = test_block_env();
        let caller = Address::with_last_byte(1);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                nonce: 7,
                ..Default::default()
            },
        );
        let explicit_nonce = request(json!({
            "from": format!("{caller:#x}"),
            "nonce": "0x63"
        }));

        let default_tx = api
            .create_txn_env(
                &BlockInfo::default(),
                &block_env,
                explicit_nonce.clone(),
                &db,
                ARC_MAINNET_CHAIN_ID,
            )
            .unwrap();
        let simulation_tx = api
            .create_txn_env_for_simulation(
                &BlockInfo::default(),
                &block_env,
                explicit_nonce,
                &db,
                ARC_MAINNET_CHAIN_ID,
            )
            .unwrap();

        assert_eq!(default_tx.nonce, 7);
        assert_eq!(simulation_tx.nonce, 99);
    }

    #[test]
    fn arc_call_builder_enforces_reth_fee_input_and_blob_rules() {
        let api = test_api();
        let block_env = test_block_env();
        let db = InMemoryDB::default();

        for (value, code, message) in [
            (
                json!({ "nonce": "0x0", "maxFeePerGas": "0x63" }),
                RETH_INVALID_INPUT_CODE,
                "max fee per gas less than block base fee",
            ),
            (
                json!({
                    "nonce": "0x0",
                    "gasPrice": "0x64",
                    "maxFeePerGas": "0x64"
                }),
                jsonrpsee::types::error::INVALID_PARAMS_CODE,
                "both gasPrice and (maxFeePerGas or maxPriorityFeePerGas) specified",
            ),
            (
                json!({ "nonce": "0x0", "blobVersionedHashes": [] }),
                RETH_TRANSACTION_REJECTED_CODE,
                "blob transaction missing blob hashes",
            ),
        ] {
            let error = api
                .create_arc_call_txn_env(
                    &block_env,
                    api.evm_cfg.cfg.clone(),
                    request(value),
                    &db,
                    ARC_MAINNET_CHAIN_ID,
                    ArcCallPreparationErrorPolicy::Reth,
                )
                .unwrap_err();
            assert_eq!(error.code(), code);
            assert_eq!(error.message(), message);
            assert!(error.data().is_none());
        }

        let conflicting_input = request(json!({
            "nonce": "0x0",
            "data": "0x01",
            "input": "0x02"
        }));
        let error = api
            .create_arc_call_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                conflicting_input.clone(),
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Reth,
            )
            .unwrap_err();
        assert_eq!(error.code(), jsonrpsee::types::error::INVALID_PARAMS_CODE);
        assert_eq!(
            error.message(),
            "both \"data\" and \"input\" are set and not equal. Please use \"input\" to pass transaction call data"
        );

        let raw = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                conflicting_input,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap();
        assert_eq!(raw.data, Bytes::from_static(&[0x02]));
    }

    #[test]
    fn arc_raw_builder_uses_minimal_blob_type_without_max_fee_blob_false_positive() {
        let api = test_api();
        let block_env = test_block_env();
        let db = InMemoryDB::default();

        let max_fee_blob_only = request(json!({
            "nonce": "0x0",
            "maxFeePerBlobGas": "0x7b"
        }));
        let max_fee_blob_only = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                max_fee_blob_only,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap();
        assert_eq!(max_fee_blob_only.tx_type, TxType::Legacy as u8);
        assert_eq!(max_fee_blob_only.max_fee_per_blob_gas, 0);

        let conflict = request(json!({
            "nonce": "0x0",
            "gasPrice": "0x1",
            "maxFeePerBlobGas": "0x7b"
        }));
        let conflict = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                conflict,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap_err();
        assert_eq!(
            conflict.code(),
            jsonrpsee::types::error::INVALID_PARAMS_CODE
        );
        assert_eq!(conflict.message(), "Invalid fee parameters");

        let hash = format!("0x01{}", "00".repeat(31));
        let hash_only = request(json!({
            "nonce": "0x0",
            "blobVersionedHashes": [hash]
        }));
        let hash_only = api
            .create_arc_raw_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                hash_only,
                &db,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap();
        assert_eq!(hash_only.tx_type, TxType::Eip4844 as u8);
        assert_eq!(hash_only.blob_hashes.len(), 1);
    }

    #[test]
    fn arc_nonce_database_errors_keep_endpoint_specific_contracts() {
        let api = test_api();
        let block_env = test_block_env();
        let request = request(json!({
            "from": "0x0000000000000000000000000000000000000001"
        }));

        let reth = api
            .create_arc_call_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                request.clone(),
                FailingDatabase,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Reth,
            )
            .unwrap_err();
        assert_eq!(reth.code(), -32603);
        assert_eq!(reth.message(), "injected Arc call database failure");
        assert!(reth.data().is_none());

        let debank = api
            .create_arc_call_txn_env(
                &block_env,
                api.evm_cfg.cfg.clone(),
                request,
                FailingDatabase,
                ARC_MAINNET_CHAIN_ID,
                ArcCallPreparationErrorPolicy::Debank,
            )
            .unwrap_err();
        assert_eq!(debank.code(), -32603);
        assert_eq!(debank.message(), "get nonce failed");
        assert!(debank.data().is_none());
    }

    #[test]
    fn arc_call_revert_reason_matches_reth_contract_and_raw_string_rules() {
        let abi_output = Bytes::from(
            Revert {
                reason: "revert: foo".to_string(),
            }
            .abi_encode(),
        );
        let raw_output = Bytes::from_static(b"revert: foo");

        for (output, expected_message) in [
            (abi_output, "execution reverted: foo"),
            (raw_output, "execution reverted: revert: foo"),
        ] {
            let expected_data =
                format!("\"0x{}\"", leafage_evm_types::hex::encode(output.as_ref()));
            let error = arc_call_result(ExecutionResult::Revert {
                gas: revm::context::result::ResultGas::new(30_000, 21_000, 0, 0, 21_000),
                logs: Vec::new(),
                output,
            })
            .unwrap_err();
            assert_eq!(error.code(), RETH_EXECUTION_ERROR_CODE);
            assert_eq!(error.message(), expected_message);
            assert_eq!(
                error.data().map(|data| data.get()),
                Some(expected_data.as_str())
            );
        }
    }
}
