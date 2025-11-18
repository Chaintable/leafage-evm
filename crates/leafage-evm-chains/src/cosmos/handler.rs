use crate::cosmos::{CosmosContext, CosmosEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct CosmosHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(CosmosEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> CosmosHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for CosmosHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for CosmosHandler<DB, INSP> {
    type Evm = CosmosEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, INSP> InspectorHandler for CosmosHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CosmosContext<DB>>,
{
    type IT = EthInterpreter;
}
