//! Opcode level deviations from the revm mainnet instruction table.
//!
//! * MIP-3 (`vm/runtime/types.hpp` `Context::expand_memory`): memory expansion
//!   costs `words / 2` (linear, no quadratic term) and the memory of all frames
//!   in the call stack together may not exceed 8 MB. revm computes the
//!   expansion cost from `GasParams::memory_cost`, which is linear+quadratic
//!   and cannot express `words / 2`, so every memory-touching opcode is
//!   wrapped: the wrapper performs the expansion with the MIP-3 rule first and
//!   then runs the stock revm instruction, which then finds the memory already
//!   large enough and charges nothing more.
//! * MIP-8 (`vm/runtime/storage.cpp`): SLOAD / SSTORE cold cost is decided per
//!   page, SSTORE costs `100 + 2800 (first page write) + 17000 (page growth)`.
//! * `vm/runtime/create.cpp`: CREATE / CREATE2 fail inside an EIP-7702
//!   delegated account (`MonadTraits::can_create_inside_delegated == false`).

use crate::monad::api::MonadContext;
use crate::monad::gas::{
    mip3_memory_cost, COLD_STORAGE_ADDITIONAL_COST_V1, MIP3_MEMORY_LIMIT, MIP8_BASE_SSTORE_COST,
    MIP8_PAGE_GROWTH_COST, MIP8_PAGE_WRITE_COST,
};
use crate::monad::page_tracker::{access_page, storage_status, update_page, HostPageStore};
use crate::monad::MonadHardfork;
use revm::bytecode::opcode::{
    CALL, CALLCODE, CALLDATACOPY, CODECOPY, CREATE, CREATE2, DELEGATECALL, EXTCODECOPY, KECCAK256,
    LOG0, LOG1, LOG2, LOG3, LOG4, MCOPY, MLOAD, MSTORE, MSTORE8, RETURN, RETURNDATACOPY, REVERT,
    SLOAD, SSTORE, STATICCALL,
};
use revm::context::ContextTr;
use revm::handler::instructions::EthInstructions;
use revm::interpreter::instructions::{contract, control, host, memory, system};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{InputsTr, MemoryTr, RuntimeFlag, StackTr};
use revm::interpreter::{
    Host, Instruction, InstructionContext, InstructionResult, Interpreter, InterpreterTypes,
};
use revm::primitives::U256;

pub(crate) fn monad_instructions<DB: revm::database::Database>(
    hardfork: MonadHardfork,
) -> EthInstructions<EthInterpreter, MonadContext<DB>> {
    let mut instructions = EthInstructions::new_mainnet_with_spec(hardfork.into());
    install_create_guard(&mut instructions);
    if hardfork.is_mip3_enabled() {
        install_mip3_memory_instructions(&mut instructions);
    }
    if hardfork.is_mip8_enabled() {
        install_mip8_storage_instructions(&mut instructions);
    }
    instructions
}

