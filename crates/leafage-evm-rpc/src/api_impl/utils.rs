use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::consensus::TxType;
use alloy::signers::Either;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{
    AccountOverride, BlockOverrides, Bytecode, CallRequest, CfgEnv, DebankEvent, DebankID,
    DebankTrace, SpecId, StateOverride, H256, U256,
};
#[cfg(feature = "optimism")]
use op_revm::{
    precompiles::OpPrecompiles, DefaultOp, L1BlockInfo, OpBuilder, OpEvm, OpTransaction,
};
use revm::context::{BlockEnv, TxEnv};
use revm::context_interface::Block;
use revm::database::{CacheDB, DatabaseRef, WrapDatabaseRef};
use revm::handler::instructions::EthInstructions;
use revm::interpreter::interpreter::EthInterpreter;
use revm::primitives::{Address, TxKind};
use revm::state::{Account, AccountStatus, EvmStorageSlot};
#[cfg(not(feature = "optimism"))]
use revm::{context::Evm, handler::EthPrecompiles, MainBuilder, MainContext};
use revm::{Context, Database, DatabaseCommit};
use revm_inspectors::tracing::types::{CallTraceNode, TraceMemberOrder};
use revm_inspectors::tracing::CallTraceArena;
use std::collections::HashMap;

/// Helper type for representing the fees of a [CallRequest]
pub(crate) struct CallFees {
    /// EIP-1559 priority fee
    max_priority_fee_per_gas: Option<U256>,
    /// Unified gas price setting
    ///
    /// Will be the configured `basefee` if unset in the request
    ///
    /// `gasPrice` for legacy,
    /// `maxFeePerGas` for EIP-1559
    gas_price: U256,
    /// Max Fee per Blob gas for EIP-4844 transactions
    max_fee_per_blob_gas: Option<U256>,
}

pub(crate) fn ensure_fees(
    call_gas_price: Option<U256>,
    call_max_fee: Option<U256>,
    call_priority_fee: Option<U256>,
    block_base_fee: U256,
    blob_versioned_hashes: Option<&[H256]>,
    max_fee_per_blob_gas: Option<U256>,
    block_blob_fee: Option<U256>,
) -> Option<CallFees> {
    let has_blob_hashes = blob_versioned_hashes
        .as_ref()
        .map(|blobs| !blobs.is_empty())
        .unwrap_or(false);

    match (
        call_gas_price,
        call_max_fee,
        call_priority_fee,
        max_fee_per_blob_gas,
    ) {
        (gas_price, None, None, None) => {
            // either legacy transaction or no fee fields are specified
            // when no fields are specified, set gas price to zero
            let gas_price = gas_price.unwrap_or(U256::ZERO);
            Some(CallFees {
                gas_price,
                max_priority_fee_per_gas: None,
                max_fee_per_blob_gas: has_blob_hashes.then_some(block_blob_fee).flatten(),
            })
        }
        (None, max_fee_per_gas, max_priority_fee_per_gas, None) => {
            // request for eip-1559 transaction
            let max_fee = max_fee_per_gas.unwrap_or(block_base_fee);

            let max_fee_per_blob_gas = has_blob_hashes.then_some(block_blob_fee).flatten();

            Some(CallFees {
                gas_price: max_fee,
                max_priority_fee_per_gas,
                max_fee_per_blob_gas,
            })
        }
        (None, max_fee_per_gas, max_priority_fee_per_gas, Some(max_fee_per_blob_gas)) => {
            // request for eip-4844 transaction
            let max_fee = max_fee_per_gas.unwrap_or(block_base_fee);

            Some(CallFees {
                gas_price: max_fee,
                max_priority_fee_per_gas,
                max_fee_per_blob_gas: Some(max_fee_per_blob_gas),
            })
        }
        _ => None,
    }
}

