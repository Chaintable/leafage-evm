//! BEP-706 millisecond block timestamp precompile at `0x70`, active from Jenner.
//!
//! Port of go-bsc `core/vm/contract_milli_timestamp.go`. It returns the executing block's
//! millisecond timestamp (`Header.MilliTimestamp()`, BEP-520: `time * 1000` plus the
//! milliseconds BSC keeps in the header's mix digest) as a 32-byte big-endian word, for a flat
//! 20 gas. Calldata is ignored.
//!
//! The value is block context revm's `BlockEnv` does not carry, so it travels on
//! [`BscTxEnv::milli_timestamp`]. Like go-bsc, a context without it falls back to
//! `time * 1000`: degraded but correct, unlike a near-1970 value.

use crate::bsc::transaction::BscTxEnv;
use alloy::primitives::{Bytes, B256, U256};
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use revm::precompile::{
    u64_to_address, Precompile, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
};
use std::borrow::Cow;

/// `params.MilliTimestampGas`.
const MILLI_TIMESTAMP_GAS: u64 = 20;

pub(crate) const MILLI_TIMESTAMP_ADDRESS: u64 = 0x70;

fn id() -> PrecompileId {
    PrecompileId::Custom(Cow::Borrowed("BSC_MILLI_TIMESTAMP"))
}

/// The static entry of the Jenner set. It keeps `0x70` in the precompile address set (so it is
/// warm, as in go-bsc) and, like go-bsc's `Run`, refuses to fabricate a value without block
/// context. `BscEvm` replaces it with [`milli_timestamp`].
pub(crate) const MILLI_TIMESTAMP: Precompile = Precompile::new(
    PrecompileId::Custom(Cow::Borrowed("BSC_MILLI_TIMESTAMP")),
    u64_to_address(MILLI_TIMESTAMP_ADDRESS),
    milli_timestamp_without_context,
);

fn milli_timestamp_without_context(
    _input: &[u8],
    _gas_limit: u64,
    reservoir: u64,
) -> PrecompileResult {
    Ok(PrecompileOutput::halt(
        PrecompileHalt::other("milliTimestamp: must be dispatched with block context"),
        reservoir,
    ))
}

/// go-bsc `Header.MilliTimestamp()`: the millisecond part sits in the mix digest.
pub fn header_milli_timestamp(timestamp: u64, mix_digest: B256) -> u64 {
    let milliseconds = U256::from_be_bytes(mix_digest.0).as_limbs()[0];
    timestamp.wrapping_mul(1000).wrapping_add(milliseconds)
}

/// The context-aware `0x70` precompile.
pub(crate) fn milli_timestamp() -> DynPrecompile {
    DynPrecompile::new_stateful(id(), |input: PrecompileInput<'_>| {
        if MILLI_TIMESTAMP_GAS > input.gas {
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::OutOfGas,
                input.reservoir,
            ));
        }
        let time = input.internals.block_timestamp().as_limbs()[0];
        let ms = input
            .internals
            .tx_env_downcast_ref::<BscTxEnv>()
            .and_then(|tx| tx.milli_timestamp)
            .filter(|ms| *ms != 0)
            .unwrap_or_else(|| time.wrapping_mul(1000));
        Ok(PrecompileOutput::new(
            MILLI_TIMESTAMP_GAS,
            Bytes::copy_from_slice(&U256::from(ms).to_be_bytes::<32>()),
            input.reservoir,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milliseconds_come_from_the_mix_digest() {
        let mut mix = B256::ZERO;
        mix[30..].copy_from_slice(&750u16.to_be_bytes());
        assert_eq!(
            header_milli_timestamp(1_700_000_000, mix),
            1_700_000_000_750
        );
        assert_eq!(
            header_milli_timestamp(1_700_000_000, B256::ZERO),
            1_700_000_000_000
        );
    }
}
