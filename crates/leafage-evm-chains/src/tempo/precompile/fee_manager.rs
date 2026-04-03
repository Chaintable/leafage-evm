//! Fee manager precompile for transaction fee collection, distribution, and token swaps.
//!
//! Users and validators choose their preferred TIP-20 fee token. When they differ,
//! fees are swapped through the built-in AMM (`TIPFeeAMM`).
//!
//! Ported from `tempo/crates/precompiles/src/tip_fee_manager/`.
//!
//! ## Storage layout
//!
//! | Slot | Field                          | Type                                        |
//! |------|--------------------------------|---------------------------------------------|
//! |  0   | validator_tokens               | Mapping<Address, Address>                   |
//! |  1   | user_tokens                    | Mapping<Address, Address>                   |
//! |  2   | collected_fees                 | Mapping<Address, Mapping<Address, U256>>    |
//! |  3   | pools                          | Mapping<B256, Pool>                         |
//! |  4   | total_supply                   | Mapping<B256, U256>                         |
//! |  5   | liquidity_balances             | Mapping<B256, Mapping<Address, U256>>       |
//! |  6   | pending_fee_swap_reservation   | Mapping<B256, u128> (transient)             |

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::storage::StorageOps;
use super::storage_types::{Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType};
use super::tip20::TIP20Token;
use super::{dispatch_call,
    fill_precompile_output, input_cost, metadata, mutate, mutate_void, view, Precompile,
    DEFAULT_FEE_TOKEN, TIP_FEE_MANAGER_ADDRESS,
};

// ===========================================================================
// AMM constants
// ===========================================================================

/// Fee multiplier for fee swaps: 0.9970 scaled by 10000 (30 bps fee).
pub const M: U256 = U256::from_limbs([9970, 0, 0, 0]);
/// Fee multiplier for rebalance swaps: 0.9985 scaled by 10000.
pub const N: U256 = U256::from_limbs([9985, 0, 0, 0]);
/// Scale factor for fixed-point AMM arithmetic (10000).
pub const SCALE: U256 = U256::from_limbs([10000, 0, 0, 0]);
/// Minimum liquidity locked permanently when initializing a pool.
pub const MIN_LIQUIDITY: U256 = U256::from_limbs([1000, 0, 0, 0]);

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    interface IFeeManager {
        function userTokens(address user) external view returns (address);
        function validatorTokens(address validator) external view returns (address);
        function collectedFees(address validator, address token) external view returns (uint256);

        function setValidatorToken(address token) external;
        function setUserToken(address token) external;
        function distributeFees(address validator, address token) external;

        event ValidatorTokenSet(address indexed validator, address token);
        event UserTokenSet(address indexed user, address token);
        event FeesDistributed(address indexed validator, address token, uint256 amount);

        error InvalidToken();
        error CannotChangeWithinBlock();
        error InsufficientLiquidity();
        error PolicyForbids();
    }

    interface ITIPFeeAMM {
        function M() external view returns (uint256);
        function N() external view returns (uint256);
        function SCALE() external view returns (uint256);
        function MIN_LIQUIDITY() external view returns (uint256);

        function getPoolId(address userToken, address validatorToken) external view returns (bytes32);
        function getPool(address userToken, address validatorToken) external view returns (Pool memory);
        function pools(bytes32 poolId) external view returns (Pool memory);
        function totalSupply(bytes32 poolId) external view returns (uint256);
        function liquidityBalances(bytes32 poolId, address user) external view returns (uint256);

        function mint(address userToken, address validatorToken, uint256 amountValidatorToken, address to) external returns (uint256);
        function burn(address userToken, address validatorToken, uint256 liquidity, address to) external returns (uint256 amountUserToken, uint256 amountValidatorToken);
        function rebalanceSwap(address userToken, address validatorToken, uint256 amountOut, address to) external returns (uint256);

        struct Pool {
            uint128 reserveUserToken;
            uint128 reserveValidatorToken;
        }

        event Mint(address indexed sender, address to, address userToken, address validatorToken, uint256 amountValidatorToken, uint256 liquidity);
        event Burn(address indexed sender, address userToken, address validatorToken, uint256 amountUserToken, uint256 amountValidatorToken, uint256 liquidity, address to);
        event RebalanceSwap(address indexed userToken, address indexed validatorToken, address swapper, uint256 amountIn, uint256 amountOut);

        error IdenticalAddresses();
        error InvalidAmount();
        error InsufficientLiquidity();
        error InsufficientReserves();
        error InvalidSwapCalculation();
        error DivisionByZero();
        error InvalidCurrency();
    }
}

