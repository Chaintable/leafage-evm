pub use alloy::eips::eip1559::BaseFeeParams;
pub use alloy::primitives::{Address, Bytes, B256 as H256, U256, U64};
pub use revm::primitives::{hex, AccountInfo, BlockEnv, Bytecode, KECCAK_EMPTY, U256 as RU256};

pub fn calculate_next_block_base_fee(
    _gas_used: u128,
    _gas_limit: u128,
    _base_fee: u128,
    _base_fee_params: BaseFeeParams,
) -> u128 {
    return 0;
}
