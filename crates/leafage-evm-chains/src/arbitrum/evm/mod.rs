mod context;
mod handler;
mod instructions;
mod poster_gas;
mod stylus;

use crate::arbitrum::hardforks::ArbitrumHardfork;
use crate::arbitrum::precompile::{ArbitrumContext, ArbitrumPrecompileEnv, ArbitrumPrecompiles};
use crate::arbitrum::tx::ArbitrumTxEnv;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::{Context, ContextSetters, Evm, FrameStack, JournalTr};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::{EthInstructions, InstructionProvider};
use revm::handler::{
    EthFrame, EvmTr, ExecuteCommitEvm, ExecuteEvm, FrameInitOrResult, FrameResult, Handler,
    ItemOrResult,
};
use revm::inspector::handler::frame_end;
use revm::inspector::{
    InspectCommitEvm, InspectEvm, Inspector, InspectorEvmTr, InspectorHandler, inspect_instructions,
};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::{CallScheme, FrameInput};
use revm::state::EvmState;
use revm::{
    Database, DatabaseCommit, DatabaseRef, Journal,
    context_interface::{
        ContextTr,
        result::{EVMError, ExecutionResult, HaltReason, ResultAndState},
    },
};
use std::ops::{Deref, DerefMut};

pub use self::context::{ArbitrumCallContext, ArbitrumExecutionContext};
use self::handler::ArbitrumHandler;
pub use self::poster_gas::ArbPosterCharge;

pub struct ArbitrumEvm<DB: Database + DatabaseRef, I> {
    pub inner: Evm<
        ArbitrumContext<DB>,
        I,
        EthInstructions<EthInterpreter, ArbitrumContext<DB>>,
        ArbitrumPrecompiles,
        EthFrame,
    >,
}

