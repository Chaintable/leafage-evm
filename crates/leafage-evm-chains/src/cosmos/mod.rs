mod api;
mod handler;
mod hardforks;
mod precompile;

pub use api::{CosmosContext, CosmosEvm};
pub use hardforks::CosmosHardfork;
pub use precompile::{unsupported::is_unsupported, CosmosPrecompiles};
pub(crate) const UNSUPPORTED_PRECOMPILE: &str = "unsupported precompile address";
