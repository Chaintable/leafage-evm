use crate::error::invalid_params_rpc_err;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{
    BlockOverrides, CallRequest, DebankEvent, DebankID, DebankTrace, Transaction, H256, U256,
};
use revm::db::CacheDB;
use revm::primitives::{
    env::{CfgEnv, CfgEnvWithHandlerCfg},
    AccessListItem, BlockEnv, SpecId, TxEnv, TxKind,
};
use revm_inspectors::tracing::{
    types::{CallTraceNode, TraceMemberOrder},
    CallTraceArena,
};

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

pub(crate) fn create_txn_env(block_env: &BlockEnv, request: CallRequest) -> RpcResult<TxEnv> {
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
        nonce,
        chain_id,
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
        block_env.basefee.into(),
        blob_versioned_hashes.as_deref(),
        max_fee_per_blob_gas.map(U256::from),
        block_env.get_blob_gasprice().map(U256::from),
    )
    .ok_or_else(|| invalid_params_rpc_err("Invalid fee parameters"))?;

    let gas_limit = gas.unwrap_or_else(|| block_env.gas_limit.min(U256::from(u64::MAX)).to());

    let env = TxEnv {
        caller: from.unwrap_or_default(),
        gas_limit: gas_limit
            .try_into()
            .map_err(|_| invalid_params_rpc_err("Invalid gas parameters"))?,
        gas_price,
        gas_priority_fee: max_priority_fee_per_gas,
        transact_to: to.unwrap_or(TxKind::Create),
        value: value.unwrap_or_default(),
        data: input.into_input().unwrap_or_default(),
        chain_id,
        nonce,
        access_list: access_list
            .unwrap_or_default()
            .iter()
            .map(|a| AccessListItem {
                address: a.address,
                storage_keys: a.storage_keys.clone(),
            })
            .collect::<Vec<_>>()
            .into(),
        // EIP-4844 fields
        blob_hashes: blob_versioned_hashes.unwrap_or_default(),
        max_fee_per_blob_gas,
        // EIP-7702 fields
        authorization_list: authorization_list.map(Into::into),
        #[cfg(feature = "optimism")]
        optimism: revm::primitives::OptimismFields {
            enveloped_tx: Some(Default::default()),
            ..Default::default()
        },
        ..Default::default()
    };

    Ok(env)
}

pub(crate) fn rebuild_txn_env(block_env: &BlockEnv, tx: &Transaction) -> RpcResult<TxEnv> {
    #[cfg(not(feature = "optimism"))]
    let request = tx.clone().into_request();
    #[cfg(feature = "optimism")]
    let request = tx.inner.clone().into_request();

    let env = create_txn_env(block_env, request)?;

    #[cfg(feature = "optimism")]
    return Ok(set_op_txn_env(env, tx));

    #[cfg(not(feature = "optimism"))]
    Ok(env)
}

#[cfg(feature = "optimism")]
fn set_op_txn_env(mut tx_env: TxEnv, tx: &Transaction) -> TxEnv {
    use alloy::eips::eip2718::Encodable2718;
    use leafage_evm_types::{OpTxEnvelope, OpTxType, TxDeposit, TxEnvelope};
    if tx.inner.transaction_type.unwrap_or_default() == OpTxType::Deposit as u8 {
        let deposit_tx = TxDeposit {
            source_hash: tx.source_hash.unwrap_or_default(),
            from: tx.inner.from,
            to: tx.inner.to.into(),
            mint: tx.mint,
            value: tx.inner.value,
            gas_limit: tx.inner.gas,
            is_system_transaction: tx.is_system_tx.unwrap_or_default(),
            input: tx.inner.input.clone(),
        };
        let op_tx_env: OpTxEnvelope = deposit_tx.into();
        tx_env.optimism.enveloped_tx = Some(op_tx_env.encoded_2718().into());
    } else {
        let op_tx_env: TxEnvelope = tx.inner.clone().try_into().expect("Invalid transaction");
        tx_env.optimism.enveloped_tx = Some(op_tx_env.encoded_2718().into());
    }
    tx_env.optimism.is_system_transaction = tx.is_system_tx;
    tx_env.optimism.mint = tx.mint;
    tx_env.optimism.source_hash = tx.source_hash;
    tx_env
}

pub(crate) fn get_handler_cfg(cfg_env: CfgEnv, spec_id: SpecId) -> CfgEnvWithHandlerCfg {
    #[allow(unused_mut)]
    let mut cfg = CfgEnvWithHandlerCfg::new_with_spec_id(cfg_env, spec_id);
    #[cfg(feature = "optimism")]
    {
        cfg.disable_base_fee = true;
        cfg.enable_optimism();
    }
    cfg
}

pub(crate) fn apply_block_overrides<DB>(
    overrides: BlockOverrides,
    db: &mut CacheDB<DB>,
    block_env: &mut BlockEnv,
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
        db.block_hashes.extend(
            block_hashes
                .into_iter()
                .map(|(num, hash)| (U256::from(num), hash)),
        )
    }

    if let Some(number) = number {
        block_env.number = number;
    }
    if let Some(difficulty) = difficulty {
        block_env.difficulty = difficulty;
    }
    if let Some(time) = time {
        block_env.timestamp = U256::from(time);
    }
    if let Some(gas_limit) = gas_limit {
        block_env.gas_limit = U256::from(gas_limit);
    }
    if let Some(coinbase) = coinbase {
        block_env.coinbase = coinbase;
    }
    if let Some(random) = random {
        block_env.prevrandao = Some(random);
    }
    if let Some(base_fee) = base_fee {
        block_env.basefee = base_fee;
    }
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
    return (traces, events);
}