/// Replace `opcode` keeping the static gas of the stock instruction.
fn replace<DB: revm::database::Database>(
    instructions: &mut EthInstructions<EthInterpreter, MonadContext<DB>>,
    opcode: u8,
    f: fn(InstructionContext<'_, MonadContext<DB>, EthInterpreter>),
) {
    let static_gas = instructions.instruction_table[opcode as usize].static_gas();
    instructions.insert_instruction(opcode, Instruction::new(f, static_gas));
}

fn install_create_guard<DB: revm::database::Database>(
    instructions: &mut EthInstructions<EthInterpreter, MonadContext<DB>>,
) {
    replace(instructions, CREATE, create_guarded::<false, false, _>);
    replace(instructions, CREATE2, create_guarded::<true, false, _>);
}

fn install_mip3_memory_instructions<DB: revm::database::Database>(
    instructions: &mut EthInstructions<EthInterpreter, MonadContext<DB>>,
) {
    replace(instructions, MLOAD, mip3_mload);
    replace(instructions, MSTORE, mip3_mstore);
    replace(instructions, MSTORE8, mip3_mstore8);
    replace(instructions, MCOPY, mip3_mcopy);
    replace(instructions, KECCAK256, mip3_keccak256);
    replace(instructions, CALLDATACOPY, mip3_calldatacopy);
    replace(instructions, CODECOPY, mip3_codecopy);
    replace(instructions, RETURNDATACOPY, mip3_returndatacopy);
    replace(instructions, EXTCODECOPY, mip3_extcodecopy);
    replace(instructions, LOG0, mip3_log::<0, _, _>);
    replace(instructions, LOG1, mip3_log::<1, _, _>);
    replace(instructions, LOG2, mip3_log::<2, _, _>);
    replace(instructions, LOG3, mip3_log::<3, _, _>);
    replace(instructions, LOG4, mip3_log::<4, _, _>);
    replace(instructions, RETURN, mip3_ret);
    replace(instructions, REVERT, mip3_revert);
    replace(instructions, CREATE, create_guarded::<false, true, _>);
    replace(instructions, CREATE2, create_guarded::<true, true, _>);
    replace(instructions, CALL, mip3_call);
    replace(instructions, CALLCODE, mip3_call_code);
    replace(instructions, DELEGATECALL, mip3_delegate_call);
    replace(instructions, STATICCALL, mip3_static_call);
}

fn install_mip8_storage_instructions<DB: revm::database::Database>(
    instructions: &mut EthInstructions<EthInterpreter, MonadContext<DB>>,
) {
    replace(instructions, SLOAD, mip8_sload);
    replace(instructions, SSTORE, mip8_sstore);
}

// ---------------------------------------------------------------------------
// MIP-3 memory
// ---------------------------------------------------------------------------

#[inline]
fn num_words(len: usize) -> usize {
    len.div_ceil(32)
}

#[inline]
fn peek<WIRE: InterpreterTypes>(interpreter: &Interpreter<WIRE>, from_top: usize) -> Option<U256> {
    let data = interpreter.stack.data();
    data.len()
        .checked_sub(from_top + 1)
        .map(|index| data[index])
}

/// `Context::expand_memory` under MIP-3. Returns `false` after halting the
/// interpreter. When it returns `true` the memory already covers
/// `offset + len` and the expansion cost has been charged, so the stock revm
/// instruction that runs afterwards will not charge again.
fn mip3_expand<WIRE: InterpreterTypes>(
    interpreter: &mut Interpreter<WIRE>,
    offset: U256,
    len: U256,
) -> bool {
    if len.is_zero() {
        return true;
    }
    let (Ok(len), Ok(offset)) = (usize::try_from(len), usize::try_from(offset)) else {
        interpreter.halt(InstructionResult::InvalidOperandOOG);
        return false;
    };
    let new_words = num_words(offset.saturating_add(len));
    if new_words <= interpreter.gas.memory().words_num {
        return true;
    }
    let new_size = new_words.saturating_mul(32);
    let total_size = interpreter
        .memory
        .local_memory_offset()
        .saturating_add(new_size);
    if total_size > MIP3_MEMORY_LIMIT {
        interpreter.halt(InstructionResult::MemoryLimitOOG);
        return false;
    }
    let new_cost = mip3_memory_cost(new_words);
    let expansion_cost = interpreter
        .gas
        .memory_mut()
        .set_words_num(new_words, new_cost)
        .unwrap_or(0);
    if !interpreter.gas.record_cost(expansion_cost) {
        interpreter.halt(InstructionResult::MemoryOOG);
        return false;
    }
    interpreter.memory.resize(new_size);
    true
}

/// Expand for the `(offset, len)` pair found at the given stack depths.
fn expand_range<WIRE: InterpreterTypes>(
    interpreter: &mut Interpreter<WIRE>,
    offset_from_top: usize,
    len_from_top: usize,
) -> bool {
    let (Some(offset), Some(len)) = (
        peek(interpreter, offset_from_top),
        peek(interpreter, len_from_top),
    ) else {
        interpreter.halt_underflow();
        return false;
    };
    mip3_expand(interpreter, offset, len)
}

/// Expand for a fixed length at the offset found at the given stack depth.
fn expand_fixed<WIRE: InterpreterTypes>(
    interpreter: &mut Interpreter<WIRE>,
    offset_from_top: usize,
    len: u64,
) -> bool {
    let Some(offset) = peek(interpreter, offset_from_top) else {
        interpreter.halt_underflow();
        return false;
    };
    mip3_expand(interpreter, offset, U256::from(len))
}

macro_rules! mip3_instruction {
    ($name:ident, $inner:expr, |$interpreter:ident| $expand:expr) => {
        fn $name<WIRE: InterpreterTypes, H: Host + ?Sized>(
            context: InstructionContext<'_, H, WIRE>,
        ) {
            {
                let $interpreter: &mut Interpreter<WIRE> = context.interpreter;
                if !$expand {
                    return;
                }
            }
            $inner(context)
        }
    };
}

// stack (top first): [offset]
mip3_instruction!(mip3_mload, memory::mload, |i| expand_fixed(i, 0, 32));
// [offset, value]
mip3_instruction!(mip3_mstore, memory::mstore, |i| expand_fixed(i, 0, 32));
mip3_instruction!(mip3_mstore8, memory::mstore8, |i| expand_fixed(i, 0, 1));
// [offset, len]
mip3_instruction!(mip3_keccak256, system::keccak256, |i| expand_range(i, 0, 1));
mip3_instruction!(mip3_ret, control::ret, |i| expand_range(i, 0, 1));
mip3_instruction!(mip3_revert, control::revert, |i| expand_range(i, 0, 1));
// [mem_offset, data_offset, len]
mip3_instruction!(mip3_calldatacopy, system::calldatacopy, |i| expand_range(
    i, 0, 2
));
mip3_instruction!(mip3_codecopy, system::codecopy, |i| expand_range(i, 0, 2));
mip3_instruction!(mip3_returndatacopy, system::returndatacopy, |i| {
    expand_range(i, 0, 2)
});
// [address, mem_offset, code_offset, len]
mip3_instruction!(mip3_extcodecopy, host::extcodecopy, |i| expand_range(
    i, 1, 3
));
// [gas, to, value, in_offset, in_len, out_offset, out_len]
mip3_instruction!(mip3_call, contract::call, |i| expand_range(i, 3, 4)
    && expand_range(i, 5, 6));
mip3_instruction!(mip3_call_code, contract::call_code, |i| expand_range(
    i, 3, 4
) && expand_range(
    i, 5, 6
));
// [gas, to, in_offset, in_len, out_offset, out_len]
mip3_instruction!(
    mip3_delegate_call,
    contract::delegate_call,
    |i| expand_range(i, 2, 3) && expand_range(i, 4, 5)
);
mip3_instruction!(mip3_static_call, contract::static_call, |i| expand_range(
    i, 2, 3
)
    && expand_range(i, 4, 5));

// [dst, src, len]
fn mip3_mcopy<WIRE: InterpreterTypes, H: Host + ?Sized>(context: InstructionContext<'_, H, WIRE>) {
    {
        let interpreter: &mut Interpreter<WIRE> = context.interpreter;
        let (Some(dst), Some(src), Some(len)) = (
            peek(interpreter, 0),
            peek(interpreter, 1),
            peek(interpreter, 2),
        ) else {
            interpreter.halt_underflow();
            return;
        };
        if !mip3_expand(interpreter, dst.max(src), len) {
            return;
        }
    }
    memory::mcopy(context)
}

// [offset, len, topics...]
fn mip3_log<const N: usize, WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    if !expand_range(context.interpreter, 0, 1) {
        return;
    }
    host::log::<N, H>(context)
}

