use super::{
    frame_result::revert_frame,
    native::{
        blocklist_storage_slot, eip7708_transfer_log, is_blocklisted_status, revert_message,
        ERR_BLOCKED_ADDRESS, ERR_SELFDESTRUCTED_BALANCE_INCREASED, ERR_ZERO_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
    },
    opcode::arc_selfdestruct_instruction,
    precompile::{extend_arc_precompiles, subcall::SubcallPrecompile},
    ArcChainConfig, ArcExecutionSpec, ArcHardfork,
};
use alloy::primitives::{Address, Bytes, Log};
use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId, U256};
use revm::{
    bytecode::{opcode::SELFDESTRUCT, Bytecode},
    context::{ContextTr, Evm as RevmEvm, FrameStack, JournalTr, Transaction, TxEnv},
    context_interface::journaled_state::{JournalCheckpoint, JournalLoadError},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult, ItemOrResult, PrecompileProvider,
    },
    inspector::{
        handler::{frame_end, frame_start},
        InspectorEvmTr, InspectorFrame,
    },
    interpreter::{
        interpreter::EthInterpreter,
        interpreter_action::{FrameInit, FrameInput},
        CallInputs, CallOutcome, CallScheme, CreateScheme, Gas, InstructionResult,
        InterpreterResult,
    },
    precompile::{PrecompileSpecId, Precompiles},
    state::AccountInfo,
    Context, Inspector, Journal,
};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};

mod exec;
mod subcall;

use subcall::{SubcallContinuation, SubcallRegistry};

const SUBCALL_DISPATCH_COST: u64 = 100;

/// REVM context used by Arc query execution.
pub type ArcContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, DB>;

/// Errors detected while turning an RPC execution environment into an Arc EVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArcEvmFactoryError {
    ChainId {
        expected: u64,
        actual: u64,
    },
    EthereumSpec {
        expected: MainnetSpecId,
        actual: MainnetSpecId,
    },
    BlockNumber(U256),
    Timestamp(U256),
}

impl fmt::Display for ArcEvmFactoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainId { expected, actual } => {
                write!(f, "Arc EVM requires chain ID {expected}, got {actual}")
            }
            Self::EthereumSpec { expected, actual } => {
                write!(
                    f,
                    "Arc EVM requires Ethereum spec {expected:?}, got {actual:?}"
                )
            }
            Self::BlockNumber(number) => {
                write!(f, "Arc block number does not fit in u64: {number}")
            }
            Self::Timestamp(timestamp) => {
                write!(f, "Arc block timestamp does not fit in u64: {timestamp}")
            }
        }
    }
}

impl Error for ArcEvmFactoryError {}

/// Creates Arc query EVMs from the chain configuration and target block environment.
#[derive(Debug, Clone, Copy)]
pub struct ArcEvmFactory {
    chain_config: ArcChainConfig,
}

impl ArcEvmFactory {
    pub const fn new(chain_config: ArcChainConfig) -> Self {
        Self { chain_config }
    }

    pub const fn chain_config(&self) -> &ArcChainConfig {
        &self.chain_config
    }

    pub fn execution_spec(
        &self,
        block_env: &BlockEnv,
    ) -> Result<ArcExecutionSpec, ArcEvmFactoryError> {
        let block_number = u64::try_from(block_env.number)
            .map_err(|_| ArcEvmFactoryError::BlockNumber(block_env.number))?;
        let timestamp = u64::try_from(block_env.timestamp)
            .map_err(|_| ArcEvmFactoryError::Timestamp(block_env.timestamp))?;
        Ok(self.chain_config.execution_spec_at(block_number, timestamp))
    }

    pub fn create<DB: Database, I>(
        &self,
        env: EvmEnv<MainnetSpecId>,
        db: DB,
        inspector: I,
    ) -> Result<ArcEvm<DB, I>, ArcEvmFactoryError> {
        self.validate_cfg(&env.cfg_env)?;
        let execution_spec = self.execution_spec(&env.block_env)?;
        Ok(ArcEvm::new(env, db, inspector, execution_spec))
    }

    fn validate_cfg(&self, cfg: &CfgEnv<MainnetSpecId>) -> Result<(), ArcEvmFactoryError> {
        let expected_chain_id = self.chain_config.chain_id();
        if cfg.chain_id != expected_chain_id {
            return Err(ArcEvmFactoryError::ChainId {
                expected: expected_chain_id,
                actual: cfg.chain_id,
            });
        }

        let expected_spec = self.chain_config.ethereum_spec();
        if cfg.spec != expected_spec {
            return Err(ArcEvmFactoryError::EthereumSpec {
                expected: expected_spec,
                actual: cfg.spec,
            });
        }
        Ok(())
    }
}

/// Arc query EVM wrapper.
///
/// Its separate type prevents Arc RPCs from being implemented by the generic
/// mainnet executor and keeps Arc handler, frame, instruction, and precompile
/// behavior in one execution path.
#[allow(missing_debug_implementations)]
pub struct ArcEvm<DB: revm::Database, I> {
    pub(crate) inner: RevmEvm<
        ArcContext<DB>,
        I,
        EthInstructions<EthInterpreter, ArcContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
    execution_spec: ArcExecutionSpec,
    subcall_registry: SubcallRegistry,
    subcall_continuations: HashMap<usize, SubcallContinuation>,
    subcall_trace_completion_hook: Option<fn(&mut I, ArcSubcallTraceCompletion)>,
}

/// Raw child and final completion results for one transparent Arc subcall trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcSubcallTraceCompletion {
    pub child_status: InstructionResult,
    pub child_output: Bytes,
    pub child_gas_used: u64,
    pub child_gas_limit: u64,
    pub final_status: InstructionResult,
    pub phase: ArcSubcallTraceCompletionPhase,
}

/// Position of subcall completion relative to the inspector's frame lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArcSubcallTraceCompletionPhase {
    BeforeFrameEnd,
    AfterFrameEnd,
}

enum BeforeFrameInit {
    Log(Log),
    Revert(FrameResult),
    None,
}

enum FrameInitOutcome {
    Pushed,
    Immediate(FrameResult),
}

struct NativeTransfer {
    from: Address,
    to: Address,
    amount: U256,
}

fn init_subcall_revert(message: &str, call_inputs: &CallInputs) -> FrameResult {
    let mut gas = Gas::new(call_inputs.gas_limit);
    if !gas.record_cost(SUBCALL_DISPATCH_COST) {
        gas.spend_all();
    }
    FrameResult::Call(CallOutcome {
        result: InterpreterResult::new(InstructionResult::Revert, revert_message(message), gas),
        memory_offset: call_inputs.return_memory_offset.clone(),
        was_precompile_called: true,
        precompile_call_logs: Default::default(),
    })
}

fn init_subcall_static_revert(call_inputs: &CallInputs) -> FrameResult {
    let mut gas = Gas::new(call_inputs.gas_limit);
    gas.spend_all();
    FrameResult::Call(CallOutcome {
        result: InterpreterResult::new(
            InstructionResult::Revert,
            revert_message("subcall precompiles cannot be invoked in static context"),
            gas,
        ),
        memory_offset: call_inputs.return_memory_offset.clone(),
        was_precompile_called: true,
        precompile_call_logs: Default::default(),
    })
}

fn subcall_oog(gas: Gas, return_memory_offset: std::ops::Range<usize>) -> FrameResult {
    FrameResult::Call(CallOutcome {
        result: InterpreterResult::new(InstructionResult::OutOfGas, Default::default(), gas),
        memory_offset: return_memory_offset,
        was_precompile_called: true,
        precompile_call_logs: Default::default(),
    })
}

fn resolve_shared_buffer<DB: Database>(ctx: &ArcContext<DB>, frame_input: &mut FrameInit) {
    if let FrameInput::Call(inputs) = &mut frame_input.frame_input {
        if matches!(inputs.input, revm::interpreter::CallInput::SharedBuffer(_)) {
            let input = inputs.input.bytes(ctx);
            inputs.input = revm::interpreter::CallInput::Bytes(input);
        }
    }
}

fn load_account_with_code_metered<J: JournalTr>(
    journal: &mut J,
    address: Address,
    gas: &mut Gas,
) -> Result<Option<AccountInfo>, <J::Database as revm::Database>::Error> {
    let skip_cold_load = gas.remaining() < revm::interpreter::gas::COLD_ACCOUNT_ACCESS_COST;
    match journal.load_account_info_skip_cold_load(address, true, skip_cold_load) {
        Ok(info) => {
            let cost = if info.is_cold {
                revm::interpreter::gas::COLD_ACCOUNT_ACCESS_COST
            } else {
                revm::interpreter::gas::WARM_STORAGE_READ_COST
            };
            if !gas.record_cost(cost) {
                return Ok(None);
            }
            Ok(Some(info.account.into_owned()))
        }
        Err(JournalLoadError::ColdLoadSkipped) => Ok(None),
        Err(JournalLoadError::DBError(error)) => Err(error),
    }
}

