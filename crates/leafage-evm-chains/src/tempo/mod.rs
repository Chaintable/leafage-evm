pub const CHAIN_ID: u64 = 4217;

pub const VIRTUAL_BALANCE: alloy::primitives::U256 = alloy::primitives::uint!(
    4242424242424242424242424242424242424242424242424242424242424242424242424242_U256
);

pub mod api;
pub use api::TempoEvm;

pub mod block;
pub mod fee_payer;
pub mod gas_params;
pub mod hardfork;
pub mod precompile;

pub mod tx;
pub use tx::TempoTxEnv;
