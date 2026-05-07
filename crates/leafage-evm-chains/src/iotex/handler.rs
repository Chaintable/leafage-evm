use crate::iotex::{IotexContext, IotexEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct IotexHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(IotexEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> IotexHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for IotexHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for IotexHandler<DB, INSP> {
    type Evm = IotexEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
}

impl<DB, INSP> InspectorHandler for IotexHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<IotexContext<DB>>,
{
    type IT = EthInterpreter;
}
