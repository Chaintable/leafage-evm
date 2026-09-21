use crate::rsk::{RskContext, RskEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct RskHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(RskEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> RskHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for RskHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for RskHandler<DB, INSP> {
    type Evm = RskEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, INSP> InspectorHandler for RskHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<RskContext<DB>>,
{
    type IT = EthInterpreter;
}
