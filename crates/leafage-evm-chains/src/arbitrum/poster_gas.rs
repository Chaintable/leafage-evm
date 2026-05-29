//! Port of Nitro's gas-estimation posterGas (the path `eth_estimateGas`
//! exercises), from `arbos/l1pricing/l1pricing.go` + `arbos/tx_processor.go`
//! (nitro commit `e8fa8e05a`).
//!
//! Nitro expresses the L1 calldata cost of posting a tx as an equivalent number
//! of L2 gas units. During estimation it builds a *fake* tx from the message,
//! brotli-compresses it, and pads conservatively. We replicate that here:
//!
//! ```text
//! l1_bytes = brotli(level, marshal_2718(fake_tx))
//! units    = (16 * l1_bytes + 256) * 1.01
//! cost_wei = price_per_unit * units * 1.10
//! price    = max(l2_base_fee * 7/8, min_base_fee)
//! posterGas = cost_wei / price            // 0 if price == 0
//! ```

use super::arbos_state::ArbPricing;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{keccak256, Signature, U256};
use once_cell::sync::Lazy;
use revm::context::TxEnv;

/// EIP-2028 non-zero calldata byte gas; Nitro charges this per compressed byte.
const TX_DATA_NON_ZERO_GAS: u64 = 16;
/// Estimation unit padding (`l1pricing.go`): `16 * 16` units added before scaling.
const ESTIMATION_PADDING_UNITS: u64 = 16 * TX_DATA_NON_ZERO_GAS;
/// Unit padding factor: `OneInBips(10000) + 100` → ×1.01.
const UNITS_PADDING_BIPS: u128 = 10_100;
/// `GasEstimationL1PricePadding` (`tx_processor.go:33`): ×1.10.
const PRICE_PADDING_BIPS: u64 = 11_000;
const ONE_IN_BIPS: u64 = 10_000;

// Fixed fields for the gas-estimation fake tx (`l1pricing.go:560-566`). The tx is
// intentionally invalid; only its compressed byte count matters, so these merely
// have to reproduce Nitro's byte layout. ChainID is left unset (0), matching
// `makeFakeTxForMessage`.
static RANDOM_NONCE: Lazy<u64> =
    Lazy::new(|| u64::from_be_bytes(keccak256("Nonce").0[..8].try_into().unwrap()));
static RANDOM_GAS_TIP_CAP: Lazy<u128> =
    Lazy::new(|| u32::from_be_bytes(keccak256("GasTipCap").0[..4].try_into().unwrap()) as u128);
static RANDOM_GAS_FEE_CAP: Lazy<u128> =
    Lazy::new(|| u32::from_be_bytes(keccak256("GasFeeCap").0[..4].try_into().unwrap()) as u128);
static RANDOM_GAS: Lazy<u64> =
    Lazy::new(|| u32::from_be_bytes(keccak256("Gas").0[..4].try_into().unwrap()) as u64);
static RANDOM_R: Lazy<U256> = Lazy::new(|| U256::from_be_bytes::<32>(keccak256("R").0));
static RANDOM_S: Lazy<U256> = Lazy::new(|| U256::from_be_bytes::<32>(keccak256("S").0));

/// EIP-2718 bytes of Nitro's gas-estimation fake tx: the request's
/// to/value/data/access_list, with Nitro's fixed random values for everything
/// else (gas is always `RandomGas` during estimation).
fn fake_tx_bytes(tx: &TxEnv) -> Vec<u8> {
    let nonce = if tx.nonce == 0 {
        *RANDOM_NONCE
    } else {
        tx.nonce
    };
    let tip = tx
        .gas_priority_fee
        .filter(|v| *v != 0)
        .unwrap_or(*RANDOM_GAS_TIP_CAP);
    let fee = if tx.gas_price == 0 {
        *RANDOM_GAS_FEE_CAP
    } else {
        tx.gas_price
    };

    let fake = TxEip1559 {
        chain_id: 0,
        nonce,
        gas_limit: *RANDOM_GAS,
        max_fee_per_gas: fee,
        max_priority_fee_per_gas: tip,
        to: tx.kind,
        value: tx.value,
        access_list: tx.access_list.clone(),
        input: tx.data.clone(),
    };
    let sig = Signature::new(*RANDOM_R, *RANDOM_S, false);
    let mut buf = Vec::new();
    fake.into_signed(sig).encode_2718(&mut buf);
    buf
}

fn brotli_len(input: &[u8], level: u64) -> Option<usize> {
    use brotli::enc::BrotliEncoderParams;
    let params = BrotliEncoderParams {
        quality: level as i32,
        ..Default::default()
    };
    let mut out = Vec::new();
    let mut reader = input;
    brotli::BrotliCompress(&mut reader, &mut out, &params).ok()?;
    Some(out.len())
}

