use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{
    ApiCore, EvmExecutor, GasFeeHandler, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter,
};
use crate::api_impl::op::evm::{create_op_evm_from_state, create_op_txn_env};
use crate::api_impl::op::legacy;
use crate::api_impl::ApiImpl;
use crate::error::rpc_error_with_code;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::op::schedule::op_stack_spec_at;
use leafage_evm_types::{BlockInfo, CallRequest, CfgEnv, DebankErrorCode, OpSpecId};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, HaltReason, InvalidTransaction};
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type OpApiImpl<DB> = ApiImpl<DB, OpSpecId, NoneEvmCustomConfig>;

/// The configured cfg with the fork of the executing block. On a chain with a built-in
/// fork schedule (`op::schedule`) the spec and its gas params follow that schedule, so a
/// historical height runs the rules it was produced under; the other OP-stack chains
/// served by this evm-type keep the configured spec.
fn block_cfg(cfg: &CfgEnv<OpSpecId>, block_env: &BlockEnv) -> CfgEnv<OpSpecId> {
    let mut cfg = cfg.clone();
    if let Some(spec) = op_stack_spec_at(
        cfg.chain_id,
        block_env.number.saturating_to(),
        block_env.timestamp.saturating_to(),
    ) {
        cfg.set_spec_and_mainnet_gas_params(spec);
    }
    cfg
}

impl<DB> GasFeeHandler for OpApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;

    /// EIP-7825 (Karst) follows the executing block's fork, not the configured spec.
    fn consensus_tx_gas_limit_cap(&self, _spec: SpecId, block_env: &BlockEnv) -> u64 {
        let spec: SpecId = block_cfg(&self.evm_cfg.cfg, block_env).spec.into();
        if spec.is_enabled_in(SpecId::OSAKA) {
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        } else {
            u64::MAX
        }
    }
}

impl<DB> EvmExecutor for OpApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
    type TransactionError = OpTransactionError;
    type EvmHaltReason = OpHaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_op_txn_env(
            block_env,
            block_cfg(&self.evm_cfg.cfg, block_env),
            request,
            db,
            chain_id,
        )
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        if legacy::is_legacy(&self.evm_cfg.cfg, block_env) {
            return legacy::transact(&self.evm_cfg.cfg, block_env, state, tx);
        }
        let mut evm = create_op_evm_from_state(
            block_env.clone(),
            block_cfg(&self.evm_cfg.cfg, block_env),
            state,
            NoOpInspector {},
        );

        let res = evm.transact(tx).map(|res| res.result.into());
        res
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut inspector = TracingInspector::new(inspector_cfg);
        if legacy::is_legacy(&self.evm_cfg.cfg, block_env) {
            let res =
                legacy::inspect_tx_commit(&self.evm_cfg.cfg, block_env, state, &mut inspector, tx);
            return res.map(|res| (res, inspector_collect(inspector)));
        }
        let mut evm = create_op_evm_from_state(
            block_env.clone(),
            block_cfg(&self.evm_cfg.cfg, block_env),
            state,
            &mut inspector,
        );

        let res = evm
            .inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)));

        res
    }
}

impl ToJsonRpcError for OpTransactionError {
    fn to_rpc_error(&self) -> jsonrpsee::types::ErrorObjectOwned {
        match self {
            OpTransactionError::Base(invalid_tx) => invalid_tx.to_rpc_error(),
            e => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
        }
    }
}

impl GetTransactionError for OpTransactionError {
    fn get_transaction_error(&self) -> Option<InvalidTransaction> {
        match self {
            OpTransactionError::Base(invalid_tx) => Some(invalid_tx.clone()),
            _ => None,
        }
    }
}

impl TxSetter for OpTransaction<TxEnv> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl GetHaltReason for OpHaltReason {
    fn get_halt_reason(&self) -> Option<HaltReason> {
        match self {
            OpHaltReason::Base(halt) => Some(halt.clone()),
            _ => None,
        }
    }
}