impl<DB: Database + DatabaseRef, I> ArbitrumEvm<DB, I> {
    pub fn new(
        block_env: BlockEnv,
        cfg: CfgEnv<ArbitrumHardfork>,
        db: DB,
        inspector: I,
        precompile_env: ArbitrumPrecompileEnv,
        execution_context: ArbitrumExecutionContext,
    ) -> Self {
        let hardfork = cfg.spec;
        let spec = hardfork.into();
        Self {
            inner: Evm {
                ctx: Context {
                    block: block_env,
                    tx: ArbitrumTxEnv::default(),
                    cfg,
                    journaled_state: Journal::new(db),
                    chain: execution_context,
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: instructions::arbitrum_instructions(spec),
                precompiles: ArbitrumPrecompiles::new_with_env(hardfork, precompile_env),
                frame_stack: Default::default(),
            },
        }
    }

    pub const fn ctx(&self) -> &ArbitrumContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut ArbitrumContext<DB> {
        &mut self.inner.ctx
    }

    fn note_frame_context(&mut self, frame_init: &FrameInit) {
        let callers_caller = self.current_frame_caller();
        self.inner
            .ctx
            .chain
            .set_current_call(frame_init.depth.saturating_add(1), callers_caller);
    }

    fn current_frame_caller(&mut self) -> alloy::primitives::Address {
        if self.inner.frame_stack.index().is_none() {
            return alloy::primitives::Address::ZERO;
        }

        match &self.inner.frame_stack.get().input {
            FrameInput::Call(inputs) => inputs.caller,
            FrameInput::Create(inputs) => inputs.caller(),
            FrameInput::Empty => alloy::primitives::Address::ZERO,
        }
    }

    fn counted_frame_address(frame: &EthFrame) -> Option<alloy::primitives::Address> {
        match &frame.input {
            FrameInput::Call(inputs)
                if !matches!(
                    inputs.scheme,
                    CallScheme::DelegateCall | CallScheme::CallCode
                ) =>
            {
                Some(inputs.target_address)
            }
            FrameInput::Create(_) => frame.data.created_address(),
            FrameInput::Call(_) | FrameInput::Empty => None,
        }
    }

    pub(super) fn pop_frame(&mut self) {
        let counted_address = {
            let frame = self.inner.frame_stack.get();
            Self::counted_frame_address(frame)
        };
        self.inner.frame_stack.pop();
        if let Some(address) = counted_address {
            self.inner.ctx.chain.exit_contract_frame(address);
        }
    }
}

impl<DB: Database + DatabaseRef, I> Deref for ArbitrumEvm<DB, I> {
    type Target = ArbitrumContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database + DatabaseRef, I> DerefMut for ArbitrumEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
{
    type Context = ArbitrumContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, ArbitrumContext<DB>>;
    type Precompiles = ArbitrumPrecompiles;
    type Frame = EthFrame;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.note_frame_context(&frame_init);
        let is_first_init = self.inner.frame_stack.index().is_none();
        let new_frame = if is_first_init {
            self.inner.frame_stack.start_init()
        } else {
            self.inner.frame_stack.get_next()
        };
        let result = EthFrame::init_with_context(
            new_frame,
            &mut self.inner.ctx,
            &mut self.inner.precompiles,
            frame_init,
        )?;

        Ok(result.map_item(|token| {
            if is_first_init {
                // SAFETY: `token` was produced by `start_init` on this stack.
                unsafe { self.inner.frame_stack.end_init(token) };
            } else {
                // SAFETY: `token` was produced by `get_next` on this initialized stack.
                unsafe { self.inner.frame_stack.push(token) };
            }
            let counted_address = {
                let frame = self.inner.frame_stack.get();
                Self::counted_frame_address(frame)
            };
            if let Some(address) = counted_address {
                self.inner.ctx.chain.enter_contract_frame(address);
            }
            self.inner.frame_stack.get()
        }))
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        if let Some(arbos_version) = stylus::frame_stylus_version(self)? {
            return stylus::run_stylus_frame::<stylus::Plain, _, _>(self, arbos_version);
        }

        let frame = self.inner.frame_stack.get();
        let context = &mut self.inner.ctx;
        let instructions = &mut self.inner.instruction;
        let action = frame
            .interpreter
            .run_plain(instructions.instruction_table(), context);
        stylus::process_next_action(context, frame, action).inspect(|item| {
            if item.is_result() {
                frame.set_finished(true);
            }
        })
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        if self.inner.frame_stack.get().is_finished() {
            self.pop_frame();
        }
        if self.inner.frame_stack.index().is_none() {
            return Ok(Some(result));
        }
        self.inner
            .frame_stack
            .get()
            .return_result::<_, ContextDbError<Self::Context>>(&mut self.inner.ctx, result)?;
        Ok(None)
    }
}

impl<DB, INSP> ExecuteEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
{
    type ExecutionResult = ExecutionResult<HaltReason>;
    type State = EvmState;
    type Error = EVMError<<DB as Database>::Error>;
    type Tx = ArbitrumTxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        ArbitrumHandler::new().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        ArbitrumHandler::new().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseCommit + DatabaseRef,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        ArbitrumHandler::new().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseCommit + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
}

impl<DB, INSP> InspectorEvmTr for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }

    fn inspect_frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let Some(arbos_version) = stylus::frame_stylus_version(self)? else {
            let (ctx, inspector, frame, instructions) =
                self.inner.ctx_inspector_frame_instructions();
            let action = inspect_instructions(
                ctx,
                &mut frame.interpreter,
                inspector,
                instructions.instruction_table(),
            );
            let mut result = stylus::process_next_action(ctx, frame, action);
            if let Ok(ItemOrResult::Result(frame_result)) = &mut result {
                let (ctx, inspector, frame) = self.inner.ctx_inspector_frame();
                let input = frame.input.clone();
                frame_end(ctx, inspector, &input, frame_result);
                frame.set_finished(true);
            }
            return result;
        };
        // Traced path: run the Stylus body, not the EVM opcode loop (which would
        // halt on the 0xEF prefix — the same class as PR #184 B2). Fire the
        // inspector's call_end so the trace records the frame exit; the `call`
        // hook already fired in inspect_frame_init, so skipping the end would
        // corrupt the inspector's call stack.
        let mut result = stylus::run_stylus_frame::<stylus::Traced, _, _>(self, arbos_version);
        if let Ok(ItemOrResult::Result(frame_result)) = &mut result {
            let (ctx, inspector, frame) = self.inner.ctx_inspector_frame();
            let input = frame.input.clone();
            frame_end(ctx, inspector, &input, frame_result);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::arbos_state;
    use revm::bytecode::Bytecode;
    use revm::context::TxEnv;
    use revm::database::{EmptyDB, in_memory_db::CacheDB};
    use revm::inspector::NoOpInspector;
    use revm::primitives::{Address, Bytes, TxKind, U256};
    use revm::state::AccountInfo;

    type TestDb = CacheDB<EmptyDB>;

    fn test_db(arbos_version: u64) -> TestDb {
        let caller = Address::with_last_byte(1);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::ARBOS_VERSION_OFFSET),
            U256::from(arbos_version),
        )
        .expect("seed ArbOS version");
        db
    }

    fn test_evm<I>(arbos_version: u64, inspector: I) -> ArbitrumEvm<TestDb, I> {
        ArbitrumEvm::new(
            BlockEnv {
                gas_limit: 1_000_000,
                ..Default::default()
            },
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            test_db(arbos_version),
            inspector,
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        )
    }

    fn initcode_returning(runtime: &[u8]) -> Bytes {
        assert!(runtime.len() <= u8::MAX as usize);
        let len = runtime.len() as u8;
        let mut initcode = vec![
            0x60, len, // PUSH1 runtime length
            0x60, 0x0c, // PUSH1 runtime offset
            0x60, 0x00, // PUSH1 memory offset
            0x39, // CODECOPY
            0x60, len, // PUSH1 runtime length
            0x60, 0x00, // PUSH1 memory offset
            0xf3, // RETURN
        ];
        initcode.extend_from_slice(runtime);
        Bytes::from(initcode)
    }

    fn create_tx(runtime: &[u8]) -> ArbitrumTxEnv {
        ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                gas_limit: 1_000_000,
                kind: TxKind::Create,
                data: initcode_returning(runtime),
                ..Default::default()
            },
            Default::default(),
        )
    }

    #[test]
    fn create_eip3541_exception_uses_arbos_component_matrix() {
        let classic = [0xef, 0xf0, 0x00, 0x01];
        let fragment = [0xef, 0xf0, 0x01, 0x01];
        let root = [0xef, 0xf0, 0x02, 0x01];
        let invalid = [0xef, 0x00, 0x01];
        let cases: &[(u64, &[u8], bool)] = &[
            (29, &classic, false),
            (30, &classic, true),
            (59, &classic, true),
            (59, &root, false),
            (60, &root, true),
            (60, &fragment, true),
            (60, &invalid, false),
        ];

        for &(arbos_version, runtime, accepted) in cases {
            let mut evm = test_evm(arbos_version, ());
            let result = evm
                .transact_one(create_tx(runtime))
                .expect("execute CREATE transaction");
            assert_eq!(
                result.is_success(),
                accepted,
                "ArbOS {arbos_version}, runtime {runtime:02x?}: {result:?}"
            );
        }
    }

    #[test]
    fn traced_create_uses_the_same_stylus_exception() {
        let fragment = [0xef, 0xf0, 0x01, 0x01];
        let invalid = [0xef, 0x00, 0x01];

        let mut accepted = test_evm(60, NoOpInspector);
        assert!(
            accepted
                .inspect_one_tx(create_tx(&fragment))
                .expect("inspect valid component CREATE")
                .is_success()
        );

        let mut rejected = test_evm(60, NoOpInspector);
        assert!(
            !rejected
                .inspect_one_tx(create_tx(&invalid))
                .expect("inspect invalid EF CREATE")
                .is_success()
        );
    }

    #[test]
    fn create_restores_the_callers_eip3541_setting() {
        let invalid = [0xef, 0x00, 0x01];
        let mut evm = test_evm(60, ());
        evm.inner.ctx.cfg.disable_eip3541 = true;

        let result = evm
            .transact_one(create_tx(&invalid))
            .expect("execute invalid EF CREATE");

        assert!(!result.is_success(), "invalid EF must still be rejected");
        assert!(evm.inner.ctx.cfg.disable_eip3541);
    }

    #[test]
    fn fragment_component_is_never_executed_directly() {
        let target = Address::from([0x22; 20]);
        let fragment = Bytes::from_static(&[0xef, 0xf0, 0x01, 0x01]);
        let mut evm = test_evm(60, ());
        evm.inner.ctx.db_mut().insert_account_info(
            target,
            AccountInfo::default().with_code(Bytecode::new_raw(fragment)),
        );
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                gas_limit: 1_000_000,
                kind: TxKind::Call(target),
                ..Default::default()
            },
            Default::default(),
        );

        let result = evm.transact_one(tx).expect("call fragment component");

        assert!(
            result.is_halt(),
            "fragment must stay on the EVM path and hit invalid 0xEF: {result:?}"
        );
    }
}
