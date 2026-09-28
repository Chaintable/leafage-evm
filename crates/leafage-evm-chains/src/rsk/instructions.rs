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
use revm::context_interface::host::LoadError;
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
    Host, Instruction, InstructionContext, InstructionResult, InterpreterAction,
};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address, B256, KECCAK_EMPTY, U256};

type Ctx<'a, DB> = InstructionContext<'a, RskContext<DB>, EthInterpreter>;
type InstructionFn<DB> = fn(Ctx<'_, DB>);

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
        (DELEGATECALL, contract::delegate_call, CALL_GAS),
        (STATICCALL, contract::static_call, CALL_GAS),
    ];
    for (opcode, f, gas) in repriced {
        instructions.insert_instruction(opcode, Instruction::new(f, gas));
    }

    instructions.insert_instruction(CALL, Instruction::new(call::<DB>, CALL_GAS));
    instructions.insert_instruction(CALLCODE, Instruction::new(call_code::<DB>, CALL_GAS));
    instructions.insert_instruction(SSTORE, Instruction::new(sstore::<DB>, 0));
    instructions.insert_instruction(
        SELFDESTRUCT,
        Instruction::new(selfdestruct::<DB>, SELFDESTRUCT_GAS),
    );
    let difficulty_gas = instructions.instruction_table[DIFFICULTY as usize].static_gas();
    instructions.insert_instruction(
        DIFFICULTY,
        Instruction::new(difficulty::<DB>, difficulty_gas),
    );

    instructions
}

/// `VM.doSSTORE`: 20 000 from zero to non-zero, 5 000 otherwise, and a 15 000
/// refund from non-zero to zero. "Old value" is the current one — there is no
/// original-value bookkeeping and no EIP-2200 stipend sentry.
fn sstore<DB: revm::database::Database>(context: Ctx<'_, DB>) {
    let InstructionContext { interpreter, host } = context;
    if interpreter.runtime_flag.is_static() {
        interpreter.halt(InstructionResult::StateChangeDuringStaticCall);
        return;
    }
    let Some([index, value]) = StackTr::popn::<2>(&mut interpreter.stack) else {
        interpreter.halt_underflow();
        return;
    };
    let target = interpreter.input.target_address();
    let Some(load) = host.sstore(target, index, value) else {
        interpreter.halt_fatal();
        return;
    };

    let was_zero = load.data.present_value.is_zero();
    let gas = if was_zero && !value.is_zero() {
        SSTORE_SET
    } else {
        SSTORE_RESET
    };
    if !interpreter.gas.record_cost(gas) {
        interpreter.halt_oog();
        return;
    }
    if !was_zero && value.is_zero() {
        interpreter.gas.record_refund(SSTORE_CLEAR_REFUND);
    }
}

/// `VM.doSUICIDE`: the 25 000 surcharge applies whenever the beneficiary is not
/// in the state, whether or not any balance moves (EIP-161 made it conditional
/// on the balance).
fn selfdestruct<DB: revm::database::Database>(context: Ctx<'_, DB>) {
    let InstructionContext { interpreter, host } = context;
    if interpreter.runtime_flag.is_static() {
        interpreter.halt(InstructionResult::StateChangeDuringStaticCall);
        return;
    }
    let Some([target]) = StackTr::popn::<1>(&mut interpreter.stack) else {
        interpreter.halt_underflow();
        return;
    };
    let target = target.into_address();

    let Some(exists) = account_exists(host, target) else {
        interpreter.halt_fatal();
        return;
    };
    if !exists
        && !interpreter
            .gas
            .record_cost(host.gas_params().new_account_cost_for_selfdestruct())
    {
        interpreter.halt_oog();
        return;
    }

    let res = match host.selfdestruct(interpreter.input.target_address(), target, false) {
        Ok(res) => res,
        Err(LoadError::ColdLoadSkipped) => return interpreter.halt_oog(),
        Err(LoadError::DBError) => return interpreter.halt_fatal(),
    };
    if !res.previously_destroyed {
        interpreter
            .gas
            .record_refund(host.gas_params().selfdestruct_refund());
    }
    interpreter.halt(InstructionResult::SelfDestruct);
}

