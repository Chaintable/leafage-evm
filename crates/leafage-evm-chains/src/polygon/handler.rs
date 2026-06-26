use crate::polygon::{PolygonContext, PolygonEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct PolygonHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(PolygonEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> PolygonHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for PolygonHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for PolygonHandler<DB, INSP> {
    type Evm = PolygonEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, INSP> InspectorHandler for PolygonHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<PolygonContext<DB>>,
{
    type IT = EthInterpreter;
}