// ---------------------------------------------------------------------------
// CREATE: delegated account guard, opcode initcode limit, MIP-3 memory
// ---------------------------------------------------------------------------

/// `vm/runtime/create.cpp`: with `can_create_inside_delegated() == false` a
/// CREATE executed by an account whose code is an EIP-7702 delegation
/// designator fails with a generic error (all gas consumed).
fn is_eip7702_delegation(code: &[u8]) -> bool {
    code.len() == 23 && code[..3] == [0xef, 0x01, 0x00]
}

// [value, offset, len(, salt)]
fn create_guarded<const IS_CREATE2: bool, const MIP3: bool, DB: revm::database::Database>(
    context: InstructionContext<'_, MonadContext<DB>, EthInterpreter>,
) {
    let hardfork = *context.host.cfg().spec();
    if context.interpreter.runtime_flag.is_static() {
        context
            .interpreter
            .halt(InstructionResult::CallNotAllowedInsideStatic);
        return;
    }
    if hardfork.is_delegated_create_blocked() {
        let target = context.interpreter.input.target_address();
        match context.host.load_account_code(target) {
            Some(code) if is_eip7702_delegation(&code.data) => {
                context.interpreter.halt(InstructionResult::NotActivated);
                return;
            }
            Some(_) => {}
            None => {
                context.interpreter.halt_fatal();
                return;
            }
        }
    }
    // `traits::max_initcode_size()` at the opcode level (48 KiB before
    // MONAD_FOUR) differs from the transaction level limit in `cfg`.
    if let Some(len) = peek(context.interpreter, 2) {
        if len > U256::from(hardfork.max_initcode_size()) {
            context
                .interpreter
                .halt(InstructionResult::CreateInitCodeSizeLimit);
            return;
        }
    }
    if MIP3 && !expand_range(context.interpreter, 1, 2) {
        return;
    }
    contract::create::<EthInterpreter, IS_CREATE2, MonadContext<DB>>(context)
}

