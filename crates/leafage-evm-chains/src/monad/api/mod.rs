use crate::monad::evm::instructions::monad_instructions;
use crate::monad::precompile::MonadPrecompiles;
use crate::monad::reserve_balance::{
    apply_reserve_balance_rule, apply_reserve_balance_rule_after_create, dipped_into_reserve,
    mark_reserve_violation, reject_on_reserve_violation, ReserveRuleOutcome,
};
use crate::monad::MonadHardfork;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::{Context, FrameStack};
use revm::context::{ContextTr, Evm, JournalTr, TxEnv};
use revm::context_interface::journaled_state::JournalCheckpoint;
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::{EthInstructions, InstructionProvider};
use revm::handler::{EthFrame, EvmTr, FrameInitOrResult, FrameResult, ItemOrResult};
use revm::inspector::handler::{frame_end, frame_start};
use revm::inspector::{inspect_instructions, InspectorEvmTr};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::{CallOutcome, FrameInput};
use revm::primitives::{Address, U256};
use revm::{Inspector, Journal};
use std::ops::{Deref, DerefMut};

mod exec;

pub type MonadContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<MonadHardfork>, DB>;

pub struct MonadEvm<DB: revm::database::Database, I> {
    pub inner: Evm<
        MonadContext<DB>,
        I,
        EthInstructions<EthInterpreter, MonadContext<DB>>,
        MonadPrecompiles,
        EthFrame,
    >,
    /// The top level message of the current transaction was rejected by the
    /// reserve balance rule (`EVMC_MONAD_RESERVE_BALANCE_VIOLATION`). Set by
    /// the frame hooks below, consumed by the handler when it builds the
    /// execution result.
    pub(crate) reserve_balance_violation: bool,
}

impl<DB: Database, I> MonadEvm<DB, I> {
    pub fn new(env: EvmEnv<MonadHardfork>, db: DB, inspector: I) -> Self {
        let hardfork = env.cfg_env.spec;

        Self {
            inner: Evm {
                ctx: Context {
                    block: env.block_env,
                    cfg: env.cfg_env,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: monad_instructions::<DB>(hardfork),
                precompiles: MonadPrecompiles::new(hardfork),
                frame_stack: Default::default(),
            },
            reserve_balance_violation: false,
        }
    }
}

impl<DB: Database, I> MonadEvm<DB, I> {
    pub const fn ctx(&self) -> &MonadContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut MonadContext<DB> {
        &mut self.inner.ctx
    }

    /// Is the next frame init the top level message of the transaction?
    fn is_first_frame(&self) -> bool {
        self.inner.frame_stack.index().is_none()
    }

    /// Journal position before the init of the top level message, see
    /// [`Self::settle_first_frame_result`]. The call depth is left untouched
    /// so precompiles still see depth 1 (error details are only kept there).
    fn first_frame_checkpoint(&mut self) -> Option<JournalCheckpoint> {
        let ctx = &mut self.inner.ctx;
        ctx.cfg()
            .spec()
            .is_reserve_balance_check_enabled()
            .then(|| {
                let journal = ctx.journal_mut();
                let checkpoint = journal.checkpoint();
                journal.checkpoint_commit();
                checkpoint
            })
    }

    /// The top level message finished during its init: a call to a
    /// precompile or to an account without code (revm never runs a frame
    /// for those). `execute_call_message` applies the reserve balance rule
    /// after `check_call_precompile` with the value transfer in place, so a
    /// failed precompile (frame already reverted by revm) is checked with
    /// the transfer replayed. The checkpoint taken before the init plays the
    /// role of the frame revm already committed. CREATE messages that fail
    /// to start (`sender_has_balance`, EIP-684 collision) return before the
    /// check in the node too.
    fn settle_first_frame_result(
        &mut self,
        checkpoint: Option<JournalCheckpoint>,
        transfer: Option<FirstFrameTransfer>,
        frame_result: &mut FrameResult,
    ) -> Result<(), DB::Error> {
        let Some(checkpoint) = checkpoint else {
            return Ok(());
        };
        let FrameResult::Call(outcome) = frame_result else {
            return Ok(());
        };
        let ctx = &mut self.inner.ctx;
        let violated = if outcome.result.result.is_ok() {
            reject_on_reserve_violation(ctx, &mut outcome.result)?
        } else {
            let journal = ctx.journal_mut();
            let scope = journal.checkpoint();
            if let Some(FirstFrameTransfer { from, to, value }) = transfer {
                journal.transfer(from, to, value)?;
            }
            let violated = dipped_into_reserve(ctx)?;
            self.inner.ctx.journal_mut().checkpoint_revert(scope);
            if violated {
                mark_reserve_violation(&mut outcome.result);
            }
            violated
        };
        if violated {
            self.inner.ctx.journal_mut().checkpoint_revert(checkpoint);
            self.reserve_balance_violation = true;
        }
        Ok(())
    }

