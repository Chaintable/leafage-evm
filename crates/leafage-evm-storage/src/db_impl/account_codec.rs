//! Process-wide account codec of the storage backends.
//!
//! A process serves one chain, whose evm type fixes the account format. Set
//! it once at startup with [`set_state_diff_codec`], before any account is
//! read or written. Disk records are decoded strictly in that format, so a DB
//! built for another format fails every account read instead of returning
//! wrong state.

use leafage_evm_types::{decode_stored_account, StateDiffCodec, StoredAccount};
use std::sync::atomic::{AtomicU8, Ordering};

static STATE_DIFF_CODEC: AtomicU8 = AtomicU8::new(STANDARD);

const STANDARD: u8 = 0;
const BLAST_V1: u8 = 1;

pub fn set_state_diff_codec(codec: StateDiffCodec) {
    let value = match codec {
        StateDiffCodec::Standard => STANDARD,
        StateDiffCodec::BlastV1 => BLAST_V1,
    };
    STATE_DIFF_CODEC.store(value, Ordering::Relaxed);
}

pub fn state_diff_codec() -> StateDiffCodec {
    match STATE_DIFF_CODEC.load(Ordering::Relaxed) {
        STANDARD => StateDiffCodec::Standard,
        BLAST_V1 => StateDiffCodec::BlastV1,
        other => unreachable!("invalid state diff codec {other}"),
    }
}

/// Decodes an `AddressToAccount` value in the process-wide format.
#[inline]
pub(crate) fn decode_account(bytes: &[u8]) -> Result<StoredAccount, alloy_rlp::Error> {
    decode_stored_account(state_diff_codec(), bytes)
}
