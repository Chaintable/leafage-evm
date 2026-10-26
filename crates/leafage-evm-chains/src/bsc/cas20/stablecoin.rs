//! The Stablecoin variant: an immutable `currency()` and decimals fixed at 6 (BEP-702 3.13).
//!
//! Port of go-bsc `core/vm/cas20_stablecoin.go`.

use alloy::primitives::B256;

use super::abi::enc_string;
use super::errors::{Cas20Err, Cas20Result};
use super::frame::Frame;
use super::sel;
use super::storage::{erc7201_root, offset_slot, Store};
use super::token::Token;

const NAMESPACE: &str = "bsc.cas20.stablecoin";

const SLOT_CURRENCY: u64 = 0;

pub(crate) const STABLECOIN_DECIMALS: u8 = 6;

fn stablecoin_slot(offset: u64) -> B256 {
    static ROOT: std::sync::OnceLock<B256> = std::sync::OnceLock::new();
    offset_slot(*ROOT.get_or_init(|| erc7201_root(NAMESPACE)), offset)
}

pub(crate) fn stablecoin_dispatch(
    f: &mut Frame<'_>,
    tok: Token,
    input: &[u8],
) -> Cas20Result<Vec<u8>> {
    if input.len() >= 4 && input[..4] == sel!("currency()") {
        let s = Store::new(f.self_address);
        return s
            .get_string_at(f, stablecoin_slot(SLOT_CURRENCY))
            .map(|v| enc_string(&v))
            .ok_or(Cas20Err::OutOfGas);
    }
    tok.dispatch(f, input)
}
