use super::{
    consensus::CeloChainData,
    precompile::{Failure, L1Precompiles},
    CeloL1Config, L1Fork,
};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::context::{ContextSetters, ContextTr, Evm, FrameStack, TxEnv};
use revm::context_interface::{
    cfg::gas_params::{GasId, GasParams},
    result::{EVMError, ExecutionResult, HaltReason, ResultAndState},
    JournalTr,
};
use revm::handler::{
    evm::{ContextDbError, FrameInitResult},
    instructions::EthInstructions,
    EthFrame, EvmTr, FrameInitOrResult, FrameResult, Handler, ItemOrResult, MainBuilder,
    MainContext, MainnetHandler,
};
use revm::inspector::{InspectorEvmTr, InspectorHandler};
use revm::interpreter::{
    interpreter::EthInterpreter, interpreter_action::FrameInit, CallInput, CallInputs, CallScheme,
    CallValue, FrameInput, Instruction, InstructionContext, InstructionExecResult,
    InstructionResult, SharedMemory,
};
use revm::primitives::{Address, AddressSet, Bytes, HashSet, U256};
use revm::state::EvmState;
use revm::{
    Context, Database, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm, InspectEvm,
    Inspector, Journal,
};
use std::sync::Arc;

pub type CeloL1Context<DB> = Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, DB, Journal<DB>>;
pub struct CeloL1Evm<'a, CTX, I> {
    pub inner: Evm<CTX, I, EthInstructions<EthInterpreter, CTX>, L1Precompiles<'a>, EthFrame>,
}

macro_rules! evm_impl {
    ($context:ty) => {
        impl<'a, 'b, DB: Database, I> EvmTr for CeloL1Evm<'a, $context, I> {
            type Context = $context;
            type Instructions = EthInstructions<EthInterpreter, Self::Context>;
            type Precompiles = L1Precompiles<'a>;
            type Frame = EthFrame;
            fn all(
                &self,
            ) -> (
                &Self::Context,
                &Self::Instructions,
                &Self::Precompiles,
                &FrameStack<EthFrame>,
            ) {
                self.inner.all()
            }
            fn all_mut(
                &mut self,
            ) -> (
                &mut Self::Context,
                &mut Self::Instructions,
                &mut Self::Precompiles,
                &mut FrameStack<EthFrame>,
            ) {
                self.inner.all_mut()
            }
            fn frame_init(
                &mut self,
                init: FrameInit,
            ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<Self::Context>> {
                let first = self.inner.frame_stack.index().is_none();
                let slot = if first {
                    self.inner.frame_stack.start_init()
                } else {
                    self.inner.frame_stack.get_next()
                };
                let result = EthFrame::init_with_context(
                    slot,
                    &mut self.inner.ctx,
                    &mut self.inner.precompiles,
                    init,
                )?;
                match result {
                    ItemOrResult::Result(result) => Ok(ItemOrResult::Result(result)),
                    ItemOrResult::Item(token) => {
                        if first {
                            unsafe { self.inner.frame_stack.end_init(token) };
                        } else {
                            unsafe { self.inner.frame_stack.push(token) };
                        }
                        let frame = self.inner.frame_stack.get();
                        if let FrameInput::Create(inputs) = &frame.input {
                            let to = match frame.data {
                                revm::handler::FrameData::Create(ref data) => data.created_address,
                                _ => unreachable!(),
                            };
                            self.inner
                                .precompiles
                                .retax_transfer(
                                    &mut self.inner.ctx,
                                    inputs.caller(),
                                    to,
                                    inputs.value(),
                                    false,
                                )
                                .map_err(revm::context_interface::context::ContextError::Custom)?;
                        }
                        Ok(ItemOrResult::Item(self.inner.frame_stack.get()))
                    }
                }
            }
            fn frame_run(
                &mut self,
            ) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<Self::Context>> {
                self.inner.frame_run()
            }
            fn frame_return_result(
                &mut self,
                result: FrameResult,
            ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
                self.inner.frame_return_result(result)
            }
        }
    };
}
evm_impl!(CeloL1Context<DB>);
evm_impl!(&'b mut CeloL1Context<DB>);

impl<DB: Database, I> ExecuteEvm for CeloL1Evm<'_, CeloL1Context<DB>, I> {
    type ExecutionResult = ExecutionResult<HaltReason>;
    type State = EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = TxEnv;
    type Block = BlockEnv;
    fn set_block(&mut self, block: BlockEnv) {
        self.inner.ctx.set_block(block);
    }
    fn transact_one(&mut self, tx: TxEnv) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MainnetHandler::<_, Self::Error, EthFrame>::default().run(self)
    }
    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }
    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        let result = MainnetHandler::<_, Self::Error, EthFrame>::default().run(self)?;
        Ok(ResultAndState::new(result, self.finalize()))
    }
}
impl<DB: Database + DatabaseCommit, I> ExecuteCommitEvm for CeloL1Evm<'_, CeloL1Context<DB>, I> {
    fn commit(&mut self, state: EvmState) {
        self.inner.ctx.db_mut().commit(state);
    }
}
impl<DB: Database, I: Inspector<CeloL1Context<DB>>> InspectEvm
    for CeloL1Evm<'_, CeloL1Context<DB>, I>
{
    type Inspector = I;
    fn set_inspector(&mut self, inspector: I) {
        self.inner.inspector = inspector;
    }
    fn inspect_one_tx(&mut self, tx: TxEnv) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MainnetHandler::<_, Self::Error, EthFrame>::default().inspect_run(self)
    }
}
impl<DB: Database + DatabaseCommit, I: Inspector<CeloL1Context<DB>>> InspectCommitEvm
    for CeloL1Evm<'_, CeloL1Context<DB>, I>
{
}
impl<DB: Database, I: Inspector<CeloL1Context<DB>>> InspectorEvmTr
    for CeloL1Evm<'_, CeloL1Context<DB>, I>
{
    type Inspector = I;
    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<EthFrame>,
        &I,
    ) {
        self.inner.all_inspector()
    }
    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<EthFrame>,
        &mut I,
    ) {
        self.inner.all_mut_inspector()
    }
}

