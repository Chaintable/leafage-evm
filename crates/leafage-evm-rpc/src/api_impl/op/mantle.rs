//! Mantle (chain 5000) calls on post-Bedrock blocks, under the fork rules of
//! `leafage_evm_chains::op::mantle`.
//!
//! Before Arsia a call runs on the Ethereum EVM with the fork's instruction
//! table and precompiles, and its gas is in token-ratio units: the request's gas
//! limit is converted to EVM gas for the run and the result back to chain gas.
//! Fees are still charged on the EVM gas of the run, so a priced request leaves
//! the sender's balance higher than the chain would.
//!
//! From Arsia on a call runs the OP EVM at Karst (Jovian on the Osaka EVM) with
//! Ethereum's Osaka precompiles.
use crate::api_impl::mainnet::evm::create_main_evm_from_state_with_precompiles;
use crate::api_impl::op::legacy::op_error;
use leafage_evm_chains::op::mantle::{
    mantle_fork_at, token_ratio, MantleFork, MantleGasError, MantleRun, MANTLE_CHAIN_ID,
};
use leafage_evm_types::{CfgEnv, MainnetSpecId, OpSpecId};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, ExecutionResult, InvalidTransaction};
use revm::context::{BlockEnv, TxEnv};
use revm::context_interface::Transaction;
use revm::handler::EthPrecompiles;
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm, Inspector};

type MantleResult<E> = Result<ExecutionResult<OpHaltReason>, EVMError<E, OpTransactionError>>;

/// The fork of the block, when this node serves Mantle and the block is
/// post-Bedrock.
pub(super) fn fork_at(cfg: &CfgEnv<OpSpecId>, block_env: &BlockEnv) -> Option<MantleFork> {
    if cfg.chain_id != MANTLE_CHAIN_ID {
        return None;
    }
    mantle_fork_at(
        block_env.number.saturating_to(),
        block_env.timestamp.saturating_to(),
    )
}

/// The fork of a Mantle block whose gas is in token-ratio units (before Arsia).
pub(super) fn scaled_fork_at(cfg: &CfgEnv<OpSpecId>, block_env: &BlockEnv) -> Option<MantleFork> {
    fork_at(cfg, block_env).filter(|fork| fork.scales_gas())
}

/// The configured cfg at Karst, the OP fork of an Arsia block.
pub(super) fn arsia_cfg(cfg: &CfgEnv<OpSpecId>) -> CfgEnv<OpSpecId> {
    let mut cfg = cfg.clone();
    cfg.set_spec_and_mainnet_gas_params(OpSpecId::KARST);
    cfg
}

/// The precompiles of an Arsia block: Ethereum's Osaka set.
pub(super) fn arsia_precompiles() -> EthPrecompiles {
    EthPrecompiles::new(SpecId::OSAKA)
}

/// The RPC gas cap in the gas unit of the block. Before Arsia a gas limit is in
/// token-ratio units, so the configured cap (EVM gas) is multiplied by the ratio.
pub(super) fn rpc_gas_cap<DB: DatabaseRef>(
    cfg: &CfgEnv<OpSpecId>,
    block_env: &BlockEnv,
    db: &DB,
    cap: Option<u64>,
) -> Option<u64> {
    if scaled_fork_at(cfg, block_env).is_none() {
        return cap;
    }
    // A failed read surfaces when the call itself runs.
    let ratio = token_ratio(db).unwrap_or(1);
    cap.map(|cap| cap.saturating_mul(ratio))
}

/// Whether a pre-Arsia run takes the L1 data fee out of its gas limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum L1Fee {
    /// `eth_call`: op-geth computes no L1 fee for it.
    Skip,
    /// A simulated transaction: the fee at the request's gas price, nothing
    /// without one.
    Charge,
    /// A gas estimate: as `Charge`, at the block base fee when the request names
    /// no price (op-geth falls back to its suggested price).
    Estimate,
}

fn l1_gas_price(l1: L1Fee, tx: &TxEnv, block_env: &BlockEnv) -> u128 {
    let base_fee = block_env.basefee as u128;
    let price = tx.effective_gas_price(base_fee);
    match l1 {
        L1Fee::Skip => 0,
        L1Fee::Charge => price,
        L1Fee::Estimate if price == 0 => base_fee,
        L1Fee::Estimate => price,
    }
}

