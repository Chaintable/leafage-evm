//! Read ArbOS pricing values straight from replica state.
//!
//! Nitro keeps all ArbOS internal state in the storage of one fictional account
//! (`types.ArbosStateAddress`), partitioned into subspaces by a keccak "page"
//! scheme (`arbos/storage/storage.go`):
//!
//! ```text
//! subspace.storageKey = keccak256(parent.storageKey ++ id)   // root key is empty
//! slot(offset)        = keccak256(storageKey ++ key[:31])[:31] ++ key[31]
//!                       where key = uint256(offset) big-endian
//! ```
//!
//! Reading these slots via [`revm::DatabaseRef::storage_ref`] is exactly what
//! `eth_getStorageAt` does (the storage wrapper keccak-hashes both the address
//! and the slot before hitting the DB). The layout is an ArbOS global — it does
//! not depend on chain id / DAC mode / gas token — so one implementation serves
//! every Orbit chain. Offsets can only change across an ArbOS *major* version;
//! the layout-check test in the design doc guards that before enabling a chain.

use once_cell::sync::Lazy;
use revm::context::TxEnv;
use revm::context_interface::transaction::Transaction;
use revm::primitives::{address, keccak256, Address, Bytes, B256, U256};
use revm::DatabaseRef;
use std::fmt::Debug;

/// `types.ArbosStateAddress`: the account whose storage holds all ArbOS state.
pub const ARBOS_STATE_ADDRESS: Address = address!("a4b05fffffffffffffffffffffffffffffffffff");
/// Dedicated storage account for delayed-message / transaction filtering state.
pub const FILTERED_TRANSACTIONS_STATE_ADDRESS: Address =
    address!("a4b0500000000000000000000000000000000001");

// Subspace ids (`arbos/arbosState/arbosstate.go`).
pub(crate) const L1_PRICING_SUBSPACE: &[u8] = &[0];
pub(crate) const L2_PRICING_SUBSPACE: &[u8] = &[1];
pub(crate) const RETRYABLE_SUBSPACE: &[u8] = &[2];
pub(crate) const ADDRESS_TABLE_SUBSPACE: &[u8] = &[3];
pub(crate) const CHAIN_OWNER_SUBSPACE: &[u8] = &[4];
pub(crate) const SEND_MERKLE_SUBSPACE: &[u8] = &[5];
pub(crate) const CHAIN_CONFIG_SUBSPACE: &[u8] = &[7];
pub(crate) const PROGRAMS_SUBSPACE: &[u8] = &[8];
pub(crate) const FEATURES_SUBSPACE: &[u8] = &[9];
pub(crate) const NATIVE_TOKEN_OWNER_SUBSPACE: &[u8] = &[10];
pub(crate) const TRANSACTION_FILTERER_SUBSPACE: &[u8] = &[11];
pub(crate) const BATCH_POSTER_TABLE_SUBSPACE: &[u8] = &[0];
pub(crate) const STYLUS_PARAMS_KEY: &[u8] = &[0];
pub(crate) const BATCH_POSTER_ADDRS_KEY: &[u8] = &[0];
pub(crate) const BATCH_POSTER_INFO_KEY: &[u8] = &[1];