/// Turn a compressed-byte count into posterGas (the L1-cost-as-L2-gas value).
/// Separated from compression so the arithmetic is deterministic and testable.
fn poster_gas_from_l1_bytes(l1_bytes: u64, l2_base_fee: u64, pricing: &ArbPricing) -> u64 {
    let raw_units = TX_DATA_NON_ZERO_GAS.saturating_mul(l1_bytes);
    let units = (raw_units as u128 + ESTIMATION_PADDING_UNITS as u128) * UNITS_PADDING_BIPS
        / ONE_IN_BIPS as u128;

    let cost = pricing.price_per_unit.saturating_mul(U256::from(units));
    let cost = cost.saturating_mul(U256::from(PRICE_PADDING_BIPS)) / U256::from(ONE_IN_BIPS);

    // gas price basis: the block base fee (see docs/todo.md for why not tx price),
    // reduced to 7/8 to simulate congestion, floored at the L2 minimum base fee.
    let mut price = U256::from(l2_base_fee).saturating_mul(U256::from(7u64)) / U256::from(8u64);
    if price < pricing.min_base_fee {
        price = pricing.min_base_fee;
    }
    if price.is_zero() {
        return 0;
    }

    u64::try_from(cost / price).unwrap_or(u64::MAX)
}

/// Replicate Nitro's gas-estimation posterGas for `tx`. `l2_base_fee` is the
/// block base fee. Returns 0 if the fake tx can't be compressed.
pub fn compute(tx: &TxEnv, l2_base_fee: u64, pricing: &ArbPricing) -> u64 {
    let l1_bytes = match brotli_len(&fake_tx_bytes(tx), pricing.brotli_level) {
        Some(n) => n as u64,
        None => return 0,
    };
    poster_gas_from_l1_bytes(l1_bytes, l2_base_fee, pricing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::{Bytes, TxKind};

    fn robinhood_pricing() -> ArbPricing {
        ArbPricing {
            // live values @ writer block 21827 (design-doc appendix)
            price_per_unit: U256::from(0x13266409u64), // 321_676_297
            min_base_fee: U256::from(0x05f5e100u64),   // 100_000_000 = 0.1 gwei
            brotli_level: 1,
        }
    }

    /// Hand-computed against the Nitro formula for a fixed compressed size
    /// (price_per_unit = 0x13266409 = 321_283_081).
    ///   units = (16*100 + 256) * 10100/10000 = 1856*10100/10000 = 1874
    ///   cost  = 321_283_081 * 1874 = 602_084_493_794
    ///   cost  = cost * 11000/10000 = 662_292_943_173
    ///   price = max(1e8 * 7/8, 1e8) = 1e8
    ///   gas   = 662_292_943_173 / 1e8 = 6622
    #[test]
    fn arithmetic_matches_nitro_formula() {
        let gas = poster_gas_from_l1_bytes(100, 100_000_000, &robinhood_pricing());
        assert_eq!(gas, 6622);
    }

    /// Base fee at/below the floor uses `min_base_fee`; the 7/8 reduction only
    /// bites above it. base_fee=0 must give the same gas as base_fee=floor.
    #[test]
    fn price_is_floored_at_min_base_fee() {
        let p = robinhood_pricing();
        let floored = poster_gas_from_l1_bytes(100, 100_000_000, &p);
        // base_fee at/below the floor uses min_base_fee: base_fee=0 == base_fee=floor.
        assert_eq!(poster_gas_from_l1_bytes(100, 0, &p), floored);
        // 1 gwei base fee: price = max(1e9*7/8, 1e8) = 875_000_000 → smaller gas.
        let high = poster_gas_from_l1_bytes(100, 1_000_000_000, &p);
        assert!(
            high < floored,
            "higher base fee → smaller posterGas, got {high}"
        );
    }

    /// A zero gas price yields gas 0 (cannot divide), matching Nitro's guard.
    #[test]
    fn zero_price_yields_zero() {
        let p = ArbPricing {
            price_per_unit: U256::from(1u64),
            min_base_fee: U256::ZERO,
            brotli_level: 1,
        };
        assert_eq!(poster_gas_from_l1_bytes(100, 0, &p), 0);
    }

    fn sample_tx(data: Vec<u8>) -> TxEnv {
        let mut tx = TxEnv::default();
        tx.kind = TxKind::Call(revm::primitives::Address::repeat_byte(0x11));
        tx.data = Bytes::from(data);
        tx
    }

    /// Fake tx is an EIP-1559 (type 0x02) envelope and its size tracks the
    /// calldata — the dominant input to compression.
    #[test]
    fn fake_tx_is_eip1559_and_tracks_calldata() {
        let small = fake_tx_bytes(&sample_tx(vec![0u8; 4]));
        let large = fake_tx_bytes(&sample_tx(vec![0u8; 4096]));
        assert_eq!(small[0], 0x02, "EIP-2718 type byte for EIP-1559");
        assert_eq!(large[0], 0x02);
        assert!(large.len() > small.len());
    }

    /// posterGas is non-zero end-to-end for a realistic call and scales up with
    /// calldata size.
    #[test]
    fn compute_is_nonzero_and_monotonic_in_calldata() {
        let p = robinhood_pricing();
        let small = compute(&sample_tx(vec![0xabu8; 100]), 100_000_000, &p);
        let large = compute(&sample_tx(vec![0xabu8; 10_000]), 100_000_000, &p);
        assert!(small > 0);
        assert!(large > small);
    }
}
