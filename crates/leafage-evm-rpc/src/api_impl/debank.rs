use crate::api::EthApiServer;
use crate::api_impl::utils::{create_txn_env, get_handler_cfg};
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{
    BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper, TransactionIndex,
};
use leafage_evm_types::{
    block_env_from_block, calc_next_block_base_fee, Address, BaseFeeParams, Block, BlockId,
    BlockNumberOrTag, Bytes, CallRequest, Index, JsonStorageKey, MultiCallErrorCode, MultiCallResp,
    MultiCallStats, SingleCallResult, Transaction, H256, RU256, U256,
};
use revm::db::DatabaseRef;
use revm::primitives::{CfgEnv, EnvWithHandlerCfg, ExecutionResult, SpecId};
use revm::Evm;
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

/// [`EthApiImpl`] implements the EthApi trait.
pub struct DebankApiImpl<DB> {
    db: DB,
    cfg: CfgEnv,
    spec_id: SpecId,
}

impl<DB: EvmStorageRead + BlockIndex> DebankApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv, spec_id: SpecId) -> Self {
        Self { db, cfg, spec_id }
    }
}
