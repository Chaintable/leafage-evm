mod call;
pub use call::CallRequest;

mod key;
pub use key::JsonStorageKey;

mod multi_call;
pub use multi_call::{MultiCallErrorCode, MultiCallResp, MultiCallStats, SingleCallResult};

pub use alloy::consensus::TxEnvelope;
pub use alloy::rpc::types::trace::parity::{
    Action, CallAction, CallOutput, CallType, CreateAction, CreateOutput,
    LocalizedTransactionTrace, RewardAction, SelfdestructAction, TraceOutput, TransactionTrace,
};
pub use alloy::rpc::types::{Block, BlockId, BlockNumberOrTag, Index, Log, TransactionInfo};

#[cfg(not(feature = "optimism"))]
pub use alloy::rpc::types::Transaction;

#[cfg(feature = "optimism")]
pub use op_alloy_rpc_types::Transaction;

#[cfg(feature = "optimism")]
pub use op_alloy_consensus::{OpTxEnvelope, OpTxType, TxDeposit};

mod pre;
pub use pre::{PreError, PreErrorCode, PreResult};