fn gas_error<E>(gas_limit: u64, error: MantleGasError) -> EVMError<E, OpTransactionError> {
    let invalid = match error {
        MantleGasError::Intrinsic { required } | MantleGasError::L1Fee { required } => {
            InvalidTransaction::CallGasCostMoreThanGasLimit {
                gas_limit,
                initial_gas: required,
            }
        }
        MantleGasError::Floor { required } => InvalidTransaction::GasFloorMoreThanGasLimit {
            gas_floor: required,
            gas_limit,
        },
    };
    EVMError::Transaction(OpTransactionError::Base(invalid))
}

/// The Ethereum cfg of a pre-Arsia run. The EIP-7623 floor is left to
/// [`MantleRun::settle`], which applies it in chain gas.
fn scaled_cfg(cfg: &CfgEnv<OpSpecId>, fork: MantleFork) -> CfgEnv<MainnetSpecId> {
    let mut cfg = cfg
        .clone()
        .with_spec_and_gas_params(fork.eth_spec(), fork.gas_params());
    cfg.disable_eip7623 = true;
    cfg
}

fn fork_precompiles(fork: MantleFork) -> EthPrecompiles {
    EthPrecompiles {
        precompiles: fork.precompiles(),
        spec: fork.eth_spec(),
    }
}

/// The request as the transaction of the EVM run: its gas limit converted from
/// chain gas to EVM gas.
fn scaled_tx<StateDB: DatabaseRef>(
    cfg: &CfgEnv<OpSpecId>,
    fork: MantleFork,
    block_env: &BlockEnv,
    state: &StateDB,
    tx: OpTransaction<TxEnv>,
    l1: L1Fee,
) -> Result<(TxEnv, MantleRun), EVMError<StateDB::Error, OpTransactionError>> {
    let mut tx = tx.base;
    let l1_gas_price = l1_gas_price(l1, &tx, block_env);
    let run = MantleRun::for_tx(fork, state, &tx, cfg.chain_id, l1_gas_price)
        .map_err(EVMError::<StateDB::Error, OpTransactionError>::Database)?
        .map_err(|error| gas_error::<StateDB::Error>(tx.gas_limit, error))?;
    tx.gas_limit = run.evm_gas_limit;
    Ok((tx, run))
}

/// The result of the EVM run with its gas in chain gas.
fn settled(
    result: ExecutionResult<OpHaltReason>,
    run: &MantleRun,
) -> ExecutionResult<OpHaltReason> {
    match result {
        ExecutionResult::Success {
            reason,
            gas,
            logs,
            output,
        } => ExecutionResult::Success {
            reason,
            gas: run.settle(gas),
            logs,
            output,
        },
        ExecutionResult::Revert { gas, logs, output } => ExecutionResult::Revert {
            gas: run.settle(gas),
            logs,
            output,
        },
        ExecutionResult::Halt { reason, gas, logs } => ExecutionResult::Halt {
            reason,
            gas: run.settle(gas),
            logs,
        },
    }
}

/// Runs a call on a pre-Arsia block (`fork` from [`scaled_fork_at`]).
pub(super) fn transact<StateDB: DatabaseRef>(
    cfg: &CfgEnv<OpSpecId>,
    fork: MantleFork,
    block_env: &BlockEnv,
    state: StateDB,
    tx: OpTransaction<TxEnv>,
    l1: L1Fee,
) -> MantleResult<StateDB::Error> {
    let (tx, run) = scaled_tx(cfg, fork, block_env, &state, tx, l1)?;
    let mut evm = create_main_evm_from_state_with_precompiles(
        block_env.clone(),
        scaled_cfg(cfg, fork),
        state,
        NoOpInspector {},
        fork_precompiles(fork),
    );
    evm.instruction = fork.instructions();
    evm.transact(tx)
        .map(|res| settled(res.result.map_haltreason(OpHaltReason::Base), &run))
        .map_err(op_error)
}

/// Runs and commits a simulated transaction on a pre-Arsia block (`fork` from
/// [`scaled_fork_at`]).
pub(super) fn inspect_tx_commit<StateDB, INSP>(
    cfg: &CfgEnv<OpSpecId>,
    fork: MantleFork,
    block_env: &BlockEnv,
    state: StateDB,
    inspector: INSP,
    tx: OpTransaction<TxEnv>,
) -> MantleResult<StateDB::Error>
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
    let (tx, run) = scaled_tx(cfg, fork, block_env, &state, tx, L1Fee::Charge)?;
    let mut evm = create_main_evm_from_state_with_precompiles(
        block_env.clone(),
        scaled_cfg(cfg, fork),
        state,
        inspector,
        fork_precompiles(fork),
    );
    evm.instruction = fork.instructions();
    evm.inspect_tx_commit(tx)
        .map(|res| settled(res.map_haltreason(OpHaltReason::Base), &run))
        .map_err(op_error)
}
