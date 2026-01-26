use crate::cosmos::precompile::erc20::IERC20::{balanceOfCall, IERC20Calls};
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_sol_types::{sol, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use revm::primitives::{Bytes, U256};
use revm::Database;

pub fn create_erc20_precompile(erc20precompile: Erc20Precompile) -> DynPrecompile {
    DynPrecompile::new_stateful(PrecompileId::custom("COSMOS_ERC20"), move |input| {
        erc20precompile.run(input)
    })
}

sol! {
    #[sol(all_derives)]
    #[sol(extra_methods)]
    // SPDX-License-Identifier: MIT
// OpenZeppelin Contracts (last updated v4.6.0) (token/ERC20/IERC20.sol)

pragma solidity ^0.8.0;

/**
 * @dev Interface of the ERC20 standard as defined in the EIP.
 */
interface IERC20 {

    /**
     * @dev Returns the name of the token.
     */
    function name() external view returns (string memory);

    /**
     * @dev Returns the symbol of the token.
     */
    function symbol() external view returns (string memory);

    /**
     * @dev Returns the decimals places of the token.
     */
    function decimals() external view returns (uint8);
    /**
     * @dev Emitted when `value` tokens are moved from one account (`from`) to
     * another (`to`).
     *
     * Note that `value` may be zero.
     */
    event Transfer(address indexed from, address indexed to, uint256 value);

    /**
     * @dev Emitted when the allowance of a `spender` for an `owner` is set by
     * a call to {approve}. `value` is the new allowance.
     */
    event Approval(address indexed owner, address indexed spender, uint256 value);

    /**
     * @dev Returns the amount of tokens in existence.
     */
    function totalSupply() external view returns (uint256);

    /**
     * @dev Returns the amount of tokens owned by `account`.
     */
    function balanceOf(address account) external view returns (uint256);

    /**
     * @dev Moves `amount` tokens from the caller's account to `to`.
     *
     * Returns a boolean value indicating whether the operation succeeded.
     *
     * Emits a {Transfer} event.
     */
    function transfer(address to, uint256 amount) external returns (bool);

    /**
     * @dev Returns the remaining number of tokens that `spender` will be
     * allowed to spend on behalf of `owner` through {transferFrom}. This is
     * zero by default.
     *
     * This value changes when {approve} or {transferFrom} are called.
     */
    function allowance(address owner, address spender) external view returns (uint256);

    /**
     * @dev Sets `amount` as the allowance of `spender` over the caller's tokens.
     *
     * Returns a boolean value indicating whether the operation succeeded.
     *
     * IMPORTANT: Beware that changing an allowance with this method brings the risk
     * that someone may use both the old and the new allowance by unfortunate
     * transaction ordering. One possible solution to mitigate this race
     * condition is to first reduce the spender's allowance to 0 and set the
     * desired value afterwards:
     * https://github.com/ethereum/EIPs/issues/20#issuecomment-263524729
     *
     * Emits an {Approval} event.
     */
    function approve(address spender, uint256 amount) external returns (bool);

    /**
     * @dev Moves `amount` tokens from `from` to `to` using the
     * allowance mechanism. `amount` is then deducted from the caller's
     * allowance.
     *
     * Returns a boolean value indicating whether the operation succeeded.
     *
     * Emits a {Transfer} event.
     */
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}}

mod gas {
    pub(super) const TRANSFER: u64 = 3_000_000;
    pub(super) const APPROVE: u64 = 30_956;
    pub(super) const INCREASE_ALLOWANCE: u64 = 34_605;
    pub(super) const DECREASE_ALLOWANCE: u64 = 34_519;
    pub(super) const NAME: u64 = 3_421;
    pub(super) const SYMBOL: u64 = 3_464;
    pub(super) const DECIMALS: u64 = 427;
    pub(super) const TOTAL_SUPPLY: u64 = 2_477;
    pub(super) const BALANCE_OF: u64 = 2_851;
    pub(super) const ALLOWANCE: u64 = 3_246;
}

pub struct Erc20Precompile {
    name: String,
    symbol: String,
    decimals: u8,
    total_supply: U256,
}

impl Erc20Precompile {
    pub fn new(name: String, symbol: String, decimals: u8, total_supply: U256) -> Self {
        Self {
            name,
            symbol,
            decimals,
            total_supply,
        }
    }
    pub fn run(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        if input.data.len() < 4 {
            return Err(PrecompileError::other("invalid input"));
        }
        let call = IERC20Calls::abi_decode(&input.data)
            .map_err(|err| PrecompileError::other(format!("{:?}", err)))?;
        if input.gas < Self::required_gas(&call) {
            return Err(PrecompileError::OutOfGas);
        }
        match call {
            IERC20Calls::name(_) => self.name(),
            IERC20Calls::symbol(_) => self.symbol(),
            IERC20Calls::decimals(_) => self.decimals(),
            IERC20Calls::totalSupply(_) => self.total_supply(),
            IERC20Calls::balanceOf(call) => Self::balance_of(input, &call),
            _ => Err(PrecompileError::other("unsupported erc20 method")),
        }
    }
    fn required_gas(call: &IERC20Calls) -> u64 {
        match call {
            IERC20Calls::totalSupply(_) => gas::TOTAL_SUPPLY,
            IERC20Calls::balanceOf(_) => gas::BALANCE_OF,
            IERC20Calls::transfer(_) => gas::TRANSFER,
            IERC20Calls::allowance(_) => gas::ALLOWANCE,
            IERC20Calls::approve(_) => gas::APPROVE,
            IERC20Calls::transferFrom(_) => gas::TRANSFER,
            IERC20Calls::name(_) => gas::NAME,
            IERC20Calls::symbol(_) => gas::SYMBOL,
            IERC20Calls::decimals(_) => gas::DECIMALS,
        }
    }

    fn name(&self) -> PrecompileResult {
        let ret = (self.name.clone(),);
        Ok(PrecompileOutput::new(
            gas::NAME,
            Bytes::from(ret.abi_encode()),
        ))
    }

    fn symbol(&self) -> PrecompileResult {
        let ret = (self.symbol.clone(),);
        Ok(PrecompileOutput::new(
            gas::SYMBOL,
            Bytes::from(ret.abi_encode()),
        ))
    }

    fn decimals(&self) -> PrecompileResult {
        let ret = (U256::from(self.decimals),);
        Ok(PrecompileOutput::new(
            gas::DECIMALS,
            Bytes::from(ret.abi_encode()),
        ))
    }

    fn total_supply(&self) -> PrecompileResult {
        let ret = (self.total_supply,);
        Ok(PrecompileOutput::new(
            gas::TOTAL_SUPPLY,
            Bytes::from(ret.abi_encode()),
        ))
    }
    fn balance_of(mut input: PrecompileInput, call: &balanceOfCall) -> PrecompileResult {
        let mut db = input.internals.db_mut();
        let account = db
            .basic(call.account)
            .map_err(|err| PrecompileError::other(format!("fetch db error: {:?}", err)))?;
        let balance = match account {
            None => U256::ZERO,
            Some(account) => account.balance,
        };
        let ret = (balance,);
        Ok(PrecompileOutput::new(
            gas::BALANCE_OF,
            Bytes::from(ret.abi_encode()),
        ))
    }
}
