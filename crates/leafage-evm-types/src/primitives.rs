pub use ethers_core::types::{Address, Bytes, H160, H256, U256, U64};
pub use revm::primitives::Address as B160;
pub use revm::primitives::{AccountInfo, BlockEnv, Bytecode, KECCAK_EMPTY, U256 as RU256};
use serde::{Deserialize, Serialize};

/// Base fee max change denominator as defined in [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559)
pub const EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR: u64 = 8;

/// Elasticity multiplier as defined in [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559)
pub const EIP1559_DEFAULT_ELASTICITY_MULTIPLIER: u64 = 2;

/// BaseFeeParams contains the config parameters that control block base fee computation
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct BaseFeeParams {
    /// The base_fee_max_change_denominator from EIP-1559
    pub max_change_denominator: u64,
    /// The elasticity multiplier from EIP-1559
    pub elasticity_multiplier: u64,
}

impl BaseFeeParams {
    /// Get the base fee parameters for Ethereum mainnet
    pub const fn ethereum() -> BaseFeeParams {
        BaseFeeParams {
            max_change_denominator: EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
            elasticity_multiplier: EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
        }
    }
}

pub fn calculate_next_block_base_fee(
    gas_used: u64,
    gas_limit: u64,
    base_fee: u64,
    base_fee_params: BaseFeeParams,
) -> u64 {
    let gas_target = gas_limit / base_fee_params.elasticity_multiplier;
    if gas_used == gas_target {
        return base_fee;
    }
    if gas_used > gas_target {
        let gas_used_delta = gas_used - gas_target;
        let base_fee_delta = std::cmp::max(
            1,
            base_fee as u128 * gas_used_delta as u128
                / gas_target as u128
                / base_fee_params.max_change_denominator as u128,
        );
        base_fee + (base_fee_delta as u64)
    } else {
        let gas_used_delta = gas_target - gas_used;
        let base_fee_per_gas_delta = base_fee as u128 * gas_used_delta as u128
            / gas_target as u128
            / base_fee_params.max_change_denominator as u128;

        base_fee.saturating_sub(base_fee_per_gas_delta as u64)
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    pub address: Address,
    pub storage_keys: Vec<H256>,
}

pub type AccessList = Vec<AccessListItem>;

pub fn access_list_flattened(access_list: AccessList) -> Vec<(B160, Vec<RU256>)> {
    access_list
        .into_iter()
        .map(|item| {
            (
                item.address.as_fixed_bytes().into(),
                item.storage_keys
                    .into_iter()
                    .map(|v| RU256::from_be_bytes(v.into()))
                    .collect(),
            )
        })
        .collect()
}

pub fn trim_left_zero_bytes(bytes: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < bytes.len() && bytes[i] == 0 {
        i += 1;
    }
    &bytes[i..]
}
