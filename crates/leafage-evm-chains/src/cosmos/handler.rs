use crate::cosmos::{CosmosContext, CosmosEvm, UNSUPPORTED_PRECOMPILE};
use alloy::primitives::TxKind;
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::handler::{EthFrame, Handler, MainnetHandler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::InitialAndFloorGas;
use revm::Inspector;

pub struct CosmosHandler<DB: revm::database::Database, INSP> {
    pub mainnet: MainnetHandler<CosmosEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>,
}

impl<DB: revm::database::Database, INSP> CosmosHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
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

    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        let initial_and_floor_gas = self.mainnet.validate(evm)?;
        if let TxKind::Call(ref addr) = evm.tx.kind {
            if super::precompile::unsupported::is_unsupported(addr) {
                return Err(Self::Error::Custom(format!(
                    "{UNSUPPORTED_PRECOMPILE}: {}",
                    addr
                )));
            }
        }
        Ok(initial_and_floor_gas)
    }
}

impl<DB, INSP> InspectorHandler for CosmosHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CosmosContext<DB>>,
{
    type IT = EthInterpreter;
}
