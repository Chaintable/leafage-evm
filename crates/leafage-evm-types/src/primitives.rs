pub use alloy::eips::eip1559::BaseFeeParams;
pub use alloy::primitives::{hex, Address, Bytes, B256 as H256, KECCAK256_EMPTY, U256, U64};
pub use op_revm::OpSpecId;
pub use revm::context::{BlockEnv, CfgEnv};
pub use revm::primitives::hardfork::SpecId as MainnetSpecId;
pub use revm::state::{AccountInfo, Bytecode};

/// EIP-1559 update with full-width intermediates. None means the next fee exceeds u128.
pub fn calc_next_block_base_fee(
    gas_used: u64,
    gas_limit: u64,
    base_fee: u128,
    params: BaseFeeParams,
) -> Option<u128> {
    let target = (gas_limit as u128)
        .checked_div(params.elasticity_multiplier)
        .unwrap_or(0);
    if target == 0 || params.max_change_denominator == 0 {
        return Some(base_fee);
    }
    let used = gas_used as u128;
    let change = U256::from(base_fee) * U256::from(used.abs_diff(target))
        / U256::from(target)
        / U256::from(params.max_change_denominator);
    let fee = if used > target {
        U256::from(base_fee) + change.max(U256::ONE)
    } else {
        U256::from(base_fee) - change
    };
    fee.try_into().ok()
}
