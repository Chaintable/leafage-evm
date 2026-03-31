use crate::citrea::{CitreaContext, CitreaEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct CitreaHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(CitreaEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> CitreaHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for CitreaHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for CitreaHandler<DB, INSP> {
    type Evm = CitreaEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, INSP> InspectorHandler for CitreaHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaContext<DB>>,
{
    type IT = EthInterpreter;
}
