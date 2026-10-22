//! BSC mainnet replay on an archive node: the [`BscPipeline`] behind
//! `trace_debankBlock` (see [`crate::pipeline::rpc`]) and `pipeline-export`.

mod chain;
mod patches;
mod replay;

#[cfg(test)]
mod live_test;

pub(crate) use chain::BscPipeline;