// ===========================================================================
// Pool / PoolKey types
// ===========================================================================

/// AMM pool reserves for a user-token / validator-token pair.
#[derive(Debug, Clone, Default)]
pub struct Pool {
    pub reserve_user_token: u128,
    pub reserve_validator_token: u128,
}

impl StorableType for Pool {
    const LAYOUT: Layout = Layout::Slots(1);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for Pool {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let word = storage.load(slot)?;
        // Packed in a single slot: reserve_user_token at offset 0 (low 16 bytes),
        // reserve_validator_token at offset 16 (high 16 bytes).
        // Solidity packs first field at low offset.
        let bytes = word.to_be_bytes::<32>();
        let reserve_validator_token = u128::from_be_bytes(bytes[0..16].try_into().unwrap());
        let reserve_user_token = u128::from_be_bytes(bytes[16..32].try_into().unwrap());
        Ok(Self {
            reserve_user_token,
            reserve_validator_token,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[0..16].copy_from_slice(&self.reserve_validator_token.to_be_bytes());
        bytes[16..32].copy_from_slice(&self.reserve_user_token.to_be_bytes());
        storage.store(slot, U256::from_be_bytes(bytes))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)
    }
}

/// Identifies a directional token pair in the fee AMM.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub user_token: Address,
    pub validator_token: Address,
}

impl PoolKey {
    pub fn new(user_token: Address, validator_token: Address) -> Self {
        Self {
            user_token,
            validator_token,
        }
    }

    pub fn get_id(&self) -> B256 {
        keccak256((self.user_token, self.validator_token).abi_encode())
    }
}

/// Computes the output amount for a fee swap: `amount_in * M / SCALE`.
#[inline]
pub fn compute_amount_out(amount_in: U256) -> Result<U256> {
    amount_in
        .checked_mul(M)
        .map(|product| product / SCALE)
        .ok_or_else(|| {
            TempoPrecompileError::Fatal("underflow/overflow in compute_amount_out".into())
        })
}

// ===========================================================================
// TipFeeManager struct (manual macro expansion)
// ===========================================================================

