use crate::hemi::api::{HemiEvm, HemiOpContext};
use alloy_evm::Database;
use op_revm::{OpHaltReason, OpTransactionError};
use revm::context::result::EVMError;
use revm::handler::{EthFrame, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Inspector;

pub struct HemiHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(HemiEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> HemiHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for HemiHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for HemiHandler<DB, INSP> {
    type Evm = HemiEvm<DB, INSP>;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type HaltReason = OpHaltReason;
}

impl<DB, INSP> InspectorHandler for HemiHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<HemiOpContext<DB>>,
{
    type IT = EthInterpreter;
}