impl<DB> ApiCore for OpApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::op::mainnet::OP_MAINNET_CHAIN_ID;
    use leafage_evm_chains::op::schedule::SONEIUM_CHAIN_ID;
    use leafage_evm_types::U256;

    fn block(number: u64, timestamp: u64) -> BlockEnv {
        BlockEnv {
            number: U256::from(number),
            timestamp: U256::from(timestamp),
            ..Default::default()
        }
    }

    fn op_mainnet_cfg() -> CfgEnv<OpSpecId> {
        let mut cfg = CfgEnv::new_with_spec(OpSpecId::KARST);
        cfg.chain_id = OP_MAINNET_CHAIN_ID;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(50_000_000);
        cfg
    }

    fn api(cfg: CfgEnv<OpSpecId>) -> OpApiImpl<()> {
        OpApiImpl::<()>::new(
            (),
            cfg,
            None,
            None,
            None,
            None,
            false,
            false,
            String::new(),
            0,
            None,
            None,
            None,
        )
    }

    #[test]
    fn mainnet_spec_and_gas_params_follow_the_block() {
        let cfg = op_mainnet_cfg();
        for (block_env, spec) in [
            (block(105_235_062, 1_686_068_901), OpSpecId::BEDROCK),
            (block(105_235_063, 1_686_068_903), OpSpecId::REGOLITH),
            (block(114_696_812, 1_704_992_401), OpSpecId::CANYON),
            (block(135_603_811, 1_746_806_399), OpSpecId::HOLOCENE),
            (block(135_603_812, 1_746_806_401), OpSpecId::ISTHMUS),
            (block(153_963_812, 1_783_526_401), OpSpecId::KARST),
        ] {
            let actual = block_cfg(&cfg, &block_env);
            assert_eq!(actual.spec, spec);
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
            // Everything else is the configured cfg.
            assert_eq!(actual.chain_id, cfg.chain_id);
            assert_eq!(actual.tx_gas_limit_cap, cfg.tx_gas_limit_cap);
            assert!(actual.disable_base_fee);
        }
    }

    #[test]
    fn soneium_follows_its_own_forks() {
        let mut cfg = op_mainnet_cfg();
        cfg.chain_id = SONEIUM_CHAIN_ID;
        for (block_env, spec) in [
            // OP Mainnet reached Holocene at 1_736_445_601; Soneium was still on Granite.
            (block(1_932_625, 1_737_000_001), OpSpecId::GRANITE),
            (block(2_719_225, 1_738_573_201), OpSpecId::HOLOCENE),
            (block(6_835_824, 1_746_806_399), OpSpecId::HOLOCENE),
            (block(6_835_825, 1_746_806_401), OpSpecId::ISTHMUS),
            (block(25_195_825, 1_783_526_401), OpSpecId::KARST),
        ] {
            let actual = block_cfg(&cfg, &block_env);
            assert_eq!(actual.spec, spec);
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
        }
    }

    #[test]
    fn other_chains_keep_the_configured_spec() {
        let mut cfg = CfgEnv::new_with_spec(OpSpecId::JOVIAN);
        cfg.chain_id = 4153;
        let actual = block_cfg(&cfg, &block(1, 0));
        assert_eq!(actual.spec, OpSpecId::JOVIAN);
        assert_eq!(actual.gas_params, cfg.gas_params);
    }

    #[test]
    fn estimate_cap_follows_the_block() {
        let op = api(op_mainnet_cfg());
        let jovian = block(153_963_811, 1_783_526_399);
        let karst = block(153_963_812, 1_783_526_401);
        // The configured spec (passed by debank estimateGas) no longer decides.
        assert_eq!(
            op.consensus_tx_gas_limit_cap(SpecId::OSAKA, &jovian),
            u64::MAX
        );
        assert_eq!(
            op.consensus_tx_gas_limit_cap(SpecId::OSAKA, &karst),
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        );

        // Other OP-stack chains keep the configured (Karst) cap at every height.
        let mut other = op_mainnet_cfg();
        other.chain_id = 8453;
        assert_eq!(
            api(other).consensus_tx_gas_limit_cap(SpecId::OSAKA, &jovian),
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        );
    }
}