fn init_frame<'a, DB: Database>(
    frame_stack: &'a mut FrameStack<EthFrame>,
    ctx: &mut ArcContext<DB>,
    precompiles: &mut PrecompilesMap,
    frame_input: FrameInit,
) -> Result<FrameInitResult<'a, EthFrame>, ContextDbError<ArcContext<DB>>> {
    let is_first_init = frame_stack.index().is_none();
    let new_frame = if is_first_init {
        frame_stack.start_init()
    } else {
        frame_stack.get_next()
    };
    let result = EthFrame::init_with_context(new_frame, ctx, precompiles, frame_input)?;

    Ok(result.map_item(|token| {
        if is_first_init {
            unsafe { frame_stack.end_init(token) };
        } else {
            unsafe { frame_stack.push(token) };
        }
        frame_stack.get()
    }))
}

fn should_keep_transfer_log(result: &FrameResult) -> bool {
    match result {
        FrameResult::Call(outcome) => outcome.instruction_result().is_ok(),
        FrameResult::Create(outcome) => {
            outcome.instruction_result().is_ok() && outcome.address.is_some()
        }
    }
}

fn should_emit_transfer_log<T>(result: &ItemOrResult<T, FrameResult>) -> bool {
    match result {
        ItemOrResult::Item(_) => true,
        ItemOrResult::Result(result) => should_keep_transfer_log(result),
    }
}

