use super::{
    frame_result::revert_frame,
    native::{
        blocklist_storage_slot, eip7708_transfer_log, is_blocklisted_status, ERR_BLOCKED_ADDRESS,
        ERR_SELFDESTRUCTED_BALANCE_INCREASED, ERR_ZERO_ADDRESS, NATIVE_COIN_CONTROL_ADDRESS,
    },
    opcode::arc_selfdestruct,
    precompile::extend_arc_precompiles,
    ArcChainConfig, ArcExecutionSpec, ArcHardfork,
};
use alloy::primitives::{Address, Log};
use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId, U256};
use revm::{
    bytecode::opcode::SELFDESTRUCT,
    context::{ContextTr, Evm as RevmEvm, FrameStack, JournalTr, Transaction, TxEnv},
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
        CallOutcome, CallScheme, CreateScheme, Instruction,
    },
    precompile::{PrecompileSpecId, Precompiles},
    Context, Inspector, Journal,
};
use std::{
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
};

mod exec;

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
/// mainnet executor. A4 and A5 extend this wrapper with Arc handler, frame,
/// instruction, and precompile behavior.
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
            Instruction::new(arc_selfdestruct::<DB>, 5_000),
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
        if flags.is_active(ArcHardfork::Zero5)
            && self
                .inner
                .ctx
                .journal_mut()
                .load_account(to)?
                .is_selfdestructed()
        {
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
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
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
        self.inner.frame_return_result(result)
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
        let (ctx, inspector) = self.ctx_inspector();
        if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            frame_end(ctx, inspector, &frame_init.frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        let frame_input = frame_init.frame_input.clone();
        let logs_i = ctx.journal().logs().len();
        if let ItemOrResult::Result(mut output) = self.frame_init(frame_init)? {
            let (ctx, inspector) = self.ctx_inspector();
            // Arc can emit EIP-7708 while initializing any successful value
            // frame. REVM's default inspector only forwards journal logs for
            // immediate precompile frames, so forward every log added during
            // Arc frame init before preserving its precompile-only log list.
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
            frame_end(ctx, inspector, &frame_input, &mut output);
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
    use crate::arc::{native::revert_message, ArcHardfork, ArcHardforkFlags, ARC_MAINNET_CHAIN_ID};
    use alloy::primitives::{address, keccak256, Address, Bytes, LogData, B256};
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

    fn evm_env() -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;
        let block = BlockEnv {
            number: U256::from(1),
            timestamp: U256::from(1),
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

    fn arc_evm(db: InMemoryDB) -> ArcEvm<InMemoryDB, NoOpInspector> {
        ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(evm_env(), db, NoOpInspector {})
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
