use crate::api_impl::core::{Api, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter};
use crate::api_impl::{ApiCore, EvmExecutor};
use crate::DebankApiServer;
use alloy::core::sol;
use alloy::primitives::TxKind;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::{
    BlockId, BlockNumberOrTag, BlockType, CallRequest, DebankBlockContext, DebankErrorCode,
    DebankTransaction,
};
use revm::primitives::Address;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

sol! {
    #[sol(rpc)]
    interface IERC20 {
        #[derive(Debug)]
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function mint(address to, uint256 amount) external;
        function burn(uint256 amount) external;
    }
}
impl<C> Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead + BlockIndex,
    C::Tx: TxSetter + Clone,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    DebankErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    pub(crate) async fn replay_blocks(&self, blocks: Vec<Vec<DebankTransaction>>) -> RpcResult<()> {
        let start = std::time::Instant::now();
        let block_len = blocks.len();
        let block_id = DebankBlockContext {
            block_id: BlockId::Number(BlockNumberOrTag::Latest),
            block_type: BlockType::Equals,
        };
        let transactions_len = blocks.iter().map(|b| b.len()).sum::<usize>();
        info!(target: "warmup","Start replay blocks with {block_len} blocks {transactions_len} transactions");
        for block in blocks {
            let transactions = block;
            let calls: Vec<_> = transactions
                .into_iter()
                .map(|tx| {
                    let mut transaction_request: TransactionRequest = tx.into();
                    transaction_request.gas_price = Some(0);
                    transaction_request.max_fee_per_gas = None;
                    transaction_request.max_priority_fee_per_gas = None;
                    transaction_request.max_fee_per_gas = None;
                    transaction_request
                })
                .collect();
            if let Err(err) = self
                .simulate_transactions(calls, Some(block_id.clone()), None)
                .await
            {
                warn!(target: "warm", "Error while simulating transactions: {}", err);
            }
        }
        info!(target: "warmup", "Replay {block_len} blocks {transactions_len} transactions time elapsed: {:?}", start.elapsed());
        Ok(())
    }
    pub(crate) async fn warmup_erc20_address(
        &self,
        owner: &Address,
        erc20_addresses: &[Address],
    ) -> RpcResult<()> {
        const ERC20_ADDRESS_BATCH: usize = 16;
        const TASK_CONCURRENT: usize = 64;

        let start = std::time::Instant::now();
        let input = IERC20::balanceOfCall { owner: *owner };
        let mut tasks = JoinSet::new();
        let semaphore = Arc::new(Semaphore::new(TASK_CONCURRENT));
        for erc20_addresses in erc20_addresses.chunks(ERC20_ADDRESS_BATCH) {
            let requests = erc20_addresses
                .iter()
                .map(|address| {
                    let request = CallRequest {
                        to: TxKind::Call(*address).into(),
                        input: input.abi_encode().into(),
                        ..Default::default()
                    };
                    request
                })
                .collect();
            tasks.spawn({
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let this = self.clone();
                async move {
                    this.contract_multi_call(requests, None, None, None, None, None, None)
                        .await?;
                    drop(permit);
                    RpcResult::Ok(())
                }
            });
        }
        info!(target: "warmup", "Warmup erc20 {} tokens time elapsed: {:?}", erc20_addresses.len(),start.elapsed());
        Ok(())
    }
}
