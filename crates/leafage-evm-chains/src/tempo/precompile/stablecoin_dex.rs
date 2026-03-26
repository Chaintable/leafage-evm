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

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use std::collections::HashSet;

use super::error::{Result, TempoPrecompileError};
use super::fee_manager::validate_usd_currency;
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_types::{
    Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType, VecHandler,
};
use super::tip20::{is_tip20_prefix, TIP20Token};
use super::tip20_factory::TIP20Factory;
use super::tip403_registry::{is_policy_lookup_error, AuthRole, TIP403Registry};
use super::{
    fill_precompile_output, input_cost, mutate, mutate_void, view, Precompile,
    STABLECOIN_DEX_ADDRESS, PATH_USD_ADDRESS,
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
struct TickLevel {
    head: u128,
    tail: u128,
    total_liquidity: u128,
}

impl TickLevel {
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
#[derive(Debug, Clone)]
struct Order {
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
    ) -> Result<Self> {
        if is_bid && flip_tick <= tick {
            return Err(err_invalid_flip_tick());
        }
        if !is_bid && flip_tick >= tick {
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

    #[allow(dead_code)]
    fn create_flipped_order(&self, new_order_id: u128) -> Result<Self> {
        if !self.is_flip {
            return Err(err_order_does_not_exist()); // not a flip order
        }
        if self.remaining != 0 {
            return Err(err_order_does_not_exist()); // not fully filled
        }
        Ok(Self {
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
        })
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
struct OrderbookData {
    base: Address,
    quote: Address,
    best_bid_tick: i16,
    best_ask_tick: i16,
}

impl Default for OrderbookData {
    fn default() -> Self {
        Self {
            base: Address::ZERO,
            quote: Address::ZERO,
            best_bid_tick: i16::MIN,
            best_ask_tick: i16::MAX,
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
        }
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

        Ok(OrderbookData {
            base,
            quote,
            best_bid_tick,
            best_ask_tick,
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
    orders: Mapping<u128, Order>,
    // Slot 2: balances (Mapping<Address, Mapping<Address, u128>>)
    balances: Mapping<Address, Mapping<Address, u128>>,
    // Slot 3: next_order_id
    next_order_id: Slot<u128>,
    // Slot 4: book_keys
    book_keys: VecHandler<B256>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl StablecoinDEX {
    pub fn new() -> Self {
        let address = STABLECOIN_DEX_ADDRESS;
        Self {
            books_slot: U256::from(0),
            orders: Mapping::new(U256::from(1), address),
            balances: Mapping::new(U256::from(2), address),
            next_order_id: Slot::new(U256::from(3), address),
            book_keys: VecHandler::new(U256::from(4), address),
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
        self.storage
            .emit_event(self.address, event.into_log_data())
    }

    /// Initializes the stablecoin DEX precompile.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
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

    /// Transfer tokens from the DEX to `to` via TIP20 system_transfer_from.
    fn transfer(&mut self, token: Address, to: Address, amount: u128) -> Result<()> {
        TIP20Token::from_address(token)?.system_transfer_from(
            STABLECOIN_DEX_ADDRESS,
            to,
            U256::from(amount),
        )?;
        Ok(())
    }

    /// Transfer tokens from `from` to the DEX via TIP20 system_transfer_from.
    fn transfer_from(&mut self, token: Address, from: Address, amount: u128) -> Result<()> {
        TIP20Token::from_address(token)?.system_transfer_from(
            from,
            STABLECOIN_DEX_ADDRESS,
            U256::from(amount),
        )?;
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
        let mut handle = self.book_handle(book_key);

        if handle.read_data()?.is_initialized() {
            return Err(err_pair_already_exists());
        }

        let book = OrderbookData::new(base, quote);
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

        let mut handle = self.book_handle(book_key);
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

        TIP20Token::from_address(non_escrow_token)?
            .ensure_transfer_authorized(self.address, sender)?;
        self.decrement_balance_or_transfer_from(sender, escrow_token, escrow_amount)?;

        let order_id = self.next_order_id_val()?;
        self.increment_next_order_id()?;
        let order = if is_bid {
            Order::new_bid(order_id, sender, book_key, amount, tick)
        } else {
            Order::new_ask(order_id, sender, book_key, amount, tick)
        };
        self.commit_order_to_book(order)?;

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
    fn commit_order_to_book(&mut self, mut order: Order) -> Result<()> {
        let mut handle = self.book_handle(order.book_key);
        let orderbook = handle.read_data()?;
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
            let mut prev_order = self.orders[prev_tail].read()?;
            prev_order.next = order.order_id;
            self.orders[prev_tail].write(prev_order)?;

            order.prev = prev_tail;
            level.tail = order.order_id;
        }

        let new_liquidity = level
            .total_liquidity
            .checked_add(order.remaining)
            .ok_or(TempoPrecompileError::under_overflow())?;
        level.total_liquidity = new_liquidity;

        handle.write_tick_level(order.tick, order.is_bid, level)?;
        self.orders[order.order_id].write(order)
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

        let mut handle = self.book_handle(book_key);
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
        if (is_bid && flip_tick <= tick) || (!is_bid && flip_tick >= tick) {
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

        TIP20Token::from_address(non_escrow_token)?
            .ensure_transfer_authorized(self.address, sender)?;

        if internal_balance_only {
            TIP20Token::from_address(escrow_token)?
                .ensure_transfer_authorized(sender, self.address)?;
            let user_balance = self.balance_of(sender, escrow_token)?;
            if user_balance < escrow_amount {
                return Err(err_insufficient_balance());
            }
            self.sub_balance(sender, escrow_token, escrow_amount)?;
        } else {
            self.decrement_balance_or_transfer_from(sender, escrow_token, escrow_amount)?;
        }

        let order_id = self.next_order_id_val()?;
        let order = Order::new_flip(order_id, sender, book_key, amount, tick, is_bid, flip_tick)?;

        self.next_order_id.write(order_id + 1)?;
        self.commit_order_to_book(order)?;

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
        let mut stored = self.orders[order.order_id].read()?;
        stored.remaining = new_remaining;
        self.orders[order.order_id].write(stored)?;

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
            if let Err(e) = self.place_flip(
                order.maker,
                orderbook.base,
                order.amount,
                !order.is_bid,
                order.flip_tick,
                order.tick,
                true,
            ) {
                if e.is_system_error() {
                    return Err(e);
                }
                // Business logic errors are swallowed for flip orders
            }
        }

        self.orders[order.order_id].delete()?;

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
                let new_order = self.orders[new_level.head].read()?;
                Some((new_level, new_order))
            }
        } else {
            level.head = order.next;
            let mut next_order = self.orders[order.next].read()?;
            next_order.prev = 0;
            self.orders[order.next].write(next_order)?;

            let new_liquidity = level
                .total_liquidity
                .checked_sub(fill_amount)
                .ok_or(TempoPrecompileError::under_overflow())?;
            level.total_liquidity = new_liquidity;

            handle.write_tick_level(order.tick, order.is_bid, level)?;
            let new_order = self.orders[order.next].read()?;
            Some((level, new_order))
        };

        Ok((amount_out, next_tick_info))
    }

    fn get_best_price_level(&self, book_key: B256, is_bid: bool) -> Result<TickLevel> {
        let mut handle = self.book_handle(book_key);
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
        book_key: B256,
        bid: bool,
        mut amount_in: u128,
        taker: Address,
    ) -> Result<u128> {
        let mut level = self.get_best_price_level(book_key, bid)?;
        let mut order = self.orders[level.head].read()?;
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
                    self.fill_order(book_key, &mut order, level, taker)?;
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
        book_key: B256,
        bid: bool,
        mut amount_out: u128,
        taker: Address,
    ) -> Result<u128> {
        let mut level = self.get_best_price_level(book_key, bid)?;
        let mut order = self.orders[level.head].read()?;
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
                    self.fill_order(book_key, &mut order, level, taker)?;
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
        let mut handle = self.book_handle(book_key);
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
        let mut handle = self.book_handle(book_key);
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
                let base_needed =
                    quote_to_base(remaining_out, current_tick, RoundingDirection::Up)
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
            let mut handle = self.book_handle(book_key);
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
        for (book_key, base_for_quote) in route {
            amount = self.fill_orders_exact_in(book_key, base_for_quote, amount, sender)?;
        }

        if amount < min_amount_out {
            return Err(err_insufficient_output());
        }

        self.transfer(token_out, sender, amount)?;
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
        for (book_key, base_for_quote) in route.iter().rev() {
            amount = self.fill_orders_exact_out(*book_key, *base_for_quote, amount, sender)?;
        }

        if amount > max_amount_in {
            return Err(err_max_input_exceeded());
        }

        self.decrement_balance_or_transfer_from(sender, token_in, amount)?;
        self.transfer(token_out, sender, amount_out)?;
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
            let mut prev = self.orders[order.prev].read()?;
            prev.next = order.next;
            self.orders[order.prev].write(prev)?;
        } else {
            level.head = order.next;
        }

        if order.next != 0 {
            let mut next = self.orders[order.next].read()?;
            next.prev = order.prev;
            self.orders[order.next].write(next)?;
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
            let quote_amount =
                base_to_quote(order.remaining, order.tick, RoundingDirection::Up)
                    .ok_or(TempoPrecompileError::under_overflow())?;
            self.increment_balance(order.maker, orderbook.quote, quote_amount)?;
        } else {
            self.increment_balance(order.maker, orderbook.base, order.remaining)?;
        }

        self.orders[order.order_id].delete()?;

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

        let mut handle = self.book_handle(order.book_key);
        let book = handle.read_data()?;
        let token = if order.is_bid {
            book.quote
        } else {
            book.base
        };

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

fn dispatch_call<T>(
    calldata: &[u8],
    decode: impl FnOnce(&[u8]) -> core::result::Result<T, alloy::sol_types::Error>,
    f: impl FnOnce(T) -> PrecompileResult,
) -> PrecompileResult {
    let storage = StorageCtx::default();

    if calldata.len() < 4 {
        return Ok(fill_precompile_output(
            PrecompileOutput::new_reverted(0, Bytes::new()),
            &storage,
        ));
    }

    let result = decode(calldata);

    match result {
        Ok(call) => f(call).map(|res| fill_precompile_output(res, &storage)),
        Err(alloy::sol_types::Error::UnknownSelector { selector, .. }) => {
            unknown_selector(*selector, storage.gas_used())
                .map(|res| fill_precompile_output(res, &storage))
        }
        Err(_) => Ok(fill_precompile_output(
            PrecompileOutput::new_reverted(0, Bytes::new()),
            &storage,
        )),
    }
}

fn unknown_selector(selector: [u8; 4], gas: u64) -> PrecompileResult {
    TempoPrecompileError::UnknownFunctionSelector(selector).into_precompile_result(gas)
}

impl Precompile for StablecoinDEX {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            IStablecoinDEX::IStablecoinDEXCalls::abi_decode,
            |call| match call {
                IStablecoinDEX::IStablecoinDEXCalls::place(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.place(s, c.token, c.amount, c.isBid, c.tick)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::placeFlip(call) => {
                    mutate(call, msg_sender, |s, c| {
                        self.place_flip(s, c.token, c.amount, c.isBid, c.tick, c.flipTick, false)
                    })
                }
                IStablecoinDEX::IStablecoinDEXCalls::balanceOf(call) => {
                    view(call, |c| self.balance_of(c.user, c.token))
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
                    mutate(call, msg_sender, |_, c| self.create_pair(c.base))
                }
                IStablecoinDEX::IStablecoinDEXCalls::withdraw(call) => {
                    mutate_void(call, msg_sender, |s, c| self.withdraw(s, c.token, c.amount))
                }
                IStablecoinDEX::IStablecoinDEXCalls::cancel(call) => {
                    mutate_void(call, msg_sender, |s, c| self.cancel(s, c.orderId))
                }
                IStablecoinDEX::IStablecoinDEXCalls::cancelStaleOrder(call) => {
                    mutate_void(call, msg_sender, |_, c| self.cancel_stale_order(c.orderId))
                }
                IStablecoinDEX::IStablecoinDEXCalls::swapExactAmountIn(call) => {
                    mutate(call, msg_sender, |s, c| {
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
                IStablecoinDEX::IStablecoinDEXCalls::MIN_TICK(call) => {
                    view(call, |_| Ok(MIN_TICK))
                }
                IStablecoinDEX::IStablecoinDEXCalls::MAX_TICK(call) => {
                    view(call, |_| Ok(MAX_TICK))
                }
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