#[cfg(feature = "optimism")]
pub(crate) fn create_txn_env<ODB: DatabaseRef>(
    block_env: &BlockEnv,
    request: CallRequest,
    db: ODB,
    cfg: &CfgEnv<SpecId>,
) -> RpcResult<OpTransaction<TxEnv>> {
    let tx_type = if request.authorization_list.is_some() {
        TxType::Eip7702
    } else if request.sidecar.is_some() || request.max_fee_per_blob_gas.is_some() {
        TxType::Eip4844
    } else if request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some() {
        TxType::Eip1559
    } else if request.access_list.is_some() {
        TxType::Eip2930
    } else {
        TxType::Legacy
    } as u8;

    let CallRequest {
        from,
        to,
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        max_fee_per_blob_gas,
        gas,
        value,
        input,
        mut chain_id,
        access_list,
        blob_versioned_hashes,
        authorization_list,
        ..
    } = request;

    let CallFees {
        max_priority_fee_per_gas,
        gas_price,
        max_fee_per_blob_gas,
    } = ensure_fees(
        gas_price.map(U256::from),
        max_fee_per_gas.map(U256::from),
        max_priority_fee_per_gas.map(U256::from),
        U256::from(block_env.basefee),
        blob_versioned_hashes.as_deref(),
        max_fee_per_blob_gas.map(U256::from),
        block_env.blob_gasprice().map(U256::from),
    )
    .ok_or_else(|| invalid_params_rpc_err("Invalid fee parameters"))?;

    let gas_limit = gas.unwrap_or_else(|| block_env.gas_limit.min(u64::MAX));

    let caller = from.unwrap_or_default();

    if chain_id.is_none() {
        chain_id = Some(cfg.chain_id);
    }

    let nonce = db
        .basic_ref(caller)
        .map_err(|_| internal_rpc_err("get nonce failed"))?
        .map(|acc| acc.nonce)
        .unwrap_or_default();

    let base = TxEnv {
        tx_type,
        gas_limit: gas_limit
            .try_into()
            .map_err(|_| invalid_params_rpc_err("Invalid gas parameters"))?,
        nonce,
        caller,
        gas_price: gas_price.saturating_to(),
        gas_priority_fee: max_priority_fee_per_gas.map(|v| v.saturating_to()),
        kind: to.unwrap_or(TxKind::Create),
        value: value.unwrap_or_default(),
        data: input.into_input().unwrap_or_default(),
        chain_id,
        access_list: access_list.unwrap_or_default(),
        // EIP-4844 fields
        blob_hashes: blob_versioned_hashes.unwrap_or_default(),
        max_fee_per_blob_gas: max_fee_per_blob_gas
            .map(|v| v.saturating_to())
            .unwrap_or_default(),
        // EIP-7702 fields
        authorization_list: authorization_list
            .unwrap_or_default()
            .into_iter()
            .map(Either::Left)
            .collect(),
        ..Default::default()
    };

    Ok(OpTransaction {
        base,
        enveloped_tx: Some(leafage_evm_types::Bytes::new()),
        deposit: Default::default(),
    })
}

#[cfg(not(feature = "optimism"))]
pub(crate) fn create_txn_env<ODB: DatabaseRef>(
    block_env: &BlockEnv,
    request: CallRequest,
    db: ODB,
    cfg: &CfgEnv<SpecId>,
) -> RpcResult<TxEnv> {
    let tx_type = if request.authorization_list.is_some() {
        TxType::Eip7702
    } else if request.sidecar.is_some() || request.max_fee_per_blob_gas.is_some() {
        TxType::Eip4844
    } else if request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some() {
        TxType::Eip1559
    } else if request.access_list.is_some() {
        TxType::Eip2930
    } else {
        TxType::Legacy
    } as u8;

    let CallRequest {
        from,
        to,
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        max_fee_per_blob_gas,
        gas,
        value,
        input,
        mut chain_id,
        access_list,
        blob_versioned_hashes,
        authorization_list,
        ..
    } = request;

    let CallFees {
        max_priority_fee_per_gas,
        gas_price,
        max_fee_per_blob_gas,
    } = ensure_fees(
        gas_price.map(U256::from),
        max_fee_per_gas.map(U256::from),
        max_priority_fee_per_gas.map(U256::from),
        U256::from(block_env.basefee),
        blob_versioned_hashes.as_deref(),
        max_fee_per_blob_gas.map(U256::from),
        block_env.blob_gasprice().map(U256::from),
    )
    .ok_or_else(|| invalid_params_rpc_err("Invalid fee parameters"))?;

    let gas_limit = gas.unwrap_or_else(|| block_env.gas_limit.min(u64::MAX));

    let caller = from.unwrap_or_default();

    if chain_id.is_none() {
        chain_id = Some(cfg.chain_id);
    }

    let nonce = db
        .basic_ref(caller)
        .map_err(|_| internal_rpc_err("get nonce failed"))?
        .map(|acc| acc.nonce)
        .unwrap_or_default();

    let env = TxEnv {
        tx_type,
        gas_limit: gas_limit
            .try_into()
            .map_err(|_| invalid_params_rpc_err("Invalid gas parameters"))?,
        nonce,
        caller,
        gas_price: gas_price.saturating_to(),
        gas_priority_fee: max_priority_fee_per_gas.map(|v| v.saturating_to()),
        kind: to.unwrap_or(TxKind::Create),
        value: value.unwrap_or_default(),
        data: input.into_input().unwrap_or_default(),
        chain_id,
        access_list: access_list.unwrap_or_default(),
        // EIP-4844 fields
        blob_hashes: blob_versioned_hashes.unwrap_or_default(),
        max_fee_per_blob_gas: max_fee_per_blob_gas
            .map(|v| v.saturating_to())
            .unwrap_or_default(),
        // EIP-7702 fields
        authorization_list: authorization_list
            .unwrap_or_default()
            .into_iter()
            .map(Either::Left)
            .collect(),
        ..Default::default()
    };

    Ok(env)
}

