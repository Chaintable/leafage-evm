use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::consensus::TxType;
use alloy::eips::eip2935::HISTORY_STORAGE_ADDRESS;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::Either;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, MainnetSpecId, H256, U256};
use revm::context::{Evm, TxEnv};
use revm::context_interface::Block;
use revm::database::{DatabaseRef, WrapDatabaseRef};
use revm::handler::{instructions::EthInstructions, EthPrecompiles};
use revm::inspector::NoOpInspector;
use revm::interpreter::interpreter::EthInterpreter;
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::{Context, DatabaseCommit, MainBuilder, MainContext, SystemCallEvm};
use std::fmt::Debug;

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

pub(crate) fn create_mainnet_txn_env<ODB: DatabaseRef, SpecId>(
    block_env: &BlockEnv,
    cfg_env: CfgEnv<SpecId>,
    request: CallRequest,
    db: ODB,
    origin_chain_id: u64,
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

    let TransactionRequest {
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
        mut chain_id,
        access_list,
        blob_versioned_hashes,
        authorization_list,
        ..
    } = request.inner;

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

    let max_gas_limit = cfg_env
        .tx_gas_limit_cap
        .filter(|&cap| cap != 0)
        .unwrap_or(block_env.gas_limit);

    let gas_limit = gas.map_or(max_gas_limit, |g| g.min(max_gas_limit));

    let caller = from.unwrap_or_default();

    if chain_id.is_none() {
        chain_id = Some(origin_chain_id);
    }

    // Use request nonce if provided; otherwise fetch from DB.
    // Critical for Tempo 0x76 AA: nonce_key>0 bypasses protocol nonce check,
    // sender may set nonce=0 explicitly. TIP-1000 gas depends on nonce==0.
    let nonce = nonce.unwrap_or_else(|| {
        db.basic_ref(caller)
            .map(|acc| acc.map(|a| a.nonce).unwrap_or_default())
            .unwrap_or_default()
    });

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

pub(crate) fn create_main_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<MainnetSpecId>,
    state: StateDB,
    inspector: INSP,
) -> Evm<
    Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, WrapDatabaseRef<StateDB>>,
    INSP,
    EthInstructions<
        EthInterpreter,
        Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, WrapDatabaseRef<StateDB>>,
    >,
    EthPrecompiles,
    revm::handler::EthFrame,
>
where
    StateDB: DatabaseRef,
{
    let spec: revm::primitives::hardfork::SpecId = cfg.spec.into();
    Context::mainnet()
        .with_block(block_env)
        .with_cfg(cfg)
        .with_ref_db(state)
        .build_mainnet_with_inspector(inspector)
        .with_precompiles(EthPrecompiles::new(spec))
}

/// Apply the EIP-2935 (Prague) blockhashes system-contract call as part of
/// pre-execution state setup. Extracted as a free function so the mainnet and
/// arbitrum executors share one implementation. No-op before Prague or at the
/// genesis block.
pub(crate) fn apply_blockhashes_contract_call<StateDB>(
    cfg: &CfgEnv<MainnetSpecId>,
    parent_block_hash: H256,
    block_env: &BlockEnv,
    state: &mut StateDB,
) -> RpcResult<()>
where
    StateDB: DatabaseCommit + DatabaseRef + Debug,
    StateDB::Error: Sync + Send + 'static,
{
    if !cfg.spec.is_enabled_in(SpecId::PRAGUE) {
        return Ok(());
    }

    if block_env.number.is_zero() {
        return Ok(());
    }

    let result = {
        let mut evm =
            create_main_evm_from_state(block_env.clone(), cfg.clone(), &*state, NoOpInspector {});

        evm.system_call(HISTORY_STORAGE_ADDRESS, parent_block_hash.0.into())
            .map_err(|e| {
                internal_rpc_err(format!("EIP-2935 blockhashes contract call failed: {e}"))
            })?
    };

    state.commit(result.state);
    Ok(())
}
