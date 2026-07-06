//! Port of Nitro's fake-transaction L1 poster gas paths, from
//! `arbos/l1pricing/l1pricing.go` + `arbos/tx_processor.go` (nitro commit
//! `e8fa8e05a`).
//!
//! Nitro expresses the L1 calldata cost of posting a tx as an equivalent number
//! of L2 gas units. For gas-estimation paths, it builds a fake dynamic-fee tx,
//! brotli-compresses it, pads the units by 1.01, adds an additional 1.10 cost
//! padding, and uses the 7/8 gas-price adjustment:
//!
//! ```text
//! l1_bytes = brotli(level, marshal_2718(fake_tx))
//! units    = (16 * l1_bytes + 256) * 1.01
//! cost_wei = price_per_unit * units
//! estimate_cost_wei = cost_wei * 1.10
//! price    = max(l2_base_fee * 7/8, min_base_fee)
//! posterGas = estimate_cost_wei / price   // 0 if price == 0
//! ```

use crate::arbitrum::arbos_state::ArbPricing;
use alloy::primitives::{keccak256, U256};
use alloy_rlp::Encodable;
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
const LEGACY_TX_TYPE: u8 = 0;
const ACCESS_LIST_TX_TYPE: u8 = 1;

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
/// Nitro's fake-tx `V` (`l1pricing.go:564`): `randV = ArbitrumOne chainId (42161) * 3
/// = 126483`, hardcoded to arb1 — NOT the current Orbit chain's id. It RLP-encodes to
/// 4 bytes (`0x83 01EE13`), vs a normal EIP-1559 signature's 1-byte `y_parity`, so
/// reproducing it keeps the fake tx the same size as Nitro's before brotli.
const FAKE_SIG_V: u64 = 126_483;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArbPosterCharge {
    pub poster_gas: u64,
    pub poster_fee: U256,
    pub calldata_units: u64,
    pub paid_gas_price: U256,
}