impl<DB: Database, I> ArcEvm<DB, I> {
    fn new(
        env: EvmEnv<MainnetSpecId>,
        db: DB,
        inspector: I,
        execution_spec: ArcExecutionSpec,
    ) -> Self {
        let mut cfg = env.cfg_env;
        // Arc owns Zero5 native transfer logs. Keep both REVM Amsterdam EIP-7708 paths off
        // even if this EVM is later constructed with a newer Ethereum base spec.
        cfg.amsterdam_eip7708_disabled = true;
        cfg.amsterdam_eip7708_delayed_burn_disabled = true;
        let spec = cfg.spec;
        let mut precompiles =
            PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(spec)));
        extend_arc_precompiles(&mut precompiles, execution_spec.arc_flags);
        let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
        instructions.insert_instruction(
            SELFDESTRUCT,
            arc_selfdestruct_instruction::<DB>(execution_spec.arc_flags),
        );
        let mut journaled_state = Journal::new(db);
        // Arc implements Zero5 transfer logs itself while its Ethereum base spec remains Osaka.
        // Disable REVM's future Amsterdam EIP-7708 paths, including delayed-burn tracking.
        journaled_state.set_eip7708_config(true, true);
        Self {
            inner: RevmEvm {
                ctx: Context {
                    block: env.block_env,
                    cfg,
                    journaled_state,
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: instructions,
                precompiles,
                frame_stack: Default::default(),
            },
            execution_spec,
            subcall_registry: SubcallRegistry::for_hardforks(execution_spec.arc_flags),
            subcall_continuations: HashMap::new(),
            subcall_trace_completion_hook: None,
        }
    }

    /// Installs an observer for transparent subcall completion metadata.
    pub fn set_subcall_trace_completion_hook(
        &mut self,
        hook: fn(&mut I, ArcSubcallTraceCompletion),
    ) {
        self.subcall_trace_completion_hook = Some(hook);
    }

    fn notify_subcall_trace_completion(&mut self, completion: ArcSubcallTraceCompletion) {
        if let Some(hook) = self.subcall_trace_completion_hook {
            hook(&mut self.inner.inspector, completion);
        }
    }

    pub const fn execution_spec(&self) -> ArcExecutionSpec {
        self.execution_spec
    }

    pub const fn ctx(&self) -> &ArcContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut ArcContext<DB> {
        &mut self.inner.ctx
    }

    fn is_address_blocklisted(
        &mut self,
        address: Address,
    ) -> Result<bool, ContextDbError<ArcContext<DB>>> {
        let state_load = self
            .inner
            .ctx
            .journal_mut()
            .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(address))?;
        Ok(is_blocklisted_status(state_load.data))
    }

    fn create_transfer(
        &mut self,
        inputs: &mut revm::interpreter::CreateInputs,
        depth: usize,
    ) -> Result<Option<NativeTransfer>, ContextDbError<ArcContext<DB>>> {
        if inputs.value().is_zero() {
            return Ok(None);
        }

        match inputs.scheme() {
            CreateScheme::Create => {
                let nonce = if depth == 0 {
                    self.inner.ctx.tx().nonce()
                } else {
                    self.inner
                        .ctx
                        .journal_mut()
                        .load_account(inputs.caller())?
                        .info
                        .nonce
                };
                Ok(Some(NativeTransfer {
                    from: inputs.caller(),
                    to: inputs.created_address(nonce),
                    amount: inputs.value(),
                }))
            }
            CreateScheme::Create2 { .. } => {
                let created_address = inputs.created_address(0);
                inputs.set_scheme(CreateScheme::Custom {
                    address: created_address,
                });
                Ok(Some(NativeTransfer {
                    from: inputs.caller(),
                    to: created_address,
                    amount: inputs.value(),
                }))
            }
            CreateScheme::Custom { .. } => Ok(None),
        }
    }

    fn transfer_participants(
        &mut self,
        frame_input: &mut FrameInit,
    ) -> Result<Option<NativeTransfer>, ContextDbError<ArcContext<DB>>> {
        match &mut frame_input.frame_input {
            FrameInput::Empty => Ok(None),
            FrameInput::Create(inputs) => self.create_transfer(inputs, frame_input.depth),
            FrameInput::Call(inputs) => match inputs.scheme {
                CallScheme::Call => Ok(Some(NativeTransfer {
                    from: inputs.transfer_from(),
                    to: inputs.transfer_to(),
                    amount: inputs.transfer_value().unwrap_or_default(),
                })),
                CallScheme::CallCode | CallScheme::DelegateCall | CallScheme::StaticCall => {
                    Ok(None)
                }
            },
        }
    }

    fn before_frame_init(
        &mut self,
        frame_input: &mut FrameInit,
    ) -> Result<BeforeFrameInit, ContextDbError<ArcContext<DB>>> {
        let Some(NativeTransfer { from, to, amount }) = self.transfer_participants(frame_input)?
        else {
            return Ok(BeforeFrameInit::None);
        };
        if amount.is_zero() {
            return Ok(BeforeFrameInit::None);
        }

        let flags = self.execution_spec.arc_flags;
        if flags.is_active(ArcHardfork::Zero5) && (from.is_zero() || to.is_zero()) {
            return Ok(BeforeFrameInit::Revert(revert_frame(
                frame_input,
                ERR_ZERO_ADDRESS,
            )));
        }
        if self.is_address_blocklisted(from)? || self.is_address_blocklisted(to)? {
            return Ok(BeforeFrameInit::Revert(revert_frame(
                frame_input,
                ERR_BLOCKED_ADDRESS,
            )));
        }
        let target_is_selfdestructed = if flags.is_active(ArcHardfork::Zero5) {
            let journal = self.inner.ctx.journal_mut();
            if flags.is_active(ArcHardfork::Zero7) {
                // Zero7 makes this an observation-only probe. The target must not remain
                // warm merely because Arc checked whether it was already selfdestructed.
                let checkpoint = journal.checkpoint();
                let result = journal
                    .load_account(to)
                    .map(|account| account.is_selfdestructed());
                journal.checkpoint_revert(checkpoint);
                result?
            } else {
                journal
                    .load_account(to)
                    .map(|account| account.is_selfdestructed())?
            }
        } else {
            false
        };
        if target_is_selfdestructed {
            return Ok(BeforeFrameInit::Revert(revert_frame(
                frame_input,
                ERR_SELFDESTRUCTED_BALANCE_INCREASED,
            )));
        }

        if flags.is_active(ArcHardfork::Zero5) && from != to {
            Ok(BeforeFrameInit::Log(eip7708_transfer_log(from, to, amount)))
        } else {
            Ok(BeforeFrameInit::None)
        }
    }

    fn checked_frame_init(
        &mut self,
        mut frame_input: FrameInit,
    ) -> Result<FrameInitOutcome, ContextDbError<ArcContext<DB>>> {
        let transfer_log = match self.before_frame_init(&mut frame_input)? {
            BeforeFrameInit::Log(log) => Some(log),
            BeforeFrameInit::Revert(result) => return Ok(FrameInitOutcome::Immediate(result)),
            BeforeFrameInit::None => None,
        };
        let is_precompile = match &frame_input.frame_input {
            FrameInput::Call(inputs) => {
                <PrecompilesMap as PrecompileProvider<ArcContext<DB>>>::contains(
                    &self.inner.precompiles,
                    &inputs.bytecode_address,
                )
            }
            FrameInput::Create(_) | FrameInput::Empty => false,
        };

        let result = if is_precompile && self.execution_spec.arc_flags.is_active(ArcHardfork::Zero5)
        {
            let checkpoint = transfer_log.map(|log| {
                let checkpoint = self.inner.ctx.journal_mut().checkpoint();
                self.inner.ctx.journal_mut().log(log);
                checkpoint
            });
            let result = init_frame(
                &mut self.inner.frame_stack,
                &mut self.inner.ctx,
                &mut self.inner.precompiles,
                frame_input,
            )?;
            if let Some(checkpoint) = checkpoint {
                if should_emit_transfer_log(&result) {
                    self.inner.ctx.journal_mut().checkpoint_commit();
                } else {
                    self.inner.ctx.journal_mut().checkpoint_revert(checkpoint);
                }
            }
            result
        } else {
            let result = init_frame(
                &mut self.inner.frame_stack,
                &mut self.inner.ctx,
                &mut self.inner.precompiles,
                frame_input,
            )?;
            if let Some(log) = transfer_log {
                if should_emit_transfer_log(&result) {
                    self.inner.ctx.journal_mut().log(log);
                }
            }
            result
        };

        match result {
            ItemOrResult::Item(_) => Ok(FrameInitOutcome::Pushed),
            ItemOrResult::Result(result) => Ok(FrameInitOutcome::Immediate(result)),
        }
    }

    fn init_subcall(
        &mut self,
        mut frame_input: FrameInit,
        precompile: Arc<dyn SubcallPrecompile>,
    ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<ArcContext<DB>>> {
        let FrameInput::Call(inputs) = &frame_input.frame_input else {
            return Ok(ItemOrResult::Result(revert_frame(
                &frame_input,
                "internal error: subcall interception on non-call frame",
            )));
        };
        if inputs.scheme != CallScheme::Call {
            return Ok(ItemOrResult::Result(init_subcall_revert(
                "subcall precompiles only support CALL scheme",
                inputs,
            )));
        }
        if inputs.is_static {
            return Ok(ItemOrResult::Result(init_subcall_static_revert(inputs)));
        }
        if inputs.transfers_value() {
            return Ok(ItemOrResult::Result(init_subcall_revert(
                "subcall precompiles do not support value transfers",
                inputs,
            )));
        }

        resolve_shared_buffer(&self.inner.ctx, &mut frame_input);
        let FrameInput::Call(inputs) = &frame_input.frame_input else {
            unreachable!("call input was checked before resolving its shared buffer")
        };
        let init_result = match precompile.init_subcall(inputs) {
            Ok(result) => result,
            Err(error) => {
                return Ok(ItemOrResult::Result(init_subcall_revert(
                    &error.to_string(),
                    inputs,
                )));
            }
        };

        if init_result.child_inputs.caller != inputs.caller
            && init_result.child_inputs.caller != self.inner.ctx.tx().caller()
        {
            return Ok(ItemOrResult::Result(init_subcall_revert(
                "sender spoofing requires tx.origin as sender",
                inputs,
            )));
        }

        let return_memory_offset = inputs.return_memory_offset.clone();
        let parent_gas_limit = inputs.gas_limit;
        let depth = frame_input.depth;
        let mut child_inputs = init_result.child_inputs;

        // These loads intentionally happen before the checkpoint. Like a normal CALL opcode,
        // account warming survives child failure and CallFrom completion failure.
        self.inner
            .ctx
            .journal_mut()
            .load_account(child_inputs.caller)?;
        let mut gas = Gas::new(parent_gas_limit);
        if !gas.record_cost(init_result.gas_overhead) {
            gas.spend_all();
            return Ok(ItemOrResult::Result(subcall_oog(gas, return_memory_offset)));
        }

        let Some(target) = load_account_with_code_metered(
            self.inner.ctx.journal_mut(),
            child_inputs.target_address,
            &mut gas,
        )?
        else {
            gas.spend_all();
            return Ok(ItemOrResult::Result(subcall_oog(gas, return_memory_offset)));
        };

        if let Some(delegate_address) = target.code.as_ref().and_then(Bytecode::eip7702_address) {
            let Some(delegate) = load_account_with_code_metered(
                self.inner.ctx.journal_mut(),
                delegate_address,
                &mut gas,
            )?
            else {
                gas.spend_all();
                return Ok(ItemOrResult::Result(subcall_oog(gas, return_memory_offset)));
            };
            if let Some(code) = delegate.code {
                child_inputs.known_bytecode = Some((delegate.code_hash, code));
            }
        }

        // Neutralize checkpoint depth so the synthetic child remains adjacent to the visible
        // parent in tracing, while retaining a checkpoint that can undo a successful child if
        // CallFrom completion later fails.
        let checkpoint = self.inner.ctx.journal_mut().checkpoint();
        self.inner.ctx.journal_mut().checkpoint_commit();

        let remaining = gas.remaining();
        #[allow(clippy::arithmetic_side_effects)]
        let child_gas_limit = remaining - remaining / 64;
        child_inputs.gas_limit = child_gas_limit;
        #[allow(clippy::arithmetic_side_effects)]
        let child_depth = depth + 1;
        let child_frame_input = FrameInit {
            depth: child_depth,
            memory: frame_input.memory,
            frame_input: FrameInput::Call(child_inputs),
        };

        let continuation = SubcallContinuation {
            precompile,
            gas_limit: parent_gas_limit,
            init_subcall_gas_overhead: gas.spent(),
            return_memory_offset,
            continuation_data: init_result.continuation_data,
            checkpoint,
        };
        match self.checked_frame_init(child_frame_input)? {
            FrameInitOutcome::Pushed => {
                self.subcall_continuations.insert(depth, continuation);
                Ok(ItemOrResult::Item(self.inner.frame_stack.get()))
            }
            FrameInitOutcome::Immediate(child_result) => {
                let trace_completion = self.subcall_trace_completion_hook.is_some().then(|| {
                    (
                        child_result.instruction_result(),
                        child_result.interpreter_result().output.clone(),
                        child_result.gas().spent(),
                        child_result.gas().limit(),
                    )
                });
                let final_result = self.complete_subcall(child_result, continuation)?;
                if let Some((child_status, child_output, child_gas_used, child_gas_limit)) =
                    trace_completion
                {
                    self.notify_subcall_trace_completion(ArcSubcallTraceCompletion {
                        child_status,
                        child_output,
                        child_gas_used,
                        child_gas_limit,
                        final_status: final_result.instruction_result(),
                        phase: ArcSubcallTraceCompletionPhase::BeforeFrameEnd,
                    });
                }
                Ok(ItemOrResult::Result(final_result))
            }
        }
    }

    fn complete_subcall(
        &mut self,
        child_result: FrameResult,
        continuation: SubcallContinuation,
    ) -> Result<FrameResult, ContextDbError<ArcContext<DB>>> {
        let child_gas = child_result.gas();
        let (child_succeeded, child_halted) = match &child_result {
            FrameResult::Call(outcome) => {
                let result = outcome.result.result;
                (result.is_ok(), !result.is_ok_or_revert())
            }
            FrameResult::Create(_) => (false, true),
        };
        let completion = continuation
            .precompile
            .complete_subcall(continuation.continuation_data, &child_result);
        let completion_gas = completion.as_ref().map_or(0, |result| result.gas_overhead);
        let metered_gas_used = continuation
            .init_subcall_gas_overhead
            .checked_add(child_gas.spent())
            .expect("subcall gas overflow after child execution")
            .checked_add(completion_gas)
            .expect("subcall gas overflow during completion");
        let gas_used = if child_halted {
            continuation.gas_limit.max(metered_gas_used)
        } else {
            metered_gas_used
        };
        let mut gas = Gas::new(continuation.gas_limit);
        if !gas.record_cost(gas_used) {
            gas.spend_all();
            if child_succeeded {
                self.revert_subcall_checkpoint(continuation.checkpoint);
            }
            return Ok(subcall_oog(gas, continuation.return_memory_offset));
        }

        match completion {
            Ok(result) if result.success => {
                if child_succeeded {
                    gas.record_refund(child_gas.refunded());
                }
                Ok(FrameResult::Call(CallOutcome {
                    result: InterpreterResult::new(InstructionResult::Return, result.output, gas),
                    memory_offset: continuation.return_memory_offset,
                    was_precompile_called: true,
                    precompile_call_logs: Default::default(),
                }))
            }
            failure => {
                let output = match failure {
                    Ok(result) => result.output,
                    Err(_) => {
                        gas.spend_all();
                        Default::default()
                    }
                };
                if child_succeeded {
                    self.revert_subcall_checkpoint(continuation.checkpoint);
                }
                Ok(FrameResult::Call(CallOutcome {
                    result: InterpreterResult::new(InstructionResult::Revert, output, gas),
                    memory_offset: continuation.return_memory_offset,
                    was_precompile_called: true,
                    precompile_call_logs: Default::default(),
                }))
            }
        }
    }

    fn revert_subcall_checkpoint(&mut self, checkpoint: JournalCheckpoint) {
        let depth = self.inner.ctx.journal_mut().depth();
        let _ = self.inner.ctx.journal_mut().checkpoint();
        self.inner.ctx.journal_mut().checkpoint_revert(checkpoint);
        debug_assert_eq!(self.inner.ctx.journal_mut().depth(), depth);
    }
}

