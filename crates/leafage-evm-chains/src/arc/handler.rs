use super::evm::{ArcContext, ArcEvm};
use super::ArcHardforkFlags;
use alloy_evm::Database;
use revm::{
    context::result::{EVMError, HaltReason},
    handler::{EthFrame, Handler},
    inspector::InspectorHandler,
    interpreter::interpreter::EthInterpreter,
    Inspector,
};
use std::marker::PhantomData;

type ArcHandlerMarker<DB, I> = (
    ArcEvm<DB, I>,
    EVMError<<DB as revm::Database>::Error>,
    EthFrame,
);

/// Arc transaction handler.
///
/// A2 intentionally retains Ethereum's default handler behavior. Arc-specific
/// overrides are added here in A4 so normal and inspected execution share them.
pub struct ArcHandler<DB: revm::Database, I> {
    _marker: PhantomData<ArcHandlerMarker<DB, I>>,
    _hardfork_flags: ArcHardforkFlags,
}

impl<DB: revm::Database, I> ArcHandler<DB, I> {
    pub fn new(hardfork_flags: ArcHardforkFlags) -> Self {
        Self {
            _marker: PhantomData,
            _hardfork_flags: hardfork_flags,
        }
    }
}

impl<DB: Database, I> Handler for ArcHandler<DB, I> {
    type Evm = ArcEvm<DB, I>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, I> InspectorHandler for ArcHandler<DB, I>
where
    DB: Database,
    I: Inspector<ArcContext<DB>, EthInterpreter>,
{
    type IT = EthInterpreter;
}
