//! OP Mainnet pre-Bedrock calls: run on the Ethereum EVM with the l2geth (OVM 2.0)
//! rules of `leafage_evm_chains::op::legacy` instead of an OP fork.
use crate::api_impl::mainnet::evm::create_main_evm_from_state;
use leafage_evm_chains::op::legacy::{
    deployer_allowed_ref, not_whitelisted_output, op_legacy_instructions, op_legacy_spec_at,
    OVM_FEE_WALLET,
};
use leafage_evm_chains::op::mainnet::{OP_MAINNET_BEDROCK_BLOCK, OP_MAINNET_CHAIN_ID};
use leafage_evm_types::{CfgEnv, MainnetSpecId, OpSpecId, U256};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, ExecutionResult, InvalidTransaction, ResultGas};
use revm::context::{BlockEnv, TxEnv};
use revm::context_interface::cfg::gas::calculate_initial_tx_gas_for_tx;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm, Inspector};

type LegacyResult<E> = Result<ExecutionResult<OpHaltReason>, EVMError<E, OpTransactionError>>;

/// Whether this call runs a pre-Bedrock OP Mainnet block.
pub(super) fn is_legacy(cfg: &CfgEnv<OpSpecId>, block_env: &BlockEnv) -> bool {
    cfg.chain_id == OP_MAINNET_CHAIN_ID && block_env.number < U256::from(OP_MAINNET_BEDROCK_BLOCK)
}

/// The l2geth view of the block: Istanbul / Berlin rules, COINBASE is the OVM fee
/// wallet and DIFFICULTY is zero. Everything else (fee and limit switches) is the
/// configured cfg.
fn legacy_env(cfg: &CfgEnv<OpSpecId>, block_env: &BlockEnv) -> (CfgEnv<MainnetSpecId>, BlockEnv) {
    let spec = op_legacy_spec_at(block_env.number.saturating_to());
    let cfg = cfg.clone().with_spec_and_mainnet_gas_params(spec);
    let mut block = block_env.clone();
    block.beneficiary = OVM_FEE_WALLET;
    block.difficulty = U256::ZERO;
    (cfg, block)
}

/// l2geth 1.9 calls carry no typed-transaction fields: run every request as a
/// legacy transaction at its gas price.
fn legacy_tx(tx: OpTransaction<TxEnv>) -> TxEnv {
    let mut tx = tx.base;
    tx.tx_type = 0;
    tx.gas_priority_fee = None;
    tx.access_list = Default::default();
    tx.blob_hashes.clear();
    tx.max_fee_per_blob_gas = 0;
    tx.authorization_list.clear();
    tx
}

/// A top-level deployment from a deployer the whitelist rejects: l2geth reverts
/// inside `evm.create` with all gas but the intrinsic gas returned.
fn rejected_deployment<DB: DatabaseRef>(
    spec: MainnetSpecId,
    state: &DB,
    tx: &TxEnv,
) -> Result<Option<ExecutionResult<OpHaltReason>>, DB::Error> {
    if spec.is_enabled_in(MainnetSpecId::BERLIN) || !tx.kind.is_create() {
        return Ok(None);
    }
    if deployer_allowed_ref(state, tx.caller)? {
        return Ok(None);
    }
    let intrinsic = calculate_initial_tx_gas_for_tx(tx, spec, None);
    if tx.gas_limit < intrinsic.initial_regular_gas {
        // Let the EVM report the intrinsic gas failure.
        return Ok(None);
    }
    Ok(Some(ExecutionResult::Revert {
        gas: ResultGas::default().with_total_gas_spent(intrinsic.initial_regular_gas),
        logs: Vec::new(),
        output: not_whitelisted_output(tx.caller),
    }))
}

fn op_error<E>(error: EVMError<E, InvalidTransaction>) -> EVMError<E, OpTransactionError> {
    match error {
        EVMError::Transaction(e) => EVMError::Transaction(OpTransactionError::Base(e)),
        EVMError::Header(e) => EVMError::Header(e),
        EVMError::Database(e) => EVMError::Database(e),
        EVMError::Custom(e) => EVMError::Custom(e),
        EVMError::CustomAny(e) => EVMError::CustomAny(e),
    }
}

pub(super) fn transact<StateDB: DatabaseRef>(
    cfg: &CfgEnv<OpSpecId>,
    block_env: &BlockEnv,
    state: StateDB,
    tx: OpTransaction<TxEnv>,
) -> LegacyResult<StateDB::Error> {
    let (cfg, block) = legacy_env(cfg, block_env);
    let spec = cfg.spec;
    let tx = legacy_tx(tx);
    if let Some(result) = rejected_deployment(spec, &state, &tx).map_err(EVMError::Database)? {
        return Ok(result);
    }
    let mut evm = create_main_evm_from_state(block, cfg, state, NoOpInspector {});
    evm.instruction = op_legacy_instructions(spec);
    evm.transact(tx)
        .map(|res| res.result.map_haltreason(OpHaltReason::Base))
        .map_err(op_error)
}

pub(super) fn inspect_tx_commit<StateDB, INSP>(
    cfg: &CfgEnv<OpSpecId>,
    block_env: &BlockEnv,
    state: StateDB,
    inspector: INSP,
    tx: OpTransaction<TxEnv>,
) -> LegacyResult<StateDB::Error>
where
    StateDB: DatabaseRef + DatabaseCommit,
    INSP: Inspector<
        revm::Context<
            BlockEnv,
            TxEnv,
            CfgEnv<MainnetSpecId>,
            revm::database::WrapDatabaseRef<StateDB>,
        >,
    >,
{
    let (cfg, block) = legacy_env(cfg, block_env);
    let spec = cfg.spec;
    let tx = legacy_tx(tx);
    if let Some(result) = rejected_deployment(spec, &state, &tx).map_err(EVMError::Database)? {
        return Ok(result);
    }
    let mut evm = create_main_evm_from_state(block, cfg, state, inspector);
    evm.instruction = op_legacy_instructions(spec);
    evm.inspect_tx_commit(tx)
        .map(|res| res.map_haltreason(OpHaltReason::Base))
        .map_err(op_error)
}