// Offsets within their respective storage spaces.
const PRICE_PER_UNIT_OFFSET: u64 = 7; // L1PricingState
const MIN_BASE_FEE_WEI_OFFSET: u64 = 3; // L2PricingState
pub(crate) const BROTLI_COMPRESSION_LEVEL_OFFSET: u64 = 7; // root ArbOS storage
pub(crate) const ARBOS_VERSION_OFFSET: u64 = 0;
pub(crate) const UPGRADE_VERSION_OFFSET: u64 = 1;
pub(crate) const UPGRADE_TIMESTAMP_OFFSET: u64 = 2;
pub(crate) const NETWORK_FEE_ACCOUNT_OFFSET: u64 = 3;
pub(crate) const GENESIS_BLOCK_NUM_OFFSET: u64 = 5;
pub(crate) const INFRA_FEE_ACCOUNT_OFFSET: u64 = 6;
pub(crate) const NATIVE_TOKEN_ENABLED_FROM_TIME_OFFSET: u64 = 8;
pub(crate) const TRANSACTION_FILTERING_ENABLED_FROM_TIME_OFFSET: u64 = 9;
pub(crate) const FILTERED_FUNDS_RECIPIENT_OFFSET: u64 = 10;
pub(crate) const COLLECT_TIPS_OFFSET: u64 = 11;
pub(crate) const L1_PAY_REWARDS_TO_OFFSET: u64 = 0;
pub(crate) const L1_EQUILIBRATION_UNITS_OFFSET: u64 = 1;
pub(crate) const L1_INERTIA_OFFSET: u64 = 2;
pub(crate) const L1_PER_UNIT_REWARD_OFFSET: u64 = 3;
pub(crate) const L1_LAST_UPDATE_TIME_OFFSET: u64 = 4;
pub(crate) const L1_FUNDS_DUE_FOR_REWARDS_OFFSET: u64 = 5;
pub(crate) const L1_UNITS_SINCE_UPDATE_OFFSET: u64 = 6;
pub(crate) const L1_PRICE_PER_UNIT_OFFSET: u64 = PRICE_PER_UNIT_OFFSET;
pub(crate) const L1_LAST_SURPLUS_OFFSET: u64 = 8;
pub(crate) const L1_PER_BATCH_GAS_COST_OFFSET: u64 = 9;
pub(crate) const L1_AMORTIZED_COST_CAP_BIPS_OFFSET: u64 = 10;
pub(crate) const L1_FEES_AVAILABLE_OFFSET: u64 = 11;
pub(crate) const L1_GAS_FLOOR_PER_TOKEN_OFFSET: u64 = 12;
pub(crate) const BATCH_POSTER_TOTAL_FUNDS_DUE_OFFSET: u64 = 0;
pub(crate) const BATCH_POSTER_FUNDS_DUE_OFFSET: u64 = 0;
pub(crate) const BATCH_POSTER_PAY_TO_OFFSET: u64 = 1;
pub(crate) const L2_SPEED_LIMIT_PER_SECOND_OFFSET: u64 = 0;
pub(crate) const L2_PER_BLOCK_GAS_LIMIT_OFFSET: u64 = 1;
pub(crate) const L2_BASE_FEE_WEI_OFFSET: u64 = 2;
pub(crate) const L2_MIN_BASE_FEE_WEI_OFFSET: u64 = MIN_BASE_FEE_WEI_OFFSET;
pub(crate) const L2_GAS_BACKLOG_OFFSET: u64 = 4;
pub(crate) const L2_PRICING_INERTIA_OFFSET: u64 = 5;
pub(crate) const L2_BACKLOG_TOLERANCE_OFFSET: u64 = 6;
pub(crate) const L2_PER_TX_GAS_LIMIT_OFFSET: u64 = 7;
const RETRYABLE_LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbRetryableInfo {
    pub timeout: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub value: U256,
    pub beneficiary: Address,
    pub tries: u64,
    pub data: Bytes,
}

/// `subspace.storageKey = keccak256(parent.storageKey ++ id)`.
fn subspace_key(parent_key: &[u8], id: &[u8]) -> [u8; 32] {
    keccak256([parent_key, id].concat()).0
}

/// `mapAddress`: keep the low byte of the key verbatim, hash the rest with the
/// storage key. `key = uint256(offset)` big-endian.
pub(crate) fn slot_for_key(storage_key: &[u8], key: [u8; 32]) -> U256 {
    let mut input = Vec::with_capacity(storage_key.len() + 31);
    input.extend_from_slice(storage_key);
    input.extend_from_slice(&key[..31]);
    let hashed = keccak256(&input).0;
    let mut slot = [0u8; 32];
    slot[..31].copy_from_slice(&hashed[..31]);
    slot[31] = key[31];
    U256::from_be_bytes::<32>(slot)
}

