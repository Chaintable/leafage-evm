use crate::error::invalid_params_rpc_err;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{access_list_flattened, CallRequest, H256, U256};
use revm::primitives::{BlockEnv, TransactTo, TxEnv};

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
            let gas_price = gas_price.unwrap_or(U256::zero());
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
        gas,
        value,
        data,
        nonce,
        access_list,
        chain_id,
        blob_versioned_hashes,
        max_fee_per_blob_gas,
        ..
    } = request;

    let CallFees {
        max_priority_fee_per_gas,
        gas_price,
        max_fee_per_blob_gas,
    } = ensure_fees(
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        block_env.basefee.into(),
        blob_versioned_hashes.as_deref(),
        max_fee_per_blob_gas,
        block_env.get_blob_gasprice().map(U256::from),
    )
    .ok_or_else(|| invalid_params_rpc_err("Invalid fee parameters"))?;

    let gas_limit = gas.unwrap_or(block_env.gas_limit.into());

    let env = TxEnv {
        gas_limit: gas_limit.as_u64(),
        nonce: nonce.map(|n| n.as_u64()),
        caller: from.unwrap_or_default().0.into(),
        gas_price: gas_price.into(),
        gas_priority_fee: max_priority_fee_per_gas.map(|p| p.into()),
        transact_to: to
            .map(|to| TransactTo::Call(to.0.into()))
            .unwrap_or_else(TransactTo::create),
        value: value.unwrap_or_default().into(),
        data: data.unwrap_or_default().0.into(),
        chain_id: chain_id.map(|c| c.as_u64()),
        max_fee_per_blob_gas: max_fee_per_blob_gas.map(|g| g.into()),
        blob_hashes: blob_versioned_hashes
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.0.into())
            .collect(),
        access_list: access_list.map(access_list_flattened).unwrap_or_default(),
    };

    Ok(env)
}

pub(crate) fn decode_revert_reason(out: impl AsRef<[u8]>) -> Option<String> {
    use ethers_core::abi::AbiDecode;
    let out = out.as_ref();
    if out.len() < 4 {
        return None;
    }
    String::decode(&out[4..]).ok()
}
