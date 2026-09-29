//! Instruction table for RSK: the static costs RSK never raised or lowered
//! after EIP-150, and the opcodes whose dynamic gas or result differ from
//! Ethereum. See `rsk/gas.rs` for the schedule.

use crate::rsk::gas::{
    BALANCE, CALL as CALL_GAS, EXT_CODE, EXT_CODE_HASH, SELFDESTRUCT as SELFDESTRUCT_GAS, SLOAD,
    SSTORE_CLEAR_REFUND, SSTORE_RESET, SSTORE_SET,
};
use crate::rsk::precompile::is_rsk_precompile;
use crate::rsk::RskContext;
use revm::bytecode::opcode::{
    BALANCE as OP_BALANCE, CALL, CALLCODE, DELEGATECALL, DIFFICULTY, EXTCODECOPY, EXTCODEHASH,
    EXTCODESIZE, SELFDESTRUCT, SLOAD as OP_SLOAD, SSTORE, STATICCALL,
};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::cfg::gas_params::GasId;
use revm::handler::instructions::EthInstructions;
use revm::interpreter::instructions::contract::get_memory_input_and_out_ranges;
use revm::interpreter::instructions::utility::IntoAddress;
use revm::interpreter::instructions::{contract, host};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::{
    CallInput, CallInputs, CallScheme, CallValue, FrameInput,
};
use revm::interpreter::interpreter_types::{InputsTr, LoopControl, RuntimeFlag, StackTr};
use revm::interpreter::{
    Host, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
    InterpreterAction,
};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address, B256, KECCAK_EMPTY, U256};

type Ctx<'a, DB> = InstructionContext<'a, RskContext<DB>, EthInterpreter>;
type InstructionFn<DB> = fn(Ctx<'_, DB>) -> InstructionExecResult;

pub(crate) fn rsk_instructions<DB: revm::database::Database>(
    spec: SpecId,
) -> EthInstructions<EthInterpreter, RskContext<DB>> {
    let mut instructions = EthInstructions::new_mainnet_with_spec(spec);

    // EIP-150 prices; EIP-1884 and EIP-2929 never made it to RSK.
    let repriced: [(u8, InstructionFn<DB>, u64); 7] = [
        (OP_SLOAD, host::sload, SLOAD),
        (OP_BALANCE, host::balance, BALANCE),
        (EXTCODEHASH, extcodehash, EXT_CODE_HASH),
        (EXTCODESIZE, extcodesize, EXT_CODE),
        (EXTCODECOPY, host::extcodecopy, EXT_CODE),
        (DELEGATECALL, contract::call::<DELEGATECALL, _, _>, CALL_GAS),
        (STATICCALL, contract::call::<STATICCALL, _, _>, CALL_GAS),
    ];
    for (opcode, f, gas) in repriced {
        instructions.insert_instruction(opcode, Instruction::new(f), static_gas(gas));
    }

    instructions.insert_instruction(CALL, Instruction::new(call::<DB>), static_gas(CALL_GAS));
    instructions.insert_instruction(
        CALLCODE,
        Instruction::new(call_code::<DB>),
        static_gas(CALL_GAS),
    );
    instructions.insert_instruction(SSTORE, Instruction::new(sstore::<DB>), 0);
    instructions.insert_instruction(
        SELFDESTRUCT,
        Instruction::new(selfdestruct::<DB>),
        static_gas(SELFDESTRUCT_GAS),
    );
    let difficulty_gas = instructions.gas_table()[DIFFICULTY as usize];
    instructions.insert_instruction(
        DIFFICULTY,
        Instruction::new(difficulty::<DB>),
        difficulty_gas,
    );

    instructions
}

/// Static gas lives in a `u16` gas table since revm 39.
fn static_gas(gas: u64) -> u16 {
    u16::try_from(gas).expect("RSK static gas fits in u16")
}

/// `VM.doSSTORE`: 20 000 from zero to non-zero, 5 000 otherwise, and a 15 000
/// refund from non-zero to zero. "Old value" is the current one — there is no
/// original-value bookkeeping and no EIP-2200 stipend sentry.
fn sstore<DB: revm::database::Database>(context: Ctx<'_, DB>) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    if interpreter.runtime_flag.is_static() {
        return Err(InstructionResult::StateChangeDuringStaticCall);
    }
    let Some([index, value]) = StackTr::popn::<2>(&mut interpreter.stack) else {
        return Err(InstructionResult::StackUnderflow);
    };
    let target = interpreter.input.target_address();
    let load = host
        .sstore(target, index, value)
        .ok_or(InstructionResult::FatalExternalError)?;

    let was_zero = load.data.present_value.is_zero();
    let gas = if was_zero && !value.is_zero() {
        SSTORE_SET
    } else {
        SSTORE_RESET
    };
    if !interpreter.gas.record_regular_cost(gas) {
        return Err(InstructionResult::OutOfGas);
    }
    if !was_zero && value.is_zero() {
        interpreter.gas.record_refund(SSTORE_CLEAR_REFUND);
    }
    Ok(())
}