/// `mapAddress`: keep the low byte of the key verbatim, hash the rest with the
/// storage key. `key = uint256(offset)` big-endian.
pub(crate) fn slot_at(storage_key: &[u8], offset: u64) -> U256 {
    slot_for_key(storage_key, U256::from(offset).to_be_bytes::<32>())
}

pub(crate) fn child_key(parent_key: &[u8], id: &[u8]) -> [u8; 32] {
    subspace_key(parent_key, id)
}

static PRICE_PER_UNIT_SLOT: Lazy<U256> = Lazy::new(|| {
    slot_at(
        &subspace_key(&[], L1_PRICING_SUBSPACE),
        PRICE_PER_UNIT_OFFSET,
    )
});
static MIN_BASE_FEE_SLOT: Lazy<U256> = Lazy::new(|| {
    slot_at(
        &subspace_key(&[], L2_PRICING_SUBSPACE),
        MIN_BASE_FEE_WEI_OFFSET,
    )
});
static BROTLI_LEVEL_SLOT: Lazy<U256> = Lazy::new(|| slot_at(&[], BROTLI_COMPRESSION_LEVEL_OFFSET));
static ARBOS_VERSION_SLOT: Lazy<U256> = Lazy::new(|| slot_at(&[], ARBOS_VERSION_OFFSET));
static COLLECT_TIPS_SLOT: Lazy<U256> = Lazy::new(|| slot_at(&[], COLLECT_TIPS_OFFSET));
static GENESIS_BLOCK_NUM_SLOT: Lazy<U256> = Lazy::new(|| slot_at(&[], GENESIS_BLOCK_NUM_OFFSET));
static NETWORK_FEE_ACCOUNT_SLOT: Lazy<U256> =
    Lazy::new(|| slot_at(&[], NETWORK_FEE_ACCOUNT_OFFSET));
/// The three ArbOS pricing values posterGas estimation needs.
#[derive(Debug, Clone)]
pub struct ArbPricing {
    /// L1 price per calldata unit, in wei (dynamic, updates per block).
    pub price_per_unit: U256,
    /// L2 minimum base fee, in wei (static).
    pub min_base_fee: U256,
    /// Brotli compression level used for L1 posting (static).
    pub brotli_level: u64,
}

pub trait ArbStateReader: DatabaseRef {
    fn read_root(&self, slot: U256) -> Option<U256> {
        self.storage_ref(ARBOS_STATE_ADDRESS, slot).ok()
    }

    /// Read the pricing values from replica state. Returns `None` — meaning no
    /// L1 overhead, a safe degrade — if any slot read fails or `price_per_unit`
    /// is zero (pre-L1-pricing blocks, matching Nitro's early-block semantics).
    fn read_pricing(&self) -> Option<ArbPricing> {
        let price_per_unit = self.read_root(*PRICE_PER_UNIT_SLOT)?;
        if price_per_unit.is_zero() {
            return None;
        }
        let min_base_fee = self.read_root(*MIN_BASE_FEE_SLOT)?;
        let brotli_raw = self.read_root(*BROTLI_LEVEL_SLOT)?;
        // Brotli quality is 0..=11; clamp defensively against an unexpected slot value.
        let brotli_level = u64::try_from(brotli_raw).unwrap_or(0).min(11);
        Some(ArbPricing {
            price_per_unit,
            min_base_fee,
            brotli_level,
        })
    }

    fn arbos_version(&self) -> u64 {
        self.read_root(*ARBOS_VERSION_SLOT)
            .map(|value| value.to::<u64>())
            .unwrap_or_default()
    }

    fn genesis_block_num(&self) -> U256 {
        self.read_root(*GENESIS_BLOCK_NUM_SLOT).unwrap_or_default()
    }

    fn collect_tips(&self) -> bool {
        collect_tips(self)
    }

    fn network_fee_account(&self) -> Option<Address> {
        self.read_root(*NETWORK_FEE_ACCOUNT_SLOT)
            .map(address_from_word)
    }