pub fn apply_block_overrides<DB>(
    overrides: BlockOverrides,
    db: &mut CacheDB<DB>,
    env: &mut BlockEnv,
) {
    let BlockOverrides {
        number,
        difficulty,
        time,
        gas_limit,
        coinbase,
        random,
        base_fee,
        block_hash,
    } = overrides;

    if let Some(block_hashes) = block_hash {
        // override block hashes
        db.cache.block_hashes.extend(
            block_hashes
                .into_iter()
                .map(|(num, hash)| (U256::from(num), hash)),
        )
    }

    if let Some(number) = number {
        env.number = number.saturating_to();
    }
    if let Some(difficulty) = difficulty {
        env.difficulty = difficulty;
    }
    if let Some(time) = time {
        env.timestamp = U256::from(time);
    }
    if let Some(gas_limit) = gas_limit {
        env.gas_limit = gas_limit;
    }
    if let Some(coinbase) = coinbase {
        env.beneficiary = coinbase;
    }
    if let Some(random) = random {
        env.prevrandao = Some(random);
    }
    if let Some(base_fee) = base_fee {
        env.basefee = base_fee.saturating_to();
    }
}

/// Applies the given state overrides (a set of [`AccountOverride`]) to the [`CacheDB`].
pub fn apply_state_overrides<DB>(overrides: StateOverride, db: &mut CacheDB<DB>) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    for (account, account_overrides) in overrides {
        apply_account_override(account, account_overrides, db)?;
    }
    Ok(())
}

/// Applies a single [`AccountOverride`] to the [`CacheDB`].
fn apply_account_override<DB>(
    account: Address,
    account_override: AccountOverride,
    db: &mut CacheDB<DB>,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    let mut info = db
        .basic(account)
        .map_err(|_| internal_rpc_err("Failed to get basic account info"))?
        .unwrap_or_default();

    if let Some(nonce) = account_override.nonce {
        info.nonce = nonce;
    }
    if let Some(code) = account_override.code {
        info.code = Some(
            Bytecode::new_raw_checked(code)
                .map_err(|err| invalid_params_rpc_err(format!("Invalid bytecode {}", err)))?,
        );
    }
    if let Some(balance) = account_override.balance {
        info.balance = balance;
    }

    // Create a new account marked as touched
    let mut acc = Account {
        info,
        status: AccountStatus::Touched,
        storage: HashMap::default(),
        transaction_id: 0,
    };

    let storage_diff = match (account_override.state, account_override.state_diff) {
        (Some(_), Some(_)) => {
            return Err(invalid_params_rpc_err(format!(
                "account {:?} has both 'state' and 'stateDiff'",
                account
            )))
        }
        (None, None) => None,
        // If we need to override the entire state, we firstly mark account as destroyed to clear
        // its storage, and then we mark it is "NewlyCreated" to make sure that old storage won't be
        // used.
        (Some(state), None) => {
            // Destroy the account to ensure that its storage is cleared
            db.commit(HashMap::from_iter([(
                account,
                Account {
                    status: AccountStatus::SelfDestructed | AccountStatus::Touched,
                    ..Default::default()
                },
            )]));
            // Mark the account as created to ensure that old storage is not read
            acc.mark_created();
            Some(state)
        }
        (None, Some(state)) => Some(state),
    };

    if let Some(state) = storage_diff {
        for (slot, value) in state {
            acc.storage.insert(
                slot.into(),
                EvmStorageSlot {
                    // we use inverted value here to ensure that storage is treated as changed
                    original_value: (!value).into(),
                    present_value: value.into(),
                    transaction_id: 0,
                    is_cold: false,
                },
            );
        }
    }

    db.commit(HashMap::from_iter([(account, acc)]));

    Ok(())
}

enum DebankTraceOrLog {
    Trace(DebankTraceNode),
    Log(DebankEvent),
}

struct DebankTraceNode {
    trace: DebankTrace,
    children: Vec<DebankTraceOrLog>,
}