fn invalid<H: revm::interpreter::Host>(
    _: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    Err(InstructionResult::NotActivated)
}

fn instructions<H: revm::interpreter::Host>(fork: L1Fork) -> EthInstructions<EthInterpreter, H> {
    let mut table = EthInstructions::new_mainnet_with_spec(fork.spec());
    // Celo L1 never enabled the PoW DIFFICULTY opcode (0x44).
    table.insert_instruction(0x44, Instruction::new(invalid::<H>), 0);
    if fork < L1Fork::Gingerbread {
        table.insert_instruction(
            revm::bytecode::opcode::BASEFEE,
            Instruction::new(invalid::<H>),
            0,
        );
        table.insert_instruction(
            revm::bytecode::opcode::GASLIMIT,
            Instruction::new(invalid::<H>),
            0,
        );
    }
    if fork < L1Fork::Espresso {
        table.insert_gas(revm::bytecode::opcode::BALANCE, 400);
        table.insert_gas(revm::bytecode::opcode::EXTCODEHASH, 400);
        table.insert_gas(revm::bytecode::opcode::SLOAD, 200);
    }
    table
}

fn system_sload<H: revm::interpreter::Host>(
    context: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    context
        .host
        .load_account_delegated(context.interpreter.input.target_address)
        .ok_or(InstructionResult::FatalExternalError)?;
    revm::interpreter::instructions::host::sload(context)
}

fn system_sstore<H: revm::interpreter::Host>(
    context: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    context
        .host
        .load_account_delegated(context.interpreter.input.target_address)
        .ok_or(InstructionResult::FatalExternalError)?;
    revm::interpreter::instructions::host::sstore(context)
}

pub fn create_celo_l1_evm<'a, DB: Database, I>(
    block: BlockEnv,
    cfg: CfgEnv<MainnetSpecId>,
    db: DB,
    inspector: I,
    chain: &'a dyn CeloChainData,
) -> Result<CeloL1Evm<'a, CeloL1Context<DB>, I>, String> {
    let mut cfg = CeloL1Config::cfg_at(cfg, &block)?;
    let fork = L1Fork::at(block.number.to::<u64>())?;
    if fork == L1Fork::Espresso {
        cfg.gas_params.override_gas([
            (GasId::cold_account_additional_cost(), 800),
            (GasId::cold_storage_additional_cost(), 700),
            (GasId::cold_storage_cost(), 800),
            (GasId::sstore_reset_without_cold_load_cost(), 4100),
            (GasId::sstore_reset_refund(), 4100),
            (GasId::sstore_clearing_slot_refund(), 6100),
        ]);
    }
    let mut evm = Context::mainnet()
        .with_block(block)
        .with_cfg(cfg)
        .with_db(db)
        .build_mainnet_with_inspector(inspector)
        .with_precompiles(L1Precompiles::new(fork, chain));
    evm.instruction = instructions(fork);
    Ok(CeloL1Evm { inner: evm })
}

