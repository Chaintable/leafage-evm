use crate::moonbeam::{MoonbeamContext, MoonbeamEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct MoonbeamHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(MoonbeamEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> MoonbeamHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for MoonbeamHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for MoonbeamHandler<DB, INSP> {
    type Evm = MoonbeamEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, INSP> InspectorHandler for MoonbeamHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<MoonbeamContext<DB>>,
{
    type IT = EthInterpreter;
}
