/// Tempo block environment with millisecond timestamp support.
///
/// Wraps standard [`BlockEnv`] with `timestamp_millis_part` for the
/// `MILLIS_TIMESTAMP` (0x4F) custom opcode (active pre-T1C only).
///
/// Ported from Tempo writer: crates/revm/src/block.rs
use revm::{
    context::{Block, BlockEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::{Address, B256, U256, uint},
};
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, Default)]
pub struct TempoBlockEnv {
    pub inner: BlockEnv,
    /// Milliseconds portion of the timestamp (0-999).
    /// Pipeline does not carry this field; defaults to 0.
    pub timestamp_millis_part: u64,
}

impl TempoBlockEnv {
    /// Returns the current timestamp in milliseconds.
    /// Used by the `MILLIS_TIMESTAMP` (0x4F) opcode.
    pub fn timestamp_millis(&self) -> U256 {
        self.inner
            .timestamp
            .saturating_mul(uint!(1000_U256))
            .saturating_add(U256::from(self.timestamp_millis_part))
    }
}

impl Deref for TempoBlockEnv {
    type Target = BlockEnv;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for TempoBlockEnv {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Block for TempoBlockEnv {
    #[inline]
    fn number(&self) -> U256 {
        self.inner.number()
    }
    #[inline]
    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }
    #[inline]
    fn timestamp(&self) -> U256 {
        self.inner.timestamp()
    }
    #[inline]
    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }
    #[inline]
    fn basefee(&self) -> u64 {
        self.inner.basefee()
    }
    #[inline]
    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }
    #[inline]
    fn prevrandao(&self) -> Option<B256> {
        self.inner.prevrandao()
    }
    #[inline]
    fn blob_excess_gas_and_price(&self) -> Option<BlobExcessGasAndPrice> {
        self.inner.blob_excess_gas_and_price()
    }
}
