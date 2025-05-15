pub use alloy::eips::eip1559::{calc_next_block_base_fee, BaseFeeParams};
pub use alloy::primitives::{hex, Address, Bytes, B256 as H256, KECCAK256_EMPTY, U256, U64};
#[cfg(feature = "optimism")]
pub use op_revm::OpSpecId as SpecId;
#[cfg(not(feature = "optimism"))]
pub use revm::primitives::hardfork::SpecId;

pub use revm::context::{result::ExecutionResult, BlockEnv, CfgEnv};

pub use revm::state::{AccountInfo, Bytecode};
