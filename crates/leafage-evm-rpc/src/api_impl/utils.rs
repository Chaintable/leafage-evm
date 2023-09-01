use jsonrpsee::core::RpcResult;
use leafage_evm_types::{access_list_flattened, CallRequest, U256};
use revm::primitives::{BlockEnv, TransactTo, TxEnv};

pub(crate) fn ensure_fees(
    call_gas_price: Option<U256>,
    call_max_fee: Option<U256>,
    call_priority_fee: Option<U256>,
    base_fee: U256,
) -> (Option<U256>, Option<U256>) {
    match (call_gas_price, call_max_fee, call_priority_fee) {
        (gas_price, None, None) => {
            // either legacy transaction or no fee fields are specified
            // when no fields are specified, set gas price to zero
            let gas_price = gas_price.unwrap_or(U256::zero());
            (Some(gas_price), None)
        }
        (None, max_fee_per_gas, max_priority_fee_per_gas) => {
            // request for eip-1559 transaction
            let max_fee = max_fee_per_gas.unwrap_or(base_fee);

            if let Some(max_priority) = max_priority_fee_per_gas {
                if max_priority.0 > max_fee.0 {
                    // Fail early
                    return (None, None);
                }
            }
            (Some(max_fee), max_priority_fee_per_gas)
        }
        _ => (None, None),
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
        ..
    } = request;

    let (max_priority_fee_per_gas, gas_price) = ensure_fees(
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        block_env.basefee.into(),
    );

    let gas_limit = gas.unwrap_or(block_env.gas_limit.into());

    let env = TxEnv {
        gas_limit: gas_limit.as_u64(),
        nonce: nonce.map(|n| n.as_u64()),
        caller: from.unwrap_or_default().into(),
        gas_price: gas_price.unwrap_or_default().into(),
        gas_priority_fee: max_priority_fee_per_gas.map(|p| p.into()),
        transact_to: to
            .map(|to| TransactTo::Call(to.into()))
            .unwrap_or_else(TransactTo::create),
        value: value.unwrap_or_default().into(),
        data: data.unwrap_or_default().0,
        chain_id: chain_id.map(|c| c.as_u64()),
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
