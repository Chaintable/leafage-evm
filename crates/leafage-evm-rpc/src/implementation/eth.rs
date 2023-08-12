use crate::api::EthApiServer;
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, WrapDB};
use leafage_evm_types::{access_list_flattened, BlockId, CallRequest, RpcBytes, U256};
use revm::primitives::{BlockEnv, CfgEnv, Env, ExecutionResult, TransactTo, TxEnv};
use revm::EVM;

pub struct EthApiImpl<DB> {
    db: DB,
    cfg: CfgEnv,
}

fn ensure_fees(
    call_gas_price: Option<U256>,
    call_max_fee: Option<U256>,
    call_priority_fee: Option<U256>,
    base_fee: U256,
) -> (Option<U256>, Option<U256>) {
    match (call_gas_price, call_max_fee, call_priority_fee) {
        (gas_price, None, None) => {
            // either legacy transaction or no fee fields are specified
            // when no fields are specified, set gas price to zero
            let gas_price = gas_price.unwrap_or(U256::ZERO);
            (Some(gas_price), None)
        }
        (None, max_fee_per_gas, max_priority_fee_per_gas) => {
            // request for eip-1559 transaction
            let max_fee = max_fee_per_gas.unwrap_or(base_fee);

            if let Some(max_priority) = max_priority_fee_per_gas {
                if max_priority > max_fee {
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
        block_env.basefee,
    );

    let gas_limit = gas.unwrap_or(block_env.gas_limit.min(U256::from(u64::MAX)));

    let env = TxEnv {
        gas_limit: gas_limit
            .try_into()
            .map_err(|_| invalid_params_rpc_err("Gas Uint Overflow"))?,
        nonce: nonce
            .map(|n| {
                n.try_into()
                    .map_err(|_| invalid_params_rpc_err("Nonce Too High"))
            })
            .transpose()?,
        caller: from.unwrap_or_default().into(),
        gas_price: gas_price.unwrap_or_default(),
        gas_priority_fee: max_priority_fee_per_gas,
        transact_to: to
            .map(|to| TransactTo::Call(to.into()))
            .unwrap_or_else(TransactTo::create),
        value: value.unwrap_or_default(),
        data: data.unwrap_or_default(),
        chain_id: chain_id.map(|c| c.try_into().unwrap()),
        access_list: access_list.map(access_list_flattened).unwrap_or_default(),
    };

    Ok(env)
}

impl<DB: EvmStorageRead> EthApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv) -> Self {
        Self { db, cfg }
    }

    pub async fn call_impl(&self, request: CallRequest, block_id: BlockId) -> RpcResult<RpcBytes> {
        let mut cfg = self.cfg.clone();
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.disable_block_gas_limit = true;
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let block = state
            .block_info()
            .map_err(|e| internal_rpc_err(e.to_string()))?
            .into();
        let tx = create_txn_env(&block, request)?;
        let env = Env { block, cfg, tx };
        // let state =
        let mut evm = EVM::with_env(env);
        evm.database(WrapDB(state));
        let res = evm
            .transact_ref()
            .map_err(|e| internal_rpc_err(format!("{:?}", e)))?;
        match res.result {
            ExecutionResult::Success { output, .. } => Ok(output.into_data().into()),
            ExecutionResult::Revert { output, .. } => {
                Err(internal_rpc_err(format!("Reverted: {:?}", output)).into())
            }
            ExecutionResult::Halt { reason, gas_used } => {
                Err(internal_rpc_err(format!("Halted: {:?} {}", reason, gas_used)).into())
            }
        }
    }
}

#[async_trait::async_trait]
impl<DB> EthApiServer for EthApiImpl<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    async fn call(&self, request: CallRequest, block_id: BlockId) -> RpcResult<RpcBytes> {
        self.call_impl(request, block_id).await
    }
}