/// Fee manager precompile that handles fee collection, distribution, and AMM swaps.
pub struct TipFeeManager {
    // Slot 0: validator_tokens
    pub validator_tokens: Mapping<Address, Address>,
    // Slot 1: user_tokens
    pub user_tokens: Mapping<Address, Address>,
    // Slot 2: collected_fees
    pub collected_fees: Mapping<Address, Mapping<Address, U256>>,
    // Slot 3: pools
    pub pools: Mapping<B256, Pool>,
    // Slot 4: total_supply
    pub total_supply: Mapping<B256, U256>,
    // Slot 5: liquidity_balances
    pub liquidity_balances: Mapping<B256, Mapping<Address, U256>>,
    // Slot 6: pending_fee_swap_reservation (transient storage)
    pub pending_fee_swap_reservation: Mapping<B256, u128>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl TipFeeManager {
    pub fn new() -> Self {
        let address = TIP_FEE_MANAGER_ADDRESS;
        Self {
            validator_tokens: Mapping::new(U256::from(0), address),
            user_tokens: Mapping::new(U256::from(1), address),
            collected_fees: Mapping::new(U256::from(2), address),
            pools: Mapping::new(U256::from(3), address),
            total_supply: Mapping::new(U256::from(4), address),
            liquidity_balances: Mapping::new(U256::from(5), address),
            pending_fee_swap_reservation: Mapping::new(U256::from(6), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage
            .emit_event(self.address, event.into_log_data())
    }

    /// Initializes the fee manager precompile.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    // -----------------------------------------------------------------------
    // Core fee manager methods
    // -----------------------------------------------------------------------

    /// Returns the validator's preferred fee token, falling back to `DEFAULT_FEE_TOKEN`.
    pub fn get_validator_token(&self, beneficiary: Address) -> Result<Address> {
        let token = self.validator_tokens[beneficiary].read()?;
        if token.is_zero() {
            Ok(DEFAULT_FEE_TOKEN)
        } else {
            Ok(token)
        }
    }

    /// Sets the caller's preferred fee token as a validator.
    pub fn set_validator_token(
        &mut self,
        sender: Address,
        call: IFeeManager::setValidatorTokenCall,
        beneficiary: Address,
    ) -> Result<()> {
        // Validate that the token is a valid deployed TIP20 via factory
        if !super::tip20_factory::TIP20Factory::new().is_tip20(call.token)? {
            return Err(TempoPrecompileError::Revert(
                IFeeManager::InvalidToken {}.abi_encode().into(),
            ));
        }

        // Prevent changing within the validator's own block
        if sender == beneficiary {
            return Err(TempoPrecompileError::Revert(
                IFeeManager::CannotChangeWithinBlock {}.abi_encode().into(),
            ));
        }

        // Validate that the fee token is USD
        validate_usd_currency(call.token)?;

        self.validator_tokens[sender].write(call.token)?;

        self.emit_event(IFeeManager::ValidatorTokenSet {
            validator: sender,
            token: call.token,
        })
    }

    /// Sets the caller's preferred fee token as a user.
    pub fn set_user_token(
        &mut self,
        sender: Address,
        call: IFeeManager::setUserTokenCall,
    ) -> Result<()> {
        if !super::tip20_factory::TIP20Factory::new().is_tip20(call.token)? {
            return Err(TempoPrecompileError::Revert(
                IFeeManager::InvalidToken {}.abi_encode().into(),
            ));
        }

        validate_usd_currency(call.token)?;

        self.user_tokens[sender].write(call.token)?;

        self.emit_event(IFeeManager::UserTokenSet {
            user: sender,
            token: call.token,
        })
    }

    /// Collects fees from `fee_payer` before transaction execution.
    ///
    /// Transfers `max_amount` of `user_token` to the fee manager and checks pool liquidity.
    pub fn collect_fee_pre_tx(
        &mut self,
        fee_payer: Address,
        user_token: Address,
        max_amount: U256,
        beneficiary: Address,
    ) -> Result<Address> {
        let validator_token = self.get_validator_token(beneficiary)?;

        let mut tip20_token = TIP20Token::from_address(user_token)?;
        tip20_token.ensure_transfer_authorized(fee_payer, self.address)?;
        tip20_token.transfer_fee_pre_tx(fee_payer, max_amount)?;

        if user_token != validator_token {
            let pool_id = PoolKey::new(user_token, validator_token).get_id();
            let _amount_out_needed = self.check_sufficient_liquidity(pool_id, max_amount)?;
            // T1C+ reservation handled in full Tempo node; leafage omits transient storage reservation
        }

        Ok(user_token)
    }

    /// Finalizes fee collection after transaction execution.
    pub fn collect_fee_post_tx(
        &mut self,
        fee_payer: Address,
        actual_spending: U256,
        refund_amount: U256,
        fee_token: Address,
        beneficiary: Address,
    ) -> Result<()> {
        let mut tip20_token = TIP20Token::from_address(fee_token)?;
        tip20_token.transfer_fee_post_tx(fee_payer, refund_amount, actual_spending)?;

        let validator_token = self.get_validator_token(beneficiary)?;

        if fee_token != validator_token && !actual_spending.is_zero() {
            self.execute_fee_swap(fee_token, validator_token, actual_spending)?;
        }

        let amount = if fee_token == validator_token {
            actual_spending
        } else {
            compute_amount_out(actual_spending)?
        };

        self.increment_collected_fees(beneficiary, validator_token, amount)?;
        Ok(())
    }

    /// Increment collected fees for a validator/token pair.
    fn increment_collected_fees(
        &mut self,
        validator: Address,
        token: Address,
        amount: U256,
    ) -> Result<()> {
        if amount.is_zero() {
            return Ok(());
        }
        let collected_fees = self.collected_fees[validator][token].read()?;
        self.collected_fees[validator][token].write(
            collected_fees.checked_add(amount).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in increment_collected_fees".into())
            })?,
        )
    }

    /// Transfers a validator's accumulated fee balance and zeroes the ledger.
    pub fn distribute_fees(&mut self, validator: Address, token: Address) -> Result<()> {
        let amount = self.collected_fees[validator][token].read()?;
        if amount.is_zero() {
            return Ok(());
        }
        self.collected_fees[validator][token].write(U256::ZERO)?;

        let mut tip20_token = TIP20Token::from_address(token)?;
        tip20_token.transfer(
            self.address,
            super::tip20::ITIP20::transferCall {
                to: validator,
                amount,
            },
        )?;

        self.emit_event(IFeeManager::FeesDistributed {
            validator,
            token,
            amount,
        })
    }

    /// Reads the stored fee token preference for a user.
    pub fn user_tokens_view(&self, call: IFeeManager::userTokensCall) -> Result<Address> {
        self.user_tokens[call.user].read()
    }

    /// Reads the stored fee token preference for a validator, defaulting to `DEFAULT_FEE_TOKEN`.
    pub fn validator_tokens_view(
        &self,
        call: IFeeManager::validatorTokensCall,
    ) -> Result<Address> {
        let token = self.validator_tokens[call.validator].read()?;
        if token.is_zero() {
            Ok(DEFAULT_FEE_TOKEN)
        } else {
            Ok(token)
        }
    }

    // -----------------------------------------------------------------------
    // AMM methods
    // -----------------------------------------------------------------------

    /// Returns the deterministic pool ID for a directional token pair.
    pub fn pool_id(&self, user_token: Address, validator_token: Address) -> B256 {
        PoolKey::new(user_token, validator_token).get_id()
    }

    /// Returns the Pool reserves for the given user/validator token pair.
    pub fn get_pool(&self, call: ITIPFeeAMM::getPoolCall) -> Result<Pool> {
        let pool_id = self.pool_id(call.userToken, call.validatorToken);
        self.pools[pool_id].read()
    }

    /// Checks that the pool has enough reserves for the fee swap.
    pub fn check_sufficient_liquidity(&mut self, pool_id: B256, max_amount: U256) -> Result<u128> {
        let amount_out_needed = compute_amount_out(max_amount)?;
        let pool = self.pools[pool_id].read()?;

        if amount_out_needed > U256::from(pool.reserve_validator_token) {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InsufficientLiquidity {}.abi_encode().into(),
            ));
        }