// ---------------------------------------------------------------------------
// MIP-8 storage
// ---------------------------------------------------------------------------

/// `runtime::sload` with `mip_8_active`: the cold surcharge is decided by the
/// page tracker, not by the slot access list.
fn mip8_sload<WIRE: InterpreterTypes, H: Host + ?Sized>(context: InstructionContext<'_, H, WIRE>) {
    let InstructionContext { interpreter, host } = context;
    let Some(([], index)) = interpreter.stack.popn_top::<0>() else {
        interpreter.halt_underflow();
        return;
    };
    let target = interpreter.input.target_address();
    if access_page(&mut HostPageStore(host), target, *index)
        && !interpreter.gas.record_cost(COLD_STORAGE_ADDITIONAL_COST_V1)
    {
        interpreter.halt_oog();
        return;
    }
    let Some(storage) = host.sload(target, *index) else {
        interpreter.halt_fatal();
        return;
    };
    *index = storage.data;
}

/// `runtime::sstore` with `mip_8_active`.
fn mip8_sstore<WIRE: InterpreterTypes, H: Host + ?Sized>(context: InstructionContext<'_, H, WIRE>) {
    let InstructionContext { interpreter, host } = context;
    if interpreter.runtime_flag.is_static() {
        interpreter.halt(InstructionResult::StateChangeDuringStaticCall);
        return;
    }
    let Some([index, value]) = interpreter.stack.popn::<2>() else {
        interpreter.halt_underflow();
        return;
    };
    // EIP-2200 sentry, checked against the gas left before the base cost.
    if interpreter.gas.remaining() <= host.gas_params().call_stipend() {
        interpreter.halt(InstructionResult::ReentrancySentryOOG);
        return;
    }
    let target = interpreter.input.target_address();

    let mut gas = MIP8_BASE_SSTORE_COST;
    if access_page(&mut HostPageStore(host), target, index) {
        gas += COLD_STORAGE_ADDITIONAL_COST_V1;
    }
    let Some(load) = host.sstore(target, index, value) else {
        interpreter.halt_fatal();
        return;
    };
    let status = storage_status(
        load.data.original_value,
        load.data.present_value,
        load.data.new_value,
    );
    let (first_page_write, grew_state) =
        update_page(&mut HostPageStore(host), target, index, status);
    if first_page_write {
        gas += MIP8_PAGE_WRITE_COST;
    }
    if grew_state {
        gas += MIP8_PAGE_GROWTH_COST;
    }
    if !interpreter.gas.record_cost(gas) {
        interpreter.halt_oog();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::database_interface::EmptyDB;

    #[test]
    fn replaced_instructions_keep_stock_static_gas() {
        let stock = EthInstructions::<EthInterpreter, MonadContext<EmptyDB>>::new_mainnet_with_spec(
            MonadHardfork::MonadTen.into(),
        );
        let monad = monad_instructions::<EmptyDB>(MonadHardfork::MonadTen);
        for op in [
            MLOAD,
            MSTORE,
            MSTORE8,
            MCOPY,
            KECCAK256,
            CALLDATACOPY,
            CODECOPY,
            RETURNDATACOPY,
            EXTCODECOPY,
            LOG0,
            LOG4,
            RETURN,
            REVERT,
            CREATE,
            CREATE2,
            CALL,
            CALLCODE,
            DELEGATECALL,
            STATICCALL,
            SLOAD,
            SSTORE,
        ] {
            assert_eq!(
                monad.instruction_table[op as usize].static_gas(),
                stock.instruction_table[op as usize].static_gas(),
                "opcode {op:#x}"
            );
        }
        assert_eq!(monad.instruction_table[SLOAD as usize].static_gas(), 100);
        assert_eq!(monad.instruction_table[SSTORE as usize].static_gas(), 0);
    }

    #[test]
    fn detects_eip7702_delegation_designator() {
        let mut code = vec![0xef, 0x01, 0x00];
        code.extend_from_slice(&[0x11; 20]);
        assert!(is_eip7702_delegation(&code));
        assert!(!is_eip7702_delegation(&code[..22]));
        assert!(!is_eip7702_delegation(&[0x60, 0x00]));
        assert!(!is_eip7702_delegation(&[]));
    }
}
