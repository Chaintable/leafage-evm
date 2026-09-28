use crate::rsk::{RskContext, RskEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::context::{ContextTr, JournalTr};
use revm::handler::{pre_execution, EthFrame, EvmTr, FrameResult, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::primitives::hardfork::SpecId;
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

    /// RSK has no EIP-6780: `SELFDESTRUCT` always removes the contract. The
    /// interpreter runs at Cancun for the opcodes, the journal one fork behind
    /// for the account semantics (nothing else in it depends on Cancun).
    fn load_accounts(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        pre_execution::load_accounts::<_, Self::Error>(evm)?;
        let spec: SpecId = (*evm.ctx().cfg().spec()).into();
        if spec.is_enabled_in(SpecId::CANCUN) {
            evm.ctx_mut().journal_mut().set_spec_id(SpecId::SHANGHAI);
        }
        Ok(())
    }

    /// `TransactionExecutor.refundGas`: refunds are capped at half of the gas
    /// used. RSK has no EIP-3529, which lowered the cap to a fifth.
    fn refund(&self, _evm: &mut Self::Evm, exec_result: &mut FrameResult, eip7702_refund: i64) {
        let gas = exec_result.gas_mut();
        gas.record_refund(eip7702_refund);
        gas.set_final_refund(false);
    }
}

impl<DB, INSP> InspectorHandler for RskHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<RskContext<DB>>,
{
    type IT = EthInterpreter;
}
