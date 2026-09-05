#![allow(private_interfaces)]
//! On-chain CLOB (Central Limit Order Book) for stablecoin trading.
//!
//! Supports limit orders, market swaps, and flip orders across TIP-20 token pairs
//! with tick-based pricing and price-time priority.
//!
//! Ported from `tempo/crates/precompiles/src/stablecoin_dex/`.
//!
//! ## Storage layout
//!
//! | Slot | Field          | Type                                        |
//! |------|----------------|---------------------------------------------|
//! |  0   | books          | Mapping<B256, Orderbook>                    |
//! |  1   | orders         | Mapping<u128, Order>                        |
//! |  2   | balances       | Mapping<Address, Mapping<Address, u128>>    |
//! |  3   | next_order_id  | u128                                        |
//! |  4   | book_keys      | Vec<B256>                                   |
//! |  5   | dex_storage_credits | Mapping<Address, u64>                   |
//!
//! ## Cross-precompile dependencies
//!
//! - **TIP20Token**: transfer/transfer_from for escrow, ensure_transfer_authorized for policy checks
//! - **TIP20Factory**: is_tip20 validation for pair creation
//! - **TIP403Registry**: is_authorized_as for cancel_stale_order
//! - **FeeManager**: validate_usd_currency for pair creation
//!
//! Token transfers (transfer, transfer_from) delegate to TIP20 system_transfer_from.
//! View methods (balance_of, get_order, quote_swap_*) work correctly against on-chain state.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};
use std::{
    collections::HashSet,
    ops::{Deref, Index, IndexMut},
};

use super::error::{Result, TempoPrecompileError};
use super::fee_manager::validate_usd_currency;
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_credits::{StorageCreditDeltas, StorageCredits};
use super::storage_types::{
    Handler, HandlerCache, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType, StorageKey,
    VecHandler, packing,
};
use super::tip20::{TIP20Token, is_tip20_prefix};
use super::tip20_factory::TIP20Factory;
use super::tip403_registry::{AuthRole, TIP403Registry, is_policy_lookup_error};
use super::{
    PATH_USD_ADDRESS, Precompile, STABLECOIN_DEX_ADDRESS, dispatch_call, input_cost, mutate,
    mutate_void, unknown_selector, view,
};

// ===========================================================================
// Constants
// ===========================================================================

/// Minimum order size of $100 USD
pub const MIN_ORDER_AMOUNT: u128 = 100_000_000;

/// Allowed tick spacing for order placement
pub const TICK_SPACING: i16 = 10;

/// Minimum allowed tick value
pub const MIN_TICK: i16 = -2000;
/// Maximum allowed tick value
pub const MAX_TICK: i16 = 2000;
/// Scaling factor for tick-to-price conversion
pub const PRICE_SCALE: u32 = 100_000;

/// Lowest representable scaled price
const MIN_PRICE: u32 = 98_000;
/// Highest representable scaled price
const MAX_PRICE: u32 = 102_000;

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    interface IStablecoinDEX {
        function place(address token, uint128 amount, bool isBid, int16 tick) external returns (uint128);
        function placeFlip(address token, uint128 amount, bool isBid, int16 tick, int16 flipTick) external returns (uint128);
        function balanceOf(address user, address token) external view returns (uint128);
        function storageCredits(address user) external view returns (uint64 credits);
        function getOrder(uint128 orderId) external view returns (Order memory);
        function getTickLevel(address base, int16 tick, bool isBid) external view returns (uint128 head, uint128 tail, uint128 totalLiquidity);
        function pairKey(address tokenA, address tokenB) external view returns (bytes32);
        function books(bytes32 pairKey) external view returns (Orderbook memory);
        function nextOrderId() external view returns (uint128);
        function createPair(address base) external returns (bytes32);
        function withdraw(address token, uint128 amount) external;
        function cancel(uint128 orderId) external;
        function cancelStaleOrder(uint128 orderId) external;
        function swapExactAmountIn(address tokenIn, address tokenOut, uint128 amountIn, uint128 minAmountOut) external returns (uint128);
        function swapExactAmountOut(address tokenIn, address tokenOut, uint128 amountOut, uint128 maxAmountIn) external returns (uint128);
        function quoteSwapExactAmountIn(address tokenIn, address tokenOut, uint128 amountIn) external view returns (uint128);
        function quoteSwapExactAmountOut(address tokenIn, address tokenOut, uint128 amountOut) external view returns (uint128);
        function bookIndexForKey(bytes32 bookKey) external view returns (bool set, uint32 index);
        function bookKeyForIndex(uint32 index) external view returns (bytes32 bookKey);
        function setBookIndex(uint32 index) external;

        function MIN_TICK() external view returns (int16);
        function MAX_TICK() external view returns (int16);
        function TICK_SPACING() external view returns (int16);
        function PRICE_SCALE() external view returns (uint32);
        function MIN_ORDER_AMOUNT() external view returns (uint128);
        function MIN_PRICE() external view returns (uint32);
        function MAX_PRICE() external view returns (uint32);
        function tickToPrice(int16 tick) external view returns (uint32);
        function priceToTick(uint32 price) external view returns (int16);

        struct Order {
            uint128 orderId;
            address maker;
            bytes32 bookKey;
            bool isBid;
            int16 tick;
            uint128 amount;
            uint128 remaining;
            uint128 prev;
            uint128 next;
            bool isFlip;
            int16 flipTick;
        }

        struct Orderbook {
            address base;
            address quote;
            int16 bestBidTick;
            int16 bestAskTick;
        }

        struct PriceLevel {
            uint128 head;
            uint128 tail;
            uint128 totalLiquidity;
        }

        event OrderPlaced(uint128 orderId, address maker, address token, uint128 amount, bool isBid, int16 tick, bool isFlipOrder, int16 flipTick);
        event OrderFilled(uint128 orderId, address maker, address taker, uint128 amountFilled, bool partialFill);
        event OrderCancelled(uint128 orderId);
        event PairCreated(bytes32 key, address base, address quote);
        event OrderFlipped(uint128 indexed orderId, address indexed maker, address indexed token, uint128 amount, bool isBid, int16 tick, int16 flipTick);
        event FlipFailed(uint128 indexed orderId, address indexed maker, bytes4 reason);

        error OrderDoesNotExist();
        error Unauthorized();
        error InsufficientBalance();
        error InsufficientLiquidity();
        error InsufficientOutput();
        error MaxInputExceeded();
        error InvalidBaseToken();
        error InvalidToken();
        error InvalidCurrency();
        error IdenticalTokens();
        error PairAlreadyExists();
        error PairDoesNotExist();
        error TickOutOfBounds(int16 tick);
        error InvalidTick();
        error InvalidFlipTick();
        error BelowMinimumOrderSize(uint128 amount);
        error OrderNotStale();
        error IndexAlreadySet();
    }
}

// ===========================================================================
// Error helpers
// ===========================================================================

fn err_order_does_not_exist() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::OrderDoesNotExist {}.abi_encode().into())
}

fn err_unauthorized() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::Unauthorized {}.abi_encode().into())
}

fn err_insufficient_balance() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InsufficientBalance {}.abi_encode().into())
}

fn err_insufficient_liquidity() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InsufficientLiquidity {}.abi_encode().into())
}

fn err_insufficient_output() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InsufficientOutput {}.abi_encode().into())
}

fn err_max_input_exceeded() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::MaxInputExceeded {}.abi_encode().into())
}

fn err_invalid_base_token() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InvalidBaseToken {}.abi_encode().into())
}

fn err_invalid_token() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InvalidToken {}.abi_encode().into())
}

fn err_pair_already_exists() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::PairAlreadyExists {}.abi_encode().into())
}

fn err_pair_does_not_exist() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::PairDoesNotExist {}.abi_encode().into())
}

fn err_tick_out_of_bounds(tick: i16) -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::TickOutOfBounds { tick }.abi_encode().into())
}

fn err_invalid_tick() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InvalidTick {}.abi_encode().into())
}

fn err_invalid_flip_tick() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::InvalidFlipTick {}.abi_encode().into())
}

fn err_below_minimum_order_size(amount: u128) -> TempoPrecompileError {
    TempoPrecompileError::Revert(
        IStablecoinDEX::BelowMinimumOrderSize { amount }
            .abi_encode()
            .into(),
    )
}

fn err_identical_tokens() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::IdenticalTokens {}.abi_encode().into())
}

fn err_order_not_stale() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::OrderNotStale {}.abi_encode().into())
}

fn err_index_already_set() -> TempoPrecompileError {
    TempoPrecompileError::Revert(IStablecoinDEX::IndexAlreadySet {}.abi_encode().into())
}

// ===========================================================================
// Price/tick helpers
// ===========================================================================

/// Rounding direction for price conversions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundingDirection {
    Down,
    Up,
}

/// Convert base token amount to quote token amount at a given tick.
fn base_to_quote(base_amount: u128, tick: i16, rounding: RoundingDirection) -> Option<u128> {
    let price = U256::from(tick_to_price(tick));
    let base = U256::from(base_amount);
    let scale = U256::from(PRICE_SCALE);
    let numerator = base * price;
    let result = match rounding {
        RoundingDirection::Down => numerator / scale,
        RoundingDirection::Up => numerator.div_ceil(scale),
    };
    result.try_into().ok()
}

/// Convert quote token amount to base token amount at a given tick.
fn quote_to_base(quote_amount: u128, tick: i16, rounding: RoundingDirection) -> Option<u128> {
    let price = U256::from(tick_to_price(tick));
    let quote = U256::from(quote_amount);
    let scale = U256::from(PRICE_SCALE);
    let numerator = quote * scale;
    let result = match rounding {
        RoundingDirection::Down => numerator / price,
        RoundingDirection::Up => numerator.div_ceil(price),
    };
    result.try_into().ok()
}

/// Convert relative tick to scaled price.
fn tick_to_price(tick: i16) -> u32 {
    (PRICE_SCALE as i32 + tick as i32) as u32
}

/// Convert scaled price to relative tick.
fn price_to_tick(price: u32) -> Result<i16> {
    if !(MIN_PRICE..=MAX_PRICE).contains(&price) {
        let invalid_tick = (price as i32 - PRICE_SCALE as i32) as i16;
        return Err(err_tick_out_of_bounds(invalid_tick));
    }
    Ok((price as i32 - PRICE_SCALE as i32) as i16)
}

/// Validates tick spacing alignment.
fn validate_tick_spacing(tick: i16) -> Result<()> {
    if tick % TICK_SPACING != 0 {
        return Err(err_invalid_tick());
    }
    Ok(())
}

/// Compute deterministic book key from ordered (base, quote) token pair.
fn compute_book_key(base: Address, quote: Address) -> B256 {
    let mut buf = [0u8; 40];
    buf[..20].copy_from_slice(base.as_slice());
    buf[20..].copy_from_slice(quote.as_slice());
    keccak256(buf)
}

// ===========================================================================
// TickLevel storage type
// ===========================================================================

/// A price level in the orderbook with a doubly-linked list of orders.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickLevel {
    head: u128,
    tail: u128,
    total_liquidity: u128,
}

impl TickLevel {
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.head == 0 && self.tail == 0
    }
}

impl StorableType for TickLevel {
    // 3 x u128 = 48 bytes = 2 slots (first slot: head+tail packed, second: total_liquidity)
    // Actually Tempo packs u128 at 16 bytes each, so 3 x 16 = 48 bytes = 2 slots
    const LAYOUT: Layout = Layout::Slots(2);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for TickLevel {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        // Slot+0: head (u128 at offset 0, bytes 16..32) + tail (u128 at offset 16, bytes 0..16)
        let word0 = storage.load(slot)?;
        let bytes0 = word0.to_be_bytes::<32>();
        let head = u128::from_be_bytes(bytes0[16..32].try_into().unwrap());
        let tail = u128::from_be_bytes(bytes0[0..16].try_into().unwrap());

        // Slot+1: total_liquidity (u128 at offset 0, bytes 16..32)
        let word1 = storage.load(slot + U256::from(1))?;
        let bytes1 = word1.to_be_bytes::<32>();
        let total_liquidity = u128::from_be_bytes(bytes1[16..32].try_into().unwrap());

        Ok(Self {
            head,
            tail,
            total_liquidity,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes0 = [0u8; 32];
        bytes0[16..32].copy_from_slice(&self.head.to_be_bytes());
        bytes0[0..16].copy_from_slice(&self.tail.to_be_bytes());
        storage.store(slot, U256::from_be_bytes(bytes0))?;

        let mut bytes1 = [0u8; 32];
        bytes1[16..32].copy_from_slice(&self.total_liquidity.to_be_bytes());
        storage.store(slot + U256::from(1), U256::from_be_bytes(bytes1))?;

        Ok(())
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, U256::ZERO)?;
        storage.store(slot + U256::from(1), U256::ZERO)?;
        Ok(())
    }
}

// ===========================================================================
// Order storage type
// ===========================================================================

/// An order in the CLOB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Order {
    order_id: u128,
    maker: Address,
    book_key: B256,
    is_bid: bool,
    tick: i16,
    amount: u128,
    remaining: u128,
    prev: u128,
    next: u128,
    is_flip: bool,
    flip_tick: i16,
}

impl Default for Order {
    fn default() -> Self {
        Self {
            order_id: 0,
            maker: Address::ZERO,
            book_key: B256::ZERO,
            is_bid: false,
            tick: 0,
            amount: 0,
            remaining: 0,
            prev: 0,
            next: 0,
            is_flip: false,
            flip_tick: 0,
        }
    }
}

impl Order {
    fn new_bid(order_id: u128, maker: Address, book_key: B256, amount: u128, tick: i16) -> Self {
        Self {
            order_id,
            maker,
            book_key,
            is_bid: true,
            tick,
            amount,
            remaining: amount,
            prev: 0,
            next: 0,
            is_flip: false,
            flip_tick: 0,
        }
    }

    fn new_ask(order_id: u128, maker: Address, book_key: B256, amount: u128, tick: i16) -> Self {
        Self {
            order_id,
            maker,
            book_key,
            is_bid: false,
            tick,
            amount,
            remaining: amount,
            prev: 0,
            next: 0,
            is_flip: false,
            flip_tick: 0,
        }
    }

    fn new_flip(
        order_id: u128,
        maker: Address,
        book_key: B256,
        amount: u128,
        tick: i16,
        is_bid: bool,
        flip_tick: i16,
        hardfork: crate::tempo::hardfork::TempoHardfork,
    ) -> Result<Self> {
        let invalid = if is_bid {
            flip_tick < tick || (!hardfork.is_t5() && flip_tick == tick)
        } else {
            flip_tick > tick || (!hardfork.is_t5() && flip_tick == tick)
        };
        if invalid {
            return Err(err_invalid_flip_tick());
        }
        Ok(Self {
            order_id,
            maker,
            book_key,
            is_bid,
            tick,
            amount,
            remaining: amount,
            prev: 0,
            next: 0,
            is_flip: true,
            flip_tick,
        })
    }

