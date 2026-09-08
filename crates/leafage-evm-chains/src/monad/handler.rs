//! Transaction level deviations.
//!
//! * `monad_transaction_gas.cpp` `compute_gas_refund`: the refund is always
//!   zero, `gas_used == gas_limit - gas_left`.
//! * `execute_transaction.cpp` with MIP-8: the EIP-2930 access list warms
//!   storage *pages*, not slots.

use crate::monad::page_tracker::{access_page, JournalPageStore};
use crate::monad::{MonadContext, MonadEvm};
use alloy_evm::Database;
use revm::context::result::{EVMError, HaltReason};
use revm::context::{ContextTr, Transaction, TransactionType};
use revm::context_interface::transaction::AccessListItemTr;
use revm::handler::{pre_execution, EthFrame, EvmTr, FrameTr, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::primitives::U256;
use revm::Inspector;

pub struct MonadHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(MonadEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> MonadHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for MonadHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for MonadHandler<DB, INSP> {
    type Evm = MonadEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn load_accounts(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        pre_execution::load_accounts::<_, Self::Error>(evm)?;
        let ctx = evm.ctx_mut();
        if !ctx.cfg().spec().is_mip8_enabled() {
            return Ok(());
        }
        let (tx, journal) = ctx.tx_journal_mut();
        if tx.tx_type() == TransactionType::Legacy {
            return Ok(());
        }
        if let Some(access_list) = tx.access_list() {
            let mut store = JournalPageStore(journal);
            for item in access_list {
                let address = *item.address();
                for slot in item.storage_slots() {
                    access_page(&mut store, address, U256::from_be_bytes(slot.0));
                }
            }
        }
        Ok(())
    }

    fn refund(
        &self,
        _evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        _eip7702_refund: i64,
    ) {
        exec_result.gas_mut().set_refund(0);
    }
}

impl<DB, INSP> InspectorHandler for MonadHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<MonadContext<DB>>,
{
    type IT = EthInterpreter;
}