fn build_trace_node(
    tx_id: H256,
    parent_trace_id: String,
    pos_in_parent_trace: usize,
    node: &CallTraceNode,
    nodes: &Vec<CallTraceNode>,
) -> DebankTraceNode {
    let mut debank_node = DebankTraceNode {
        trace: node.into(),
        children: Vec::new(),
    };

    debank_node.trace.parent_trace_id = parent_trace_id;
    debank_node.trace.pos_in_parent_trace = pos_in_parent_trace;
    debank_node.trace.tx_id = tx_id;
    debank_node.trace.id = debank_node.trace.debank_id();

    let id = debank_node.trace.id.clone();
    let contract_id = node.execution_address();

    for pos in node.ordering.iter() {
        match &pos {
            TraceMemberOrder::Call(i) => {
                let child_node = &nodes[node.children[*i]];
                if !child_node.trace.success {
                    continue;
                }
                let child_trace = build_trace_node(
                    tx_id,
                    id.clone(),
                    debank_node.children.len(),
                    child_node,
                    nodes,
                );
                if child_trace.trace.storage_change {
                    debank_node.trace.storage_change = true;
                }
                debank_node
                    .children
                    .push(DebankTraceOrLog::Trace(child_trace));
            }
            TraceMemberOrder::Log(i) => {
                let mut child_event: DebankEvent = (&node.logs[*i]).into();
                child_event.pos_in_parent_trace = debank_node.children.len();
                child_event.contract_id = contract_id;
                child_event.tx_id = tx_id;
                child_event.parent_trace_id = id.clone();
                child_event.id = child_event.debank_id();
                debank_node
                    .children
                    .push(DebankTraceOrLog::Log(child_event));
            }
            _ => {}
        }
    }
    debank_node
}

fn finish_build_traces(
    node: &mut DebankTraceNode,
    traces: &mut Vec<DebankTrace>,
    events: &mut Vec<DebankEvent>,
) {
    traces.push(node.trace.clone());
    for child in node.children.iter_mut() {
        match child {
            DebankTraceOrLog::Trace(trace) => {
                trace.trace.parent_trace_id = node.trace.id.clone();
                finish_build_traces(trace, traces, events);
            }
            DebankTraceOrLog::Log(log) => {
                events.push(log.clone());
            }
        }
    }
}

pub(crate) fn build_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
) -> (Vec<DebankTrace>, Vec<DebankEvent>) {
    let nodes = traces.into_nodes();
    if nodes.is_empty() {
        return (vec![], vec![]);
    }
    let mut top = build_trace_node(tx_id, "".to_string(), 0, &nodes[0], &nodes);
    let mut traces = vec![];
    let mut events = vec![];
    finish_build_traces(&mut top, &mut traces, &mut events);
    (traces, events)
}

#[cfg(not(feature = "optimism"))]
pub(crate) fn create_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<SpecId>,
    state: StateDB,
    inspector: INSP,
) -> Evm<
    Context<BlockEnv, TxEnv, CfgEnv<SpecId>, WrapDatabaseRef<StateDB>>,
    INSP,
    EthInstructions<
        EthInterpreter,
        Context<BlockEnv, TxEnv, CfgEnv<SpecId>, WrapDatabaseRef<StateDB>>,
    >,
    EthPrecompiles,
    revm::handler::EthFrame,
>
where
    StateDB: DatabaseRef,
{
    Context::mainnet()
        .with_block(block_env)
        .with_cfg(cfg)
        .with_ref_db(state)
        .build_mainnet_with_inspector(inspector)
        .with_precompiles(EthPrecompiles::default())
}

#[cfg(feature = "optimism")]
pub(crate) fn create_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<SpecId>,
    state: StateDB,
    inspector: INSP,
) -> OpEvm<
    Context<
        BlockEnv,
        OpTransaction<TxEnv>,
        CfgEnv<SpecId>,
        WrapDatabaseRef<StateDB>,
        revm::Journal<WrapDatabaseRef<StateDB>>,
        L1BlockInfo,
    >,
    INSP,
    EthInstructions<
        EthInterpreter,
        Context<
            BlockEnv,
            OpTransaction<TxEnv>,
            CfgEnv<SpecId>,
            WrapDatabaseRef<StateDB>,
            revm::Journal<WrapDatabaseRef<StateDB>>,
            L1BlockInfo,
        >,
    >,
>
where
    StateDB: DatabaseRef,
{
    Context::op()
        .with_block(block_env)
        .with_cfg(cfg.clone())
        .with_ref_db(state)
        .build_op_with_inspector(inspector)
        .with_precompiles(OpPrecompiles::default())
}