        amount_out_needed.try_into().map_err(|_| {
            TempoPrecompileError::Fatal("overflow in check_sufficient_liquidity".into())
        })
    }

    /// Executes a rebalance swap.
    pub fn rebalance_swap(
        &mut self,
        msg_sender: Address,
        user_token: Address,
        validator_token: Address,
        amount_out: U256,
        to: Address,
    ) -> Result<U256> {
        if amount_out.is_zero() {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InvalidAmount {}.abi_encode().into(),
            ));
        }

        let pool_id = self.pool_id(user_token, validator_token);
        let mut pool = self.pools[pool_id].read()?;

        let amount_in = amount_out
            .checked_mul(N)
            .and_then(|product| product.checked_div(SCALE))
            .and_then(|result| result.checked_add(U256::from(1)))
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in rebalance_swap".into())
            })?;

        let amount_in_u128: u128 = amount_in.try_into().map_err(|_| {
            TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into())
        })?;
        let amount_out_u128: u128 = amount_out.try_into().map_err(|_| {
            TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into())
        })?;

        pool.reserve_validator_token = pool
            .reserve_validator_token
            .checked_add(amount_in_u128)
            .ok_or_else(|| {
                TempoPrecompileError::Revert(
                    ITIPFeeAMM::InsufficientReserves {}.abi_encode().into(),
                )
            })?;

        pool.reserve_user_token = pool.reserve_user_token.checked_sub(amount_out_u128).ok_or_else(
            || TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into()),
        )?;

        self.pools[pool_id].write(pool)?;

        // Transfer validator tokens from swapper into the pool
        TIP20Token::from_address(validator_token)?.system_transfer_from(
            msg_sender,
            self.address,
            amount_in,
        )?;

        // Transfer user tokens from pool to recipient
        TIP20Token::from_address(user_token)?.transfer(
            self.address,
            super::tip20::ITIP20::transferCall {
                to,
                amount: amount_out,
            },
        )?;

        self.emit_event(ITIPFeeAMM::RebalanceSwap {
            userToken: user_token,
            validatorToken: validator_token,
            swapper: msg_sender,
            amountIn: amount_in,
            amountOut: amount_out,
        })?;

        Ok(amount_in)
    }

    /// Mints LP tokens by depositing validator-token into a pool.
    pub fn mint(
        &mut self,
        msg_sender: Address,
        user_token: Address,
        validator_token: Address,
        amount_validator_token: U256,
        to: Address,
    ) -> Result<U256> {
        if user_token == validator_token {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::IdenticalAddresses {}.abi_encode().into(),
            ));
        }

        if amount_validator_token.is_zero() {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InvalidAmount {}.abi_encode().into(),
            ));
        }

        validate_usd_currency(user_token)?;
        validate_usd_currency(validator_token)?;

        let pool_id = self.pool_id(user_token, validator_token);
        let mut pool = self.pools[pool_id].read()?;
        let mut total_supply_val = self.total_supply[pool_id].read()?;

        let liquidity = if pool.reserve_user_token == 0 && pool.reserve_validator_token == 0 {
            let two = U256::from(2);
            let half_amount = amount_validator_token.checked_div(two).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in mint".into())
            })?;

            if half_amount <= MIN_LIQUIDITY {
                return Err(TempoPrecompileError::Revert(
                    ITIPFeeAMM::InsufficientLiquidity {}.abi_encode().into(),
                ));
            }

            total_supply_val = total_supply_val.checked_add(MIN_LIQUIDITY).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in mint".into())
            })?;
            self.total_supply[pool_id].write(total_supply_val)?;

            half_amount.checked_sub(MIN_LIQUIDITY).ok_or_else(|| {
                TempoPrecompileError::Revert(
                    ITIPFeeAMM::InsufficientLiquidity {}.abi_encode().into(),
                )
            })?
        } else {
            let product = N
                .checked_mul(U256::from(pool.reserve_user_token))
                .and_then(|p| p.checked_div(SCALE))
                .ok_or_else(|| {
                    TempoPrecompileError::Revert(
                        ITIPFeeAMM::InvalidSwapCalculation {}.abi_encode().into(),
                    )
                })?;

            let denom = U256::from(pool.reserve_validator_token)
                .checked_add(product)
                .ok_or_else(|| {
                    TempoPrecompileError::Revert(
                        ITIPFeeAMM::InvalidAmount {}.abi_encode().into(),
                    )
                })?;

            if denom.is_zero() {
                return Err(TempoPrecompileError::Revert(
                    ITIPFeeAMM::DivisionByZero {}.abi_encode().into(),
                ));
            }

            amount_validator_token
                .checked_mul(total_supply_val)
                .and_then(|n| n.checked_div(denom))
                .ok_or_else(|| {
                    TempoPrecompileError::Revert(
                        ITIPFeeAMM::InvalidSwapCalculation {}.abi_encode().into(),
                    )
                })?
        };

        if liquidity.is_zero() {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InsufficientLiquidity {}.abi_encode().into(),
            ));
        }

        // Transfer validator tokens from sender into the pool
        TIP20Token::from_address(validator_token)?.system_transfer_from(
            msg_sender,
            self.address,
            amount_validator_token,
        )?;

        let validator_amount: u128 = amount_validator_token.try_into().map_err(|_| {
            TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into())
        })?;

        pool.reserve_validator_token = pool
            .reserve_validator_token
            .checked_add(validator_amount)
            .ok_or_else(|| {
                TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into())
            })?;

        self.pools[pool_id].write(pool)?;

        self.total_supply[pool_id].write(
            total_supply_val.checked_add(liquidity).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in mint total_supply".into())
            })?,
        )?;

        let balance = self.liquidity_balances[pool_id][to].read()?;
        self.liquidity_balances[pool_id][to].write(
            balance.checked_add(liquidity).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in mint balance".into())
            })?,
        )?;

        self.emit_event(ITIPFeeAMM::Mint {
            sender: msg_sender,
            to,
            userToken: user_token,
            validatorToken: validator_token,
            amountValidatorToken: amount_validator_token,
            liquidity,
        })?;

        Ok(liquidity)
    }

    /// Burns LP tokens and returns the pro-rata share of both pool tokens.
    pub fn burn(
        &mut self,
        msg_sender: Address,
        user_token: Address,
        validator_token: Address,
        liquidity: U256,
        to: Address,
    ) -> Result<(U256, U256)> {
        if user_token == validator_token {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::IdenticalAddresses {}.abi_encode().into(),
            ));
        }

        if liquidity.is_zero() {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InvalidAmount {}.abi_encode().into(),
            ));
        }

        validate_usd_currency(user_token)?;
        validate_usd_currency(validator_token)?;

        let pool_id = self.pool_id(user_token, validator_token);
        let balance = self.liquidity_balances[pool_id][msg_sender].read()?;
        if balance < liquidity {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InsufficientLiquidity {}.abi_encode().into(),
            ));
        }

        let mut pool = self.pools[pool_id].read()?;
        let total_supply_val = self.total_supply[pool_id].read()?;

        let amount_user_token = liquidity
            .checked_mul(U256::from(pool.reserve_user_token))
            .and_then(|p| p.checked_div(total_supply_val))
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in burn amounts".into())
            })?;
        let amount_validator_token = liquidity
            .checked_mul(U256::from(pool.reserve_validator_token))
            .and_then(|p| p.checked_div(total_supply_val))
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in burn amounts".into())
            })?;

        // Update balances and supply
        self.liquidity_balances[pool_id][msg_sender].write(
            balance.checked_sub(liquidity).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in burn balance".into())
            })?,
        )?;
        self.total_supply[pool_id].write(
            total_supply_val.checked_sub(liquidity).ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in burn total_supply".into())
            })?,
        )?;

        // Update reserves
        let user_amount: u128 = amount_user_token.try_into().map_err(|_| {
            TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into())
        })?;
        let validator_amount: u128 = amount_validator_token.try_into().map_err(|_| {
            TempoPrecompileError::Revert(ITIPFeeAMM::InvalidAmount {}.abi_encode().into())
        })?;

        pool.reserve_user_token = pool.reserve_user_token.checked_sub(user_amount).ok_or_else(
            || {
                TempoPrecompileError::Revert(
                    ITIPFeeAMM::InsufficientReserves {}.abi_encode().into(),
                )
            },
        )?;
        pool.reserve_validator_token =
            pool.reserve_validator_token
                .checked_sub(validator_amount)
                .ok_or_else(|| {
                    TempoPrecompileError::Revert(
                        ITIPFeeAMM::InsufficientReserves {}.abi_encode().into(),
                    )
                })?;
        self.pools[pool_id].write(pool)?;

        // Transfer pool tokens to the burn recipient
        TIP20Token::from_address(user_token)?.transfer(
            self.address,
            super::tip20::ITIP20::transferCall {
                to,
                amount: amount_user_token,
            },
        )?;

        TIP20Token::from_address(validator_token)?.transfer(
            self.address,
            super::tip20::ITIP20::transferCall {
                to,
                amount: amount_validator_token,
            },
        )?;

        self.emit_event(ITIPFeeAMM::Burn {
            sender: msg_sender,
            userToken: user_token,
            validatorToken: validator_token,
            amountUserToken: amount_user_token,
            amountValidatorToken: amount_validator_token,
            liquidity,
            to,
        })?;

        Ok((amount_user_token, amount_validator_token))
    }

    /// Executes a fee swap, converting `user_token` to `validator_token` at fixed rate.
    pub fn execute_fee_swap(
        &mut self,
        user_token: Address,
        validator_token: Address,
        amount_in: U256,
    ) -> Result<U256> {
        let pool_id = self.pool_id(user_token, validator_token);
        let mut pool = self.pools[pool_id].read()?;

        let amount_out = compute_amount_out(amount_in)?;

        if amount_out > U256::from(pool.reserve_validator_token) {
            return Err(TempoPrecompileError::Revert(
                ITIPFeeAMM::InsufficientLiquidity {}.abi_encode().into(),
            ));
        }

        let amount_in_u128: u128 = amount_in.try_into().map_err(|_| {
            TempoPrecompileError::Fatal("overflow in execute_fee_swap".into())
        })?;
        let amount_out_u128: u128 = amount_out.try_into().map_err(|_| {
            TempoPrecompileError::Fatal("overflow in execute_fee_swap".into())
        })?;

        pool.reserve_user_token = pool
            .reserve_user_token
            .checked_add(amount_in_u128)
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in execute_fee_swap reserves".into())
            })?;
        pool.reserve_validator_token = pool
            .reserve_validator_token
            .checked_sub(amount_out_u128)
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in execute_fee_swap reserves".into())
            })?;

        self.pools[pool_id].write(pool)?;
        Ok(amount_out)
    }
}

