mod block_id;
pub use block_id::{BlockId, BlockNumber};

mod call;
pub use call::CallRequest;

pub use ethers_core::types::Bytes as RpcBytes;