impl<DB: Database, I> Deref for ArcEvm<DB, I> {
    type Target = ArcContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for ArcEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB: Database, I> EvmTr for ArcEvm<DB, I> {
    type Context = ArcContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, ArcContext<DB>>;
    type Precompiles = PrecompilesMap;
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
        mut frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        if let FrameInput::Call(inputs) = &frame_input.frame_input {
            if let Some((precompile, allowed_callers)) =
                self.subcall_registry.get(&inputs.target_address)
            {
                if !allowed_callers.is_allowed(&inputs.caller) {
                    return Ok(ItemOrResult::Result(init_subcall_revert(
                        "unauthorized caller",
                        inputs,
                    )));
                }
                let precompile = Arc::clone(precompile);

                // CallFrom currently requires zero value, so this is normally a no-op. Keep the
                // Arc transfer checks in front of the interception so future subcall precompiles
                // cannot bypass blocklist or EIP-7708 behavior by allowing value.
                match self.before_frame_init(&mut frame_input)? {
                    BeforeFrameInit::Revert(result) => {
                        return Ok(ItemOrResult::Result(result));
                    }
                    BeforeFrameInit::Log(_) | BeforeFrameInit::None => {}
                }
                return self.init_subcall(frame_input, precompile);
            }
        }

        match self.checked_frame_init(frame_input)? {
            FrameInitOutcome::Pushed => Ok(ItemOrResult::Item(self.inner.frame_stack.get())),
            FrameInitOutcome::Immediate(result) => Ok(ItemOrResult::Result(result)),
        }
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        let frame_was_finished = self.inner.frame_stack.get().is_finished();
        let finished_depth = self.inner.frame_stack.get().depth;

        if frame_was_finished {
            self.inner.frame_stack.pop();
        }
        let stack_empty = self.inner.frame_stack.index().is_none();

        if frame_was_finished {
            if let Some(key) = finished_depth.checked_sub(1) {
                if let Some(continuation) = self.subcall_continuations.remove(&key) {
                    let trace_completion =
                        self.subcall_trace_completion_hook.is_some().then(|| {
                            (
                                result.instruction_result(),
                                result.interpreter_result().output.clone(),
                                result.gas().spent(),
                                result.gas().limit(),
                            )
                        });
                    let final_result = self.complete_subcall(result, continuation)?;
                    if let Some((child_status, child_output, child_gas_used, child_gas_limit)) =
                        trace_completion
                    {
                        self.notify_subcall_trace_completion(ArcSubcallTraceCompletion {
                            child_status,
                            child_output,
                            child_gas_used,
                            child_gas_limit,
                            final_status: final_result.instruction_result(),
                            phase: ArcSubcallTraceCompletionPhase::AfterFrameEnd,
                        });
                    }
                    if stack_empty {
                        return Ok(Some(final_result));
                    }
                    self.inner
                        .frame_stack
                        .get()
                        .return_result::<_, ContextDbError<Self::Context>>(
                            &mut self.inner.ctx,
                            final_result,
                        )?;
                    return Ok(None);
                }
            }
        }

        if stack_empty {
            return Ok(Some(result));
        }
        self.inner
            .frame_stack
            .get()
            .return_result::<_, ContextDbError<Self::Context>>(&mut self.inner.ctx, result)?;
        Ok(None)
    }
}

impl<DB, I> InspectorEvmTr for ArcEvm<DB, I>
where
    DB: Database,
    I: Inspector<ArcContext<DB>, EthInterpreter>,
{
    type Inspector = I;

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

    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let is_subcall = matches!(
            &frame_init.frame_input,
            FrameInput::Call(inputs)
                if self.subcall_registry.get(&inputs.target_address).is_some()
        );
        if !is_subcall {
            return self.inspect_frame_init_impl(frame_init, None);
        }

        resolve_shared_buffer(&self.inner.ctx, &mut frame_init);
        let FrameInput::Call(inputs) = &frame_init.frame_input else {
            unreachable!("subcall registry only matches call frames")
        };
        let trace_input = self
            .subcall_registry
            .get(&inputs.target_address)
            .and_then(|(precompile, _)| precompile.trace_child_call(inputs))
            .map(|inputs| FrameInput::Call(Box::new(inputs)))
            .unwrap_or_else(|| frame_init.frame_input.clone());

        self.inspect_frame_init_impl(frame_init, Some(trace_input))
    }
}

