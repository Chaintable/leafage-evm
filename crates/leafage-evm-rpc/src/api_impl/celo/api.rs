use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler};
use crate::api_impl::op::evm::create_op_txn_env;
use crate::api_impl::ApiImpl;
use crate::error::invalid_params_rpc_err;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::celo::{evm::create_celo_evm, CeloConfig};
use leafage_evm_types::{BlockInfo, CallRequest, OpSpecId};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::EVMError;
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type CeloApiImpl<DB> = ApiImpl<DB, OpSpecId, CeloConfig>;

impl<DB> ApiCore for CeloApiImpl<DB> where DB: Sync + Send + 'static {}

impl<DB> GasFeeHandler for CeloApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
}

impl<DB> EvmExecutor for CeloApiImpl<DB>
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
            CeloConfig::cfg_at(self.evm_cfg.cfg.clone(), block_env)
                .map_err(invalid_params_rpc_err)?,
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
        let mut evm = create_celo_evm(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            WrapDatabaseRef(state),
            NoOpInspector {},
        )
        .map_err(EVMError::Custom)?;
        evm.transact(tx).map(|res| res.result.into())
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
        let mut evm = create_celo_evm(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            WrapDatabaseRef(state),
            &mut inspector,
        )
        .map_err(EVMError::Custom)?;
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::types::{TransactionInput, TransactionRequest};
    use leafage_evm_chains::celo::{CELO_CHAIN_ID, CELO_TOKEN, L2_MIGRATION_BLOCK};
    use leafage_evm_types::{AccountInfo, Address, Bytecode, Bytes, CfgEnv, U256};
    use revm::{database::InMemoryDB, DatabaseRef};

    #[test]
    fn call_and_simulation_execute_token_transfer_and_revert() {
        let sender = Address::repeat_byte(0x11);
        let recipient = Address::repeat_byte(0x22);
        let mut cfg = CfgEnv::new_with_spec(OpSpecId::JOVIAN);
        cfg.chain_id = CELO_CHAIN_ID;
        cfg.disable_base_fee = true;
        let api: CeloApiImpl<()> = ApiImpl::new(
            (),
            cfg,
            Some(CeloConfig),
            None,
            None,
            None,
            false,
            false,
            String::new(),
            100,
            None,
            None,
            None,
        );
        let block = BlockEnv {
            number: revm::primitives::U256::from(L2_MIGRATION_BLOCK),
            timestamp: U256::from(1_742_957_258),
            ..Default::default()
        };
        let mut data = vec![0; 96];
        data[12..32].copy_from_slice(sender.as_slice());
        data[44..64].copy_from_slice(recipient.as_slice());
        data[95] = 3;
        for revert in [false, true] {
            // Token stand-in: forward calldata to 0xfd, then return the recipient's
            // native balance. A missing precompile would return zero instead of 3.
            let mut code = vec![
                0x36, 0x5f, 0x5f, 0x37, 0x5f, 0x5f, 0x36, 0x5f, 0x5f, 0x60, 0xfd, 0x5a, 0xf1, 0x50,
            ];
            if revert {
                code.extend([0x5f, 0x5f, 0xfd]);
            } else {
                code.push(0x73);
                code.extend(recipient.as_slice());
                code.extend([0x31, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
            }
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                sender,
                AccountInfo {
                    balance: U256::from(10),
                    ..Default::default()
                },
            );
            db.insert_account_info(
                CELO_TOKEN,
                AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
            );
            let request = CallRequest {
                inner: TransactionRequest::default()
                    .from(sender)
                    .to(CELO_TOKEN)
                    .gas_limit(100_000)
                    .input(TransactionInput::new(Bytes::from(data.clone()))),
                ..Default::default()
            };
            let tx = api
                .create_txn_env(&BlockInfo::default(), &block, request, &db, CELO_CHAIN_ID)
                .unwrap();
            let ordinary = api.transact(&block, &db, tx.clone()).unwrap();
            let (inspected, _) = api
                .inspect_tx_commit(
                    &block,
                    &mut db,
                    TracingInspectorConfig::default_parity(),
                    |_| (),
                    tx,
                )
                .unwrap();
            assert_eq!(ordinary, inspected);
            assert_eq!(ordinary.is_success(), !revert);
            if !revert {
                assert_eq!(
                    U256::from_be_slice(ordinary.output().unwrap()),
                    U256::from(3)
                );
            }
            assert_eq!(
                db.basic_ref(sender).unwrap().unwrap().balance,
                U256::from(if revert { 10 } else { 7 })
            );
            assert_eq!(
                db.basic_ref(recipient).unwrap().unwrap_or_default().balance,
                U256::from(if revert { 0 } else { 3 })
            );
        }
    }
}