/// EIP-2718 bytes of Nitro's fake tx: the request's
/// to/value/data/access_list, with Nitro's fixed random values for everything else
/// (gas is always `RandomGas` during estimation). Hand-rolled RLP rather than alloy's
/// signed encoding, because Nitro's `V` is a 4-byte integer ([`FAKE_SIG_V`]) that
/// alloy's bool-parity `Signature` can't express — and the byte count feeds brotli, so
/// it must match Nitro exactly.
fn fake_tx_bytes(tx: &TxEnv, force_random_gas: bool) -> Vec<u8> {
    let chain_id: u64 = 0; // Nitro leaves the fake tx's ChainID unset (encodes as 0)
    let nonce: u64 = if tx.nonce == 0 {
        *RANDOM_NONCE
    } else {
        tx.nonce
    };
    let tip: u128 = tx
        .gas_priority_fee
        .filter(|v| *v != 0)
        .or_else(|| {
            (matches!(tx.tx_type, LEGACY_TX_TYPE | ACCESS_LIST_TX_TYPE) && tx.gas_price != 0)
                .then_some(tx.gas_price)
        })
        .unwrap_or(*RANDOM_GAS_TIP_CAP);
    let fee: u128 = if tx.gas_price == 0 {
        *RANDOM_GAS_FEE_CAP
    } else {
        tx.gas_price
    };
    let gas: u64 = if force_random_gas || tx.gas_limit == 0 {
        *RANDOM_GAS
    } else {
        tx.gas_limit
    };
    let r: U256 = *RANDOM_R;
    let s: U256 = *RANDOM_S;

    // 0x02 || rlp([chainId, nonce, maxPrioFee, maxFee, gas, to, value, input,
    //              accessList, v, r, s]) — geth DynamicFeeTx MarshalBinary layout.
    let payload_len = chain_id.length()
        + nonce.length()
        + tip.length()
        + fee.length()
        + gas.length()
        + tx.kind.length()
        + tx.value.length()
        + tx.data.length()
        + tx.access_list.length()
        + FAKE_SIG_V.length()
        + r.length()
        + s.length();

    let mut out = Vec::with_capacity(payload_len + 8);
    out.push(0x02); // EIP-2718 type byte (EIP-1559)
    alloy_rlp::Header {
        list: true,
        payload_length: payload_len,
    }
    .encode(&mut out);
    chain_id.encode(&mut out);
    nonce.encode(&mut out);
    tip.encode(&mut out);
    fee.encode(&mut out);
    gas.encode(&mut out);
    tx.kind.encode(&mut out);
    tx.value.encode(&mut out);
    tx.data.encode(&mut out);
    tx.access_list.encode(&mut out);
    FAKE_SIG_V.encode(&mut out);
    r.encode(&mut out);
    s.encode(&mut out);
    out
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

impl ArbPricing {
    /// Replicate Nitro's gas-estimation posterGas for `tx`. `l2_base_fee` is the
    /// block base fee. Returns 0 if the fake tx can't be compressed.
    pub fn poster_gas(&self, tx: &TxEnv, l2_base_fee: u64) -> u64 {
        let cost = match self.poster_data_cost(tx, true) {
            Some(cost) => self.pad_estimation_cost(cost),
            None => return 0,
        };
        self.poster_gas_from_cost(cost, U256::from(l2_base_fee), true)
    }

    /// Replicate Nitro's `NodeInterface.gasEstimateL1Component` conversion. It
    /// uses the fake tx with random gas and L1 price padding, but divides by the
    /// current L2 base fee directly instead of applying gas-estimation's 7/8
    /// price adjustment.
    pub fn gas_estimate_l1_component(&self, tx: &TxEnv, l2_base_fee: u64) -> u64 {
        let cost = match self.poster_data_cost(tx, true) {
            Some(cost) => self.pad_estimation_cost(cost),
            None => return 0,
        };
        self.poster_gas_from_cost(cost, U256::from(l2_base_fee), false)
    }

    /// Compute `GetCurrentTxL1GasFees` for an eth_call-style message. This
    /// mirrors Nitro's `GasChargingHook`: fake tx units are padded by
    /// `PosterDataCost`, but `GetPosterGas` does not apply gas-estimation price
    /// or cost padding for eth_call run mode.
    pub(crate) fn current_tx_l1_fee(&self, tx: &TxEnv, paid_gas_price: U256) -> U256 {
        if paid_gas_price.is_zero() {
            return U256::ZERO;
        }

        let cost = match self.poster_data_cost(tx, false) {
            Some(cost) => cost,
            None => return U256::ZERO,
        };
        let poster_gas = u64::try_from(cost / paid_gas_price).unwrap_or(u64::MAX);
        paid_gas_price.saturating_mul(U256::from(poster_gas))
    }

    /// Units added to `UnitsSinceUpdate` by Nitro's `GasChargingHook` before
    /// executing an eth_call-style message.
    pub(crate) fn current_tx_l1_units(&self, tx: &TxEnv) -> u64 {
        self.poster_data_units(tx, false).unwrap_or_default()
    }

    /// Compute the transaction-local values Nitro's `GasChargingHook` exposes
    /// through ArbGasInfo during call execution. Leafage runs this uniformly for
    /// call and estimate paths so RPC gas estimation does not need a separate
    /// `estimate_l1_overhead` pass.
    pub fn gas_charging_charge(&self, tx: &TxEnv, paid_gas_price: U256) -> ArbPosterCharge {
        let calldata_units = self.poster_data_units(tx, true).unwrap_or_default();
        if paid_gas_price.is_zero() || calldata_units == 0 {
            return ArbPosterCharge {
                calldata_units,
                paid_gas_price,
                ..Default::default()
            };
        }

        let poster_cost = self
            .price_per_unit
            .saturating_mul(U256::from(calldata_units));
        let poster_cost = self.pad_estimation_cost(poster_cost);

        let poster_gas = self.poster_gas_from_cost(poster_cost, paid_gas_price, true);
        let poster_fee = paid_gas_price.saturating_mul(U256::from(poster_gas));

        ArbPosterCharge {
            poster_gas,
            poster_fee,
            calldata_units,
            paid_gas_price,
        }
    }

    /// Turn L1 poster cost into posterGas (the L1-cost-as-L2-gas value).
    fn poster_gas_from_cost(&self, cost: U256, l2_gas_price: U256, adjust_price: bool) -> u64 {
        let mut price = l2_gas_price;
        if adjust_price {
            price = price.saturating_mul(U256::from(7u64)) / U256::from(8u64);
            if price < self.min_base_fee {
                price = self.min_base_fee;
            }
        }
        if price.is_zero() {
            return 0;
        }

        u64::try_from(cost / price).unwrap_or(u64::MAX)
    }

    fn poster_data_cost(&self, tx: &TxEnv, force_random_gas: bool) -> Option<U256> {
        Some(
            self.price_per_unit
                .saturating_mul(U256::from(self.poster_data_units(tx, force_random_gas)?)),
        )
    }

    fn poster_data_units(&self, tx: &TxEnv, force_random_gas: bool) -> Option<u64> {
        let l1_bytes = brotli_len(&fake_tx_bytes(tx, force_random_gas), self.brotli_level)? as u64;
        let raw_units = TX_DATA_NON_ZERO_GAS.saturating_mul(l1_bytes);
        let units = (raw_units as u128 + ESTIMATION_PADDING_UNITS as u128) * UNITS_PADDING_BIPS
            / ONE_IN_BIPS as u128;
        Some(units.min(u64::MAX as u128) as u64)
    }

    fn pad_estimation_cost(&self, cost: U256) -> U256 {
        cost.saturating_mul(U256::from(PRICE_PADDING_BIPS)) / U256::from(ONE_IN_BIPS)
    }
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
        let p = robinhood_pricing();
        let cost = p.price_per_unit.saturating_mul(U256::from(1874u64));
        let gas = p.poster_gas_from_cost(
            p.pad_estimation_cost(cost),
            U256::from(100_000_000u64),
            true,
        );
        assert_eq!(gas, 6622);
    }

    /// This checks the raw Nitro price-floor conversion. Public handler/RPC
    /// entry points still skip L1 poster gas entirely when the block basefee is 0.
    #[test]
    fn price_is_floored_at_min_base_fee() {
        let p = robinhood_pricing();
        let cost = p.pad_estimation_cost(U256::from(1_000_000_000_000u64));
        let floored = p.poster_gas_from_cost(cost, U256::from(100_000_000u64), true);
        // Inside this helper, base_fee at/below the floor uses min_base_fee.
        assert_eq!(p.poster_gas_from_cost(cost, U256::ZERO, true), floored);
        // 1 gwei base fee: price = max(1e9*7/8, 1e8) = 875_000_000 -> smaller gas.
        let high = p.poster_gas_from_cost(cost, U256::from(1_000_000_000u64), true);
        assert!(
            high < floored,
            "higher base fee -> smaller posterGas, got {high}"
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
        assert_eq!(
            p.poster_gas_from_cost(U256::from(100u64), U256::ZERO, false),
            0
        );
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
        let small = fake_tx_bytes(&sample_tx(vec![0u8; 4]), true);
        let large = fake_tx_bytes(&sample_tx(vec![0u8; 4096]), true);
        assert_eq!(small[0], 0x02, "EIP-2718 type byte for EIP-1559");
        assert_eq!(large[0], 0x02);
        assert!(large.len() > small.len());
        // V must carry Nitro's randV = 126483, RLP-encoded as the 4 bytes 0x83 01 EE 13
        // (not a 1-byte y_parity) — this is what keeps the fake tx byte-identical to
        // Nitro's before brotli. See FAKE_SIG_V.
        assert!(
            small.windows(4).any(|w| w == [0x83, 0x01, 0xEE, 0x13]),
            "fake tx must encode V=126483 as 4 bytes (matches Nitro's randV)"
        );
    }

    #[test]
    fn gas_price_populates_fake_tx_fee_and_tip_caps_for_legacy_and_access_list() {
        fn encoded_gas_price_count(tx_type: u8) -> usize {
            let mut tx = sample_tx(vec![0u8; 4]);
            tx.tx_type = tx_type;
            tx.gas_price = 123_456;
            tx.gas_priority_fee = None;

            let bytes = fake_tx_bytes(&tx, true);
            let encoded_gas_price = [0x83, 0x01, 0xE2, 0x40];
            bytes
                .windows(encoded_gas_price.len())
                .filter(|window| *window == encoded_gas_price)
                .count()
        }

        assert!(
            encoded_gas_price_count(LEGACY_TX_TYPE) >= 2,
            "legacy gasPrice must populate both GasTipCap and GasFeeCap"
        );
        assert!(
            encoded_gas_price_count(ACCESS_LIST_TX_TYPE) >= 2,
            "EIP-2930 gasPrice must populate both GasTipCap and GasFeeCap"
        );
    }

    #[test]
    fn eip1559_max_fee_without_priority_fee_keeps_random_fake_tx_tip_cap() {
        let mut tx = sample_tx(vec![0u8; 4]);
        tx.tx_type = 2;
        tx.gas_price = 123_456;
        tx.gas_priority_fee = None;

        let bytes = fake_tx_bytes(&tx, true);
        let encoded_gas_price = [0x83, 0x01, 0xE2, 0x40];
        let count = bytes
            .windows(encoded_gas_price.len())
            .filter(|window| *window == encoded_gas_price)
            .count();
        assert_eq!(count, 1, "EIP-1559 maxFeePerGas is not GasTipCap");
    }

    /// posterGas is non-zero end-to-end for a realistic call and scales up with
    /// calldata size.
    #[test]
    fn compute_is_nonzero_and_monotonic_in_calldata() {
        let p = robinhood_pricing();
        let small = p.poster_gas(&sample_tx(vec![0xabu8; 100]), 100_000_000);
        let large = p.poster_gas(&sample_tx(vec![0xabu8; 10_000]), 100_000_000);
        assert!(small > 0);
        assert!(large > small);
    }

    #[test]
    fn current_tx_l1_fee_uses_eth_call_poster_cost_without_estimation_padding() {
        let p = robinhood_pricing();
        let mut tx = sample_tx(vec![0xabu8; 128]);
        tx.gas_limit = 21_000;
        let paid_gas_price = U256::from(100_000_000u64);
        let cost = p.poster_data_cost(&tx, false).expect("poster data cost");
        let poster_gas = cost / paid_gas_price;

        assert_eq!(
            p.current_tx_l1_fee(&tx, paid_gas_price),
            poster_gas.saturating_mul(paid_gas_price)
        );
    }

    #[test]
    fn gas_estimate_l1_component_does_not_apply_price_adjustment() {
        let p = robinhood_pricing();
        let tx = sample_tx(vec![0xabu8; 128]);
        let high_base_fee = 1_000_000_000u64;

        assert!(
            p.gas_estimate_l1_component(&tx, high_base_fee) < p.poster_gas(&tx, high_base_fee),
            "l1 component divides by base fee directly; poster_gas uses 7/8 adjusted price"
        );
    }

    #[test]
    fn current_tx_l1_fee_saturates_poster_gas_to_u64() {
        let p = ArbPricing {
            price_per_unit: U256::MAX,
            min_base_fee: U256::from(1u64),
            brotli_level: 1,
        };
        let mut tx = sample_tx(vec![0xabu8; 128]);
        tx.gas_limit = 21_000;

        assert_eq!(
            p.current_tx_l1_fee(&tx, U256::from(1u64)),
            U256::from(u64::MAX)
        );
    }
}