    fn paid_l1_gas_price(&self, tx: &TxEnv, block_base_fee: u64) -> U256 {
        if self.collect_tips() {
            let price = tx.effective_gas_price(block_base_fee as u128);
            if price != 0 {
                return U256::from(price);
            }
        }
        U256::from(block_base_fee)
    }

    fn current_tx_l1_gas_fee(&self, tx: &TxEnv, block_base_fee: u64) -> U256 {
        if block_base_fee == 0 {
            return U256::ZERO;
        }

        let Some(pricing) = self.read_pricing() else {
            return U256::ZERO;
        };

        let paid_gas_price = self.paid_l1_gas_price(tx, block_base_fee);

        pricing.current_tx_l1_fee(tx, paid_gas_price)
    }

    fn current_tx_l1_gas_units(&self, tx: &TxEnv, block_base_fee: u64) -> u64 {
        if block_base_fee == 0 {
            return 0;
        }

        self.read_pricing()
            .map(|pricing| pricing.current_tx_l1_units(tx))
            .unwrap_or_default()
    }

    fn read_retryable_info(&self, ticket_id: B256) -> Result<Option<ArbRetryableInfo>, String>
    where
        Self::Error: Debug,
    {
        read_retryable_info_from_state(self, ticket_id)
    }
}

impl<T: DatabaseRef + ?Sized> ArbStateReader for T {}

fn collect_tips<S: ArbStateReader + ?Sized>(state: &S) -> bool {
    let version = state.arbos_version();
    version == 9
        || (version >= 60
            && state
                .read_root(*COLLECT_TIPS_SLOT)
                .is_some_and(|value| !value.is_zero()))
}

fn read_retryable_info_from_state<S>(
    state: &S,
    ticket_id: B256,
) -> Result<Option<ArbRetryableInfo>, String>
where
    S: DatabaseRef + ?Sized,
    S::Error: Debug,
{
    let retryables_key = child_key(&[], RETRYABLE_SUBSPACE);
    let retryable_key = child_key(&retryables_key, ticket_id.as_slice());
    let timeout = read_u64(state, &retryable_key, 5)?;
    if timeout == 0 {
        return Ok(None);
    }

    let windows_left = read_u64(state, &retryable_key, 6)?;
    let timeout = timeout.saturating_add(windows_left.saturating_mul(RETRYABLE_LIFETIME_SECONDS));
    let tries = read_u64(state, &retryable_key, 0)?;
    let from = read_address(state, &retryable_key, 1)?;
    let to = read_address_or_nil(state, &retryable_key, 2)?;
    let value = read_storage(state, &retryable_key, 3)?;
    let beneficiary = read_address(state, &retryable_key, 4)?;
    let calldata_key = child_key(&retryable_key, &[1]);
    let data = read_bytes(state, &calldata_key)?;

    Ok(Some(ArbRetryableInfo {
        timeout,
        from,
        to,
        value,
        beneficiary,
        tries,
        data,
    }))
}

fn read_key<S: DatabaseRef + ?Sized>(
    state: &S,
    storage_key: &[u8],
    key: [u8; 32],
) -> Result<U256, String>
where
    S::Error: Debug,
{
    state
        .storage_ref(ARBOS_STATE_ADDRESS, slot_for_key(storage_key, key))
        .map_err(|err| format!("{err:?}"))
}

fn read_storage<S: DatabaseRef + ?Sized>(
    state: &S,
    storage_key: &[u8],
    offset: u64,
) -> Result<U256, String>
where
    S::Error: Debug,
{
    read_key(state, storage_key, U256::from(offset).to_be_bytes())
}

fn read_u64<S: DatabaseRef + ?Sized>(
    state: &S,
    storage_key: &[u8],
    offset: u64,
) -> Result<u64, String>
where
    S::Error: Debug,
{
    Ok(read_storage(state, storage_key, offset)?.to::<u64>())
}

