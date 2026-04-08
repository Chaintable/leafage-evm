use crate::citrea::api::{CitreaHandlerContext, CitreaHandlerEvm, TxInfo};
use crate::citrea::l1_fee::{calc_diff_size, BROTLI_COMPRESSION_PERCENTAGE, BROTLI_EXTRA_BYTES};
use alloy_evm::Database;
use leafage_evm_types::U256;
use revm::context::result::{EVMError, ExecutionResult, HaltReason, ResultGas};
use revm::context_interface::{ContextTr, JournalTr, LocalContextTr, Transaction};
use revm::handler::{EthFrame, EvmTr, FrameTr, Handler, MainnetHandler};
use revm::inspector::{Inspector, InspectorHandler};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::SuccessOrHalt;

pub struct CitreaHandler<DB: revm::database::Database, INSP> {
    pub mainnet: MainnetHandler<CitreaHandlerEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>,
}

impl<DB: revm::database::Database, INSP> CitreaHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for CitreaHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for CitreaHandler<DB, INSP> {
    type Evm = CitreaHandlerEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(revm::context::ContextError::Db(e)) => return Err(e.into()),
            Err(revm::context::ContextError::Custom(e)) => {
                return Err(EVMError::Custom(e));
            }
            Ok(_) => (),
        }

        let caller = evm.ctx().tx().caller();
        let journal_entries = &evm.inner.ctx.journaled_state.inner.journal;
        let state = &evm.inner.ctx.journaled_state.inner.state;
        let diff_size = calc_diff_size(journal_entries, state, caller);

        let l1_fee_rate = evm.inner.ctx.chain.l1_fee_rate;
        let compressed_size =
            (diff_size * BROTLI_COMPRESSION_PERCENTAGE / 100) + BROTLI_EXTRA_BYTES;
        let l1_fee = U256::from(l1_fee_rate) * U256::from(compressed_size);

        evm.inner.ctx.chain.tx_info = TxInfo {
            l1_fee,
            diff_size: compressed_size,
        };

        let output = result.output();
        let instruction_result = result.into_interpreter_result();
        let logs = evm.ctx().journal_mut().take_logs();

        let exec_result = match SuccessOrHalt::from(instruction_result.result) {
            SuccessOrHalt::Success(reason) => ExecutionResult::Success {
                reason,
                gas: result_gas,
                logs,
                output,
            },
            SuccessOrHalt::Revert => ExecutionResult::Revert {
                gas: result_gas,
                logs,
                output: output.into_data(),
            },
            SuccessOrHalt::Halt(reason) => ExecutionResult::Halt {
                reason,
                gas: result_gas,
                logs,
            },
            flag @ (SuccessOrHalt::FatalExternalError | SuccessOrHalt::Internal(_)) => {
                panic!(
                    "Encountered unexpected internal return flag: {flag:?} with instruction result: {instruction_result:?}"
                )
            }
        };

        evm.ctx().journal_mut().commit_tx();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }
}

impl<DB, INSP> InspectorHandler for CitreaHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaHandlerContext<DB>>,
{
    type IT = EthInterpreter;
}