/// `VM.doSUICIDE`: the 25 000 surcharge applies whenever the beneficiary is not
/// in the state, whether or not any balance moves (EIP-161 made it conditional
/// on the balance).
fn selfdestruct<DB: revm::database::Database>(context: Ctx<'_, DB>) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    if interpreter.runtime_flag.is_static() {
        return Err(InstructionResult::StateChangeDuringStaticCall);
    }
    let Some([target]) = StackTr::popn::<1>(&mut interpreter.stack) else {
        return Err(InstructionResult::StackUnderflow);
    };
    let target = target.into_address();

    let exists = account_exists(host, target).ok_or(InstructionResult::FatalExternalError)?;
    if !exists
        && !interpreter
            .gas
            .record_regular_cost(host.gas_params().new_account_cost_for_selfdestruct())
    {
        return Err(InstructionResult::OutOfGas);
    }

    // `LoadError::ColdLoadSkipped` maps to OutOfGas, `LoadError::DBError` to FatalExternalError.
    let res = host.selfdestruct(interpreter.input.target_address(), target, false)?;
    if !res.previously_destroyed {
        interpreter
            .gas
            .record_refund(host.gas_params().selfdestruct_refund());
    }
    Err(InstructionResult::SelfDestruct)
}

/// `VM.doCODESIZE`: a precompiled or native contract has no code in the state,
/// RSK reports `2^256 - 1` (RSKIP90) so that Solidity's "is there a contract"
/// check before a high level call passes. Without it the call reverts locally
/// instead of reaching the native contract guard and being forwarded.
fn extcodesize<DB: revm::database::Database>(context: Ctx<'_, DB>) -> InstructionExecResult {
    let is_precompile = StackTr::top(&mut context.interpreter.stack)
        .is_some_and(|top| is_rsk_precompile(&top.into_address()));
    if !is_precompile {
        return host::extcodesize(context);
    }
    if let Some(top) = StackTr::top(&mut context.interpreter.stack) {
        *top = U256::MAX;
    }
    Ok(())
}

/// `VM.doEXTCODEHASH`: the empty hash for a precompiled or native contract and
/// for any account in the state without code — zero only when the account is
/// missing (EIP-1052 also answers zero for an empty account).
fn extcodehash<DB: revm::database::Database>(context: Ctx<'_, DB>) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    let Some(top) = StackTr::top(&mut interpreter.stack) else {
        return Err(InstructionResult::StackUnderflow);
    };
    let address = top.into_address();
    let hash = if is_rsk_precompile(&address) {
        Some(KECCAK_EMPTY)
    } else {
        match account_exists(host, address) {
            Some(false) => Some(B256::ZERO),
            Some(true) => host
                .load_account_info_skip_cold_load(address, false, false)
                .ok()
                .map(|account| account.code_hash()),
            None => None,
        }
    };
    *top = hash.ok_or(InstructionResult::FatalExternalError)?.into();
    Ok(())
}

/// RSK still answers `DIFFICULTY` with the block difficulty; revm switches to
/// PREVRANDAO from Paris on.
fn difficulty<DB: revm::database::Database>(context: Ctx<'_, DB>) -> InstructionExecResult {
    let difficulty = context.host.difficulty();
    if !context.interpreter.stack.push(difficulty) {
        return Err(InstructionResult::StackOverflow);
    }
    Ok(())
}