    fn create_flipped_order(&self, new_order_id: u128) -> Self {
        debug_assert!(self.is_flip);
        Self {
            order_id: new_order_id,
            maker: self.maker,
            book_key: self.book_key,
            is_bid: !self.is_bid,
            tick: self.flip_tick,
            amount: self.amount,
            remaining: self.amount,
            prev: 0,
            next: 0,
            is_flip: true,
            flip_tick: self.tick,
        }
    }
}

impl StorableType for Order {
    // Order has 11 fields. Tempo #[derive(Storable)] packs them as:
    // slot 0: order_id (u128, 16 bytes) + maker offset packed = actually...
    // The Storable macro packs: u128(16) + Address(20) = 36 bytes -> 2 slots
    // Actually with Tempo packing: slot0 = order_id(u128@0) + maker bottom part...
    // Let's use 5 slots as the macro would produce for this set of fields.
    //
    // Slot 0: order_id(u128, 16b) -- fills low 16 bytes
    // Slot 1: maker(Address, 20b) -- fills low 20 bytes
    // Slot 2: book_key(B256, 32b) -- full slot
    // Slot 3: is_bid(bool,1b) + tick(i16,2b) + amount(u128,16b) -- packed (19 bytes)
    // Slot 4: remaining(u128, 16b) -- 16 bytes
    // Slot 5: prev(u128, 16b) -- 16 bytes
    // Slot 6: next(u128, 16b) -- 16 bytes
    // Slot 7: is_flip(bool,1b) + flip_tick(i16,2b) -- packed (3 bytes)
    //
    // Actually the Tempo derive(Storable) packs adjacent small fields and puts
    // each u128 in its own slot since u128 = 16 bytes (half a slot).
    // The packing algorithm is: accumulate bytes until >= 32, then start new slot.
    //
    // Fields: u128(16) | Address(20) | B256(32) | bool(1) + i16(2) | u128(16) | u128(16) | u128(16) | u128(16) | bool(1) + i16(2)
    // Packing:
    //   slot 0: order_id(16) + maker starts but 16+20=36 > 32, so order_id alone = needs own slot? No...
    //   Actually u128 is 16 bytes so it's packable. 16+20 = 36 > 32, so:
    //   slot 0: order_id (u128, 16 bytes, offset 0)
    //   slot 0 cont: can't fit Address(20), start new slot
    //   slot 1: maker (Address, 20 bytes, offset 0)
    //   slot 1 cont: can't fit B256(32), start new slot
    //   slot 2: book_key (B256, 32 bytes) - full slot
    //   slot 3: is_bid(1) + tick(2) + amount starts: 1+2+16=19 < 32, pack all
    //   slot 3: is_bid(1) + tick(2) + amount(16) = 19 bytes packed
    //   slot 3 cont: remaining(16): 19+16=35 > 32, start new slot
    //   slot 4: remaining(16)
    //   slot 4 cont: prev(16): 16+16=32 = 32, pack both
    //   slot 4: remaining(16) + prev(16) = 32 bytes packed
    //   slot 5: next(16)
    //   slot 5 cont: is_flip(1) + flip_tick(2): 16+1+2=19 < 32, pack
    //   slot 5: next(16) + is_flip(1) + flip_tick(2) = 19 bytes packed
    //
    // Total: 6 slots
    const LAYOUT: Layout = Layout::Slots(6);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for Order {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        // Slot 0: order_id (u128 at offset 0, bytes 16..32)
        let w0 = storage.load(slot)?;
        let b0 = w0.to_be_bytes::<32>();
        let order_id = u128::from_be_bytes(b0[16..32].try_into().unwrap());

        // Slot 1: maker (Address at offset 0, bytes 12..32)
        let w1 = storage.load(slot + U256::from(1))?;
        let b1 = w1.to_be_bytes::<32>();
        let maker = Address::from_slice(&b1[12..32]);

        // Slot 2: book_key (B256, full slot)
        let w2 = storage.load(slot + U256::from(2))?;
        let book_key = B256::from(w2.to_be_bytes::<32>());

        // Slot 3: is_bid(1) + tick(2) + amount(16) = 19 bytes packed from offset 0
        let w3 = storage.load(slot + U256::from(3))?;
        let b3 = w3.to_be_bytes::<32>();
        let is_bid = b3[31] != 0;
        let tick = i16::from_be_bytes(b3[29..31].try_into().unwrap());
        let amount = u128::from_be_bytes(b3[13..29].try_into().unwrap());

        // Slot 4: remaining(16) + prev(16) = 32 bytes packed
        let w4 = storage.load(slot + U256::from(4))?;
        let b4 = w4.to_be_bytes::<32>();
        let remaining = u128::from_be_bytes(b4[16..32].try_into().unwrap());
        let prev = u128::from_be_bytes(b4[0..16].try_into().unwrap());

        // Slot 5: next(16) + is_flip(1) + flip_tick(2) = 19 bytes packed
        let w5 = storage.load(slot + U256::from(5))?;
        let b5 = w5.to_be_bytes::<32>();
        let next = u128::from_be_bytes(b5[16..32].try_into().unwrap());
        let is_flip = b5[15] != 0;
        let flip_tick = i16::from_be_bytes(b5[13..15].try_into().unwrap());

        Ok(Self {
            order_id,
            maker,
            book_key,
            is_bid,
            tick,
            amount,
            remaining,
            prev,
            next,
            is_flip,
            flip_tick,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        // Slot 0: order_id
        let mut b0 = [0u8; 32];
        b0[16..32].copy_from_slice(&self.order_id.to_be_bytes());
        storage.store(slot, U256::from_be_bytes(b0))?;

        // Slot 1: maker
        let mut b1 = [0u8; 32];
        b1[12..32].copy_from_slice(self.maker.as_slice());
        storage.store(slot + U256::from(1), U256::from_be_bytes(b1))?;

        // Slot 2: book_key
        storage.store(slot + U256::from(2), U256::from_be_bytes(self.book_key.0))?;

        // Slot 3: is_bid + tick + amount
        let mut b3 = [0u8; 32];
        b3[31] = if self.is_bid { 1 } else { 0 };
        b3[29..31].copy_from_slice(&self.tick.to_be_bytes());
        b3[13..29].copy_from_slice(&self.amount.to_be_bytes());
        storage.store(slot + U256::from(3), U256::from_be_bytes(b3))?;

        // Slot 4: remaining + prev
        let mut b4 = [0u8; 32];
        b4[16..32].copy_from_slice(&self.remaining.to_be_bytes());
        b4[0..16].copy_from_slice(&self.prev.to_be_bytes());
        storage.store(slot + U256::from(4), U256::from_be_bytes(b4))?;

        // Slot 5: next + is_flip + flip_tick
        let mut b5 = [0u8; 32];
        b5[16..32].copy_from_slice(&self.next.to_be_bytes());
        b5[15] = if self.is_flip { 1 } else { 0 };
        b5[13..15].copy_from_slice(&self.flip_tick.to_be_bytes());
        storage.store(slot + U256::from(5), U256::from_be_bytes(b5))?;

        Ok(())
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        for i in 0..6 {
            storage.store(slot + U256::from(i), U256::ZERO)?;
        }
        Ok(())
    }
}

/// Physical storage layout used by a DEX order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum OrderVersion {
    Legacy = 0,
    V1 = 1,
    V2 = 2,
}

impl TryFrom<U256> for OrderVersion {
    type Error = TempoPrecompileError;

    fn try_from(slot0: U256) -> Result<Self> {
        match packing::extract_from_word::<u8>(slot0, 31, 1)? {
            0 => Ok(Self::Legacy),
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            version => Err(TempoPrecompileError::Fatal(format!(
                "unknown stablecoin DEX order storage version {version}"
            ))),
        }
    }
}

struct OrderFlags;

impl OrderFlags {
    const IS_BID: u8 = 1;
    const IS_FLIP: u8 = 1 << 1;

    fn pack(order: &Order) -> u8 {
        u8::from(order.is_bid) * Self::IS_BID | u8::from(order.is_flip) * Self::IS_FLIP
    }

    fn is_bid(metadata: u8) -> bool {
        metadata & Self::IS_BID != 0
    }

    fn is_flip(metadata: u8) -> bool {
        metadata & Self::IS_FLIP != 0
    }
}

const LEGACY_MAKER_LOC: packing::FieldLocation = packing::FieldLocation::new(1, 0, 20);
const LEGACY_REMAINING_LOC: packing::FieldLocation = packing::FieldLocation::new(4, 0, 16);
const LEGACY_PREV_LOC: packing::FieldLocation = packing::FieldLocation::new(4, 16, 16);
const LEGACY_NEXT_LOC: packing::FieldLocation = packing::FieldLocation::new(5, 0, 16);
const COMPACT_MAKER_LOC: packing::FieldLocation = packing::FieldLocation::new(0, 0, 20);
const COMPACT_REMAINING_LOC: packing::FieldLocation = packing::FieldLocation::new(1, 16, 16);
const COMPACT_PREV_LOC: packing::FieldLocation = packing::FieldLocation::new(2, 0, 16);
const COMPACT_NEXT_LOC: packing::FieldLocation = packing::FieldLocation::new(2, 16, 16);

fn pack_field<T: super::storage_types::FromWord + StorableType>(
    word: U256,
    value: &T,
    offset: usize,
) -> Result<U256> {
    packing::insert_into_word(word, value, offset, T::BYTES)
}

fn unpack_field<T: super::storage_types::FromWord + StorableType>(
    word: U256,
    offset: usize,
) -> Result<T> {
    packing::extract_from_word(word, offset, T::BYTES)
}

/// T8 TIP-1062 compact layout. The order ID is recovered from the mapping key.
#[derive(Debug, Clone)]
struct V1Order {
    maker: Address,
    metadata: u8,
    tick: i16,
    flip_tick: i16,
    amount: u128,
    remaining: u128,
    prev: u128,
    next: u128,
    book_key: B256,
}

impl V1Order {
    const SLOTS: usize = 4;

    fn from_order(order: Order) -> Self {
        Self {
            maker: order.maker,
            metadata: OrderFlags::pack(&order),
            tick: order.tick,
            flip_tick: order.flip_tick,
            amount: order.amount,
            remaining: order.remaining,
            prev: order.prev,
            next: order.next,
            book_key: order.book_key,
        }
    }

    fn into_order(self, order_id: u128) -> Order {
        Order {
            order_id,
            maker: self.maker,
            book_key: self.book_key,
            is_bid: OrderFlags::is_bid(self.metadata),
            tick: self.tick,
            amount: self.amount,
            remaining: self.remaining,
            prev: self.prev,
            next: self.next,
            is_flip: OrderFlags::is_flip(self.metadata),
            flip_tick: self.flip_tick,
        }
    }

