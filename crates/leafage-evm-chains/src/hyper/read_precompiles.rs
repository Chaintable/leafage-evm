//! HyperEVM's HyperCore read precompiles, `0x800..=0x814`.
//!
//! These precompiles read the perps engine's state (oracle prices, positions,
//! account margins, …). They are pure reads with a gas cost linear in the
//! calldata and returned bytes, but the bytes themselves come from HyperCore and
//! never reach the EVM state an archive replays. A caller therefore supplies a
//! [`Resolver`] that fetches the raw answer, while this module owns the parts
//! that are chain behaviour: which addresses are precompiles, how much they
//! charge, and how a rejected input behaves.
//!
//! See nanoreth's `read_precompile_forwarder` for the original implementation of
//! the forwarding scheme this is used for.

use alloy::primitives::U160;
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput, PrecompilesMap};
use leafage_evm_types::{Address, Bytes};
use revm::{
    handler::EthPrecompiles,
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
    },
    primitives::hardfork::SpecId,
};
use std::sync::Arc;

/// The lowest HyperCore read precompile.
pub const LOWEST_ADDRESS: u64 = 0x800;

/// The highest HyperCore read precompile.
pub const HIGHEST_ADDRESS: u64 = 0x814;

/// Gas a read precompile charges regardless of payload size.
pub const BASE_GAS: u64 = 1000;

/// Gas a read precompile charges per byte of input plus output.
pub const GAS_PER_BYTE: u64 = 33;

/// Gas charged by a read precompile for the given input and output sizes.
///
/// HyperCore nodes do not report the gas a read precompile used over `eth_call`,
/// but it is a pure function of the payload sizes. The constants were measured
/// against mainnet by binary searching the minimum gas limit that lets a direct
/// call to each read precompile succeed.
pub const fn gas(input_len: usize, output_len: usize) -> u64 {
    BASE_GAS + GAS_PER_BYTE * (input_len as u64 + output_len as u64)
}

/// What the resolver learnt about one `(address, input)` pair.
#[derive(Clone, Debug)]
pub enum Response {
    /// The precompile returned these bytes.
    Output(Bytes),
    /// The precompile rejected the input.
    Rejected,
}

/// Fetches the answer to a read precompile call.
///
/// The RPC layer implements this on top of a HyperCore node. Errors are fatal:
/// an answer that could not be fetched must abort the simulation rather than look
/// like a rejected input (which would surface as a plausible but wrong
/// out-of-gas result).
pub trait Resolver: Send + Sync + 'static {
    /// Runs `input` against the precompile at `address`.
    fn resolve(&self, address: Address, input: &[u8]) -> Result<Response, PrecompileError>;
}

/// Builds the precompile set for `spec`: Ethereum's precompiles plus a read
/// precompile for every address in `0x800..=0x814`, answered by `resolver`.
///
/// The read precompiles are regular precompiles for warm/cold accounting and for
/// `CALL`/`STATICCALL`/`DELEGATECALL` handling.
pub fn precompiles(spec: SpecId, resolver: Arc<dyn Resolver>) -> PrecompilesMap {
    let eth = EthPrecompiles::new(spec);
    let mut precompiles = PrecompilesMap::from_static(eth.precompiles);
    for number in LOWEST_ADDRESS..=HIGHEST_ADDRESS {
        let address = Address::from(U160::from(number));
        let resolver = Arc::clone(&resolver);
        precompiles.apply_precompile(&address, move |_| {
            Some(DynPrecompile::new(
                PrecompileId::custom("hl-read-precompile"),
                move |input: PrecompileInput<'_>| resolve_one(&*resolver, address, &input),
            ))
        });
    }
    precompiles
}

fn resolve_one(
    resolver: &dyn Resolver,
    address: Address,
    input: &PrecompileInput<'_>,
) -> PrecompileResult {
    match resolver.resolve(address, input.data)? {
        Response::Output(bytes) => {
            let gas_used = gas(input.data.len(), bytes.len());
            if gas_used > input.gas {
                return Ok(PrecompileOutput::halt(
                    PrecompileHalt::OutOfGas,
                    input.reservoir,
                ));
            }
            Ok(PrecompileOutput::new(gas_used, bytes, input.reservoir))
        }
        // HL burns the whole gas limit when a read precompile rejects its input.
        Response::Rejected => Ok(PrecompileOutput::halt(
            PrecompileHalt::OutOfGas,
            input.reservoir,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(input_len, output_len, gas)` samples taken from mainnet by binary searching the minimum
    /// gas limit that lets a direct `eth_call` to a read precompile succeed, minus the intrinsic
    /// cost of the transaction. Covers `0x801`, `0x806`..`0x80a`, `0x80c` and `0x80e`.
    const SAMPLES: [(usize, usize, u64); 6] = [
        (0, 32, 2056),
        (32, 32, 3112),
        (32, 64, 4168),
        (64, 96, 6280),
        (32, 256, 10504),
        (32, 384, 14728),
    ];

    #[test]
    fn gas_matches_measured_samples() {
        for (input_len, output_len, expected) in SAMPLES {
            assert_eq!(
                gas(input_len, output_len),
                expected,
                "input_len={input_len} output_len={output_len}"
            );
        }
    }

    #[test]
    fn read_precompiles_cover_the_stated_range() {
        assert_eq!(LOWEST_ADDRESS, 0x800);
        assert_eq!(HIGHEST_ADDRESS, 0x814);
    }
}