    /// Top level CREATE whose init code succeeded, checked once revm
    /// settled the frame (see [`ReserveRuleOutcome::CheckAfterDeploy`]).
    fn settle_create_after_deploy(
        &mut self,
        result: &mut Result<FrameInitOrResult<EthFrame>, ContextDbError<MonadContext<DB>>>,
    ) -> Result<(), DB::Error> {
        let Ok(ItemOrResult::Result(FrameResult::Create(outcome))) = result else {
            return Ok(());
        };
        let frame = self.inner.frame_stack.get();
        if apply_reserve_balance_rule_after_create(&mut self.inner.ctx, frame, &mut outcome.result)?
        {
            self.reserve_balance_violation = true;
        }
        Ok(())
    }
}

/// The value transfer of the top level message as it was actually executed
/// (an inspector may rewrite the call inputs in `frame_start`), replayed for
/// the reserve check of a failed precompile call.
#[derive(Clone, Copy)]
struct FirstFrameTransfer {
    from: Address,
    to: Address,
    value: U256,
}

impl FirstFrameTransfer {
    fn of(frame_input: &FrameInput) -> Option<Self> {
        let FrameInput::Call(inputs) = frame_input else {
            return None;
        };
        let value = inputs.transfer_value().filter(|value| !value.is_zero())?;
        Some(Self {
            from: inputs.caller,
            to: inputs.target_address,
            value,
        })
    }
}

impl<DB: Database, I> Deref for MonadEvm<DB, I> {
    type Target = MonadContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for MonadEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for MonadEvm<DB, INSP>
where
    DB: Database,
{
    type Context = MonadContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, MonadContext<DB>>;
    type Precompiles = MonadPrecompiles;
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

    /// Stock revm frame init; the top level message that finishes during
    /// its init goes through the reserve balance rule.
    fn frame_init(
        &mut self,
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        if !self.is_first_frame() {
            return self.inner.frame_init(frame_input);
        }
        let checkpoint = self.first_frame_checkpoint();
        let transfer = FirstFrameTransfer::of(&frame_input.frame_input);
        if let ItemOrResult::Result(mut output) = self.inner.frame_init(frame_input)? {
            self.settle_first_frame_result(checkpoint, transfer, &mut output)?;
            return Ok(ItemOrResult::Result(output));
        }
        Ok(ItemOrResult::Item(self.inner.frame_stack.get()))
    }

    /// Stock revm frame run with the reserve balance rule applied to the
    /// result of the top level message before the frame is committed or
    /// reverted (`execute_message.cpp`).
    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let Evm {
            ctx,
            instruction,
            frame_stack,
            ..
        } = &mut self.inner;
        let frame = frame_stack.get();

        let mut action = frame
            .interpreter
            .run_plain(instruction.instruction_table(), ctx);
        let outcome = apply_reserve_balance_rule(ctx, frame, &mut action)?;
        self.reserve_balance_violation |= outcome == ReserveRuleOutcome::Violated;

        let mut result = self
            .inner
            .frame_stack
            .get()
            .process_next_action(&mut self.inner.ctx, action);
        if outcome == ReserveRuleOutcome::CheckAfterDeploy {
            self.settle_create_after_deploy(&mut result)?;
        }
        if let Ok(ItemOrResult::Result(_)) = &result {
            self.inner.frame_stack.get().set_finished(true);
        }
        result
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP> InspectorEvmTr for MonadEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<MonadContext<DB>, EthInterpreter>,
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

    /// Stock revm inspected frame init plus the reserve balance rule, see
    /// [`EvmTr::frame_init`]; the inspector sees the settled outcome.
    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector) = self.ctx_inspector();
        if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            frame_end(ctx, inspector, &frame_init.frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        let frame_input = frame_init.frame_input.clone();
        let logs_i = ctx.journal().logs().len();
        let checkpoint = if self.is_first_frame() {
            self.first_frame_checkpoint()
        } else {
            None
        };
        let transfer = FirstFrameTransfer::of(&frame_input);
        if let ItemOrResult::Result(mut output) = self.inner.frame_init(frame_init)? {
            self.settle_first_frame_result(checkpoint, transfer, &mut output)?;
            let (ctx, inspector) = self.ctx_inspector();
            // for precompiles send logs to inspector.
            if let FrameResult::Call(CallOutcome {
                was_precompile_called,
                precompile_call_logs,
                ..
            }) = &mut output
            {
                if *was_precompile_called {
                    let logs = ctx.journal_mut().logs()[logs_i..].to_vec();
                    for log in logs.iter().chain(precompile_call_logs.iter()).cloned() {
                        inspector.log(ctx, log);
                    }
                }
            }
            frame_end(ctx, inspector, &frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        // if it is new frame, initialize the interpreter.
        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        inspector.initialize_interp(&mut frame.interpreter, ctx);
        Ok(ItemOrResult::Item(frame))
    }

    /// Stock revm inspected frame run plus the reserve balance rule, see
    /// [`EvmTr::frame_run`].
    fn inspect_frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let Evm {
            ctx,
            inspector,
            instruction,
            frame_stack,
            ..
        } = &mut self.inner;
        let frame = frame_stack.get();

        let mut action = inspect_instructions(
            ctx,
            &mut frame.interpreter,
            &mut *inspector,
            instruction.instruction_table(),
        );
        let outcome = apply_reserve_balance_rule(ctx, frame, &mut action)?;
        self.reserve_balance_violation |= outcome == ReserveRuleOutcome::Violated;

        let mut result = self
            .inner
            .frame_stack
            .get()
            .process_next_action(&mut self.inner.ctx, action);
        if outcome == ReserveRuleOutcome::CheckAfterDeploy {
            self.settle_create_after_deploy(&mut result)?;
        }
        if let Ok(ItemOrResult::Result(frame_result)) = &mut result {
            let (ctx, inspector, frame) = self.ctx_inspector_frame();
            frame_end(ctx, inspector, &frame.input, frame_result);
            frame.set_finished(true);
        }
        result
    }
}
