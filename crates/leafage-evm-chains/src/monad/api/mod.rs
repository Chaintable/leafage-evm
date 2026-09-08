use crate::monad::evm::instructions::monad_instructions;
use crate::monad::precompile::MonadPrecompiles;
use crate::monad::reserve_balance::{apply_reserve_balance_rule, reject_on_reserve_violation};
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
use revm::interpreter::CallOutcome;
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

    /// Checkpoint wrapping the init of the top level message, see
    /// [`Self::settle_first_frame_result`].
    fn first_frame_checkpoint(&mut self) -> Option<JournalCheckpoint> {
        let ctx = &mut self.inner.ctx;
        ctx.cfg()
            .spec()
            .is_reserve_balance_check_enabled()
            .then(|| ctx.journal_mut().checkpoint())
    }

    /// The top level message is running: its own frame checkpoint takes
    /// over from the wrapping one.
    fn commit_first_frame_checkpoint(&mut self, checkpoint: Option<JournalCheckpoint>) {
        if checkpoint.is_some() {
            self.inner.ctx.journal_mut().checkpoint_commit();
        }
    }

    /// The top level message finished during its init: a call to a
    /// precompile or to an account without code (revm never runs a frame
    /// for those). `execute_call_message` applies the reserve balance rule
    /// after `check_call_precompile`, so it covers the value transfer; the
    /// wrapping checkpoint plays the role of the frame revm already
    /// committed. CREATE messages that fail to start (`sender_has_balance`,
    /// EIP-684 collision) return before the check in the node too.
    fn settle_first_frame_result(
        &mut self,
        checkpoint: Option<JournalCheckpoint>,
        frame_result: &mut FrameResult,
    ) -> Result<(), DB::Error> {
        let Some(checkpoint) = checkpoint else {
            return Ok(());
        };
        let violated = match frame_result {
            FrameResult::Call(outcome) => {
                reject_on_reserve_violation(&mut self.inner.ctx, None, &mut outcome.result)?
            }
            FrameResult::Create(_) => false,
        };
        let journal = self.inner.ctx.journal_mut();
        if violated {
            journal.checkpoint_revert(checkpoint);
            self.reserve_balance_violation = true;
        } else {
            journal.checkpoint_commit();
        }
        Ok(())
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
        if let ItemOrResult::Result(mut output) = self.inner.frame_init(frame_input)? {
            self.settle_first_frame_result(checkpoint, &mut output)?;
            return Ok(ItemOrResult::Result(output));
        }
        self.commit_first_frame_checkpoint(checkpoint);
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
        if apply_reserve_balance_rule(ctx, frame, &mut action)? {
            self.reserve_balance_violation = true;
        }

        frame.process_next_action(ctx, action).inspect(|i| {
            if i.is_result() {
                frame.set_finished(true);
            }
        })
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
        let is_first_frame = self.is_first_frame();
        let checkpoint = if is_first_frame {
            self.first_frame_checkpoint()
        } else {
            None
        };
        if let ItemOrResult::Result(mut output) = self.inner.frame_init(frame_init)? {
            if is_first_frame {
                self.settle_first_frame_result(checkpoint, &mut output)?;
            }
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
        self.commit_first_frame_checkpoint(checkpoint);

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
        if apply_reserve_balance_rule(ctx, frame, &mut action)? {
            self.reserve_balance_violation = true;
        }

        let mut result = frame.process_next_action(ctx, action);
        if let Ok(ItemOrResult::Result(frame_result)) = &mut result {
            frame_end(ctx, inspector, &frame.input, frame_result);
            frame.set_finished(true);
        }
        result
    }
}