impl ContractStorage for TipFeeManager {
    #[inline]
    fn address(&self) -> Address {
        self.address
    }

    #[inline]
    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

// ===========================================================================
// Helper: validate USD currency
// ===========================================================================

/// Validates that a TIP-20 token is USD-denominated.
pub fn validate_usd_currency(token: Address) -> Result<()> {
    let tip20 = TIP20Token::from_address(token)?;
    let currency = tip20.currency()?;
    if currency != "USD" {
        return Err(TempoPrecompileError::Revert(
            super::tip20::ITIP20::InvalidCurrency {}.abi_encode().into(),
        ));
    }
    Ok(())
}

// ===========================================================================
// Dispatch
// ===========================================================================

/// Unified calldata discriminant for both `IFeeManager` and `ITIPFeeAMM` selectors.
enum TipFeeManagerCall {
    FeeManager(IFeeManager::IFeeManagerCalls),
    Amm(ITIPFeeAMM::ITIPFeeAMMCalls),
}

impl TipFeeManagerCall {
    fn decode(calldata: &[u8]) -> core::result::Result<Self, alloy::sol_types::Error> {
        let selector: [u8; 4] = calldata[..4].try_into().expect("calldata len >= 4");

        if IFeeManager::IFeeManagerCalls::valid_selector(selector) {
            IFeeManager::IFeeManagerCalls::abi_decode(calldata).map(Self::FeeManager)
        } else {
            ITIPFeeAMM::ITIPFeeAMMCalls::abi_decode(calldata).map(Self::Amm)
        }
    }
}

/// Dispatches calldata, handling selector validation and ABI decode errors.

impl Precompile for TipFeeManager {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(calldata, TipFeeManagerCall::decode, |call| match call {
            // IFeeManager view functions
            TipFeeManagerCall::FeeManager(IFeeManager::IFeeManagerCalls::userTokens(call)) => {
                view(call, |c| self.user_tokens_view(c))
            }
            TipFeeManagerCall::FeeManager(IFeeManager::IFeeManagerCalls::validatorTokens(
                call,
            )) => view(call, |c| self.validator_tokens_view(c)),
            TipFeeManagerCall::FeeManager(IFeeManager::IFeeManagerCalls::collectedFees(call)) => {
                view(call, |c| self.collected_fees[c.validator][c.token].read())
            }

            // IFeeManager mutate functions
            TipFeeManagerCall::FeeManager(IFeeManager::IFeeManagerCalls::setValidatorToken(
                call,
            )) => mutate_void(call, msg_sender, |s, c| {
                let beneficiary = self.storage.beneficiary();
                self.set_validator_token(s, c, beneficiary)
            }),
            TipFeeManagerCall::FeeManager(IFeeManager::IFeeManagerCalls::setUserToken(call)) => {
                mutate_void(call, msg_sender, |s, c| self.set_user_token(s, c))
            }
            TipFeeManagerCall::FeeManager(IFeeManager::IFeeManagerCalls::distributeFees(
                call,
            )) => mutate_void(call, msg_sender, |_, c| {
                self.distribute_fees(c.validator, c.token)
            }),

            // ITIPFeeAMM metadata functions
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::M(_)) => {
                metadata::<ITIPFeeAMM::MCall>(|| Ok(M))
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::N(_)) => {
                metadata::<ITIPFeeAMM::NCall>(|| Ok(N))
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::SCALE(_)) => {
                metadata::<ITIPFeeAMM::SCALECall>(|| Ok(SCALE))
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::MIN_LIQUIDITY(_)) => {
                metadata::<ITIPFeeAMM::MIN_LIQUIDITYCall>(|| Ok(MIN_LIQUIDITY))
            }

            // ITIPFeeAMM view functions
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::getPoolId(call)) => {
                view(call, |c| Ok(self.pool_id(c.userToken, c.validatorToken)))
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::getPool(call)) => {
                view(call, |c| {
                    let pool = self.get_pool(c)?;
                    Ok(ITIPFeeAMM::Pool {
                        reserveUserToken: pool.reserve_user_token,
                        reserveValidatorToken: pool.reserve_validator_token,
                    })
                })
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::pools(call)) => {
                view(call, |c| {
                    let pool = self.pools[c.poolId].read()?;
                    Ok(ITIPFeeAMM::Pool {
                        reserveUserToken: pool.reserve_user_token,
                        reserveValidatorToken: pool.reserve_validator_token,
                    })
                })
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::totalSupply(call)) => {
                view(call, |c| self.total_supply[c.poolId].read())
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::liquidityBalances(call)) => {
                view(call, |c| self.liquidity_balances[c.poolId][c.user].read())
            }

            // ITIPFeeAMM mutate functions
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::mint(call)) => {
                mutate(call, msg_sender, |s, c| {
                    self.mint(s, c.userToken, c.validatorToken, c.amountValidatorToken, c.to)
                })
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::burn(call)) => {
                mutate(call, msg_sender, |s, c| {
                    let (amount_user_token, amount_validator_token) =
                        self.burn(s, c.userToken, c.validatorToken, c.liquidity, c.to)?;
                    Ok(ITIPFeeAMM::burnReturn {
                        amountUserToken: amount_user_token,
                        amountValidatorToken: amount_validator_token,
                    })
                })
            }
            TipFeeManagerCall::Amm(ITIPFeeAMM::ITIPFeeAMMCalls::rebalanceSwap(call)) => {
                mutate(call, msg_sender, |s, c| {
                    self.rebalance_swap(s, c.userToken, c.validatorToken, c.amountOut, c.to)
                })
            }
        })
    }
}