/// Execute an internal Celo system call against the same journal, without a
/// transaction prelude or fee settlement. Registry queries are unmetered and
/// do not alter the caller's access list. Reserve calls retain their changes.
pub(super) fn system_call<DB: Database>(
    context: &mut CeloL1Context<DB>,
    fork: L1Fork,
    chain: &dyn CeloChainData,
    caller: Address,
    to: Address,
    input: Bytes,
    gas: u64,
    is_static: bool,
    unmetered: bool,
) -> Result<Bytes, Failure> {
    let depth = context.journaled_state.depth();
    let previous_warmth = unmetered.then(|| {
        let tx_id = context.journaled_state.transaction_id;
        let mut accounts = AddressSet::default();
        let mut slots: HashSet<(Address, U256)> = HashSet::default();
        for (address, account) in &context.journaled_state.state {
            if !account.is_cold_transaction_id(tx_id) {
                accounts.insert(*address);
            }
            for (key, slot) in &account.storage {
                if !slot.is_cold_transaction_id(tx_id) {
                    slots.insert((*address, *key));
                }
            }
        }
        (accounts, slots)
    });
    // revm's frame constructor receives accounts and bytecode already loaded by
    // the CALL instruction. System calls bypass that instruction, so do it here.
    let caller_cold = context
        .journaled_state
        .load_account(caller)
        .map_err(|e| Failure::Node(e.to_string()))?
        .is_cold;
    let account = context
        .journaled_state
        .load_account_with_code(to)
        .map_err(|e| Failure::Node(e.to_string()))?;
    let to_cold = account.is_cold;
    let known_bytecode = (
        account.info.code_hash(),
        account.info.code.clone().unwrap_or_default(),
    );
    // Direct native EVM calls bypass CALL's account-access gas and warming.
    if caller_cold {
        context
            .journaled_state
            .state
            .get_mut(&caller)
            .unwrap()
            .mark_cold();
    }
    if to_cold {
        context
            .journaled_state
            .state
            .get_mut(&to)
            .unwrap()
            .mark_cold();
    }
    let previous_cfg = context.cfg.clone();
    let previous_local = std::mem::take(&mut context.local);
    let mut instruction = instructions::<&mut CeloL1Context<DB>>(fork);
    if !unmetered && fork >= L1Fork::Espresso {
        // Native AddSlotToAccessList also adds the owning account. Direct
        // system calls can enter an otherwise cold account, unlike ordinary
        // CALL opcodes. revm's storage host assumes its owner is already warm.
        instruction.insert_instruction(
            revm::bytecode::opcode::SLOAD,
            Instruction::new(system_sload::<&mut CeloL1Context<DB>>),
            100,
        );
        instruction.insert_instruction(
            revm::bytecode::opcode::SSTORE,
            Instruction::new(system_sstore::<&mut CeloL1Context<DB>>),
            0,
        );
    }
    if unmetered {
        instruction.gas_table_mut().fill(0);
        let mut gas_params = GasParams::new(Arc::new([0; 256]));
        gas_params.override_gas([
            (GasId::memory_quadratic_reduction(), u64::MAX),
            (GasId::call_stipend_reduction(), 64),
            (GasId::max_refund_quotient(), 5),
        ]);
        context.cfg.gas_params = gas_params;
    }
    let inputs = CallInputs {
        input: CallInput::Bytes(input),
        return_memory_offset: 0..0,
        gas_limit: gas,
        reservoir: 0,
        bytecode_address: to,
        known_bytecode,
        target_address: to,
        caller,
        value: CallValue::Transfer(U256::ZERO),
        scheme: if is_static {
            CallScheme::StaticCall
        } else {
            CallScheme::Call
        },
        is_static,
        charged_new_account_state_gas: false,
    };
    let mut evm = CeloL1Evm {
        inner: Evm {
            ctx: &mut *context,
            inspector: (),
            instruction,
            precompiles: L1Precompiles::new(fork, chain),
            frame_stack: FrameStack::new_prealloc(8),
        },
    };
    let result = MainnetHandler::<_, EVMError<DB::Error>, EthFrame>::default().run_exec_loop(
        &mut evm,
        FrameInit {
            depth,
            memory: SharedMemory::new(),
            frame_input: FrameInput::Call(Box::new(inputs)),
        },
    );
    context.cfg = previous_cfg;
    context.local = previous_local;
    if let Some((accounts, slots)) = previous_warmth {
        for (address, account) in &mut context.journaled_state.state {
            if !accounts.contains(address) {
                account.mark_cold();
            }
            for (key, slot) in &mut account.storage {
                if !slots.contains(&(*address, *key)) {
                    slot.mark_cold();
                }
            }
        }
    }
    let result = result
        .map_err(|e| Failure::Node(e.to_string()))?
        .into_interpreter_result();
    if result.result.is_ok() {
        Ok(result.output)
    } else {
        Err(Failure::Invalid)
    }
}