    fn load<S: StorageOps>(storage: &S, slot: U256) -> Result<Self> {
        let word0 = storage.load(slot)?;
        let word1 = storage.load(slot + U256::from(1))?;
        let word2 = storage.load(slot + U256::from(2))?;
        let word3 = storage.load(slot + U256::from(3))?;
        Ok(Self {
            maker: unpack_field(word0, 0)?,
            metadata: unpack_field(word0, 20)?,
            tick: unpack_field(word0, 21)?,
            flip_tick: unpack_field(word0, 23)?,
            amount: unpack_field(word1, 0)?,
            remaining: unpack_field(word1, 16)?,
            prev: unpack_field(word2, 0)?,
            next: unpack_field(word2, 16)?,
            book_key: B256::from(word3.to_be_bytes::<32>()),
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256) -> Result<()> {
        let mut word0 = U256::ZERO;
        word0 = pack_field(word0, &self.maker, 0)?;
        word0 = pack_field(word0, &self.metadata, 20)?;
        word0 = pack_field(word0, &self.tick, 21)?;
        word0 = pack_field(word0, &self.flip_tick, 23)?;
        word0 = pack_field(word0, &(OrderVersion::V1 as u8), 31)?;
        storage.store(slot, word0)?;

        let mut word1 = U256::ZERO;
        word1 = pack_field(word1, &self.amount, 0)?;
        word1 = pack_field(word1, &self.remaining, 16)?;
        storage.store(slot + U256::from(1), word1)?;

        let mut word2 = U256::ZERO;
        word2 = pack_field(word2, &self.prev, 0)?;
        word2 = pack_field(word2, &self.next, 16)?;
        storage.store(slot + U256::from(2), word2)?;
        storage.store(slot + U256::from(3), U256::from_be_bytes(self.book_key.0))
    }
}

/// T8 TIP-1087 compact layout. The book key is recovered through `book_keys`.
#[derive(Debug, Clone)]
struct V2Order {
    maker: Address,
    metadata: u8,
    tick: i16,
    flip_tick: i16,
    book_index: u32,
    amount: u128,
    remaining: u128,
    prev: u128,
    next: u128,
}

impl V2Order {
    const SLOTS: usize = 3;

    fn from_order(order: Order, book_index: u32) -> Self {
        Self {
            maker: order.maker,
            metadata: OrderFlags::pack(&order),
            tick: order.tick,
            flip_tick: order.flip_tick,
            book_index,
            amount: order.amount,
            remaining: order.remaining,
            prev: order.prev,
            next: order.next,
        }
    }

    fn into_order(self, order_id: u128, book_key: B256) -> Order {
        Order {
            order_id,
            maker: self.maker,
            book_key,
            is_bid: OrderFlags::is_bid(self.metadata),
            tick: self.tick,
            amount: self.amount,
            remaining: self.remaining,
            prev: self.prev,
            next: self.next,
            is_flip: OrderFlags::is_flip(self.metadata),
            flip_tick: self.flip_tick,
        }
    }

    fn load<S: StorageOps>(storage: &S, slot: U256) -> Result<Self> {
        let word0 = storage.load(slot)?;
        let word1 = storage.load(slot + U256::from(1))?;
        let word2 = storage.load(slot + U256::from(2))?;
        Ok(Self {
            maker: unpack_field(word0, 0)?,
            metadata: unpack_field(word0, 20)?,
            tick: unpack_field(word0, 21)?,
            flip_tick: unpack_field(word0, 23)?,
            book_index: unpack_field(word0, 25)?,
            amount: unpack_field(word1, 0)?,
            remaining: unpack_field(word1, 16)?,
            prev: unpack_field(word2, 0)?,
            next: unpack_field(word2, 16)?,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256) -> Result<()> {
        let mut word0 = U256::ZERO;
        word0 = pack_field(word0, &self.maker, 0)?;
        word0 = pack_field(word0, &self.metadata, 20)?;
        word0 = pack_field(word0, &self.tick, 21)?;
        word0 = pack_field(word0, &self.flip_tick, 23)?;
        word0 = pack_field(word0, &self.book_index, 25)?;
        word0 = pack_field(word0, &(OrderVersion::V2 as u8), 31)?;
        storage.store(slot, word0)?;

        let mut word1 = U256::ZERO;
        word1 = pack_field(word1, &self.amount, 0)?;
        word1 = pack_field(word1, &self.remaining, 16)?;
        storage.store(slot + U256::from(1), word1)?;

        let mut word2 = U256::ZERO;
        word2 = pack_field(word2, &self.prev, 0)?;
        word2 = pack_field(word2, &self.next, 16)?;
        storage.store(slot + U256::from(2), word2)
    }
}

/// One-based orderbook ID; zero indicates a pre-T8 book without an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BookId(u32);

impl BookId {
    const UNSET: Self = Self(0);

    fn from_index(index: u32) -> Self {
        Self(index + 1)
    }

    fn index(self) -> Option<u32> {
        self.0.checked_sub(1)
    }
}

impl Deref for BookId {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Version-aware storage handler for one DEX order.
#[derive(Debug, Clone)]
struct OrderHandler {
    base_slot: U256,
    order_id: u128,
    address: Address,
}

impl OrderHandler {
    fn new(base_slot: U256, order_id: u128, address: Address) -> Self {
        Self {
            base_slot,
            order_id,
            address,
        }
    }

    fn version_and_slot(&self) -> Result<(OrderVersion, Option<U256>)> {
        if !StorageCtx.spec().is_t8() {
            return Ok((OrderVersion::Legacy, None));
        }
        let slot0 = self.load(self.base_slot)?;
        Ok((OrderVersion::try_from(slot0)?, Some(slot0)))
    }

    fn version(&self) -> Result<OrderVersion> {
        self.version_and_slot().map(|(version, _)| version)
    }

    fn maker(&self) -> Result<Address> {
        let (version, slot0) = self.version_and_slot()?;
        let loc = match version {
            OrderVersion::Legacy => LEGACY_MAKER_LOC,
            OrderVersion::V1 | OrderVersion::V2 => COMPACT_MAKER_LOC,
        };
        if loc.offset_slots == 0 {
            if let Some(slot0) = slot0 {
                return unpack_field(slot0, loc.offset_bytes);
            }
        }
        Slot::new_at_loc(self.base_slot, loc, self.address).read()
    }

    fn remaining(&self) -> Result<Slot<u128>> {
        self.u128_field(LEGACY_REMAINING_LOC, COMPACT_REMAINING_LOC)
    }

    fn prev(&self) -> Result<Slot<u128>> {
        self.u128_field(LEGACY_PREV_LOC, COMPACT_PREV_LOC)
    }

    fn next(&self) -> Result<Slot<u128>> {
        self.u128_field(LEGACY_NEXT_LOC, COMPACT_NEXT_LOC)
    }

    fn u128_field(
        &self,
        legacy: packing::FieldLocation,
        compact: packing::FieldLocation,
    ) -> Result<Slot<u128>> {
        let loc = match self.version()? {
            OrderVersion::Legacy => legacy,
            OrderVersion::V1 | OrderVersion::V2 => compact,
        };
        Ok(Slot::new_at_loc(self.base_slot, loc, self.address))
    }

    fn read_in_book(&self, book_key: B256) -> Result<Order> {
        self.read_with_book_key(Some(book_key))
    }

    fn read_with_book_key(&self, known_book: Option<B256>) -> Result<Order> {
        match self.version()? {
            OrderVersion::Legacy => Order::load(self, self.base_slot, LayoutCtx::FULL),
            OrderVersion::V1 => {
                V1Order::load(self, self.base_slot).map(|order| order.into_order(self.order_id))
            }
            OrderVersion::V2 => {
                let order = V2Order::load(self, self.base_slot)?;
                let book_key = match known_book {
                    Some(book_key) => book_key,
                    None => StablecoinDEX::new().book_key_for_index(order.book_index)?,
                };
                Ok(order.into_order(self.order_id, book_key))
            }
        }
    }

    fn write_in_book(&mut self, value: Order, book_id: BookId) -> Result<()> {
        self.write_with_book_id(value, Some(book_id))
    }

    fn write_with_book_id(&mut self, value: Order, known_id: Option<BookId>) -> Result<()> {
        debug_assert_eq!(value.order_id, self.order_id);
        if !StorageCtx.spec().is_t8() {
            return value.store(self, self.base_slot, LayoutCtx::FULL);
        }

        let (old_version, slot0) = self.version_and_slot()?;
        let old_slots = match old_version {
            OrderVersion::Legacy => Order::SLOTS,
            OrderVersion::V1 => V1Order::SLOTS,
            OrderVersion::V2 => V2Order::SLOTS,
        };
        let book_index = match known_id {
            Some(id) => id.index(),
            None => StablecoinDEX::new().book_key_index(value.book_key)?,
        };
        let new_slots = if let Some(book_index) = book_index {
            V2Order::from_order(value, book_index).store(self, self.base_slot)?;
            V2Order::SLOTS
        } else {
            V1Order::from_order(value).store(self, self.base_slot)?;
            V1Order::SLOTS
        };

        if slot0.is_none_or(|word| !word.is_zero()) {
            for offset in new_slots..old_slots {
                self.store(self.base_slot + U256::from(offset), U256::ZERO)?;
            }
        }
        Ok(())
    }
}

impl StorageOps for OrderHandler {
    fn load(&self, slot: U256) -> Result<U256> {
        StorageCtx.sload(self.address, slot)
    }

    fn store(&mut self, slot: U256, value: U256) -> Result<()> {
        StorageCtx.sstore(self.address, slot, value)
    }
}

impl Handler<Order> for OrderHandler {
    fn read(&self) -> Result<Order> {
        self.read_with_book_key(None)
    }

    fn write(&mut self, value: Order) -> Result<()> {
        self.write_with_book_id(value, None)
    }

    fn delete(&mut self) -> Result<()> {
        let slots = match self.version()? {
            OrderVersion::Legacy => Order::SLOTS,
            OrderVersion::V1 => V1Order::SLOTS,
            OrderVersion::V2 => V2Order::SLOTS,
        };
        for offset in 0..slots {
            self.store(self.base_slot + U256::from(offset), U256::ZERO)?;
        }
        Ok(())
    }

    fn t_read(&self) -> Result<Order> {
        Err(TempoPrecompileError::Fatal(
            "transient order storage is unsupported".to_string(),
        ))
    }

    fn t_write(&mut self, _value: Order) -> Result<()> {
        Err(TempoPrecompileError::Fatal(
            "transient order storage is unsupported".to_string(),
        ))
    }

    fn t_delete(&mut self) -> Result<()> {
        Err(TempoPrecompileError::Fatal(
            "transient order storage is unsupported".to_string(),
        ))
    }
}

#[derive(Debug)]
struct OrderMapping {
    cache: HandlerCache<u128, OrderHandler>,
}

impl OrderMapping {
    fn new() -> Self {
        Self {
            cache: HandlerCache::new(),
        }
    }

    fn at(&self, order_id: u128) -> &OrderHandler {
        self.cache.get_or_insert(&order_id, || {
            OrderHandler::new(
                order_id.mapping_slot(U256::from(1)),
                order_id,
                STABLECOIN_DEX_ADDRESS,
            )
        })
    }

    fn at_mut(&mut self, order_id: u128) -> &mut OrderHandler {
        self.cache.get_or_insert_mut(&order_id, || {
            OrderHandler::new(
                order_id.mapping_slot(U256::from(1)),
                order_id,
                STABLECOIN_DEX_ADDRESS,
            )
        })
    }
}

impl Index<u128> for OrderMapping {
    type Output = OrderHandler;

    fn index(&self, order_id: u128) -> &Self::Output {
        self.at(order_id)
    }
}

impl IndexMut<u128> for OrderMapping {
    fn index_mut(&mut self, order_id: u128) -> &mut Self::Output {
        self.at_mut(order_id)
    }
}

impl Clone for OrderMapping {
    fn clone(&self) -> Self {
        Self::new()
    }
}

// ===========================================================================
// Orderbook storage type
// ===========================================================================

/// Orderbook for a token pair with tick bitmaps for price discovery.
///
/// Storage layout (Storable):
///   - slot+0: base (Address, 20 bytes)
///   - slot+1: quote (Address, 20 bytes)
///   - slot+2: bids (Mapping<i16, TickLevel>)
///   - slot+3: asks (Mapping<i16, TickLevel>)
///   - slot+4: best_bid_tick(i16) + best_ask_tick(i16) packed (4 bytes)
///   - slot+5: bid_bitmap (Mapping<i16, U256>)
///   - slot+6: ask_bitmap (Mapping<i16, U256>)
#[derive(Debug, Clone)]
pub(crate) struct OrderbookData {
    base: Address,
    quote: Address,
    best_bid_tick: i16,
    best_ask_tick: i16,
    book_id: u32,
}

impl Default for OrderbookData {
    fn default() -> Self {
        Self {
            base: Address::ZERO,
            quote: Address::ZERO,
            best_bid_tick: i16::MIN,
            best_ask_tick: i16::MAX,
            book_id: *BookId::UNSET,
        }
    }
}

impl OrderbookData {
    fn new(base: Address, quote: Address) -> Self {
        Self {
            base,
            quote,
            best_bid_tick: i16::MIN,
            best_ask_tick: i16::MAX,
            book_id: *BookId::UNSET,
        }
    }

    fn new_with_index(base: Address, quote: Address, index: u32) -> Self {
        Self {
            book_id: *BookId::from_index(index),
            ..Self::new(base, quote)
        }
    }

    fn id(&self) -> BookId {
        BookId(self.book_id)
    }

    fn is_initialized(&self) -> bool {
        self.base != Address::ZERO
    }
}

/// Full orderbook handler with mappings.
struct OrderbookHandle {
    slot: U256,
    address: Address,
    bids: Mapping<i16, TickLevel>,
    asks: Mapping<i16, TickLevel>,
    bid_bitmap: Mapping<i16, U256>,
    ask_bitmap: Mapping<i16, U256>,
}

impl OrderbookHandle {
    fn new(slot: U256, address: Address) -> Self {
        Self {
            slot,
            address,
            bids: Mapping::new(slot + U256::from(2), address),
            asks: Mapping::new(slot + U256::from(3), address),
            bid_bitmap: Mapping::new(slot + U256::from(5), address),
            ask_bitmap: Mapping::new(slot + U256::from(6), address),
        }
    }

    /// Reads the base orderbook data (base, quote, best_bid_tick, best_ask_tick).
    fn read_data(&self) -> Result<OrderbookData> {
        let ctx = StorageCtx::default();

        // slot+0: base
        let w0 = ctx.sload(self.address, self.slot)?;
        let b0 = w0.to_be_bytes::<32>();
        let base = Address::from_slice(&b0[12..32]);

        // slot+1: quote
        let w1 = ctx.sload(self.address, self.slot + U256::from(1))?;
        let b1 = w1.to_be_bytes::<32>();
        let quote = Address::from_slice(&b1[12..32]);

        // slot+4: best_bid_tick(i16) + best_ask_tick(i16) packed
        let w4 = ctx.sload(self.address, self.slot + U256::from(4))?;
        let b4 = w4.to_be_bytes::<32>();
        let best_bid_tick = i16::from_be_bytes(b4[30..32].try_into().unwrap());
        let best_ask_tick = i16::from_be_bytes(b4[28..30].try_into().unwrap());
        let book_id = u32::from_be_bytes(b4[24..28].try_into().unwrap());

        Ok(OrderbookData {
            base,
            quote,
            best_bid_tick,
            best_ask_tick,
            book_id,
        })
    }

    /// Writes the base orderbook data.
    fn write_data(&self, data: &OrderbookData) -> Result<()> {
        let mut ctx = StorageCtx::default();

        // slot+0: base
        let mut b0 = [0u8; 32];
        b0[12..32].copy_from_slice(data.base.as_slice());
        ctx.sstore(self.address, self.slot, U256::from_be_bytes(b0))?;

        // slot+1: quote
        let mut b1 = [0u8; 32];
        b1[12..32].copy_from_slice(data.quote.as_slice());
        ctx.sstore(
            self.address,
            self.slot + U256::from(1),
            U256::from_be_bytes(b1),
        )?;

        // slot+4: packed ticks
        let mut b4 = [0u8; 32];
        b4[30..32].copy_from_slice(&data.best_bid_tick.to_be_bytes());
        b4[28..30].copy_from_slice(&data.best_ask_tick.to_be_bytes());
        b4[24..28].copy_from_slice(&data.book_id.to_be_bytes());
        ctx.sstore(
            self.address,
            self.slot + U256::from(4),
            U256::from_be_bytes(b4),
        )?;

        Ok(())
    }

    fn write_best_bid_tick(&mut self, tick: i16) -> Result<()> {
        let mut data = self.read_data()?;
        data.best_bid_tick = tick;
        self.write_data(&data)
    }

    fn write_best_ask_tick(&mut self, tick: i16) -> Result<()> {
        let mut data = self.read_data()?;
        data.best_ask_tick = tick;
        self.write_data(&data)
    }

    fn write_book_id(&mut self, book_id: BookId) -> Result<()> {
        let mut ctx = StorageCtx::default();
        let slot = self.slot + U256::from(4);
        let current = ctx.sload(self.address, slot)?;
        let updated = packing::insert_into_word(current, &*book_id, 4, 4)?;
        ctx.sstore(self.address, slot, updated)
    }

    fn read_tick_level(&self, tick: i16, is_bid: bool) -> Result<TickLevel> {
        if is_bid {
            self.bids[tick].read()
        } else {
            self.asks[tick].read()
        }
    }

    fn write_tick_level(&mut self, tick: i16, is_bid: bool, level: TickLevel) -> Result<()> {
        if is_bid {
            self.bids[tick].write(level)
        } else {
            self.asks[tick].write(level)
        }
    }

    fn delete_tick_level(&mut self, tick: i16, is_bid: bool) -> Result<()> {
        if is_bid {
            self.bids[tick].delete()
        } else {
            self.asks[tick].delete()
        }
    }

    fn set_tick_bit(&mut self, tick: i16, is_bid: bool) -> Result<()> {
        let word_index = tick >> 8;
        let current = if is_bid {
            self.bid_bitmap[word_index].read()?
        } else {
            self.ask_bitmap[word_index].read()?
        };
        let bit_index = (tick & 0xFF) as usize;
        let mask = U256::from(1u8) << bit_index;
        if is_bid {
            self.bid_bitmap[word_index].write(current | mask)
        } else {
            self.ask_bitmap[word_index].write(current | mask)
        }
    }

    fn delete_tick_bit(&mut self, tick: i16, is_bid: bool) -> Result<()> {
        let word_index = tick >> 8;
        let current = if is_bid {
            self.bid_bitmap[word_index].read()?
        } else {
            self.ask_bitmap[word_index].read()?
        };
        let bit_index = (tick & 0xFF) as usize;
        let mask = !(U256::from(1u8) << bit_index);
        if is_bid {
            self.bid_bitmap[word_index].write(current & mask)
        } else {
            self.ask_bitmap[word_index].write(current & mask)
        }
    }

    /// Finds the next initialized tick. Searches downward for bids, upward for asks.
    fn next_initialized_tick(&self, tick: i16, is_bid: bool) -> Result<(i16, bool)> {
        if is_bid {
            self.next_initialized_bid_tick(tick)
        } else {
            self.next_initialized_ask_tick(tick)
        }
    }

    fn next_initialized_ask_tick(&self, tick: i16) -> Result<(i16, bool)> {
        if tick >= MAX_TICK {
            return Ok((MAX_TICK, false));
        }
        let mut next_tick = tick + 1;
        let max_word_index = MAX_TICK >> 8;

        loop {
            let word_index = next_tick >> 8;
            if word_index > max_word_index {
                return Ok((next_tick, false));
            }
            let bit_index = (next_tick & 0xFF) as usize;
            let word = self.ask_bitmap[word_index].read()?;
            let mask = if bit_index == 0 {
                U256::MAX
            } else {
                U256::MAX << bit_index
            };
            let masked_word = word & mask;

            if masked_word != U256::ZERO {
                let lowest_bit = masked_word.trailing_zeros();
                let found_tick = (word_index << 8) | (lowest_bit as i16);
                if found_tick <= MAX_TICK {
                    return Ok((found_tick, true));
                }
                return Ok((found_tick, false));
            }

            let next_word_index = word_index + 1;
            if next_word_index > max_word_index {
                return Ok((next_word_index << 8, false));
            }
            next_tick = next_word_index << 8;
        }
    }

    fn next_initialized_bid_tick(&self, tick: i16) -> Result<(i16, bool)> {
        if tick <= MIN_TICK {
            return Ok((MIN_TICK, false));
        }
        let mut next_tick = tick - 1;
        let min_word_index = MIN_TICK >> 8;

        loop {
            let word_index = next_tick >> 8;
            if word_index < min_word_index {
                return Ok((next_tick, false));
            }
            let bit_index = (next_tick & 0xFF) as usize;
            let word = self.bid_bitmap[word_index].read()?;
            let mask = if bit_index == 255 {
                U256::MAX
            } else {
                U256::MAX >> (255 - bit_index)
            };
            let masked_word = word & mask;

            if masked_word != U256::ZERO {
                let leading = masked_word.leading_zeros();
                let highest_bit = 255 - leading;
                let found_tick = (word_index << 8) | (highest_bit as i16);
                if found_tick >= MIN_TICK {
                    return Ok((found_tick, true));
                }
                return Ok((found_tick, false));
            }

            let prev_word_index = word_index - 1;
            if prev_word_index < min_word_index {
                return Ok(((prev_word_index << 8) | 0xFF, false));
            }
            next_tick = (prev_word_index << 8) | 0xFF;
        }
    }
}

// ===========================================================================
// StablecoinDEX struct
// ===========================================================================

/// On-chain CLOB for stablecoin trading.
pub struct StablecoinDEX {
    // Slot 0: books (Mapping<B256, Orderbook>)
    // Note: each Orderbook occupies 7 sub-slots in the mapping value space
    books_slot: U256,
    // Slot 1: orders (Mapping<u128, Order>)
    orders: OrderMapping,
    // Slot 2: balances (Mapping<Address, Mapping<Address, u128>>)
    balances: Mapping<Address, Mapping<Address, u128>>,
    // Slot 3: next_order_id
    next_order_id: Slot<u128>,
    // Slot 4: book_keys
    book_keys: VecHandler<B256>,
    // Slot 5: reusable order storage credits by maker
    dex_storage_credits: Mapping<Address, u64>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl StablecoinDEX {
    pub fn new() -> Self {
        let address = STABLECOIN_DEX_ADDRESS;
        Self {
            books_slot: U256::from(0),
            orders: OrderMapping::new(),
            balances: Mapping::new(U256::from(2), address),
            next_order_id: Slot::new(U256::from(3), address),
            book_keys: VecHandler::new(U256::from(4), address),
            dex_storage_credits: Mapping::new(U256::from(5), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }

    /// Initializes the stablecoin DEX precompile.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    /// Returns the number of reusable order storage credits owned by `user`.
    pub fn storage_credits(&self, user: Address) -> Result<u64> {
        self.dex_storage_credits[user].read()
    }

    fn preserve_storage_credits(&mut self) -> Result<()> {
        if self.storage.spec().is_t7() {
            StorageCredits::new().preserve(self.address)?;
        }
        Ok(())
    }

    fn credit_dex_storage_slots(&mut self, user: Address, slots: u64) -> Result<()> {
        if slots == 0 || !self.storage.spec().is_t7() {
            return Ok(());
        }

        let current = self.dex_storage_credits[user].read()?;
        let updated = current.saturating_add(slots);
        if current != 0 {
            return self.dex_storage_credits[user].write(updated);
        }

        let (_, delta) = StorageCredits::new().with_budget(self.address, 1, || {
            self.dex_storage_credits[user].write(updated)
        })?;
        if delta != -1 {
            return Err(TempoPrecompileError::Fatal(format!(
                "DEX storage credit bookkeeping spend mismatch: reserved 1, delta {delta}"
            )));
        }
        Ok(())
    }

    fn delete_order(&mut self, order: &Order) -> Result<u64> {
        StorageCredits::new()
            .track_minted_credits(self.address, || self.orders[order.order_id].delete())
            .map(|(_, credits)| credits)
    }

    fn rewrite_order(&mut self, order: Order, book_id: BookId) -> Result<u64> {
        StorageCredits::new()
            .track_minted_credits(self.address, || {
                self.orders[order.order_id].write_in_book(order, book_id)
            })
            .map(|(_, credits)| credits)
    }

    fn unlink_neighbor_and_credit_maker(
        &mut self,
        order_id: u128,
        update: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let (_, credits) =
            StorageCredits::new().track_minted_credits(self.address, || update(self))?;
        if credits == 0 {
            return Ok(());
        }

        let maker = self.orders[order_id].maker()?;
        self.credit_dex_storage_slots(maker, credits)
    }

    fn delete_order_and_track_deltas(
        &mut self,
        storage_credits: &mut StorageCreditDeltas,
        order: &Order,
    ) -> Result<()> {
        let credits = self.delete_order(order)?;
        storage_credits.credit_slots(order.maker, credits);
        Ok(())
    }

    fn write_order_spending_dex_storage_credits(
        &mut self,
        order: Order,
        book_id: BookId,
    ) -> Result<()> {
        let user = order.maker;
        let user_credits = self.dex_storage_credits[user].read()?;
        if user_credits == 0 {
            return self.orders[order.order_id].write_in_book(order, book_id);
        }

        self.dex_storage_credits[user].delete()?;
        let (_, delta) = StorageCredits::new().with_budget(self.address, user_credits, || {
            self.orders[order.order_id].write_in_book(order, book_id)
        })?;
        let spent_credits = if delta < 0 { (-delta) as u64 } else { 0 };
        self.credit_dex_storage_slots(user, user_credits.saturating_sub(spent_credits))
    }

    /// Helper to get a book handle for a given pair key.
    fn book_handle(&self, pair_key: B256) -> OrderbookHandle {
        // Mapping slot calculation: keccak256(key . base_slot)
        let key_slot = {
            let mut data = [0u8; 64];
            data[..32].copy_from_slice(pair_key.as_slice());
            data[32..64].copy_from_slice(&self.books_slot.to_be_bytes::<32>());
            U256::from_be_bytes(keccak256(data).0)
        };
        OrderbookHandle::new(key_slot, self.address)
    }

    fn next_order_id_val(&self) -> Result<u128> {
        Ok(self.next_order_id.read()?.max(1))
    }

    fn increment_next_order_id(&mut self) -> Result<()> {
        let next = self.next_order_id_val()?;
        self.next_order_id.write(next + 1)
    }

    /// Returns the user's DEX balance for `token`.
    pub fn balance_of(&self, user: Address, token: Address) -> Result<u128> {
        self.balances[user][token].read()
    }

    fn set_balance(&mut self, user: Address, token: Address, amount: u128) -> Result<()> {
        self.balances[user][token].write(amount)
    }

    fn increment_balance(&mut self, user: Address, token: Address, amount: u128) -> Result<()> {
        let current = self.balance_of(user, token)?;
        self.set_balance(
            user,
            token,
            current
                .checked_add(amount)
                .ok_or(TempoPrecompileError::under_overflow())?,
        )
    }

    fn sub_balance(&mut self, user: Address, token: Address, amount: u128) -> Result<()> {
        let current = self.balance_of(user, token)?;
        self.set_balance(
            user,
            token,
            current
                .checked_sub(amount)
                .ok_or(TempoPrecompileError::under_overflow())?,
        )
    }

    /// Transfer tokens from the DEX to `to`.
    fn transfer(&mut self, token: Address, to: Address, amount: u128) -> Result<()> {
        TIP20Token::from_address(token)?.transfer(
            STABLECOIN_DEX_ADDRESS,
            super::tip20::ITIP20::transferCall {
                to,
                amount: U256::from(amount),
            },
        )?;
        Ok(())
    }

    /// Transfer tokens from `from` to the DEX.
    fn transfer_from(&mut self, token: Address, from: Address, amount: u128) -> Result<()> {
        let mut token = TIP20Token::from_address(token)?;
        if self.storage.spec().is_t5() {
            token.system_transfer_from(STABLECOIN_DEX_ADDRESS, from, U256::from(amount))?;
        } else {
            token.transfer_from(
                STABLECOIN_DEX_ADDRESS,
                super::tip20::ITIP20::transferFromCall {
                    from,
                    to: STABLECOIN_DEX_ADDRESS,
                    amount: U256::from(amount),
                },
            )?;
        }
        Ok(())
    }

    /// Decrement user's DEX balance or transfer from wallet.
    fn decrement_balance_or_transfer_from(
        &mut self,
        user: Address,
        token: Address,
        amount: u128,
    ) -> Result<()> {
        TIP20Token::from_address(token)?.ensure_transfer_authorized(user, self.address)?;

        let user_balance = self.balance_of(user, token)?;
        if user_balance >= amount {
            self.sub_balance(user, token, amount)
        } else {
            let remaining = amount
                .checked_sub(user_balance)
                .ok_or(TempoPrecompileError::under_overflow())?;
            self.transfer_from(token, user, remaining)?;
            self.set_balance(user, token, 0)
        }
    }

    /// Returns the order for a given ID.
    pub fn get_order(&self, order_id: u128) -> Result<Order> {
        let order = self.orders[order_id].read()?;
        if !order.maker.is_zero() && order.order_id < self.next_order_id_val()? {
            Ok(order)
        } else {
            Err(err_order_does_not_exist())
        }
    }

    /// Returns the orderbook data for a given pair key.
    pub fn books(&self, pair_key: B256) -> Result<OrderbookData> {
        self.book_handle(pair_key).read_data()
    }

    /// Returns the zero-based `book_keys` index stored in an initialized orderbook.
    fn book_key_index(&self, book_key: B256) -> Result<Option<u32>> {
        let book = self.books(book_key)?;
        if !book.is_initialized() {
            return Err(err_pair_does_not_exist());
        }
        Ok(book.id().index())
    }

    /// Resolves a book key from the append-only `book_keys` vector.
    fn book_key_for_index(&self, index: u32) -> Result<B256> {
        self.book_keys
            .at(index as usize)?
            .ok_or_else(err_pair_does_not_exist)?
            .read()
    }

    /// Assigns an existing pre-T8 orderbook its one-based ID.
    fn set_book_index(&mut self, index: u32) -> Result<()> {
        let book_key = self.book_key_for_index(index)?;
        if let Some(current_index) = self.book_key_index(book_key)? {
            if current_index == index {
                return Ok(());
            }
            return Err(err_index_already_set());
        }
        self.book_handle(book_key)
            .write_book_id(BookId::from_index(index))
    }

    /// Returns a tick level.
    pub fn get_price_level(&self, base: Address, tick: i16, is_bid: bool) -> Result<TickLevel> {
        let quote = TIP20Token::from_address(base)?.quote_token()?;
        let book_key = compute_book_key(base, quote);
        self.book_handle(book_key).read_tick_level(tick, is_bid)
    }

    /// Converts a relative tick to a scaled price.
    pub fn tick_to_price_fn(&self, tick: i16) -> Result<u32> {
        validate_tick_spacing(tick)?;
        Ok(tick_to_price(tick))
    }

    /// Converts a scaled price to a relative tick.
    pub fn price_to_tick_fn(&self, price: u32) -> Result<i16> {
        let tick = price_to_tick(price)?;
        validate_tick_spacing(tick)?;
        Ok(tick)
    }

    /// Creates a new trading pair.
    pub fn create_pair(&mut self, base: Address) -> Result<B256> {
        if !TIP20Factory::new().is_tip20(base)? {
            return Err(err_invalid_base_token());
        }

        let quote = TIP20Token::from_address(base)?.quote_token()?;
        validate_usd_currency(base)?;
        validate_usd_currency(quote)?;

        let book_key = compute_book_key(base, quote);
        let handle = self.book_handle(book_key);

        if handle.read_data()?.is_initialized() {
            return Err(err_pair_already_exists());
        }

        let book = if self.storage.spec().is_t8() {
            OrderbookData::new_with_index(base, quote, self.book_keys.len()? as u32)
        } else {
            OrderbookData::new(base, quote)
        };
        handle.write_data(&book)?;
        self.book_keys.push(book_key)?;

        self.emit_event(IStablecoinDEX::PairCreated {
            key: book_key,
            base,
            quote,
        })?;

        Ok(book_key)
    }

    fn validate_or_create_pair(&mut self, book: &OrderbookData, token: Address) -> Result<()> {
        if book.base.is_zero() {
            self.create_pair(token)?;
        }
        Ok(())
    }

    /// Places a limit order.
    pub fn place(
        &mut self,
        sender: Address,
        token: Address,
        amount: u128,
        is_bid: bool,
        tick: i16,
    ) -> Result<u128> {
        let quote_token = TIP20Token::from_address(token)?.quote_token()?;
        let book_key = compute_book_key(token, quote_token);

        let handle = self.book_handle(book_key);
        let book = handle.read_data()?;
        self.validate_or_create_pair(&book, token)?;

        if !(MIN_TICK..=MAX_TICK).contains(&tick) {
            return Err(err_tick_out_of_bounds(tick));
        }
        if tick % TICK_SPACING != 0 {
            return Err(err_invalid_tick());
        }
        if amount < MIN_ORDER_AMOUNT {
            return Err(err_below_minimum_order_size(amount));
        }

        let (escrow_token, escrow_amount, non_escrow_token) = if is_bid {
            let quote_amount = base_to_quote(amount, tick, RoundingDirection::Up)
                .ok_or_else(err_insufficient_balance)?;
            (quote_token, quote_amount, token)
        } else {
            (token, amount, quote_token)
        };

        let non_escrow_tip20 = TIP20Token::from_address(non_escrow_token)?;
        non_escrow_tip20.ensure_transfer_authorized(self.address, sender)?;
        // TIP-1046 (T4+): when this order fills, the non-escrow token may be
        // moved via internal-balance updates that bypass TIP-20 transfer's
        // own paused gate. Enforce paused at order placement.
        if self.storage.spec().is_t4() {
            non_escrow_tip20.check_not_paused()?;
        }
        self.decrement_balance_or_transfer_from(sender, escrow_token, escrow_amount)?;

        let order_id = self.next_order_id_val()?;
        self.increment_next_order_id()?;
        let order = if is_bid {
            Order::new_bid(order_id, sender, book_key, amount, tick)
        } else {
            Order::new_ask(order_id, sender, book_key, amount, tick)
        };
        self.commit_order_to_book(order, true)?;

        self.emit_event(IStablecoinDEX::OrderPlaced {
            orderId: order_id,
            maker: sender,
            token,
            amount,
            isBid: is_bid,
            tick,
            isFlipOrder: false,
            flipTick: 0,
        })?;

        Ok(order_id)
    }

    /// Commits an order to the orderbook.
    fn commit_order_to_book(&mut self, mut order: Order, charge_credits: bool) -> Result<()> {
        let mut handle = self.book_handle(order.book_key);
        let orderbook = handle.read_data()?;
        let book_id = orderbook.id();
        let mut level = handle.read_tick_level(order.tick, order.is_bid)?;

        let prev_tail = level.tail;
        if prev_tail == 0 {
            level.head = order.order_id;
            level.tail = order.order_id;

            handle.set_tick_bit(order.tick, order.is_bid)?;

            if order.is_bid {
                if order.tick > orderbook.best_bid_tick {
                    handle.write_best_bid_tick(order.tick)?;
                }
            } else if order.tick < orderbook.best_ask_tick {
                handle.write_best_ask_tick(order.tick)?;
            }
        } else {
            if self.storage.spec().is_t8() {
                self.orders[prev_tail].next()?.write(order.order_id)?;
            } else {
                let mut prev_order = self.orders[prev_tail].read_in_book(order.book_key)?;
                prev_order.next = order.order_id;
                self.orders[prev_tail].write_in_book(prev_order, book_id)?;
            }

            order.prev = prev_tail;
            level.tail = order.order_id;
        }

        let new_liquidity = level
            .total_liquidity
            .checked_add(order.remaining)
            .ok_or(TempoPrecompileError::under_overflow())?;
        level.total_liquidity = new_liquidity;

        handle.write_tick_level(order.tick, order.is_bid, level)?;
        if charge_credits && self.storage.spec().is_t7() {
            self.write_order_spending_dex_storage_credits(order, book_id)
        } else if !charge_credits && self.storage.spec().is_t8() {
            let maker = order.maker;
            let credits = self.rewrite_order(order, book_id)?;
            self.credit_dex_storage_slots(maker, credits)
        } else {
            self.orders[order.order_id].write_in_book(order, book_id)
        }
    }

    /// Places a flip order.
    #[allow(clippy::too_many_arguments)]
    pub fn place_flip(
        &mut self,
        sender: Address,
        token: Address,
        amount: u128,
        is_bid: bool,
        tick: i16,
        flip_tick: i16,
        internal_balance_only: bool,
    ) -> Result<u128> {
        let quote_token = TIP20Token::from_address(token)?.quote_token()?;
        let book_key = compute_book_key(token, quote_token);

        let batch = self.storage.checkpoint();

        let handle = self.book_handle(book_key);
        let book = handle.read_data()?;
        self.validate_or_create_pair(&book, token)?;

        if !(MIN_TICK..=MAX_TICK).contains(&tick) {
            return Err(err_tick_out_of_bounds(tick));
        }
        if tick % TICK_SPACING != 0 {
            return Err(err_invalid_tick());
        }
        if !(MIN_TICK..=MAX_TICK).contains(&flip_tick) {
            return Err(err_tick_out_of_bounds(flip_tick));
        }
        if flip_tick % TICK_SPACING != 0 {
            return Err(err_invalid_flip_tick());
        }
        if (flip_tick == tick && !self.storage.spec().is_t5())
            || (is_bid && flip_tick < tick)
            || (!is_bid && flip_tick > tick)
        {
            return Err(err_invalid_flip_tick());
        }
        if amount < MIN_ORDER_AMOUNT {
            return Err(err_below_minimum_order_size(amount));
        }

        let (escrow_token, escrow_amount, non_escrow_token) = if is_bid {
            let quote_amount = base_to_quote(amount, tick, RoundingDirection::Up)
                .ok_or_else(err_insufficient_balance)?;
            (quote_token, quote_amount, token)
        } else {
            (token, amount, quote_token)
        };

        let non_escrow_tip20 = TIP20Token::from_address(non_escrow_token)?;
        non_escrow_tip20.ensure_transfer_authorized(self.address, sender)?;
        // TIP-1046 (T4+): see place_order — paused check at placement time.
        if self.storage.spec().is_t4() {
            non_escrow_tip20.check_not_paused()?;
        }

        if internal_balance_only {
            let escrow_tip20 = TIP20Token::from_address(escrow_token)?;
            escrow_tip20.ensure_transfer_authorized(sender, self.address)?;
            // TIP-1046 (T4+): internal-balance-only path bypasses TIP-20
            // transferFrom, so we must check the pause state ourselves.
            if self.storage.spec().is_t4() {
                escrow_tip20.check_not_paused()?;
            }
            let user_balance = self.balance_of(sender, escrow_token)?;
            if user_balance < escrow_amount {
                return Err(err_insufficient_balance());
            }
            self.sub_balance(sender, escrow_token, escrow_amount)?;
        } else {
            self.decrement_balance_or_transfer_from(sender, escrow_token, escrow_amount)?;
        }

        let order_id = self.next_order_id_val()?;
        let order = Order::new_flip(
            order_id,
            sender,
            book_key,
            amount,
            tick,
            is_bid,
            flip_tick,
            self.storage.spec(),
        )?;

        self.next_order_id.write(order_id + 1)?;
        self.commit_order_to_book(order, true)?;

        self.emit_event(IStablecoinDEX::OrderPlaced {
            orderId: order_id,
            maker: sender,
            token,
            amount,
            isBid: is_bid,
            tick,
            isFlipOrder: true,
            flipTick: flip_tick,
        })?;

        batch.commit();
        Ok(order_id)
    }

    fn emit_order_filled(
        &mut self,
        order_id: u128,
        maker: Address,
        taker: Address,
        amount_filled: u128,
        partial_fill: bool,
    ) -> Result<()> {
        self.emit_event(IStablecoinDEX::OrderFilled {
            orderId: order_id,
            maker,
            taker,
            amountFilled: amount_filled,
            partialFill: partial_fill,
        })
    }

    /// Rewrites a fully filled T5 flip order under the same order ID.
    fn flip_in_place(
        &mut self,
        order: &Order,
        base_token: Address,
        quote_token: Address,
    ) -> Result<()> {
        let batch = self.storage.checkpoint();
        let flipped = order.create_flipped_order(order.order_id);
        let (escrow_token, escrow_amount, non_escrow_token) = if flipped.is_bid {
            let quote_amount = base_to_quote(flipped.amount, flipped.tick, RoundingDirection::Up)
                .ok_or_else(err_insufficient_balance)?;
            (quote_token, quote_amount, base_token)
        } else {
            (base_token, flipped.amount, quote_token)
        };

        if self.balance_of(flipped.maker, escrow_token)? < escrow_amount {
            return Err(err_insufficient_balance());
        }

        let escrow_tip20 = TIP20Token::from_address(escrow_token)?;
        escrow_tip20.check_not_paused()?;
        escrow_tip20.ensure_transfer_authorized(flipped.maker, self.address)?;

        let non_escrow_tip20 = TIP20Token::from_address(non_escrow_token)?;
        non_escrow_tip20.check_not_paused()?;
        non_escrow_tip20.ensure_transfer_authorized(self.address, flipped.maker)?;

        self.sub_balance(flipped.maker, escrow_token, escrow_amount)?;
        // A taker-triggered flip must not spend the maker's reusable credits.
        self.commit_order_to_book(flipped.clone(), false)?;
        self.emit_event(IStablecoinDEX::OrderFlipped {
            orderId: flipped.order_id,
            maker: flipped.maker,
            token: base_token,
            amount: flipped.amount,
            isBid: flipped.is_bid,
            tick: flipped.tick,
            flipTick: flipped.flip_tick,
        })?;

        batch.commit();
        Ok(())
    }

    /// Partial fill an order.
    fn partial_fill_order(
        &mut self,
        order: &mut Order,
        level: &mut TickLevel,
        fill_amount: u128,
        taker: Address,
    ) -> Result<u128> {
        let mut handle = self.book_handle(order.book_key);
        let orderbook = handle.read_data()?;

        let new_remaining = order.remaining - fill_amount;
        self.orders[order.order_id]
            .remaining()?
            .write(new_remaining)?;
        order.remaining = new_remaining;

        let quote_amount = base_to_quote(
            fill_amount,
            order.tick,
            if order.is_bid {
                RoundingDirection::Down
            } else {
                RoundingDirection::Up
            },
        )
        .ok_or(TempoPrecompileError::under_overflow())?;

        if order.is_bid {
            self.increment_balance(order.maker, orderbook.base, fill_amount)?;
        } else {
            self.increment_balance(order.maker, orderbook.quote, quote_amount)?;
        }

        let amount_out = if order.is_bid {
            quote_amount
        } else {
            fill_amount
        };

        let new_liquidity = level
            .total_liquidity
            .checked_sub(fill_amount)
            .ok_or(TempoPrecompileError::under_overflow())?;
        level.total_liquidity = new_liquidity;

        handle.write_tick_level(order.tick, order.is_bid, *level)?;
        self.emit_order_filled(order.order_id, order.maker, taker, fill_amount, true)?;

        Ok(amount_out)
    }

    /// Fill an order completely and return next order info.
    fn fill_order(
        &mut self,
        storage_credits: &mut StorageCreditDeltas,
        book_key: B256,
        order: &mut Order,
        mut level: TickLevel,
        taker: Address,
    ) -> Result<(u128, Option<(TickLevel, Order)>)> {
        let mut handle = self.book_handle(book_key);
        let orderbook = handle.read_data()?;
        let fill_amount = order.remaining;

        let amount_out = if order.is_bid {
            self.increment_balance(order.maker, orderbook.base, fill_amount)?;
            base_to_quote(fill_amount, order.tick, RoundingDirection::Down)
                .ok_or(TempoPrecompileError::under_overflow())?
        } else {
            let quote_amount = base_to_quote(fill_amount, order.tick, RoundingDirection::Up)
                .ok_or(TempoPrecompileError::under_overflow())?;
            self.increment_balance(order.maker, orderbook.quote, quote_amount)?;
            fill_amount
        };

        self.emit_order_filled(order.order_id, order.maker, taker, fill_amount, false)?;

        if order.is_flip {
            let result = if self.storage.spec().is_t5() {
                self.flip_in_place(order, orderbook.base, orderbook.quote)
            } else {
                self.place_flip(
                    order.maker,
                    orderbook.base,
                    order.amount,
                    !order.is_bid,
                    order.flip_tick,
                    order.tick,
                    true,
                )
                .map(|_| ())
            };
            if let Err(error) = &result {
                if error.is_system_error() {
                    return Err(error.clone());
                }
                if self.storage.spec().is_t5() {
                    self.emit_event(IStablecoinDEX::FlipFailed {
                        orderId: order.order_id,
                        maker: order.maker,
                        reason: error.selector(),
                    })?;
                }
            }

            if !self.storage.spec().is_t5() || result.is_err() {
                self.delete_order_and_track_deltas(storage_credits, order)?;
            }
        } else {
            self.delete_order_and_track_deltas(storage_credits, order)?;
        }

        let next_tick_info = if order.next == 0 {
            handle.delete_tick_level(order.tick, order.is_bid)?;
            handle.delete_tick_bit(order.tick, order.is_bid)?;

            let (tick, has_liquidity) = handle.next_initialized_tick(order.tick, order.is_bid)?;

            if order.is_bid {
                let new_best = if has_liquidity { tick } else { i16::MIN };
                handle.write_best_bid_tick(new_best)?;
            } else {
                let new_best = if has_liquidity { tick } else { i16::MAX };
                handle.write_best_ask_tick(new_best)?;
            }

            if !has_liquidity {
                None
            } else {
                let new_level = handle.read_tick_level(tick, order.is_bid)?;
                let new_order = self.orders[new_level.head].read_in_book(book_key)?;
                Some((new_level, new_order))
            }
        } else {
            level.head = order.next;
            let (_, credits) = StorageCredits::new()
                .track_minted_credits(self.address, || self.orders[order.next].prev()?.delete())?;

            let new_liquidity = level
                .total_liquidity
                .checked_sub(fill_amount)
                .ok_or(TempoPrecompileError::under_overflow())?;
            level.total_liquidity = new_liquidity;

            handle.write_tick_level(order.tick, order.is_bid, level)?;
            let new_order = self.orders[order.next].read_in_book(book_key)?;
            storage_credits.credit_slots(new_order.maker, credits);
            Some((level, new_order))
        };

        Ok((amount_out, next_tick_info))
    }

    fn get_best_price_level(&self, book_key: B256, is_bid: bool) -> Result<TickLevel> {
        let handle = self.book_handle(book_key);
        let orderbook = handle.read_data()?;

        let current_tick = if is_bid {
            if orderbook.best_bid_tick == i16::MIN {
                return Err(err_insufficient_liquidity());
            }
            orderbook.best_bid_tick
        } else {
            if orderbook.best_ask_tick == i16::MAX {
                return Err(err_insufficient_liquidity());
            }
            orderbook.best_ask_tick
        };

        handle.read_tick_level(current_tick, is_bid)
    }

    /// Fill orders for exact input amount.
    fn fill_orders_exact_in(
        &mut self,
        storage_credits: &mut StorageCreditDeltas,
        book_key: B256,
        bid: bool,
        mut amount_in: u128,
        taker: Address,
    ) -> Result<u128> {
        let mut level = self.get_best_price_level(book_key, bid)?;
        let mut order = self.orders[level.head].read_in_book(book_key)?;
        let mut total_amount_out: u128 = 0;

        while amount_in > 0 {
            let tick = order.tick;
            let fill_amount = if bid {
                amount_in.min(order.remaining)
            } else {
                let base_out = quote_to_base(amount_in, tick, RoundingDirection::Down)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                base_out.min(order.remaining)
            };

            if fill_amount < order.remaining {
                let amount_out =
                    self.partial_fill_order(&mut order, &mut level, fill_amount, taker)?;
                total_amount_out = total_amount_out
                    .checked_add(amount_out)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                break;
            } else {
                let (amount_out, next_order_info) =
                    self.fill_order(storage_credits, book_key, &mut order, level, taker)?;
                total_amount_out = total_amount_out
                    .checked_add(amount_out)
                    .ok_or(TempoPrecompileError::under_overflow())?;

                if bid {
                    if amount_in > order.remaining {
                        amount_in = amount_in
                            .checked_sub(order.remaining)
                            .ok_or(TempoPrecompileError::under_overflow())?;
                    } else {
                        amount_in = 0;
                    }
                } else {
                    let base_out = quote_to_base(amount_in, tick, RoundingDirection::Down)
                        .ok_or(TempoPrecompileError::under_overflow())?;
                    if base_out > order.remaining {
                        let quote_needed =
                            base_to_quote(order.remaining, tick, RoundingDirection::Up)
                                .ok_or(TempoPrecompileError::under_overflow())?;
                        amount_in = amount_in
                            .checked_sub(quote_needed)
                            .ok_or(TempoPrecompileError::under_overflow())?;
                    } else {
                        amount_in = 0;
                    }
                }

                if let Some((new_level, new_order)) = next_order_info {
                    level = new_level;
                    order = new_order;
                } else {
                    if amount_in > 0 {
                        return Err(err_insufficient_liquidity());
                    }
                    break;
                }
            }
        }
        Ok(total_amount_out)
    }

    /// Fill orders for exact output amount.
    fn fill_orders_exact_out(
        &mut self,
        storage_credits: &mut StorageCreditDeltas,
        book_key: B256,
        bid: bool,
        mut amount_out: u128,
        taker: Address,
    ) -> Result<u128> {
        let mut level = self.get_best_price_level(book_key, bid)?;
        let mut order = self.orders[level.head].read_in_book(book_key)?;
        let mut total_amount_in: u128 = 0;

        while amount_out > 0 {
            let tick = order.tick;
            let (fill_amount, amount_in) = if bid {
                let base_needed = quote_to_base(amount_out, tick, RoundingDirection::Up)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                let fill_amount = base_needed.min(order.remaining);
                (fill_amount, fill_amount)
            } else {
                let fill_amount = amount_out.min(order.remaining);
                let amount_in = base_to_quote(fill_amount, tick, RoundingDirection::Up)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                (fill_amount, amount_in)
            };

            if fill_amount < order.remaining {
                self.partial_fill_order(&mut order, &mut level, fill_amount, taker)?;
                total_amount_in = total_amount_in
                    .checked_add(amount_in)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                break;
            } else {
                let (amount_out_received, next_order_info) =
                    self.fill_order(storage_credits, book_key, &mut order, level, taker)?;
                total_amount_in = total_amount_in
                    .checked_add(amount_in)
                    .ok_or(TempoPrecompileError::under_overflow())?;

                if bid {
                    let base_needed = quote_to_base(amount_out, tick, RoundingDirection::Up)
                        .ok_or(TempoPrecompileError::under_overflow())?;
                    if base_needed > order.remaining {
                        amount_out = amount_out
                            .checked_sub(amount_out_received)
                            .ok_or(TempoPrecompileError::under_overflow())?;
                    } else {
                        amount_out = 0;
                    }
                } else if amount_out > order.remaining {
                    amount_out = amount_out
                        .checked_sub(amount_out_received)
                        .ok_or(TempoPrecompileError::under_overflow())?;
                } else {
                    amount_out = 0;
                }

                if let Some((new_level, new_order)) = next_order_info {
                    level = new_level;
                    order = new_order;
                } else {
                    if amount_out > 0 {
                        return Err(err_insufficient_liquidity());
                    }
                    break;
                }
            }
        }
        Ok(total_amount_in)
    }

    /// Quote exact input without executing.
    fn quote_exact_in(&self, book_key: B256, amount_in: u128, is_bid: bool) -> Result<u128> {
        let mut remaining_in = amount_in;
        let mut amount_out = 0u128;
        let handle = self.book_handle(book_key);
        let orderbook = handle.read_data()?;

        let mut current_tick = if is_bid {
            orderbook.best_bid_tick
        } else {
            orderbook.best_ask_tick
        };
        if current_tick == i16::MIN || current_tick == i16::MAX {
            return Err(err_insufficient_liquidity());
        }

        while remaining_in > 0 {
            let level = handle.read_tick_level(current_tick, is_bid)?;
            if level.total_liquidity == 0 {
                let (next_tick, initialized) =
                    handle.next_initialized_tick(current_tick, is_bid)?;
                if !initialized {
                    return Err(err_insufficient_liquidity());
                }
                current_tick = next_tick;
                continue;
            }

            let (fill_amount, amount_out_tick, amount_consumed) = if is_bid {
                let fill = remaining_in.min(level.total_liquidity);
                let quote_out = base_to_quote(fill, current_tick, RoundingDirection::Down)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                (fill, quote_out, fill)
            } else {
                let base_to_get =
                    quote_to_base(remaining_in, current_tick, RoundingDirection::Down)
                        .ok_or(TempoPrecompileError::under_overflow())?;
                let fill = base_to_get.min(level.total_liquidity);
                let quote_consumed = base_to_quote(fill, current_tick, RoundingDirection::Up)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                (fill, fill, quote_consumed)
            };

            remaining_in = remaining_in
                .checked_sub(amount_consumed)
                .ok_or(TempoPrecompileError::under_overflow())?;
            amount_out = amount_out
                .checked_add(amount_out_tick)
                .ok_or(TempoPrecompileError::under_overflow())?;

            if fill_amount == level.total_liquidity {
                let (next_tick, initialized) =
                    handle.next_initialized_tick(current_tick, is_bid)?;
                if !initialized && remaining_in > 0 {
                    return Err(err_insufficient_liquidity());
                }
                current_tick = next_tick;
            } else {
                break;
            }
        }
        Ok(amount_out)
    }

    /// Quote exact output without executing.
    fn quote_exact_out(&self, book_key: B256, amount_out: u128, is_bid: bool) -> Result<u128> {
        let mut remaining_out = amount_out;
        let mut amount_in = 0u128;
        let handle = self.book_handle(book_key);
        let orderbook = handle.read_data()?;

        let mut current_tick = if is_bid {
            orderbook.best_bid_tick
        } else {
            orderbook.best_ask_tick
        };
        if current_tick == i16::MIN || current_tick == i16::MAX {
            return Err(err_insufficient_liquidity());
        }

        while remaining_out > 0 {
            let level = handle.read_tick_level(current_tick, is_bid)?;
            if level.total_liquidity == 0 {
                let (next_tick, initialized) =
                    handle.next_initialized_tick(current_tick, is_bid)?;
                if !initialized {
                    return Err(err_insufficient_liquidity());
                }
                current_tick = next_tick;
                continue;
            }

            let (fill_amount, amount_in_tick) = if is_bid {
                let base_needed = quote_to_base(remaining_out, current_tick, RoundingDirection::Up)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                let fill_amount = base_needed.min(level.total_liquidity);
                (fill_amount, fill_amount)
            } else {
                let fill_amount = remaining_out.min(level.total_liquidity);
                let quote_needed = base_to_quote(fill_amount, current_tick, RoundingDirection::Up)
                    .ok_or(TempoPrecompileError::under_overflow())?;
                (fill_amount, quote_needed)
            };

            let amount_out_tick = if is_bid {
                base_to_quote(fill_amount, current_tick, RoundingDirection::Down)
                    .ok_or(TempoPrecompileError::under_overflow())?
                    .min(remaining_out)
            } else {
                fill_amount
            };

            remaining_out = remaining_out.saturating_sub(amount_out_tick);
            amount_in = amount_in
                .checked_add(amount_in_tick)
                .ok_or(TempoPrecompileError::under_overflow())?;

            if fill_amount == level.total_liquidity {
                let (next_tick, initialized) =
                    handle.next_initialized_tick(current_tick, is_bid)?;
                if !initialized && remaining_out > 0 {
                    return Err(err_insufficient_liquidity());
                }
                current_tick = next_tick;
            } else {
                break;
            }
        }
        Ok(amount_in)
    }

    /// Find the trade path between two tokens.
    fn find_trade_path(&self, token_in: Address, token_out: Address) -> Result<Vec<(B256, bool)>> {
        if token_in == token_out {
            return Err(err_identical_tokens());
        }
        if !is_tip20_prefix(token_in) || !is_tip20_prefix(token_out) {
            return Err(err_invalid_token());
        }

        let in_quote = TIP20Token::from_address(token_in)?.quote_token()?;
        let out_quote = TIP20Token::from_address(token_out)?.quote_token()?;

        if in_quote == token_out || out_quote == token_in {
            return self.validate_and_build_route(&[token_in, token_out]);
        }

        // Multi-hop: Find LCA and build path
        let path_in = self.find_path_to_root(token_in)?;
        let path_out = self.find_path_to_root(token_out)?;

        let path_out_set: HashSet<Address> = path_out.iter().copied().collect();
        let mut lca = None;
        for token_a in &path_in {
            if path_out_set.contains(token_a) {
                lca = Some(*token_a);
                break;
            }
        }

        let lca = lca.ok_or_else(err_pair_does_not_exist)?;

        let mut trade_path = Vec::new();
        for token in &path_in {
            trade_path.push(*token);
            if *token == lca {
                break;
            }
        }

        let lca_to_out: Vec<Address> = path_out
            .iter()
            .take_while(|&&t| t != lca)
            .copied()
            .collect();
        trade_path.extend(lca_to_out.iter().rev());

        self.validate_and_build_route(&trade_path)
    }

    fn validate_and_build_route(&self, path: &[Address]) -> Result<Vec<(B256, bool)>> {
        let mut route = Vec::new();

        for i in 0..path.len() - 1 {
            let token_in = path[i];
            let token_out = path[i + 1];

            let (base, _quote) = {
                let token_in_tip20 = TIP20Token::from_address(token_in)?;
                if token_in_tip20.quote_token()? == token_out {
                    (token_in, token_out)
                } else {
                    let token_out_tip20 = TIP20Token::from_address(token_out)?;
                    if token_out_tip20.quote_token()? == token_in {
                        (token_out, token_in)
                    } else {
                        return Err(err_pair_does_not_exist());
                    }
                }
            };

            let book_key = compute_book_key(base, _quote);
            let handle = self.book_handle(book_key);
            let orderbook = handle.read_data()?;

            if orderbook.base.is_zero() {
                return Err(err_pair_does_not_exist());
            }

            let is_base_for_quote = token_in == base;
            route.push((book_key, is_base_for_quote));
        }

        Ok(route)
    }

    fn find_path_to_root(&self, mut token: Address) -> Result<Vec<Address>> {
        let mut path = vec![token];
        while token != PATH_USD_ADDRESS {
            token = TIP20Token::from_address(token)?.quote_token()?;
            path.push(token);
        }
        Ok(path)
    }

    /// Swaps exact amount in.
    pub fn swap_exact_amount_in(
        &mut self,
        sender: Address,
        token_in: Address,
        token_out: Address,
        amount_in: u128,
        min_amount_out: u128,
    ) -> Result<u128> {
        let route = self.find_trade_path(token_in, token_out)?;
        self.decrement_balance_or_transfer_from(sender, token_in, amount_in)?;

        let mut amount = amount_in;
        let mut storage_credits = StorageCreditDeltas::new();
        for (book_key, base_for_quote) in route {
            amount = self.fill_orders_exact_in(
                &mut storage_credits,
                book_key,
                base_for_quote,
                amount,
                sender,
            )?;
        }

        if amount < min_amount_out {
            return Err(err_insufficient_output());
        }

        self.transfer(token_out, sender, amount)?;
        storage_credits.flush(|user, slots| self.credit_dex_storage_slots(user, slots))?;
        Ok(amount)
    }

    /// Swaps to receive exact amount out.
    pub fn swap_exact_amount_out(
        &mut self,
        sender: Address,
        token_in: Address,
        token_out: Address,
        amount_out: u128,
        max_amount_in: u128,
    ) -> Result<u128> {
        let route = self.find_trade_path(token_in, token_out)?;

        let mut amount = amount_out;
        let mut storage_credits = StorageCreditDeltas::new();
        for (book_key, base_for_quote) in route.iter().rev() {
            amount = self.fill_orders_exact_out(
                &mut storage_credits,
                *book_key,
                *base_for_quote,
                amount,
                sender,
            )?;
        }

        if amount > max_amount_in {
            return Err(err_max_input_exceeded());
        }

        self.decrement_balance_or_transfer_from(sender, token_in, amount)?;
        self.transfer(token_out, sender, amount_out)?;
        storage_credits.flush(|user, slots| self.credit_dex_storage_slots(user, slots))?;
        Ok(amount)
    }

    /// Quote swap exact amount in.
    pub fn quote_swap_exact_amount_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: u128,
    ) -> Result<u128> {
        let route = self.find_trade_path(token_in, token_out)?;
        let mut current_amount = amount_in;
        for (book_key, base_for_quote) in route {
            current_amount = self.quote_exact_in(book_key, current_amount, base_for_quote)?;
        }
        Ok(current_amount)
    }

    /// Quote swap exact amount out.
    pub fn quote_swap_exact_amount_out(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: u128,
    ) -> Result<u128> {
        let route = self.find_trade_path(token_in, token_out)?;
        let mut current_amount = amount_out;
        for (book_key, base_for_quote) in route.iter().rev() {
            current_amount = self.quote_exact_out(*book_key, current_amount, *base_for_quote)?;
        }
        Ok(current_amount)
    }

    /// Cancels an active order.
    pub fn cancel(&mut self, sender: Address, order_id: u128) -> Result<()> {
        let order = self.orders[order_id].read()?;
        if order.maker.is_zero() {
            return Err(err_order_does_not_exist());
        }
        if order.maker != sender {
            return Err(err_unauthorized());
        }
        if order.remaining == 0 {
            return Err(err_order_does_not_exist());
        }
        self.cancel_active_order(order)
    }

    fn cancel_active_order(&mut self, order: Order) -> Result<()> {
        let mut handle = self.book_handle(order.book_key);
        let mut level = handle.read_tick_level(order.tick, order.is_bid)?;

        // Update linked list
        if order.prev != 0 {
            self.unlink_neighbor_and_credit_maker(order.prev, |dex| {
                dex.orders[order.prev].next()?.write(order.next)
            })?;
        } else {
            level.head = order.next;
        }

        if order.next != 0 {
            self.unlink_neighbor_and_credit_maker(order.next, |dex| {
                dex.orders[order.next].prev()?.write(order.prev)
            })?;
        } else {
            level.tail = order.prev;
        }

        let new_liquidity = level
            .total_liquidity
            .checked_sub(order.remaining)
            .ok_or(TempoPrecompileError::under_overflow())?;
        level.total_liquidity = new_liquidity;

        if level.head == 0 {
            handle.delete_tick_bit(order.tick, order.is_bid)?;

            let orderbook = handle.read_data()?;
            let best_tick = if order.is_bid {
                orderbook.best_bid_tick
            } else {
                orderbook.best_ask_tick
            };

            if best_tick == order.tick {
                let (next_tick, has_liquidity) =
                    handle.next_initialized_tick(order.tick, order.is_bid)?;

                if order.is_bid {
                    let new_best = if has_liquidity { next_tick } else { i16::MIN };
                    handle.write_best_bid_tick(new_best)?;
                } else {
                    let new_best = if has_liquidity { next_tick } else { i16::MAX };
                    handle.write_best_ask_tick(new_best)?;
                }
            }
        }

        handle.write_tick_level(order.tick, order.is_bid, level)?;

        // Refund tokens to maker
        let orderbook = handle.read_data()?;
        if order.is_bid {
            let quote_amount = base_to_quote(order.remaining, order.tick, RoundingDirection::Up)
                .ok_or(TempoPrecompileError::under_overflow())?;
            self.increment_balance(order.maker, orderbook.quote, quote_amount)?;
        } else {
            self.increment_balance(order.maker, orderbook.base, order.remaining)?;
        }

        let credits = self.delete_order(&order)?;
        self.credit_dex_storage_slots(order.maker, credits)?;

        self.emit_event(IStablecoinDEX::OrderCancelled {
            orderId: order.order_id,
        })
    }

    /// Cancels a stale order (blocked by TIP-403 policy).
    pub fn cancel_stale_order(&mut self, order_id: u128) -> Result<()> {
        let order = self.orders[order_id].read()?;
        if order.maker.is_zero() {
            return Err(err_order_does_not_exist());
        }

        let handle = self.book_handle(order.book_key);
        let book = handle.read_data()?;
        let token = if order.is_bid { book.quote } else { book.base };

        let policy_id = TIP20Token::from_address(token)?.transfer_policy_id()?;
        match TIP403Registry::new().is_authorized_as(policy_id, order.maker, AuthRole::sender()) {
            Ok(true) => Err(err_order_not_stale()),
            Ok(false) => self.cancel_active_order(order),
            Err(e) if is_policy_lookup_error(&e) => self.cancel_active_order(order),
            Err(e) => Err(e),
        }
    }

    /// Withdraws from DEX balance.
    pub fn withdraw(&mut self, user: Address, token: Address, amount: u128) -> Result<()> {
        let current_balance = self.balance_of(user, token)?;
        if current_balance < amount {
            return Err(err_insufficient_balance());
        }
        self.sub_balance(user, token, amount)?;
        self.transfer(token, user, amount)
    }
}

impl ContractStorage for StablecoinDEX {
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
// Dispatch
// ===========================================================================

impl Precompile for StablecoinDEX {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            IStablecoinDEX::IStablecoinDEXCalls::valid_selector,
            |data| {
                IStablecoinDEX::IStablecoinDEXCalls::abi_decode_with_config(
                    data,
                    crate::tempo::precompile::abi_decoder_config(),
                )
            },
            |call| match call {
                IStablecoinDEX::IStablecoinDEXCalls::place(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.preserve_storage_credits()?;
                        self.place(s, c.token, c.amount, c.isBid, c.tick)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::placeFlip(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.preserve_storage_credits()?;
                        self.place_flip(s, c.token, c.amount, c.isBid, c.tick, c.flipTick, false)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::balanceOf(call) => {
                    view(call, |c| self.balance_of(c.user, c.token))
                }
                IStablecoinDEX::IStablecoinDEXCalls::storageCredits(call) => {
                    if !self.storage.spec().is_t7() {
                        unknown_selector(calldata[..4].try_into().unwrap(), 0)
                    } else {
                        view(call, |c| self.storage_credits(c.user))
                    }
                }
                IStablecoinDEX::IStablecoinDEXCalls::bookIndexForKey(call) => {
                    if !self.storage.spec().is_t8() {
                        unknown_selector(calldata[..4].try_into().unwrap(), 0)
                    } else {
                        view(call, |c| {
                            let index = self.book_key_index(c.bookKey)?;
                            Ok((index.is_some(), index.unwrap_or(*BookId::UNSET)).into())
                        })
                    }
                }
                IStablecoinDEX::IStablecoinDEXCalls::bookKeyForIndex(call) => {
                    if !self.storage.spec().is_t8() {
                        unknown_selector(calldata[..4].try_into().unwrap(), 0)
                    } else {
                        view(call, |c| self.book_key_for_index(c.index))
                    }
                }
                IStablecoinDEX::IStablecoinDEXCalls::setBookIndex(call) => {
                    if !self.storage.spec().is_t8() {
                        unknown_selector(calldata[..4].try_into().unwrap(), 0)
                    } else {
                        mutate_void(call, msg_sender, |_, c| {
                            self.preserve_storage_credits()?;
                            self.set_book_index(c.index)
                        })
                    }
                }
                IStablecoinDEX::IStablecoinDEXCalls::getOrder(call) => view(call, |c| {
                    let order = self.get_order(c.orderId)?;
                    Ok(IStablecoinDEX::Order {
                        orderId: order.order_id,
                        maker: order.maker,
                        bookKey: order.book_key,
                        isBid: order.is_bid,
                        tick: order.tick,
                        amount: order.amount,
                        remaining: order.remaining,
                        prev: order.prev,
                        next: order.next,
                        isFlip: order.is_flip,
                        flipTick: order.flip_tick,
                    })
                }),
                IStablecoinDEX::IStablecoinDEXCalls::getTickLevel(call) => view(call, |c| {
                    let level = self.get_price_level(c.base, c.tick, c.isBid)?;
                    Ok((level.head, level.tail, level.total_liquidity).into())
                }),
                IStablecoinDEX::IStablecoinDEXCalls::pairKey(call) => {
                    view(call, |c| Ok(compute_book_key(c.tokenA, c.tokenB)))
                }
                IStablecoinDEX::IStablecoinDEXCalls::books(call) => view(call, |c| {
                    let book = self.books(c.pairKey)?;
                    Ok(IStablecoinDEX::Orderbook {
                        base: book.base,
                        quote: book.quote,
                        bestBidTick: book.best_bid_tick,
                        bestAskTick: book.best_ask_tick,
                    })
                }),
                IStablecoinDEX::IStablecoinDEXCalls::nextOrderId(call) => {
                    view(call, |_| self.next_order_id_val())
                }
                IStablecoinDEX::IStablecoinDEXCalls::createPair(call) => {
                    mutate(call, msg_sender, |_, c| {
                        self.preserve_storage_credits()?;
                        self.create_pair(c.base)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::withdraw(call) => {
                    mutate_void(call, msg_sender, |s, c| {
                        self.preserve_storage_credits()?;
                        self.withdraw(s, c.token, c.amount)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::cancel(call) => {
                    mutate_void(call, msg_sender, |s, c| {
                        self.preserve_storage_credits()?;
                        self.cancel(s, c.orderId)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::cancelStaleOrder(call) => {
                    mutate_void(call, msg_sender, |_, c| {
                        self.preserve_storage_credits()?;
                        self.cancel_stale_order(c.orderId)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::swapExactAmountIn(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.preserve_storage_credits()?;
                        self.swap_exact_amount_in(
                            s,
                            c.tokenIn,
                            c.tokenOut,
                            c.amountIn,
                            c.minAmountOut,
                        )
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::swapExactAmountOut(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.preserve_storage_credits()?;
                        self.swap_exact_amount_out(
                            s,
                            c.tokenIn,
                            c.tokenOut,
                            c.amountOut,
                            c.maxAmountIn,
                        )
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::quoteSwapExactAmountIn(call) => {
                    view(call, |c| {
                        self.quote_swap_exact_amount_in(c.tokenIn, c.tokenOut, c.amountIn)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::quoteSwapExactAmountOut(call) => {
                    view(call, |c| {
                        self.quote_swap_exact_amount_out(c.tokenIn, c.tokenOut, c.amountOut)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::MIN_TICK(call) => view(call, |_| Ok(MIN_TICK)),
                IStablecoinDEX::IStablecoinDEXCalls::MAX_TICK(call) => view(call, |_| Ok(MAX_TICK)),
                IStablecoinDEX::IStablecoinDEXCalls::TICK_SPACING(call) => {
                    view(call, |_| Ok(TICK_SPACING))
                }
                IStablecoinDEX::IStablecoinDEXCalls::PRICE_SCALE(call) => {
                    view(call, |_| Ok(PRICE_SCALE))
                }
                IStablecoinDEX::IStablecoinDEXCalls::MIN_ORDER_AMOUNT(call) => {
                    view(call, |_| Ok(MIN_ORDER_AMOUNT))
                }
                IStablecoinDEX::IStablecoinDEXCalls::MIN_PRICE(call) => {
                    view(call, |_| Ok(MIN_PRICE))
                }
                IStablecoinDEX::IStablecoinDEXCalls::MAX_PRICE(call) => {
                    view(call, |_| Ok(MAX_PRICE))
                }
                IStablecoinDEX::IStablecoinDEXCalls::tickToPrice(call) => {
                    view(call, |c| self.tick_to_price_fn(c.tick))
                }
                IStablecoinDEX::IStablecoinDEXCalls::priceToTick(call) => {
                    view(call, |c| self.price_to_tick_fn(c.price))
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{FixedBytes, address, b256};
    use alloy::sol_types::{SolCall, SolEvent};

    use super::*;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::PATH_USD_ADDRESS;
    use crate::tempo::precompile::test_utils::TestStorageProvider;
    use crate::tempo::precompile::tip20::{IRolesAuth, ISSUER_ROLE, ITIP20};
    use crate::tempo::precompile::tip403_registry::{ITIP403Registry, TIP403Registry};

    fn setup_dex_tokens(
        dex: &mut StablecoinDEX,
        admin: Address,
        base_token: Address,
    ) -> Result<()> {
        TIP20Token::from_address_unchecked(PATH_USD_ADDRESS).initialize(
            Address::ZERO,
            "Path USD",
            "pathUSD",
            "USD",
            PATH_USD_ADDRESS,
            admin,
        )?;
        TIP20Token::from_address_unchecked(base_token).initialize(
            Address::ZERO,
            "Base USD",
            "baseUSD",
            "USD",
            PATH_USD_ADDRESS,
            admin,
        )?;
        dex.initialize()?;
        dex.create_pair(base_token)?;
        Ok(())
    }

    fn grant_and_mint(
        token: Address,
        admin: Address,
        recipient: Address,
        amount: u128,
    ) -> Result<()> {
        let mut tip20 = TIP20Token::from_address_unchecked(token);
        tip20.grant_role(
            admin,
            IRolesAuth::grantRoleCall {
                role: *ISSUER_ROLE,
                account: admin,
            },
        )?;
        tip20.mint(
            admin,
            ITIP20::mintCall {
                to: recipient,
                amount: U256::from(amount),
            },
        )
    }

    #[test]
    fn storage_credits_selector_activates_at_t7() {
        let call = IStablecoinDEX::storageCreditsCall {
            user: Address::ZERO,
        };
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);
        let before = StorageCtx::enter(&mut provider, || {
            StablecoinDEX::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(before.reverted);

        provider.set_spec(TempoHardfork::T7);
        let after = StorageCtx::enter(&mut provider, || {
            StablecoinDEX::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(!after.reverted);
    }

    fn test_order(order_id: u128, book_key: B256) -> Order {
        Order {
            order_id,
            maker: Address::repeat_byte(0x11),
            book_key,
            is_bid: true,
            tick: 5,
            amount: MIN_ORDER_AMOUNT,
            remaining: MIN_ORDER_AMOUNT - 1,
            prev: order_id - 1,
            next: order_id + 1,
            is_flip: true,
            flip_tick: 10,
        }
    }

    fn expected_compact_slots(order: &Order, version: OrderVersion, book_index: u32) -> [U256; 3] {
        let mut slot0 = [0u8; 32];
        slot0[0] = version as u8;
        if version == OrderVersion::V2 {
            slot0[3..7].copy_from_slice(&book_index.to_be_bytes());
        }
        slot0[7..9].copy_from_slice(&order.flip_tick.to_be_bytes());
        slot0[9..11].copy_from_slice(&order.tick.to_be_bytes());
        slot0[11] = OrderFlags::pack(order);
        slot0[12..32].copy_from_slice(order.maker.as_slice());

        let mut slot1 = [0u8; 32];
        slot1[..16].copy_from_slice(&order.remaining.to_be_bytes());
        slot1[16..].copy_from_slice(&order.amount.to_be_bytes());

        let mut slot2 = [0u8; 32];
        slot2[..16].copy_from_slice(&order.next.to_be_bytes());
        slot2[16..].copy_from_slice(&order.prev.to_be_bytes());
        [
            U256::from_be_bytes(slot0),
            U256::from_be_bytes(slot1),
            U256::from_be_bytes(slot2),
        ]
    }

    #[test]
    fn t8_order_layouts_match_tip_1062_and_tip_1087() {
        let v1_key = b256!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let v2_key = b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);

        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            let v1 = test_order(10, v1_key);
            let v2 = test_order(20, v2_key);
            dex.orders[10].write_in_book(v1.clone(), BookId::UNSET)?;
            dex.orders[20].write_in_book(v2.clone(), BookId::from_index(7))?;

            let v1_slot = dex.orders[10].base_slot;
            let v2_slot = dex.orders[20].base_slot;
            let v1_expected = expected_compact_slots(&v1, OrderVersion::V1, 0);
            let v2_expected = expected_compact_slots(&v2, OrderVersion::V2, 7);
            for (offset, expected) in v1_expected.into_iter().enumerate() {
                assert_eq!(
                    StorageCtx.sload(dex.address, v1_slot + U256::from(offset))?,
                    expected
                );
            }
            assert_eq!(
                StorageCtx.sload(dex.address, v1_slot + U256::from(3))?,
                U256::from_be_bytes(v1_key.0)
            );
            for offset in 4..Order::SLOTS {
                assert_eq!(
                    StorageCtx.sload(dex.address, v1_slot + U256::from(offset))?,
                    U256::ZERO
                );
            }
            for (offset, expected) in v2_expected.into_iter().enumerate() {
                assert_eq!(
                    StorageCtx.sload(dex.address, v2_slot + U256::from(offset))?,
                    expected
                );
            }
            for offset in V2Order::SLOTS..Order::SLOTS {
                assert_eq!(
                    StorageCtx.sload(dex.address, v2_slot + U256::from(offset))?,
                    U256::ZERO
                );
            }
            assert_eq!(dex.orders[10].read_in_book(v1_key)?, v1);
            assert_eq!(dex.orders[20].read_in_book(v2_key)?, v2);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t8_unknown_order_version_is_fatal() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);
        let result = StorageCtx::enter(&mut provider, || {
            let dex = StablecoinDEX::new();
            let slot = dex.orders[9].base_slot;
            StorageCtx.sstore(dex.address, slot, U256::from(3) << 248)?;
            dex.orders[9].read()
        });
        assert!(matches!(
            result,
            Err(TempoPrecompileError::Fatal(message))
                if message == "unknown stablecoin DEX order storage version 3"
        ));
    }

    #[test]
    fn t8_reads_mixed_versions_and_field_updates_preserve_layout() {
        let book_key = B256::repeat_byte(0x31);
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);

        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            dex.orders[1].write(test_order(1, book_key))?;
            dex.book_keys.push(book_key)?;
            Result::<()>::Ok(())
        })
        .unwrap();

        provider.set_spec(TempoHardfork::T8);
        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            dex.orders[2].write_in_book(test_order(2, book_key), BookId::UNSET)?;
            dex.orders[3].write_in_book(test_order(3, book_key), BookId::from_index(0))?;

            for (id, key, version) in [
                (1, book_key, OrderVersion::Legacy),
                (2, book_key, OrderVersion::V1),
                (3, book_key, OrderVersion::V2),
            ] {
                assert_eq!(dex.orders[id].read()?.book_key, key);
                assert_eq!(dex.orders[id].version()?, version);
                dex.orders[id].remaining()?.write(777)?;
                dex.orders[id].prev()?.write(88)?;
                dex.orders[id].next()?.write(99)?;
                let updated = dex.orders[id].read()?;
                assert_eq!(
                    (updated.remaining, updated.prev, updated.next),
                    (777, 88, 99)
                );
                assert_eq!(dex.orders[id].version()?, version);
                dex.orders[id].delete()?;
                assert_eq!(dex.orders[id].read()?.maker, Address::ZERO);
            }
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t8_full_write_lazily_migrates_and_clears_legacy_tail() {
        let book_key = B256::repeat_byte(0x41);
        let order = test_order(7, book_key);
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);
        StorageCtx::enter(&mut provider, || {
            StablecoinDEX::new().orders[7].write(order.clone())
        })
        .unwrap();

        provider.set_spec(TempoHardfork::T8);
        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            dex.book_keys.push(book_key)?;
            dex.orders[7].write_in_book(order.clone(), BookId::from_index(0))?;
            assert_eq!(dex.orders[7].version()?, OrderVersion::V2);
            assert_eq!(dex.orders[7].read()?, order);
            for offset in V2Order::SLOTS..Order::SLOTS {
                assert_eq!(
                    StorageCtx.sload(dex.address, dex.orders[7].base_slot + U256::from(offset))?,
                    U256::ZERO
                );
            }
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t8_book_index_selectors_and_conflict_semantics() {
        let admin = Address::repeat_byte(0x51);
        let base = address!("0x20c0000000000000000000000000000000000007");
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);
        let book_key = StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            setup_dex_tokens(&mut dex, admin, base)?;
            Result::<B256>::Ok(compute_book_key(base, PATH_USD_ADDRESS))
        })
        .unwrap();

        let index_call = IStablecoinDEX::bookIndexForKeyCall { bookKey: book_key };
        let key_call = IStablecoinDEX::bookKeyForIndexCall { index: 0 };
        let set_call = IStablecoinDEX::setBookIndexCall { index: 0 };
        for calldata in [
            index_call.abi_encode(),
            key_call.abi_encode(),
            set_call.abi_encode(),
        ] {
            let result = StorageCtx::enter(&mut provider, || {
                StablecoinDEX::new().call(&calldata, Address::ZERO)
            })
            .unwrap();
            assert!(result.reverted);
        }

        provider.set_spec(TempoHardfork::T8);
        for calldata in [
            index_call.abi_encode(),
            key_call.abi_encode(),
            set_call.abi_encode(),
        ] {
            let result = StorageCtx::enter(&mut provider, || {
                StablecoinDEX::new().call(&calldata, Address::ZERO)
            })
            .unwrap();
            assert!(!result.reverted);
        }

        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            assert_eq!(dex.book_key_for_index(0)?, book_key);
            assert_eq!(dex.book_key_index(book_key)?, Some(0));
            dex.set_book_index(0)?;

            dex.book_handle(book_key)
                .write_book_id(BookId::from_index(1))?;
            assert_eq!(dex.set_book_index(0), Err(err_index_already_set()));
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t7_cancelled_order_credits_are_reused_by_same_maker() {
        let admin = Address::repeat_byte(0xd1);
        let alice = Address::repeat_byte(0xd2);
        let base_token = address!("0x20c0000000000000000000000000000000000003");
        let amount = MIN_ORDER_AMOUNT;
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);

        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            setup_dex_tokens(&mut dex, admin, base_token)?;
            dex.set_balance(alice, PATH_USD_ADDRESS, amount)?;

            let order_id = dex.place(alice, base_token, amount, true, 0)?;
            assert_eq!(dex.storage_credits(alice)?, 0);

            dex.cancel(alice, order_id)?;
            let reusable = dex.storage_credits(alice)?;
            assert!(reusable > 0 && reusable <= Order::SLOTS as u64);

            dex.place(alice, base_token, amount, true, 0)?;
            assert_eq!(dex.storage_credits(alice)?, 0);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t7_tail_cancel_credits_cleared_neighbor_slot_to_neighbor_maker() {
        let admin = Address::repeat_byte(0xe1);
        let alice = Address::repeat_byte(0xe2);
        let bob = Address::repeat_byte(0xe3);
        let base_token = address!("0x20c0000000000000000000000000000000000004");
        let amount = MIN_ORDER_AMOUNT;
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);

        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            setup_dex_tokens(&mut dex, admin, base_token)?;
            dex.set_balance(alice, PATH_USD_ADDRESS, amount)?;
            dex.set_balance(bob, PATH_USD_ADDRESS, amount)?;

            let alice_order = dex.place(alice, base_token, amount, true, 0)?;
            let bob_order = dex.place(bob, base_token, amount, true, 0)?;
            assert_eq!(dex.orders[alice_order].read()?.next, bob_order);

            dex.cancel(bob, bob_order)?;
            assert_eq!(dex.orders[alice_order].read()?.next, 0);
            assert_eq!(dex.storage_credits(alice)?, 1);
            assert!(dex.storage_credits(bob)? > 0);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t7_full_fill_credits_deleted_order_to_its_maker() {
        let admin = Address::repeat_byte(0xf1);
        let alice = Address::repeat_byte(0xf2);
        let bob = Address::repeat_byte(0xf3);
        let taker = Address::repeat_byte(0xf4);
        let base_token = address!("0x20c0000000000000000000000000000000000005");
        let amount = MIN_ORDER_AMOUNT;
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);

        StorageCtx::enter(&mut provider, || {
            let mut dex = StablecoinDEX::new();
            setup_dex_tokens(&mut dex, admin, base_token)?;
            grant_and_mint(base_token, admin, alice, amount)?;
            grant_and_mint(base_token, admin, bob, amount)?;
            grant_and_mint(PATH_USD_ADDRESS, admin, taker, amount)?;

            let alice_order = dex.place(alice, base_token, amount, false, 0)?;
            let bob_order = dex.place(bob, base_token, amount, false, 0)?;
            assert_eq!(dex.orders[alice_order].read()?.next, bob_order);

            dex.swap_exact_amount_in(taker, PATH_USD_ADDRESS, base_token, amount, 0)?;
            assert!(dex.storage_credits(alice)? > 0);
            assert_eq!(dex.storage_credits(bob)?, 0);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t7_t8_partial_fill_preserves_layout_and_cancel_deletes_order() {
        let admin = Address::repeat_byte(0x61);
        let maker = Address::repeat_byte(0x62);
        let taker = Address::repeat_byte(0x63);
        let base_token = address!("0x20c0000000000000000000000000000000000006");
        let amount = MIN_ORDER_AMOUNT * 2;
        let fill = MIN_ORDER_AMOUNT;
        for hardfork in [TempoHardfork::T7, TempoHardfork::T8] {
            let mut provider = TestStorageProvider::new(hardfork);

            StorageCtx::enter(&mut provider, || {
                let mut dex = StablecoinDEX::new();
                setup_dex_tokens(&mut dex, admin, base_token)?;
                grant_and_mint(base_token, admin, maker, amount)?;
                grant_and_mint(PATH_USD_ADDRESS, admin, taker, fill)?;

                let order_id = dex.place(maker, base_token, amount, false, 0)?;
                let expected_version = if hardfork.is_t8() {
                    OrderVersion::V2
                } else {
                    OrderVersion::Legacy
                };
                assert_eq!(dex.orders[order_id].version()?, expected_version);
                let base_slot = dex.orders[order_id].base_slot;

                dex.swap_exact_amount_in(taker, PATH_USD_ADDRESS, base_token, fill, 0)?;
                assert_eq!(dex.get_order(order_id)?.remaining, amount - fill);
                assert_eq!(dex.orders[order_id].version()?, expected_version);
                assert_eq!(dex.storage_credits(maker)?, 0);

                dex.cancel(maker, order_id)?;
                assert!(dex.storage_credits(maker)? > 0);
                for offset in 0..Order::SLOTS {
                    assert_eq!(
                        StorageCtx.sload(dex.address, base_slot + U256::from(offset))?,
                        U256::ZERO
                    );
                }
                Result::<()>::Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn same_tick_flip_order_activates_at_t5() {
        let args = (
            1,
            Address::repeat_byte(1),
            B256::repeat_byte(2),
            MIN_ORDER_AMOUNT,
            100,
            true,
            100,
        );
        assert!(
            Order::new_flip(
                args.0,
                args.1,
                args.2,
                args.3,
                args.4,
                args.5,
                args.6,
                TempoHardfork::T4,
            )
            .is_err()
        );
        assert!(
            Order::new_flip(
                args.0,
                args.1,
                args.2,
                args.3,
                args.4,
                args.5,
                args.6,
                TempoHardfork::T5,
            )
            .is_ok()
        );
    }

    #[test]
    fn t5_t7_and_t8_flip_reuse_order_id_without_changing_maker_credits() {
        let admin = Address::repeat_byte(0xa1);
        let alice = Address::repeat_byte(0xa2);
        let bob = Address::repeat_byte(0xb1);
        let base_token = address!("0x20c0000000000000000000000000000000000001");
        let amount = MIN_ORDER_AMOUNT;
        let tick = 100;
        let escrow = base_to_quote(amount, tick, RoundingDirection::Up).unwrap();
        for hardfork in [TempoHardfork::T5, TempoHardfork::T7, TempoHardfork::T8] {
            let mut provider = TestStorageProvider::new(hardfork);

            StorageCtx::enter(&mut provider, || {
                let mut quote = TIP20Token::from_address_unchecked(PATH_USD_ADDRESS);
                quote.initialize(
                    Address::ZERO,
                    "Path USD",
                    "pathUSD",
                    "USD",
                    PATH_USD_ADDRESS,
                    admin,
                )?;
                quote.grant_role(
                    admin,
                    IRolesAuth::grantRoleCall {
                        role: *ISSUER_ROLE,
                        account: admin,
                    },
                )?;
                quote.mint(
                    admin,
                    ITIP20::mintCall {
                        to: alice,
                        amount: U256::from(escrow),
                    },
                )?;

                TIP20Token::from_address_unchecked(base_token).initialize(
                    Address::ZERO,
                    "Base USD",
                    "baseUSD",
                    "USD",
                    PATH_USD_ADDRESS,
                    admin,
                )?;

                let mut dex = StablecoinDEX::new();
                dex.initialize()?;
                dex.create_pair(base_token)?;
                let order_id =
                    dex.place_flip(alice, base_token, amount, true, tick, tick, false)?;
                let next_order_id = dex.next_order_id_val()?;

                dex.set_balance(bob, base_token, amount)?;
                dex.swap_exact_amount_in(bob, base_token, PATH_USD_ADDRESS, amount, 0)?;

                assert_eq!(dex.next_order_id_val()?, next_order_id);
                let flipped = dex.get_order(order_id)?;
                assert_eq!(flipped.order_id, order_id);
                assert_eq!(flipped.maker, alice);
                assert!(!flipped.is_bid);
                assert!(flipped.is_flip);
                assert_eq!(flipped.tick, tick);
                assert_eq!(flipped.flip_tick, tick);
                assert_eq!(flipped.remaining, amount);
                if hardfork.is_t7() {
                    assert_eq!(dex.storage_credits(alice)?, 0);
                }
                Result::<()>::Ok(())
            })
            .unwrap();

            assert!(
                provider
                    .events(STABLECOIN_DEX_ADDRESS)
                    .iter()
                    .any(|event| event.topics()[0] == IStablecoinDEX::OrderFlipped::SIGNATURE_HASH)
            );
        }
    }

    #[test]
    fn t5_failed_flip_emits_reason_and_removes_filled_order() {
        let admin = Address::repeat_byte(0xc1);
        let alice = Address::repeat_byte(0xc2);
        let bob = Address::repeat_byte(0xc3);
        let base_token = address!("0x20c0000000000000000000000000000000000002");
        let amount = MIN_ORDER_AMOUNT;
        let tick = 100;
        let escrow = base_to_quote(amount, tick, RoundingDirection::Up).unwrap();
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);

        let order_id = StorageCtx::enter(&mut provider, || {
            let mut quote = TIP20Token::from_address_unchecked(PATH_USD_ADDRESS);
            quote.initialize(
                Address::ZERO,
                "Path USD",
                "pathUSD",
                "USD",
                PATH_USD_ADDRESS,
                admin,
            )?;
            quote.grant_role(
                admin,
                IRolesAuth::grantRoleCall {
                    role: *ISSUER_ROLE,
                    account: admin,
                },
            )?;
            quote.mint(
                admin,
                ITIP20::mintCall {
                    to: alice,
                    amount: U256::from(escrow),
                },
            )?;

            let mut base = TIP20Token::from_address_unchecked(base_token);
            base.initialize(
                Address::ZERO,
                "Base USD",
                "baseUSD",
                "USD",
                PATH_USD_ADDRESS,
                admin,
            )?;

            let mut dex = StablecoinDEX::new();
            dex.initialize()?;
            dex.create_pair(base_token)?;
            let order_id = dex.place_flip(alice, base_token, amount, true, tick, tick, false)?;

            let mut registry = TIP403Registry::new();
            registry.initialize()?;
            let policy_id = registry.create_policy(
                admin,
                ITIP403Registry::createPolicyCall {
                    admin,
                    policyType: ITIP403Registry::PolicyType::BLACKLIST,
                },
            )?;
            registry.modify_policy_blacklist(
                admin,
                ITIP403Registry::modifyPolicyBlacklistCall {
                    policyId: policy_id,
                    account: alice,
                    restricted: true,
                },
            )?;
            base.change_transfer_policy_id(
                admin,
                ITIP20::changeTransferPolicyIdCall {
                    newPolicyId: policy_id,
                },
            )?;

            dex.set_balance(bob, base_token, amount)?;
            dex.swap_exact_amount_in(bob, base_token, PATH_USD_ADDRESS, amount, 0)?;
            assert!(dex.get_order(order_id).is_err());
            Result::<u128>::Ok(order_id)
        })
        .unwrap();

        let events = provider.events(STABLECOIN_DEX_ADDRESS);
        let failed = events
            .iter()
            .find(|event| event.topics()[0] == IStablecoinDEX::FlipFailed::SIGNATURE_HASH)
            .expect("FlipFailed event");
        let decoded = IStablecoinDEX::FlipFailed::decode_log_data(failed).unwrap();
        assert_eq!(decoded.orderId, order_id);
        assert_eq!(decoded.maker, alice);
        assert_ne!(decoded.reason, FixedBytes::<4>::ZERO);
        assert!(
            !events
                .iter()
                .any(|event| event.topics()[0] == IStablecoinDEX::OrderFlipped::SIGNATURE_HASH)
        );
    }
}