fn read_address<S: DatabaseRef + ?Sized>(
    state: &S,
    storage_key: &[u8],
    offset: u64,
) -> Result<Address, String>
where
    S::Error: Debug,
{
    Ok(address_from_word(read_storage(state, storage_key, offset)?))
}

fn read_address_or_nil<S: DatabaseRef + ?Sized>(
    state: &S,
    storage_key: &[u8],
    offset: u64,
) -> Result<Option<Address>, String>
where
    S::Error: Debug,
{
    let value = read_storage(state, storage_key, offset)?;
    if value == (U256::from(1u8) << 255) {
        return Ok(None);
    }
    Ok(Some(address_from_word(value)))
}

fn read_bytes<S: DatabaseRef + ?Sized>(state: &S, storage_key: &[u8]) -> Result<Bytes, String>
where
    S::Error: Debug,
{
    let size = read_u64(state, storage_key, 0)?;
    let mut bytes = Vec::new();
    let mut bytes_left = size;
    let mut offset = 1;

    while bytes_left >= 32 {
        let word = read_storage(state, storage_key, offset)?;
        bytes.extend_from_slice(&word.to_be_bytes::<32>());
        bytes_left -= 32;
        offset += 1;
    }

    let word = read_storage(state, storage_key, offset)?;
    if bytes_left > 0 {
        let encoded = word.to_be_bytes::<32>();
        bytes.extend_from_slice(&encoded[32 - bytes_left as usize..]);
    }

    Ok(bytes.into())
}

pub fn read_pricing<S: ArbStateReader + ?Sized>(state: &S) -> Option<ArbPricing> {
    state.read_pricing()
}

pub fn current_tx_l1_gas_fee<S: ArbStateReader + ?Sized>(
    state: &S,
    tx: &TxEnv,
    block_base_fee: u64,
) -> U256 {
    state.current_tx_l1_gas_fee(tx, block_base_fee)
}

pub fn genesis_block_num<S: ArbStateReader + ?Sized>(state: &S) -> U256 {
    state.genesis_block_num()
}

pub fn arbos_version<S: ArbStateReader + ?Sized>(state: &S) -> u64 {
    state.arbos_version()
}

pub fn read_retryable_info<S>(
    state: &S,
    ticket_id: B256,
) -> Result<Option<ArbRetryableInfo>, String>
where
    S: ArbStateReader + ?Sized,
    S::Error: Debug,
{
    state.read_retryable_info(ticket_id)
}

fn address_from_word(word: U256) -> Address {
    let bytes = word.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slots derived here must equal the values read live off the writer
    /// (kava-1, 2026-05-29; see design-doc appendix). These vectors also pin the
    /// subspace ids / offsets for Robinhood's ArbOS version.
    #[test]
    fn slot_derivation_matches_live_vectors() {
        let expect = |hex: &str| U256::from_str_radix(hex, 16).unwrap();
        assert_eq!(
            *PRICE_PER_UNIT_SLOT,
            expect("a9f6f085d78d1d37c5819e5c16c9e03198bd14e08cd1f6f8191bc6207b9e9707"),
            "pricePerUnit slot (L1Pricing subspace, offset 7)"
        );
        assert_eq!(
            *MIN_BASE_FEE_SLOT,
            expect("e54de2a4cdacc0a0059d2b6e16348103df8c4aff409c31e40ec73d11926c8203"),
            "minBaseFeeWei slot (L2Pricing subspace, offset 3)"
        );
        assert_eq!(
            *BROTLI_LEVEL_SLOT,
            expect("15fed0451499512d95f3ec5a41c878b9de55f21878b5b4e190d4667ec709b407"),
            "brotliCompressionLevel slot (root, offset 7)"
        );
    }

    /// The low byte of the offset is carried into the slot verbatim.
    #[test]
    fn slot_keeps_offset_low_byte() {
        let slot = slot_at(&[], 7);
        assert_eq!((slot & U256::from(0xff)).to::<u64>(), 7);
    }
}