fn call<DB: revm::database::Database>(mut context: Ctx<'_, DB>) -> InstructionExecResult {
    let Some([gas_limit, to, value]) = StackTr::popn::<3>(&mut context.interpreter.stack) else {
        return Err(InstructionResult::StackUnderflow);
    };
    let to = to.into_address();
    if context.interpreter.runtime_flag.is_static() && !value.is_zero() {
        return Err(InstructionResult::CallNotAllowedInsideStatic);
    }
    let target_address = to;
    new_call_frame(&mut context, gas_limit, to, target_address, value, true)
}

fn call_code<DB: revm::database::Database>(mut context: Ctx<'_, DB>) -> InstructionExecResult {
    let Some([gas_limit, to, value]) = StackTr::popn::<3>(&mut context.interpreter.stack) else {
        return Err(InstructionResult::StackUnderflow);
    };
    let to = to.into_address();
    let target_address = context.interpreter.input.target_address();
    new_call_frame(&mut context, gas_limit, to, target_address, value, false)
}

/// `VM.getMessageCall` / `VM.computeCallGas` for `CALL` and `CALLCODE`:
///
/// * `CALL` to an address that is not in the state pays 25 000, with or without
///   value (EIP-161 made it conditional on the value);
/// * a value transfer pays 9 000 and the 2 300 stipend is not free: it is added
///   to the requested gas and the sum is what the caller is charged;
/// * the callee gets `min(remaining, requested + stipend)` — no 63/64 rule.
fn new_call_frame<DB: revm::database::Database>(
    context: &mut Ctx<'_, DB>,
    stack_gas_limit: U256,
    to: Address,
    target_address: Address,
    value: U256,
    is_call: bool,
) -> InstructionExecResult {
    let stack_gas_limit = u64::try_from(stack_gas_limit).unwrap_or(u64::MAX);
    let transfers_value = !value.is_zero();

    let (input, return_memory_offset) =
        get_memory_input_and_out_ranges(context.interpreter, context.host.gas_params())?;

    let mut cost = 0;
    if is_call {
        let exists =
            account_exists(context.host, to).ok_or(InstructionResult::FatalExternalError)?;
        if !exists {
            cost += context.host.gas_params().get(GasId::new_account_cost());
        }
    }
    if transfers_value {
        cost += context.host.gas_params().transfer_value_cost();
    }
    if !context.interpreter.gas.record_regular_cost(cost) {
        return Err(InstructionResult::OutOfGas);
    }

    let stipend = if transfers_value {
        context.host.gas_params().call_stipend()
    } else {
        0
    };
    let remaining = context.interpreter.gas.remaining();
    if remaining < stipend {
        return Err(InstructionResult::OutOfGas);
    }
    let gas_limit = remaining.min(stack_gas_limit.saturating_add(stipend));
    // cannot fail, gas_limit <= remaining
    let _ = context.interpreter.gas.record_regular_cost(gas_limit);

    let (bytecode, bytecode_hash) = match context
        .host
        .load_account_info_skip_cold_load(to, true, false)
    {
        Ok(account) => (
            account.code.clone().unwrap_or_default(),
            account.code_hash(),
        ),
        Err(_) => return Err(InstructionResult::FatalExternalError),
    };

    let caller = context.interpreter.input.target_address();
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::NewFrame(FrameInput::Call(Box::new(
            CallInputs {
                input: CallInput::SharedBuffer(input),
                gas_limit,
                target_address,
                caller,
                bytecode_address: to,
                known_bytecode: (bytecode_hash, bytecode),
                value: CallValue::Transfer(value),
                scheme: if is_call {
                    CallScheme::Call
                } else {
                    CallScheme::CallCode
                },
                is_static: context.interpreter.runtime_flag.is_static(),
                return_memory_offset,
                reservoir: context.interpreter.gas.reservoir(),
                // RSK has no EIP-8037 state gas.
                charged_new_account_state_gas: false,
            },
        ))));
    Err(InstructionResult::Suspend)
}

/// `Repository.isExist`: the account is in the state, empty or not. An account
/// the database does not know becomes existing once the transaction touches it.
fn account_exists<DB: revm::database::Database>(
    host: &mut RskContext<DB>,
    address: Address,
) -> Option<bool> {
    let account = host.journal_mut().load_account(address).ok()?;
    Some(!account.data.is_loaded_as_not_existing_not_touched())
}