/// `VM.doCODESIZE`: a precompiled or native contract has no code in the state,
/// RSK reports `2^256 - 1` (RSKIP90) so that Solidity's "is there a contract"
/// check before a high level call passes. Without it the call reverts locally
/// instead of reaching the native contract guard and being forwarded.
fn extcodesize<DB: revm::database::Database>(context: Ctx<'_, DB>) {
    let is_precompile = StackTr::top(&mut context.interpreter.stack)
        .is_some_and(|top| is_rsk_precompile(&top.into_address()));
    if !is_precompile {
        return host::extcodesize(context);
    }
    if let Some(top) = StackTr::top(&mut context.interpreter.stack) {
        *top = U256::MAX;
    }
}

/// `VM.doEXTCODEHASH`: the empty hash for a precompiled or native contract and
/// for any account in the state without code — zero only when the account is
/// missing (EIP-1052 also answers zero for an empty account).
fn extcodehash<DB: revm::database::Database>(context: Ctx<'_, DB>) {
    let InstructionContext { interpreter, host } = context;
    let Some(top) = StackTr::top(&mut interpreter.stack) else {
        interpreter.halt_underflow();
        return;
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
    match hash {
        Some(hash) => *top = hash.into(),
        None => interpreter.halt_fatal(),
    }
}

/// RSK still answers `DIFFICULTY` with the block difficulty; revm switches to
/// PREVRANDAO from Paris on.
fn difficulty<DB: revm::database::Database>(context: Ctx<'_, DB>) {
    let difficulty = context.host.difficulty();
    if !context.interpreter.stack.push(difficulty) {
        context.interpreter.halt_overflow();
    }
}

fn call<DB: revm::database::Database>(mut context: Ctx<'_, DB>) {
    let Some([gas_limit, to, value]) = StackTr::popn::<3>(&mut context.interpreter.stack) else {
        context.interpreter.halt_underflow();
        return;
    };
    let to = to.into_address();
    if context.interpreter.runtime_flag.is_static() && !value.is_zero() {
        context
            .interpreter
            .halt(InstructionResult::CallNotAllowedInsideStatic);
        return;
    }
    let target_address = to;
    new_call_frame(&mut context, gas_limit, to, target_address, value, true);
}

fn call_code<DB: revm::database::Database>(mut context: Ctx<'_, DB>) {
    let Some([gas_limit, to, value]) = StackTr::popn::<3>(&mut context.interpreter.stack) else {
        context.interpreter.halt_underflow();
        return;
    };
    let to = to.into_address();
    let target_address = context.interpreter.input.target_address();
    new_call_frame(&mut context, gas_limit, to, target_address, value, false);
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
) {
    let stack_gas_limit = u64::try_from(stack_gas_limit).unwrap_or(u64::MAX);
    let transfers_value = !value.is_zero();

    let Some((input, return_memory_offset)) =
        get_memory_input_and_out_ranges(context.interpreter, context.host.gas_params())
    else {
        return;
    };

    let mut cost = 0;
    if is_call {
        let Some(exists) = account_exists(context.host, to) else {
            context.interpreter.halt_fatal();
            return;
        };
        if !exists {
            cost += context.host.gas_params().get(GasId::new_account_cost());
        }
    }
    if transfers_value {
        cost += context.host.gas_params().transfer_value_cost();
    }
    if !context.interpreter.gas.record_cost(cost) {
        context.interpreter.halt_oog();
        return;
    }

    let stipend = if transfers_value {
        context.host.gas_params().call_stipend()
    } else {
        0
    };
    let remaining = context.interpreter.gas.remaining();
    if remaining < stipend {
        context.interpreter.halt_oog();
        return;
    }
    let gas_limit = remaining.min(stack_gas_limit.saturating_add(stipend));
    // cannot fail, gas_limit <= remaining
    let _ = context.interpreter.gas.record_cost(gas_limit);

    let (bytecode, bytecode_hash) = match context
        .host
        .load_account_info_skip_cold_load(to, true, false)
    {
        Ok(account) => (
            account.code.clone().unwrap_or_default(),
            account.code_hash(),
        ),
        Err(_) => {
            context.interpreter.halt_fatal();
            return;
        }
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
                known_bytecode: Some((bytecode_hash, bytecode)),
                value: CallValue::Transfer(value),
                scheme: if is_call {
                    CallScheme::Call
                } else {
                    CallScheme::CallCode
                },
                is_static: context.interpreter.runtime_flag.is_static(),
                return_memory_offset,
            },
        ))));
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