impl<DB, I> ArcEvm<DB, I>
where
    DB: Database,
    I: Inspector<ArcContext<DB>, EthInterpreter>,
{
    fn frame_start_with_trace(
        &mut self,
        frame_init: &mut FrameInit,
        trace_override: Option<FrameInput>,
    ) -> Result<FrameInput, Box<FrameResult>> {
        let (ctx, inspector) = self.ctx_inspector();
        match trace_override {
            Some(mut trace_input) => {
                if let Some(mut output) = frame_start(ctx, inspector, &mut trace_input) {
                    frame_end(ctx, inspector, &trace_input, &mut output);
                    return Err(Box::new(output));
                }
                Ok(trace_input)
            }
            None => {
                if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
                    frame_end(ctx, inspector, &frame_init.frame_input, &mut output);
                    return Err(Box::new(output));
                }
                Ok(frame_init.frame_input.clone())
            }
        }
    }

    fn inspect_frame_init_impl(
        &mut self,
        mut frame_init: FrameInit,
        trace_override: Option<FrameInput>,
    ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<ArcContext<DB>>> {
        let trace_input = match self.frame_start_with_trace(&mut frame_init, trace_override) {
            Ok(input) => input,
            Err(output) => return Ok(ItemOrResult::Result(*output)),
        };

        let logs_i = self.inner.ctx.journal().logs().len();
        if let ItemOrResult::Result(mut output) = self.frame_init(frame_init)? {
            let (ctx, inspector) = self.ctx_inspector();
            let logs_len = ctx.journal().logs().len();
            for log_index in logs_i..logs_len {
                let log = ctx.journal().logs()[log_index].clone();
                inspector.log(ctx, log);
            }
            if let FrameResult::Call(CallOutcome {
                was_precompile_called,
                precompile_call_logs,
                ..
            }) = &mut output
            {
                if *was_precompile_called {
                    for log in precompile_call_logs.iter().cloned() {
                        inspector.log(ctx, log);
                    }
                }
            }
            frame_end(ctx, inspector, &trace_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        let logs_len = ctx.journal().logs().len();
        for log_index in logs_i..logs_len {
            let log = ctx.journal().logs()[log_index].clone();
            inspector.log(ctx, log);
        }
        if let Some(frame) = frame.eth_frame() {
            inspector.initialize_interp(&mut frame.interpreter, ctx);
        }
        Ok(ItemOrResult::Item(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::{
        native::revert_message,
        precompile::{
            call_from::{abi_decode_gas, CallFromPrecompile, CALL_FROM_ADDRESS, MEMO_ADDRESS},
            subcall::{
                SubcallCompletionResult, SubcallContinuationData, SubcallError, SubcallInitResult,
            },
        },
        ArcHardfork, ArcHardforkFlags, ARC_MAINNET_CHAIN_ID,
        ARC_ZERO7_HARDFORK_TIMESTAMP_ACTIVATION_MAINNET,
    };
    use alloy::primitives::{address, keccak256, Address, Bytes, LogData, B256};
    use alloy::sol_types::{sol, SolCall};
    use alloy_evm::precompiles::DynPrecompile;
    use revm::interpreter::{CallInput, CallInputs, CallValue, CreateInputs, SharedMemory};
    use revm::{
        bytecode::{opcode, Bytecode},
        context_interface::block::BlobExcessGasAndPrice,
        database::InMemoryDB,
        handler::PrecompileProvider,
        inspector::NoOpInspector,
        precompile::{PrecompileId, PrecompileOutput},
        primitives::TxKind,
        state::AccountInfo,
        ExecuteEvm, InspectEvm,
    };

    const SOURCE: Address = address!("1000000000000000000000000000000000000001");
    const TARGET: Address = address!("2000000000000000000000000000000000000002");
    const MOCK_PRECOMPILE: Address = address!("ff00000000000000000000000000000000000099");
    const MOCK_LOG_ADDRESS: Address = address!("aa00000000000000000000000000000000000001");

    sol! {
        interface ITestCallFrom {
            function callFrom(address sender, address target, bytes calldata data)
                external returns (bool success, bytes memory returnData);
        }
    }

    fn evm_env_at(timestamp: u64) -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;
        let block = BlockEnv {
            number: U256::from(1),
            timestamp: U256::from(timestamp),
            gas_limit: 30_000_000,
            prevrandao: Some(B256::ZERO),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
            ..Default::default()
        };
        EvmEnv::new(cfg, block)
    }

    fn evm_env() -> EvmEnv<MainnetSpecId> {
        evm_env_at(1)
    }

    fn arc_evm(db: InMemoryDB) -> ArcEvm<InMemoryDB, NoOpInspector> {
        ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(evm_env(), db, NoOpInspector {})
            .unwrap()
    }

    fn post_zero7_evm<I>(db: InMemoryDB, inspector: I) -> ArcEvm<InMemoryDB, I> {
        ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(
                evm_env_at(ARC_ZERO7_HARDFORK_TIMESTAMP_ACTIVATION_MAINNET),
                db,
                inspector,
            )
            .unwrap()
    }

    fn load_native_coin_control(evm: &mut ArcEvm<InMemoryDB, NoOpInspector>) {
        evm.ctx_mut()
            .journal_mut()
            .load_account(NATIVE_COIN_CONTROL_ADDRESS)
            .unwrap();
    }

    fn call_frame(scheme: CallScheme, from: Address, to: Address, value: U256) -> FrameInit {
        FrameInit {
            frame_input: FrameInput::Call(Box::new(CallInputs {
                scheme,
                target_address: to,
                bytecode_address: to,
                known_bytecode: None,
                value: CallValue::Transfer(value),
                input: CallInput::Bytes(Bytes::new()),
                gas_limit: 100_000,
                is_static: false,
                caller: from,
                return_memory_offset: 0..0,
            })),
            memory: SharedMemory::default(),
            depth: 1,
        }
    }

    fn create_frame(
        scheme: CreateScheme,
        from: Address,
        value: U256,
        init_code: Bytes,
    ) -> FrameInit {
        FrameInit {
            frame_input: FrameInput::Create(Box::new(CreateInputs::new(
                from, scheme, value, init_code, 100_000,
            ))),
            memory: SharedMemory::default(),
            depth: 1,
        }
    }

    fn blocklist_in_db(db: &mut InMemoryDB, address: Address) {
        db.insert_account_storage(
            NATIVE_COIN_CONTROL_ADDRESS,
            blocklist_storage_slot(address),
            U256::ONE,
        )
        .unwrap();
    }

    fn assert_call_revert(result: BeforeFrameInit, message: &str) {
        let BeforeFrameInit::Revert(FrameResult::Call(outcome)) = result else {
            panic!("expected nested CALL revert");
        };
        assert_eq!(outcome.result.output, revert_message(message));
        assert_eq!(
            outcome.result.gas.spent(),
            0,
            "Arc blocklist reads are unmetered"
        );
    }

    fn insert_contract(db: &mut InMemoryDB, address: Address, balance: U256, raw: Bytes) {
        db.insert_account_info(
            address,
            AccountInfo {
                balance,
                nonce: 1,
                code_hash: keccak256(&raw),
                code: Some(Bytecode::new_raw(raw)),
                ..Default::default()
            },
        );
    }

    fn call_with_value_bytecode(target: Address, value: U256, revert_after_call: bool) -> Bytes {
        let mut code = vec![
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH32,
        ];
        code.extend_from_slice(&value.to_be_bytes::<32>());
        code.push(opcode::PUSH20);
        code.extend_from_slice(target.as_slice());
        code.extend_from_slice(&[opcode::GAS, opcode::CALL, opcode::POP]);
        if revert_after_call {
            code.extend_from_slice(&[opcode::PUSH1, 0, opcode::PUSH1, 0, opcode::REVERT]);
        } else {
            code.push(opcode::STOP);
        }
        code.into()
    }

    fn create_with_value_bytecode(init_code: &[u8], value: U256) -> Bytes {
        let mut code = Vec::new();
        for (offset, byte) in init_code.iter().enumerate() {
            code.extend_from_slice(&[
                opcode::PUSH1,
                *byte,
                opcode::PUSH1,
                offset as u8,
                opcode::MSTORE8,
            ]);
        }
        code.extend_from_slice(&[opcode::PUSH1, init_code.len() as u8, opcode::PUSH1, 0]);
        code.push(opcode::PUSH32);
        code.extend_from_slice(&value.to_be_bytes::<32>());
        code.extend_from_slice(&[opcode::CREATE, opcode::POP, opcode::STOP]);
        code.into()
    }

    fn call_tx(caller: Address, target: Address, gas_limit: u64) -> TxEnv {
        TxEnv {
            caller,
            kind: TxKind::Call(target),
            gas_limit,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        }
    }

    fn call_from_input(sender: Address, target: Address, data: Bytes) -> Bytes {
        ITestCallFrom::callFromCall {
            sender,
            target,
            data,
        }
        .abi_encode()
        .into()
    }

    fn call_from_frame(
        caller: Address,
        sender: Address,
        target: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> FrameInit {
        FrameInit {
            frame_input: FrameInput::Call(Box::new(CallInputs {
                scheme: CallScheme::Call,
                target_address: CALL_FROM_ADDRESS,
                bytecode_address: CALL_FROM_ADDRESS,
                known_bytecode: None,
                value: CallValue::Transfer(U256::ZERO),
                input: CallInput::Bytes(call_from_input(sender, target, data)),
                gas_limit,
                is_static: false,
                caller,
                return_memory_offset: 0..0,
            })),
            memory: SharedMemory::default(),
            depth: 1,
        }
    }

    fn run_call_from_frame(frame: FrameInit, tx_origin: Address) -> CallOutcome {
        let mut evm = post_zero7_evm(InMemoryDB::default(), NoOpInspector {});
        evm.inner.ctx.tx = call_tx(tx_origin, TARGET, 1_000_000);
        load_native_coin_control(&mut evm);
        match evm.frame_init(frame).expect("CallFrom frame executes") {
            ItemOrResult::Result(FrameResult::Call(outcome)) => outcome,
            ItemOrResult::Result(FrameResult::Create(_)) => {
                panic!("CallFrom must return a CALL outcome")
            }
            ItemOrResult::Item(_) => panic!("empty target must complete immediately"),
        }
    }

    fn forwarding_call_code(target: Address) -> Bytes {
        let mut code = vec![
            opcode::CALLDATASIZE,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::CALLDATACOPY,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::CALLDATASIZE,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH20,
        ];
        code.extend_from_slice(target.as_slice());
        code.extend_from_slice(&[
            opcode::GAS,
            opcode::CALL,
            opcode::POP,
            opcode::RETURNDATASIZE,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::RETURNDATACOPY,
            opcode::RETURNDATASIZE,
            opcode::PUSH1,
            0,
            opcode::RETURN,
        ]);
        code.into()
    }

    fn return_caller_code() -> Bytes {
        Bytes::from_static(&[
            opcode::CALLER,
            opcode::PUSH1,
            0,
            opcode::MSTORE,
            opcode::PUSH1,
            32,
            opcode::PUSH1,
            0,
            opcode::RETURN,
        ])
    }

    fn counter_code() -> Bytes {
        Bytes::from_static(&[
            opcode::PUSH0,
            opcode::SLOAD,
            opcode::PUSH1,
            1,
            opcode::ADD,
            opcode::DUP1,
            opcode::PUSH0,
            opcode::SSTORE,
            opcode::PUSH0,
            opcode::MSTORE,
            opcode::PUSH1,
            32,
            opcode::PUSH0,
            opcode::RETURN,
        ])
    }

    #[derive(Default)]
    struct CallRecorder {
        calls: Vec<CallInputs>,
        subcall_completions: Vec<ArcSubcallTraceCompletion>,
    }

    fn record_subcall_completion(
        inspector: &mut CallRecorder,
        completion: ArcSubcallTraceCompletion,
    ) {
        inspector.subcall_completions.push(completion);
    }

    struct RejectingCompletionPrecompile;

    impl SubcallPrecompile for RejectingCompletionPrecompile {
        fn init_subcall(&self, inputs: &CallInputs) -> Result<SubcallInitResult, SubcallError> {
            CallFromPrecompile.init_subcall(inputs)
        }

        fn complete_subcall(
            &self,
            _continuation_data: SubcallContinuationData,
            _child_result: &FrameResult,
        ) -> Result<SubcallCompletionResult, SubcallError> {
            Ok(SubcallCompletionResult {
                output: Bytes::from_static(b"completion rejected"),
                success: false,
                gas_overhead: 0,
            })
        }

        fn trace_child_call(&self, inputs: &CallInputs) -> Option<CallInputs> {
            CallFromPrecompile.trace_child_call(inputs)
        }
    }

    impl Inspector<ArcContext<InMemoryDB>, EthInterpreter> for CallRecorder {
        fn call(
            &mut self,
            _context: &mut ArcContext<InMemoryDB>,
            inputs: &mut CallInputs,
        ) -> Option<CallOutcome> {
            self.calls.push(inputs.clone());
            None
        }
    }

    #[test]
    fn factory_resolves_arc_flags_for_each_block() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
        let spec = factory.execution_spec(&evm_env().block_env).unwrap();

        for hardfork in [
            ArcHardfork::Zero3,
            ArcHardfork::Zero4,
            ArcHardfork::Zero5,
            ArcHardfork::Zero6,
        ] {
            assert!(spec.arc_flags.is_active(hardfork));
        }
        assert!(!spec.arc_flags.is_active(ArcHardfork::Zero7));
        assert!(!spec.arc_flags.is_active(ArcHardfork::Zero8));
    }

    #[test]
    fn factory_rejects_wrong_chain_spec_and_oversized_block_fields() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());

        let mut env = evm_env();
        env.cfg_env.chain_id = 1;
        let err = match factory.create(env, InMemoryDB::default(), NoOpInspector {}) {
            Err(err) => err,
            Ok(_) => panic!("wrong Arc chain ID must be rejected"),
        };
        assert!(matches!(err, ArcEvmFactoryError::ChainId { actual: 1, .. }));

        let mut env = evm_env();
        env.cfg_env
            .set_spec_and_mainnet_gas_params(MainnetSpecId::PRAGUE);
        let err = match factory.create(env, InMemoryDB::default(), NoOpInspector {}) {
            Err(err) => err,
            Ok(_) => panic!("wrong Ethereum spec must be rejected"),
        };
        assert!(matches!(
            err,
            ArcEvmFactoryError::EthereumSpec {
                actual: MainnetSpecId::PRAGUE,
                ..
            }
        ));

        let mut block = evm_env().block_env;
        block.number = U256::from(u64::MAX) + U256::from(1);
        assert!(matches!(
            factory.execution_spec(&block),
            Err(ArcEvmFactoryError::BlockNumber(_))
        ));

        block.number = U256::ZERO;
        block.timestamp = U256::from(u64::MAX) + U256::from(1);
        assert!(matches!(
            factory.execution_spec(&block),
            Err(ArcEvmFactoryError::Timestamp(_))
        ));
    }

    #[test]
    fn osaka_standard_precompiles_include_p256() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
        let evm = factory
            .create(evm_env(), InMemoryDB::default(), NoOpInspector {})
            .unwrap();
        let p256 = address!("0000000000000000000000000000000000000100");

        assert!(<PrecompilesMap as PrecompileProvider<
            ArcContext<InMemoryDB>,
        >>::contains(&evm.inner.precompiles, &p256));
    }

    #[test]
    fn factory_disables_both_revm_eip7708_paths() {
        let evm = arc_evm(InMemoryDB::default());

        assert!(evm.ctx().cfg.amsterdam_eip7708_disabled);
        assert!(evm.ctx().cfg.amsterdam_eip7708_delayed_burn_disabled);
        assert!(evm.ctx().journaled_state.cfg.eip7708_disabled);
        assert!(evm.ctx().journaled_state.cfg.eip7708_delayed_burn_disabled);
        assert!(evm
            .ctx()
            .journaled_state
            .selfdestructed_addresses
            .is_empty());
    }

    #[test]
    fn nested_call_blocklist_checks_source_before_target_without_gas_charge() {
        let mut db = InMemoryDB::default();
        blocklist_in_db(&mut db, SOURCE);
        blocklist_in_db(&mut db, TARGET);
        let mut evm = arc_evm(db);
        load_native_coin_control(&mut evm);

        let mut frame = call_frame(CallScheme::Call, SOURCE, TARGET, U256::ONE);
        let result = evm.before_frame_init(&mut frame).unwrap();
        assert_call_revert(result, ERR_BLOCKED_ADDRESS);

        assert!(
            !evm.ctx_mut()
                .journal_mut()
                .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(SOURCE))
                .unwrap()
                .is_cold
        );
        assert!(
            evm.ctx_mut()
                .journal_mut()
                .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(TARGET))
                .unwrap()
                .is_cold
        );
    }

    #[test]
    fn nested_zero_and_zero_value_checks_short_circuit_blocklist_reads() {
        let mut db = InMemoryDB::default();
        blocklist_in_db(&mut db, SOURCE);
        blocklist_in_db(&mut db, TARGET);
        let mut evm = arc_evm(db);
        load_native_coin_control(&mut evm);

        let mut zero_target = call_frame(CallScheme::Call, SOURCE, Address::ZERO, U256::ONE);
        let result = evm.before_frame_init(&mut zero_target).unwrap();
        assert_call_revert(result, ERR_ZERO_ADDRESS);
        assert!(
            evm.ctx_mut()
                .journal_mut()
                .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(SOURCE))
                .unwrap()
                .is_cold
        );

        let mut zero_value = call_frame(CallScheme::Call, SOURCE, TARGET, U256::ZERO);
        assert!(matches!(
            evm.before_frame_init(&mut zero_value).unwrap(),
            BeforeFrameInit::None
        ));
    }

    #[test]
    fn nested_create_and_create2_check_the_derived_target() {
        let init_code = Bytes::from_static(&[0x60, 0x00]);
        let salt = U256::from(123);
        let create_target = SOURCE.create(7);
        let create2_target = SOURCE.create2(salt.to_be_bytes(), keccak256(&init_code));
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                nonce: 7,
                ..Default::default()
            },
        );
        blocklist_in_db(&mut db, create_target);
        blocklist_in_db(&mut db, create2_target);
        let mut evm = arc_evm(db);
        load_native_coin_control(&mut evm);

        let mut create = create_frame(CreateScheme::Create, SOURCE, U256::ONE, Bytes::new());
        let result = evm.before_frame_init(&mut create).unwrap();
        let BeforeFrameInit::Revert(FrameResult::Create(outcome)) = result else {
            panic!("expected nested CREATE revert");
        };
        assert_eq!(outcome.result.output, revert_message(ERR_BLOCKED_ADDRESS));
        assert!(outcome.address.is_none());

        let mut create2 =
            create_frame(CreateScheme::Create2 { salt }, SOURCE, U256::ONE, init_code);
        let result = evm.before_frame_init(&mut create2).unwrap();
        assert!(matches!(
            result,
            BeforeFrameInit::Revert(FrameResult::Create(_))
        ));
        let FrameInput::Create(inputs) = &create2.frame_input else {
            unreachable!();
        };
        assert_eq!(
            inputs.scheme(),
            CreateScheme::Custom {
                address: create2_target
            }
        );
    }

    #[test]
    fn nested_transfer_rejects_a_selfdestructed_target_and_skips_self_logs() {
        let mut evm = arc_evm(InMemoryDB::default());
        load_native_coin_control(&mut evm);
        evm.ctx_mut().journal_mut().load_account(TARGET).unwrap();
        evm.ctx_mut()
            .journaled_state
            .state
            .get_mut(&TARGET)
            .unwrap()
            .mark_selfdestruct();

        let mut destroyed_target = call_frame(CallScheme::Call, SOURCE, TARGET, U256::ONE);
        let result = evm.before_frame_init(&mut destroyed_target).unwrap();
        assert_call_revert(result, ERR_SELFDESTRUCTED_BALANCE_INCREASED);

        let mut self_transfer = call_frame(CallScheme::Call, SOURCE, SOURCE, U256::ONE);
        assert!(matches!(
            evm.before_frame_init(&mut self_transfer).unwrap(),
            BeforeFrameInit::None
        ));
    }

    #[test]
    fn value_call_emits_exactly_one_manual_eip7708_log_on_osaka() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        let mut evm = arc_evm(db);
        let tx = TxEnv {
            caller: SOURCE,
            kind: TxKind::Call(TARGET),
            value: U256::from(100),
            gas_limit: 21_000,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        };

        let result = evm.transact(tx).unwrap().result;
        assert!(result.is_success());
        assert_eq!(
            result.logs(),
            &[eip7708_transfer_log(SOURCE, TARGET, U256::from(100))]
        );
    }

    #[test]
    fn precompile_transfer_log_precedes_custom_log_and_both_revert_together() {
        fn evm_with_precompile(revert: bool) -> ArcEvm<InMemoryDB, NoOpInspector> {
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                SOURCE,
                AccountInfo {
                    balance: U256::from(10_000),
                    ..Default::default()
                },
            );
            let mut evm = arc_evm(db);
            evm.inner
                .precompiles
                .apply_precompile(&MOCK_PRECOMPILE, |_| {
                    Some(DynPrecompile::new_stateful(
                        PrecompileId::Custom("ARC_A4_LOG_FIXTURE".into()),
                        move |mut input| {
                            input.internals.log(Log {
                                address: MOCK_LOG_ADDRESS,
                                data: LogData::new_unchecked(Vec::new(), Bytes::new()),
                            });
                            Ok(if revert {
                                PrecompileOutput::new_reverted(0, Bytes::new())
                            } else {
                                PrecompileOutput::new(0, Bytes::new())
                            })
                        },
                    ))
                });
            load_native_coin_control(&mut evm);
            evm.ctx_mut().journal_mut().load_account(SOURCE).unwrap();
            evm.ctx_mut()
                .journal_mut()
                .load_account(MOCK_PRECOMPILE)
                .unwrap();
            evm
        }

        let mut success = evm_with_precompile(false);
        {
            let result = success
                .frame_init(call_frame(
                    CallScheme::Call,
                    SOURCE,
                    MOCK_PRECOMPILE,
                    U256::from(100),
                ))
                .unwrap();
            assert!(matches!(result, ItemOrResult::Result(_)));
        }
        let logs = &success.ctx().journaled_state.logs;
        assert_eq!(logs.len(), 2);
        assert_eq!(
            logs[0],
            eip7708_transfer_log(SOURCE, MOCK_PRECOMPILE, U256::from(100))
        );
        assert_eq!(logs[1].address, MOCK_LOG_ADDRESS);

        let mut reverted = evm_with_precompile(true);
        {
            let result = reverted
                .frame_init(call_frame(
                    CallScheme::Call,
                    SOURCE,
                    MOCK_PRECOMPILE,
                    U256::from(100),
                ))
                .unwrap();
            let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
                panic!("precompile should finish synchronously");
            };
            assert_eq!(
                *outcome.instruction_result(),
                revm::interpreter::InstructionResult::Revert
            );
        }
        assert!(reverted.ctx().journaled_state.logs.is_empty());
    }

    #[test]
    fn reverted_nested_call_and_create_remove_manual_transfer_logs() {
        let sender = Address::with_last_byte(0x11);
        let caller = Address::with_last_byte(0x22);
        let reverting = Address::with_last_byte(0x33);
        let revert_code = Bytes::from_static(&[opcode::PUSH1, 0, opcode::PUSH1, 0, opcode::REVERT]);
        let mut call_db = InMemoryDB::default();
        call_db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_contract(
            &mut call_db,
            caller,
            U256::from(1_000),
            call_with_value_bytecode(reverting, U256::from(100), false),
        );
        insert_contract(&mut call_db, reverting, U256::ZERO, revert_code.clone());

        let call_result = arc_evm(call_db)
            .transact(call_tx(sender, caller, 100_000))
            .unwrap()
            .result;
        assert!(call_result.is_success(), "only the nested CALL reverts");
        assert!(call_result.logs().is_empty());

        let factory = Address::with_last_byte(0x44);
        let mut create_db = InMemoryDB::default();
        create_db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_contract(
            &mut create_db,
            factory,
            U256::from(1_000),
            create_with_value_bytecode(&revert_code, U256::from(100)),
        );

        let create_result = arc_evm(create_db)
            .transact(call_tx(sender, factory, 120_000))
            .unwrap()
            .result;
        assert!(create_result.is_success(), "only the nested CREATE reverts");
        assert!(create_result.logs().is_empty());
    }

    #[test]
    fn parent_revert_rolls_back_selfdestruct_transfer_log() {
        let sender = Address::with_last_byte(0x51);
        let parent = Address::with_last_byte(0x52);
        let child = Address::with_last_byte(0x53);
        let beneficiary = Address::with_last_byte(0x54);
        let mut child_code = vec![opcode::PUSH20];
        child_code.extend_from_slice(beneficiary.as_slice());
        child_code.push(opcode::SELFDESTRUCT);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        let parent_code = call_with_value_bytecode(child, U256::ZERO, true);
        let parent_code_hash = keccak256(&parent_code);
        let child_code: Bytes = child_code.into();
        let child_code_hash = keccak256(&child_code);
        insert_contract(&mut db, parent, U256::ZERO, parent_code);
        insert_contract(&mut db, child, U256::from(42), child_code);

        let outcome = arc_evm(db)
            .transact(call_tx(sender, parent, 150_000))
            .unwrap();

        assert!(!outcome.result.is_success());
        assert!(outcome.result.logs().is_empty());
        let parent_state = outcome.state.get(&parent).unwrap();
        assert_eq!(parent_state.info.balance, U256::ZERO);
        assert_eq!(parent_state.info.code_hash, parent_code_hash);
        assert!(!parent_state.is_selfdestructed());
        let child_state = outcome.state.get(&child).unwrap();
        assert_eq!(child_state.info.balance, U256::from(42));
        assert_eq!(child_state.info.code_hash, child_code_hash);
        assert!(!child_state.is_selfdestructed());
        assert_eq!(
            outcome
                .state
                .get(&beneficiary)
                .map(|account| account.info.balance)
                .unwrap_or_default(),
            U256::ZERO
        );
        assert!(!outcome
            .state
            .get(&beneficiary)
            .is_some_and(|account| account.is_selfdestructed()));
    }

    #[test]
    fn selfdestruct_dynamic_topup_oog_reverts_state_and_log() {
        let sender = Address::with_last_byte(0x61);
        let child = Address::with_last_byte(0x62);
        let target = Address::with_last_byte(0x63);
        let mut child_code = vec![opcode::PUSH20];
        child_code.extend_from_slice(target.as_slice());
        child_code.push(opcode::SELFDESTRUCT);
        let child_code: Bytes = child_code.into();
        let child_code_hash = keccak256(&child_code);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_contract(&mut db, child, U256::from(42), child_code);

        // 30,000 execution gas covers PUSH20 + SELFDESTRUCT's static and cold-load costs,
        // so the host transfer/log runs, but it cannot cover the 25,000 new-account topup.
        let outcome = arc_evm(db)
            .transact(call_tx(sender, child, 21_000 + 30_000))
            .unwrap();

        assert!(matches!(
            &outcome.result,
            revm::context::result::ExecutionResult::Halt {
                reason: revm::context::result::HaltReason::OutOfGas(_),
                ..
            }
        ));
        assert!(outcome.result.logs().is_empty());
        let child_state = outcome.state.get(&child).unwrap();
        assert_eq!(child_state.info.balance, U256::from(42));
        assert_eq!(child_state.info.code_hash, child_code_hash);
        assert!(!child_state.is_selfdestructed());
        assert_eq!(
            outcome
                .state
                .get(&target)
                .map(|account| account.info.balance)
                .unwrap_or_default(),
            U256::ZERO
        );
        assert!(!outcome
            .state
            .get(&target)
            .is_some_and(|account| account.is_selfdestructed()));
    }

    #[test]
    fn zero7_callfrom_preserves_sender_and_is_transparent_to_inspector() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_contract(
            &mut db,
            MEMO_ADDRESS,
            U256::ZERO,
            forwarding_call_code(CALL_FROM_ADDRESS),
        );
        insert_contract(&mut db, TARGET, U256::ZERO, return_caller_code());

        let child_data = Bytes::from_static(b"arc-call-from-child");
        let input = call_from_input(SOURCE, TARGET, child_data.clone());
        let tx = TxEnv {
            caller: SOURCE,
            kind: TxKind::Call(MEMO_ADDRESS),
            gas_limit: 300_000,
            data: input.clone(),
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        };
        let mut evm = post_zero7_evm(db, CallRecorder::default());
        evm.set_subcall_trace_completion_hook(record_subcall_completion);
        let outcome = evm.inspect_tx(tx).expect("CallFrom transaction executes");

        assert!(outcome.result.is_success());
        let output = outcome.result.output().expect("successful call has output");
        let decoded = ITestCallFrom::callFromCall::abi_decode_returns(output)
            .expect("wrapper returns valid CallFrom output");
        assert!(decoded.success);
        let mut expected_caller = [0_u8; 32];
        expected_caller[12..].copy_from_slice(SOURCE.as_slice());
        assert_eq!(decoded.returnData.as_ref(), expected_caller);

        assert_eq!(evm.inner.inspector.calls.len(), 2);
        let root = &evm.inner.inspector.calls[0];
        assert_eq!(root.caller, SOURCE);
        assert_eq!(root.target_address, MEMO_ADDRESS);
        assert_eq!(root.input.bytes(&evm.inner.ctx), input);

        let child = &evm.inner.inspector.calls[1];
        assert_eq!(child.caller, SOURCE);
        assert_eq!(child.target_address, TARGET);
        assert_eq!(child.input.bytes(&evm.inner.ctx), child_data);
        assert!(evm.inner.inspector.calls.iter().all(|call| {
            call.caller != CALL_FROM_ADDRESS && call.target_address != CALL_FROM_ADDRESS
        }));
        assert_eq!(evm.inner.inspector.subcall_completions.len(), 1);
        let completion = &evm.inner.inspector.subcall_completions[0];
        assert_eq!(completion.child_status, InstructionResult::Return);
        assert_eq!(completion.child_output.as_ref(), expected_caller);
        assert!(completion.child_gas_used > 0);
        assert!(completion.child_gas_limit > completion.child_gas_used);
        assert_eq!(completion.final_status, InstructionResult::Return);
        assert_eq!(
            completion.phase,
            ArcSubcallTraceCompletionPhase::AfterFrameEnd
        );
        assert!(evm.subcall_continuations.is_empty());
    }

    #[test]
    fn zero7_subcall_completion_reports_exact_child_gas_limit() {
        const PARENT_GAS_LIMIT: u64 = 100_000;
        let delegate = Address::repeat_byte(0x82);
        let child_data = Bytes::from_static(b"arc-call-from-child");

        for delegated in [false, true] {
            let mut db = InMemoryDB::default();
            if delegated {
                let delegation = Bytecode::new_eip7702(delegate);
                db.insert_account_info(
                    TARGET,
                    AccountInfo {
                        nonce: 1,
                        code_hash: keccak256(delegation.bytes_slice()),
                        code: Some(delegation),
                        ..Default::default()
                    },
                );
                insert_contract(&mut db, delegate, U256::ZERO, Bytes::new());
            } else {
                db.insert_account_info(TARGET, AccountInfo::default());
            }

            let mut evm = post_zero7_evm(db, CallRecorder::default());
            evm.inner.ctx.tx = call_tx(SOURCE, MEMO_ADDRESS, 1_000_000);
            evm.set_subcall_trace_completion_hook(record_subcall_completion);
            let frame = call_from_frame(
                MEMO_ADDRESS,
                SOURCE,
                TARGET,
                child_data.clone(),
                PARENT_GAS_LIMIT,
            );
            let ItemOrResult::Result(FrameResult::Call(_)) = evm
                .inspect_frame_init(frame)
                .expect("empty CallFrom child completes immediately")
            else {
                panic!("empty CallFrom child must return a CALL outcome")
            };

            let access_cost =
                revm::interpreter::gas::COLD_ACCOUNT_ACCESS_COST * if delegated { 2 } else { 1 };
            let remaining = PARENT_GAS_LIMIT - abi_decode_gas(child_data.len()) - access_cost;
            let expected_child_gas_limit = remaining - remaining / 64;
            assert_eq!(evm.inner.inspector.subcall_completions.len(), 1);
            assert_eq!(
                evm.inner.inspector.subcall_completions[0].child_gas_limit,
                expected_child_gas_limit,
                "target and optional EIP-7702 delegate access must be charged before EIP-150"
            );
        }
    }

    #[test]
    fn zero7_callfrom_executes_eip7702_delegate_code() {
        let delegated = Address::repeat_byte(0x81);
        let delegate = Address::repeat_byte(0x82);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_contract(
            &mut db,
            MEMO_ADDRESS,
            U256::ZERO,
            forwarding_call_code(CALL_FROM_ADDRESS),
        );
        let delegation = Bytecode::new_eip7702(delegate);
        db.insert_account_info(
            delegated,
            AccountInfo {
                nonce: 1,
                code_hash: keccak256(delegation.bytes_slice()),
                code: Some(delegation),
                ..Default::default()
            },
        );
        insert_contract(&mut db, delegate, U256::ZERO, return_caller_code());

        let input = call_from_input(SOURCE, delegated, Bytes::from_static(b"delegated-call"));
        let tx = TxEnv {
            caller: SOURCE,
            kind: TxKind::Call(MEMO_ADDRESS),
            gas_limit: 300_000,
            data: input,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        };
        let mut evm = post_zero7_evm(db, CallRecorder::default());
        let outcome = evm.inspect_tx(tx).expect("delegated CallFrom executes");

        assert!(outcome.result.is_success());
        let decoded = ITestCallFrom::callFromCall::abi_decode_returns(
            outcome.result.output().expect("successful call has output"),
        )
        .expect("valid CallFrom output");
        assert!(decoded.success);
        let mut expected_caller = [0_u8; 32];
        expected_caller[12..].copy_from_slice(SOURCE.as_slice());
        assert_eq!(decoded.returnData.as_ref(), expected_caller);
        assert_eq!(evm.inner.inspector.calls.len(), 2);
        assert_eq!(evm.inner.inspector.calls[1].target_address, delegated);
    }

    #[test]
    fn zero7_subcall_completion_failure_rolls_back_child_and_reports_folded_status() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_contract(
            &mut db,
            MEMO_ADDRESS,
            U256::ZERO,
            forwarding_call_code(CALL_FROM_ADDRESS),
        );
        insert_contract(&mut db, TARGET, U256::ZERO, counter_code());

        let tx = TxEnv {
            caller: SOURCE,
            kind: TxKind::Call(MEMO_ADDRESS),
            gas_limit: 300_000,
            data: call_from_input(SOURCE, TARGET, Bytes::new()),
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        };
        let mut evm = post_zero7_evm(db, CallRecorder::default());
        let mut registry = SubcallRegistry::new();
        registry.register(
            CALL_FROM_ADDRESS,
            Arc::new(RejectingCompletionPrecompile),
            subcall::AllowedCallers::Only(std::collections::HashSet::from([MEMO_ADDRESS])),
        );
        evm.subcall_registry = registry;
        evm.set_subcall_trace_completion_hook(record_subcall_completion);

        let outcome = evm
            .inspect_tx(tx)
            .expect("wrapper catches completion failure");
        assert!(outcome.result.is_success());
        let stored = outcome
            .state
            .get(&TARGET)
            .and_then(|account| account.storage.get(&U256::ZERO))
            .map(|slot| slot.present_value)
            .unwrap_or_default();
        assert_eq!(stored, U256::ZERO);

        assert_eq!(evm.inner.inspector.subcall_completions.len(), 1);
        let completion = &evm.inner.inspector.subcall_completions[0];
        assert_eq!(completion.child_status, InstructionResult::Return);
        assert_eq!(completion.final_status, InstructionResult::Revert);
        assert_eq!(
            completion.phase,
            ArcSubcallTraceCompletionPhase::AfterFrameEnd
        );
    }

    #[test]
    fn zero7_callfrom_rejects_unauthorized_caller_with_dispatch_cost() {
        let mut evm = post_zero7_evm(InMemoryDB::default(), NoOpInspector {});
        let frame = call_from_frame(SOURCE, SOURCE, TARGET, Bytes::new(), 100_000);
        let ItemOrResult::Result(FrameResult::Call(outcome)) = evm
            .frame_init(frame)
            .expect("unauthorized CallFrom is an EVM revert")
        else {
            panic!("unauthorized CallFrom must finish immediately")
        };
        assert_eq!(outcome.result.output, revert_message("unauthorized caller"));
        assert_eq!(outcome.result.gas.spent(), SUBCALL_DISPATCH_COST);
    }

    #[test]
    fn zero7_callfrom_enforces_dispatch_rules_and_exact_overhead() {
        let mut wrong_scheme = call_from_frame(MEMO_ADDRESS, SOURCE, TARGET, Bytes::new(), 100_000);
        let FrameInput::Call(inputs) = &mut wrong_scheme.frame_input else {
            unreachable!()
        };
        inputs.scheme = CallScheme::DelegateCall;
        let outcome = run_call_from_frame(wrong_scheme, SOURCE);
        assert_eq!(
            outcome.result.output,
            revert_message("subcall precompiles only support CALL scheme")
        );
        assert_eq!(outcome.result.gas.spent(), SUBCALL_DISPATCH_COST);

        let mut static_call = call_from_frame(MEMO_ADDRESS, SOURCE, TARGET, Bytes::new(), 100_000);
        let FrameInput::Call(inputs) = &mut static_call.frame_input else {
            unreachable!()
        };
        inputs.is_static = true;
        let outcome = run_call_from_frame(static_call, SOURCE);
        assert_eq!(
            outcome.result.output,
            revert_message("subcall precompiles cannot be invoked in static context")
        );
        assert_eq!(outcome.result.gas.spent(), 100_000);

        let mut with_value = call_from_frame(MEMO_ADDRESS, SOURCE, TARGET, Bytes::new(), 100_000);
        let FrameInput::Call(inputs) = &mut with_value.frame_input else {
            unreachable!()
        };
        inputs.value = CallValue::Transfer(U256::ONE);
        let outcome = run_call_from_frame(with_value, SOURCE);
        assert_eq!(
            outcome.result.output,
            revert_message("subcall precompiles do not support value transfers")
        );
        assert_eq!(outcome.result.gas.spent(), SUBCALL_DISPATCH_COST);

        let spoofed = call_from_frame(
            MEMO_ADDRESS,
            Address::repeat_byte(0x99),
            TARGET,
            Bytes::new(),
            100_000,
        );
        let outcome = run_call_from_frame(spoofed, SOURCE);
        assert_eq!(
            outcome.result.output,
            revert_message("sender spoofing requires tx.origin as sender")
        );
        assert_eq!(outcome.result.gas.spent(), SUBCALL_DISPATCH_COST);

        let valid = call_from_frame(MEMO_ADDRESS, SOURCE, TARGET, Bytes::new(), 100_000);
        let outcome = run_call_from_frame(valid, SOURCE);
        assert!(outcome.result.result.is_ok());
        assert_eq!(outcome.result.gas.spent(), 2_800);
        let decoded = ITestCallFrom::callFromCall::abi_decode_returns(&outcome.result.output)
            .expect("CallFrom returns valid ABI output");
        assert!(decoded.success);
        assert!(decoded.returnData.is_empty());

        let direct_callfrom_target = call_from_frame(
            MEMO_ADDRESS,
            SOURCE,
            CALL_FROM_ADDRESS,
            Bytes::new(),
            100_000,
        );
        let outcome = run_call_from_frame(direct_callfrom_target, SOURCE);
        let decoded = ITestCallFrom::callFromCall::abi_decode_returns(&outcome.result.output)
            .expect("direct child CallFrom target is not recursively intercepted");
        assert!(decoded.success);
        assert!(decoded.returnData.is_empty());
    }

    #[test]
    fn zero7_value_transfer_probe_does_not_warm_target() {
        let mut evm = post_zero7_evm(InMemoryDB::default(), NoOpInspector {});
        load_native_coin_control(&mut evm);

        let mut frame = call_frame(CallScheme::Call, SOURCE, TARGET, U256::ONE);
        assert!(matches!(
            evm.before_frame_init(&mut frame).unwrap(),
            BeforeFrameInit::Log(_)
        ));
        assert!(
            evm.ctx_mut()
                .journal_mut()
                .load_account(TARGET)
                .unwrap()
                .is_cold,
            "Zero7 selfdestruct probe must not warm the transfer target"
        );
    }

    #[test]
    fn normal_and_inspected_execution_share_the_arc_wrapper() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        let tx = TxEnv {
            caller: SOURCE,
            kind: TxKind::Call(TARGET),
            value: U256::ONE,
            gas_limit: 21_000,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        };
        let mut normal = arc_evm(db.clone());
        let normal_result = normal.transact(tx.clone()).unwrap();

        let mut inspected = arc_evm(db);
        let inspected_result = inspected.inspect_tx(tx).unwrap();

        assert_eq!(normal_result, inspected_result);
        assert_eq!(normal_result.result.logs().len(), 1);
        assert_eq!(
            normal.execution_spec().arc_flags,
            ArcHardforkFlags::from_schedule(ArcChainConfig::mainnet().hardforks(), 1, 1)
        );
    }
}
