//! Stylus/WASM execution seam. When a CALL lands on a contract whose bytecode
//! starts with a Stylus prefix (`0xEFF0xx`), `frame_run` runs the WASM body via
//! the native runtime instead of the EVM opcode loop, then feeds a synthetic
//! `InterpreterAction::Return` back through the stock `process_next_action` so
//! journal commit/revert, `CallOutcome` wrapping, and parent gas/return wiring
//! stay identical to an EVM callee. See `docs/stylus-execution-impl-plan.md`.
//!
//! Verification status: dispatch, decode, compile, execute, the full hostio
//! set, and subcall/create driving are wired. Exact gas is in place for the
//! memory model (nitro table), storage/account access + SSTORE refund (revm
//! GasParams), init/cached cost, and the L1 block-number EvmData field.
//! The hostio *response* encodings are aligned with nitro and unit-pinned.
//! **Gas/trace parity is still NOT verified end to end** and the following stay
//! approximate: capture-hostio (14), RecentWasms (strategy A only), and
//! enforceStylusPageLimit. All are TODO(Phase 4) and must be diffed against a
//! writer / Arb One traced RPC before shipping.

use super::ArbitrumEvm;
use crate::arbitrum::arbos_state::ArbStateReader;
use crate::arbitrum::precompile::{
    ArbWasm, ArbitrumContext, HostioHandler, PreparedStylusProgram, StylusExecInput, StylusOutcome,
    StylusRuntime,
};
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::cfg::gas_params::GasParams;
use revm::context_interface::{Block, Cfg, CreateScheme, Transaction};
use revm::handler::evm::ContextDbError;
use revm::handler::{EthFrame, EvmTr, FrameInitOrResult, FrameResult, ItemOrResult};
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::{
    CallInput, CallInputs, CallScheme, CallValue, CreateInputs, FrameInput, Gas, InstructionResult,
    InterpreterAction, InterpreterResult,
};
use revm::{Database, DatabaseRef};

/// Stylus program prefixes (nitro `IsStylusProgramPrefix`): classic / fragment
/// / root. EIP-3541 forbids deploying `0xEF`-leading code via CREATE, so on an
/// Arbitrum chain the only `0xEFF0xx` account code is an activated Stylus
/// program — the prefix alone has no false positives.
const STYLUS_CLASSIC_PREFIX: &[u8] = &[0xef, 0xf0, 0x00];
const STYLUS_FRAGMENT_PREFIX: &[u8] = &[0xef, 0xf0, 0x01];
const STYLUS_ROOT_PREFIX: &[u8] = &[0xef, 0xf0, 0x02];

/// True if `code` is an activated Stylus program blob (prefix-based dispatch).
pub(super) fn is_stylus_code(code: &[u8]) -> bool {
    code.len() > STYLUS_CLASSIC_PREFIX.len()
        && (code.starts_with(STYLUS_CLASSIC_PREFIX)
            || code.starts_with(STYLUS_FRAGMENT_PREFIX)
            || code.starts_with(STYLUS_ROOT_PREFIX))
}

/// nitro `apiStatus` (`arbos/programs/api.go:48`). This encoding is used by
/// SetTrieSlots(1) and SetTransientBytes32(3) only — `contract_call` answers
/// with a `UserOutcomeKind` byte and `create` with a success flag, so the three
/// are not interchangeable.
const API_STATUS_SUCCESS: u8 = 0;
const API_STATUS_FAILURE: u8 = 1;
const API_STATUS_OUT_OF_GAS: u8 = 2;
const API_STATUS_WRITE_PROTECTION: u8 = 3;

/// ArbOS version from which `setTrieSlots` reports a spent budget as `OutOfGas`
/// instead of `Failure` (`api.go:112-118`).
const ARBOS_SET_TRIE_SLOTS_OUT_OF_GAS: u64 = 50;

/// The call hostios instead answer with a `UserOutcomeKind` byte, of which nitro
/// only ever produces these two (`api.go:409-411`).
const CALL_STATUS_SUCCESS: u8 = 0;
const CALL_STATUS_FAILURE: u8 = 2;

/// `WasmAccountTouchCost(withCode: true)` bills a worst-case EXTCODESIZE on top
/// of the 2929 access cost, scaled by the chain's code-size limit, because the
/// code length is unknown before the load (`operations_acl_arbitrum.go:157`).
const EXTCODE_SIZE_GAS_EIP150: u64 = 700;
const DEFAULT_MAX_CODE_SIZE: u64 = 24_576;

/// Error strings for the requests that answer with a Go `error.Error()` string
/// (EmitLog, Create). `write protection` matches geth's `ErrWriteProtection`.
const WRITE_PROTECTION_ERROR: &[u8] = b"write protection";
const OUT_OF_GAS_ERROR: &[u8] = b"out of gas";
const MALFORMED_REQUEST_ERROR: &[u8] = b"malformed request";

/// nitro `Program::initGas`: `MinInitGas*128 + ceil(initCost*InitCostScalar*2/100)`.
fn init_gas(program: &PreparedStylusProgram) -> u64 {
    let base = (program.min_init_gas as u64).saturating_mul(128);
    let dyno = (program.init_cost as u64).saturating_mul((program.init_cost_scalar as u64) * 2);
    base.saturating_add(dyno.div_ceil(100))
}

/// nitro `Program::cachedGas`: `MinCachedInitGas*32 + ceil(cachedCost*CachedCostScalar*2/100)`.
fn cached_gas(program: &PreparedStylusProgram) -> u64 {
    let base = (program.min_cached_init_gas as u64).saturating_mul(32);
    let dyno = (program.cached_cost as u64).saturating_mul((program.cached_cost_scalar as u64) * 2);
    base.saturating_add(dyno.div_ceil(100))
}

/// nitro `memoryExponents` (`arbos/programs/memory.go`) — the exponential memory
/// ramp, indexed by page count (0..=128). Transcribed verbatim; must stay
/// byte-identical to nitro.
#[rustfmt::skip]
const MEMORY_EXPONENTS: [u32; 129] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 3, 3, 4, 5, 5, 6, 7, 8, 9, 11, 12, 14, 17, 19, 22, 25, 29, 33, 38,
    43, 50, 57, 65, 75, 85, 98, 112, 128, 147, 168, 193, 221, 253, 289, 331, 379, 434, 497, 569,
    651, 745, 853, 976, 1117, 1279, 1463, 1675, 1917, 2194, 2511, 2874, 3290, 3765, 4309, 4932,
    5645, 6461, 7395, 8464, 9687, 11087, 12689, 14523, 16621, 19024, 21773, 24919, 28521, 32642,
    37359, 42758, 48938, 56010, 64104, 73368, 83971, 96106, 109994, 125890, 144082, 164904, 188735,
    216010, 247226, 282953, 323844, 370643, 424206, 485509, 555672, 635973, 727880, 833067, 953456,
    1091243, 1248941, 1429429, 1636000, 1872423, 2143012, 2452704, 2807151, 3212820, 3677113,
    4208502, 4816684, 5512756, 6309419, 7221210, 8264766, 9459129, 10826093, 12390601, 14181199,
    16230562, 18576084, 21260563, 24332984, 27849408, 31873999,
];

/// ArbOS version from which return data is priced at EVM parity
/// (`ArbosVersion_StylusFixes` = 31).
const ARBOS_STYLUS_FIXES: u64 = 31;

/// nitro `evmMemoryCost` (`programs.go:340`): what the EVM would charge to hold
/// `size` bytes in memory — `MemoryGas` per word plus the quadratic term.
fn evm_memory_cost(size: u64) -> u64 {
    let words = size.div_ceil(32);
    words
        .saturating_mul(3)
        .saturating_add(words.saturating_mul(words) / 512)
}

fn memory_exp(pages: u16) -> u64 {
    match MEMORY_EXPONENTS.get(pages as usize) {
        Some(value) => *value as u64,
        None => u64::MAX,
    }
}

/// nitro `MemoryModel.GasCost` (`memory.go`): linear (`pageGas` per page beyond
/// `freePages`) + exponential ramp keyed on the high-water page count `ever`.
fn memory_gas_cost(new: u16, open: u16, ever: u16, free_pages: u16, page_gas: u16) -> u64 {
    let new_open = open.saturating_add(new);
    let new_ever = ever.max(new_open);
    if new_ever <= free_pages {
        return 0;
    }
    let sub_free = |pages: u16| pages.saturating_sub(free_pages);
    let adding = sub_free(new_open).saturating_sub(sub_free(open));
    let linear = (adding as u64).saturating_mul(page_gas as u64);
    let expand = memory_exp(new_ever).saturating_sub(memory_exp(ever));
    linear.saturating_add(expand)
}

/// Runs the current (top-of-stack) frame as a Stylus program and produces its
/// frame result. The caller (`frame_run`) has confirmed the callee bytecode is
/// a Stylus blob.
pub(super) fn run_stylus_frame<DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>>
where
    DB: Database + DatabaseRef,
{
    // 1. Gather frame inputs, then drop the frame borrow so the body can take
    //    `&mut ctx`/`&mut evm` (disjoint field of the same `Evm`).
    let (code, code_hash, calldata, contract, caller, value, is_static, gas_limit, opens_span) = {
        let frame = evm.inner.frame_stack.get();
        let code = frame.interpreter.bytecode.original_byte_slice().to_vec();
        let code_hash = keccak256(&code);
        let calldata = frame.interpreter.input.input.bytes(&evm.inner.ctx);
        let contract = frame.interpreter.input.target_address;
        let caller = frame.interpreter.input.caller_address;
        let value = frame.interpreter.input.call_value;
        let is_static = frame.interpreter.runtime_flag.is_static;
        let gas_limit = frame.interpreter.gas.remaining();
        // DELEGATECALL/CALLCODE run foreign code inside the caller's own frame,
        // so nitro opens no reentrancy span for them (`PushContract`).
        let opens_span = match &frame.input {
            FrameInput::Call(inputs) => !matches!(
                inputs.scheme,
                CallScheme::DelegateCall | CallScheme::CallCode
            ),
            _ => true,
        };
        (
            code, code_hash, calldata, contract, caller, value, is_static, gas_limit, opens_span,
        )
    };

    // 2. Read Programs state + decode wasm. `None` = not an up-to-date active
    //    Stylus program -> revert (nitro `getActiveProgram` error paths).
    let timestamp = evm.inner.ctx.block().timestamp().saturating_to::<u64>();
    let prepared =
        match ArbWasm::prepare_stylus_program(&mut evm.inner.ctx, code_hash, &code, timestamp) {
            Some(prepared) => prepared,
            None => {
                return finish_frame(evm, InstructionResult::Revert, Bytes::new(), Gas::new(gas_limit))
            }
        };

    // 3. Native asm: cache hit, else compile the on-chain wasm for the host.
    let asm = match evm.inner.ctx.chain().compiled_asm(prepared.module_hash) {
        Some(asm) => asm.clone(),
        None => match StylusRuntime::compile_from_env(&prepared.wasm, prepared.version) {
            Ok(asm) => {
                let asm = Bytes::from(asm);
                evm.inner
                    .ctx
                    .chain_mut()
                    .insert_compiled_asm(prepared.module_hash, asm.clone());
                asm
            }
            Err(_) => {
                return finish_frame(evm, InstructionResult::Revert, Bytes::new(), Gas::new(gas_limit))
            }
        },
    };

    // 4. Gas pre-charge, mirroring nitro `CallProgram` order: memory-init cost
    //    for the program footprint, then program init/cached cost. Strategy A
    //    for `cached` (on-chain flag only, no block RecentWasms LRU).
    //    TODO(Phase 4): RecentWasms (strategy B), enforceStylusPageLimit penalty.
    let mut gas = Gas::new(gas_limit);
    let (open, ever) = {
        let chain = evm.inner.ctx.chain();
        (chain.stylus_pages_open(), chain.stylus_pages_ever())
    };
    let mut precharge = memory_gas_cost(
        prepared.footprint,
        open,
        ever,
        prepared.free_pages,
        prepared.page_gas,
    );
    if prepared.cached || prepared.version > 1 {
        precharge = precharge.saturating_add(cached_gas(&prepared));
    }
    if !prepared.cached {
        precharge = precharge.saturating_add(init_gas(&prepared));
    }
    if !gas.record_cost(precharge) {
        gas.spend_all();
        return finish_frame(evm, InstructionResult::OutOfGas, Bytes::new(), gas);
    }
    // Reserve the footprint pages for the call (nitro `AddStylusPages`); restored
    // to `open` after so the reservation is released but the high-water `ever`
    // sticks.
    evm.inner
        .ctx
        .chain_mut()
        .set_stylus_pages_open(open.saturating_add(prepared.footprint));

    // A program is reentrant when it already has a frame open — asked before
    // this frame is counted (nitro `tx_processor.go:139`).
    let reentrant = opens_span && evm.inner.ctx.chain().stylus_frame_is_open(contract);
    if opens_span {
        evm.inner.ctx.chain_mut().enter_stylus_frame(contract);
    }

    // EvmData.block_number is the ArbOS-recorded L1 block number (what the
    // NUMBER opcode returns on Arbitrum, per PR #184's BLOCKHASH work), falling
    // back to the L2 number on a read error.
    let l1_block_number = evm
        .inner
        .ctx
        .db()
        .blockhashes_l1_block_number()
        .unwrap_or_else(|| evm.inner.ctx.block().number().saturating_to::<u64>());

    // 5. Assemble EvmData inputs.
    let input = StylusExecInput {
        arbos_version: prepared.arbos_version,
        block_basefee: U256::from(evm.inner.ctx.block().basefee()),
        chainid: evm.inner.ctx.cfg().chain_id(),
        block_coinbase: evm.inner.ctx.block().beneficiary(),
        block_gas_limit: evm.inner.ctx.block().gas_limit(),
        block_number: l1_block_number,
        block_timestamp: timestamp,
        contract_address: contract,
        module_hash: prepared.module_hash,
        msg_sender: caller,
        msg_value: value,
        // nitro passes `evm.TxContext.GasPrice` (`programs.go:279`), which geth
        // fills with the effective price — *not* the ArbOS paid price that the
        // GASPRICE opcode reports. revm's `gas_price()` is the 1559 max fee, so
        // the effective price has to be derived here.
        tx_gas_price: U256::from(
            evm.inner
                .ctx
                .tx()
                .effective_gas_price(evm.inner.ctx.block().basefee() as u128),
        ),
        tx_origin: evm.inner.ctx.tx().caller(),
        reentrant: reentrant as u32,
        cached: prepared.cached,
        tracing: false, // TODO(Phase 3/4): wire inspector tracing.
        version: prepared.version,
        max_depth: prepared.max_stack_depth,
        ink_price: prepared.ink_price,
    };

    // 6. Execute with the hostio bridge (holds `&mut evm` for the call only, so
    //    subcalls can drive child frames on the shared stack).
    let supplied = gas.remaining();
    let mut call_gas = supplied;
    let gas_params = evm.inner.ctx.cfg().gas_params().clone();
    let max_code_size = evm.inner.ctx.cfg().max_code_size() as u64;
    let mut hostio = StylusHostio {
        evm,
        contract,
        is_static,
        delegate_caller: caller,
        delegate_value: value,
        free_pages: prepared.free_pages,
        page_gas: prepared.page_gas,
        gas_params,
        max_code_size,
        arbos_version: prepared.arbos_version,
        refund: 0,
    };
    let result = StylusRuntime::call_from_env(&asm, &calldata, input, &mut hostio, &mut call_gas);
    let refund = hostio.refund;
    // Release the footprint page reservation (nitro's deferred SetStylusPagesOpen);
    // the high-water `ever` set during the call is retained.
    evm.inner.ctx.chain_mut().set_stylus_pages_open(open);
    if opens_span {
        evm.inner.ctx.chain_mut().exit_stylus_frame(contract);
    }

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(_) => return finish_frame(evm, InstructionResult::Revert, Bytes::new(), gas),
    };

    // 7. Thread gas + refund back onto the frame result.
    let wasm_used = supplied.saturating_sub(call_gas);
    let _ = gas.record_cost(wasm_used);
    gas.record_refund(refund);
    let (result, output) = match outcome.outcome {
        StylusOutcome::Success => (InstructionResult::Return, Bytes::from(outcome.output)),
        StylusOutcome::Revert => (InstructionResult::Revert, Bytes::from(outcome.output)),
        StylusOutcome::OutOfInk => {
            gas.spend_all();
            (InstructionResult::OutOfGas, Bytes::new())
        }
        StylusOutcome::Failure
        | StylusOutcome::OutOfStack
        | StylusOutcome::NativeStackOverflow => (InstructionResult::Revert, Bytes::new()),
    };

    // Return data must cost at least what it would in the EVM. nitro does this
    // by capping the gas handed back — measured against the frame's gas *before*
    // the pre-charge — rather than by charging again (`programs.go:289-302`).
    if !output.is_empty() && prepared.arbos_version >= ARBOS_STYLUS_FIXES {
        let evm_cost = evm_memory_cost(output.len() as u64);
        if gas_limit < evm_cost {
            gas.spend_all();
            return finish_frame(evm, InstructionResult::OutOfGas, Bytes::new(), gas);
        }
        let excess = gas.remaining().saturating_sub(gas_limit - evm_cost);
        if excess > 0 {
            let _ = gas.record_cost(excess);
        }
    }
    finish_frame(evm, result, output, gas)
}

/// Feeds a synthetic result through the stock `process_next_action` so journal
/// commit/revert + `CallOutcome` wrapping + finish-flagging match an EVM callee.
fn finish_frame<DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
    result: InstructionResult,
    output: Bytes,
    gas: Gas,
) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>>
where
    DB: Database + DatabaseRef,
{
    let frame = evm.inner.frame_stack.get();
    let action = InterpreterAction::Return(InterpreterResult::new(result, output, gas));
    let context = &mut evm.inner.ctx;
    frame.process_next_action(context, action).inspect(|item| {
        if item.is_result() {
            frame.set_finished(true);
        }
    })
}

/// Drives a Stylus subcall's child frame subtree to completion synchronously
/// (WASM can't yield to revm's run loop — the G1 problem), returning the direct
/// child's `CallOutcome`. The child is pushed above the Stylus parent; grandchild
/// frames resolve via the stock `frame_return_result`, but the DIRECT child's
/// result is captured + popped manually so it never reaches the Stylus parent's
/// `return_result` (which would corrupt the parent's interpreter gas/stack).
fn drive_subframe<DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
    frame_input: FrameInput,
) -> Option<FrameResult>
where
    DB: Database + DatabaseRef,
{
    // Build the child FrameInit from the current (Stylus parent) frame.
    let child_init = {
        let parent = evm.inner.frame_stack.get();
        FrameInit {
            depth: parent.depth + 1,
            memory: parent.interpreter.memory.new_child_context(),
            frame_input,
        }
    };
    // The Stylus parent's index; the direct child lands one above it.
    let child_index = evm.inner.frame_stack.index().map(|i| i + 1);

    match evm.frame_init(child_init).ok()? {
        // Resolved without pushing a frame (precompile / empty code).
        ItemOrResult::Result(frame_result) => return Some(frame_result),
        ItemOrResult::Item(_frame) => {}
    }

    loop {
        let result = match evm.frame_run().ok()? {
            ItemOrResult::Item(init) => match evm.frame_init(init).ok()? {
                ItemOrResult::Item(_frame) => continue,
                ItemOrResult::Result(frame_result) => frame_result,
            },
            ItemOrResult::Result(frame_result) => frame_result,
        };
        // If the finished frame is the direct child, capture + pop manually.
        if evm.inner.frame_stack.index() == child_index {
            evm.inner.frame_stack.pop();
            return Some(result);
        }
        // Deeper frame: return to its (non-Stylus) parent, which resumes.
        match evm.frame_return_result(result) {
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => continue,
        }
    }
}

/// A `0x00`-led create response: the remaining bytes are an error string that
/// aborts the calling program (`arbutil` `req.rs:88-96`).
fn create_error(message: &[u8]) -> Vec<u8> {
    let mut resp = Vec::with_capacity(1 + message.len());
    resp.push(0);
    resp.extend_from_slice(message);
    resp
}

/// Services a Stylus program's hostio requests against revm state. Holds
/// `&mut ArbitrumEvm` so subcalls can drive child frames on the shared stack.
struct StylusHostio<'a, DB: Database + DatabaseRef, I> {
    evm: &'a mut ArbitrumEvm<DB, I>,
    contract: Address,
    is_static: bool,
    /// The Stylus frame's caller/value, used for DELEGATECALL context.
    delegate_caller: Address,
    delegate_value: U256,
    /// StylusParams memory model inputs, for AddPages.
    free_pages: u16,
    page_gas: u16,
    /// revm gas schedule, for exact SLOAD/SSTORE/account-touch cost + refund.
    gas_params: GasParams,
    /// Scales the EXTCODESIZE component of `AccountCode`.
    max_code_size: u64,
    /// Gates `setTrieSlots`' OutOfGas status.
    arbos_version: u64,
    refund: i64,
}

impl<DB: Database + DatabaseRef, I> HostioHandler for StylusHostio<'_, DB, I> {
    fn handle(&mut self, req_type: u32, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        match req_type {
            0 => self.get_bytes32(input),
            1 => self.set_trie_slots(input),
            2 => self.get_transient(input),
            3 => self.set_transient(input),
            4 => self.contract_call(input, CallScheme::Call),
            5 => self.contract_call(input, CallScheme::DelegateCall),
            6 => self.contract_call(input, CallScheme::StaticCall),
            7 => self.create(input, false),
            8 => self.create(input, true),
            9 => self.emit_log(input),
            10 => self.account_balance(input),
            11 => self.account_code(input),
            12 => self.account_code_hash(input),
            13 => self.add_pages(input),
            // TODO(Phase 3/4): 7-8 create (needs CreateInputs driving), 14
            //   capture-hostio (tracing).
            _ => (Vec::new(), Vec::new(), 0),
        }
    }
}

impl<DB: Database + DatabaseRef, I> StylusHostio<'_, DB, I> {
    fn ctx(&mut self) -> &mut ArbitrumContext<DB> {
        &mut self.evm.inner.ctx
    }

    /// EIP-2929 SLOAD cost (nitro `WasmStateLoadCost`).
    fn storage_load_cost(&self, is_cold: bool) -> u64 {
        self.gas_params.warm_storage_read_cost()
            + if is_cold {
                self.gas_params.cold_storage_additional_cost()
            } else {
                0
            }
    }

    /// nitro `WasmCallCost` (`operations_acl_arbitrum.go:114`): the 2929 static
    /// and cold-access costs plus the value-transfer surcharges, tallied against
    /// the caller's budget. `None` means the tally exceeded it, which nitro
    /// answers by burning the whole budget.
    fn call_base_cost(&mut self, target: Address, value: U256, budget: u64) -> Option<u64> {
        let transfers_value = value > U256::ZERO;
        let mut total = self.gas_params.warm_storage_read_cost();
        if total > budget {
            return None;
        }
        // Loading the account also warms it, as nitro's AddAddressToAccessList does.
        let (is_cold, is_empty) = match self.ctx().journal_mut().load_account(target) {
            Ok(load) => (load.is_cold, load.data.is_empty()),
            Err(_) => (false, false),
        };
        if is_cold {
            total = total.saturating_add(self.gas_params.cold_account_additional_cost());
            if total > budget {
                return None;
            }
        }
        if transfers_value && is_empty {
            total = total.saturating_add(self.gas_params.new_account_cost(true, true));
            if total > budget {
                return None;
            }
        }
        if transfers_value {
            total = total.saturating_add(self.gas_params.transfer_value_cost());
            if total > budget {
                return None;
            }
        }
        Some(total)
    }

    /// nitro `WasmAccountTouchCost(withCode: true)` — the access cost plus the
    /// worst-case EXTCODESIZE charge.
    fn account_code_cost(&self, is_cold: bool) -> u64 {
        let ext_code_cost =
            (self.max_code_size / DEFAULT_MAX_CODE_SIZE).saturating_mul(EXTCODE_SIZE_GAS_EIP150);
        ext_code_cost.saturating_add(self.account_touch_cost(is_cold))
    }

    /// EIP-2929 account-access cost (nitro `WasmAccountTouchCost`).
    fn account_touch_cost(&self, is_cold: bool) -> u64 {
        self.gas_params.warm_storage_read_cost()
            + if is_cold {
                self.gas_params.cold_account_additional_cost()
            } else {
                0
            }
    }

    fn get_bytes32(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 32 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let contract = self.contract;
        let (value, is_cold) = match self.ctx().journal_mut().sload(contract, key) {
            Ok(load) => (load.data, load.is_cold),
            Err(_) => (U256::ZERO, false),
        };
        let cost = self.storage_load_cost(is_cold);
        (value.to_be_bytes::<32>().to_vec(), Vec::new(), cost)
    }

    /// SetTrieSlots: `gasLeft[8] ++ (key[32] ++ value[32])*N`. Each slot is
    /// charged against the request's own budget and the write is skipped once it
    /// runs out (`api.go:82-118`).
    fn set_trie_slots(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static {
            return (vec![API_STATUS_WRITE_PROTECTION], Vec::new(), 0);
        }
        if input.len() < 8 {
            return (vec![API_STATUS_FAILURE], Vec::new(), 0);
        }
        let budget = u64::from_be_bytes(input[..8].try_into().unwrap());
        let contract = self.contract;
        let mut remaining = budget;
        let mut out_of_gas = false;
        let mut offset = 8;
        while input.len() >= offset + 64 {
            let key = U256::from_be_slice(&input[offset..offset + 32]);
            let value = U256::from_be_slice(&input[offset + 32..offset + 64]);
            // nitro prices the slot before writing it. revm only reports the
            // SStoreResult the price depends on *from* the write, so each write
            // goes in a checkpoint that is rolled back when it is unaffordable.
            let checkpoint = self.ctx().journal_mut().checkpoint();
            match self.ctx().journal_mut().sstore(contract, key, value) {
                Ok(load) => {
                    // Exact revm SSTORE gas + refund (EIP-2929/2200/3529).
                    // Arbitrum is always post-Istanbul.
                    let slot_cost = self.gas_params.sstore_static_gas()
                        + self
                            .gas_params
                            .sstore_dynamic_gas(true, &load.data, load.is_cold);
                    if slot_cost > remaining {
                        self.ctx().journal_mut().checkpoint_revert(checkpoint);
                        remaining = 0;
                        out_of_gas = true;
                        break;
                    }
                    self.ctx().journal_mut().checkpoint_commit();
                    self.refund += self.gas_params.sstore_refund(true, &load.data);
                    remaining -= slot_cost;
                }
                Err(_) => {
                    self.ctx().journal_mut().checkpoint_revert(checkpoint);
                    return (
                        vec![API_STATUS_FAILURE],
                        Vec::new(),
                        budget.saturating_sub(remaining),
                    );
                }
            }
            offset += 64;
        }
        // Spending the budget exactly also counts as out of gas.
        let status = if out_of_gas || remaining == 0 {
            if self.arbos_version < ARBOS_SET_TRIE_SLOTS_OUT_OF_GAS {
                API_STATUS_FAILURE
            } else {
                API_STATUS_OUT_OF_GAS
            }
        } else {
            API_STATUS_SUCCESS
        };
        (vec![status], Vec::new(), budget.saturating_sub(remaining))
    }

    fn get_transient(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 32 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let contract = self.contract;
        let value = self.ctx().journal_mut().tload(contract, key);
        (value.to_be_bytes::<32>().to_vec(), Vec::new(), 0)
    }

    /// Answers with an `apiStatus` byte; an empty response makes the Rust side
    /// fail the whole program with "empty result!" (`arbutil` `req.rs:174`).
    fn set_transient(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static {
            return (vec![API_STATUS_WRITE_PROTECTION], Vec::new(), 0);
        }
        if input.len() < 64 {
            return (vec![API_STATUS_FAILURE], Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let value = U256::from_be_slice(&input[32..64]);
        let contract = self.contract;
        self.ctx().journal_mut().tstore(contract, key, value);
        (vec![API_STATUS_SUCCESS], Vec::new(), 0)
    }

    /// ContractCall / DelegateCall / StaticCall: `addr[20] ++ value[32] ++
    /// gasLeft[8] ++ gasReq[8] ++ calldata`. Response: `status[1]`, returndata
    /// on `raw_data`.
    /// TODO(Phase 4): exact base cost (nitro `WasmCallCost`), stipend, 63/64,
    /// and status encoding — diff against writer.
    fn contract_call(&mut self, input: &[u8], scheme: CallScheme) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 68 {
            return (vec![CALL_STATUS_FAILURE], Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let value = U256::from_be_slice(&input[20..52]);
        let gas_left = u64::from_be_bytes(input[52..60].try_into().unwrap());
        let gas_req = u64::from_be_bytes(input[60..68].try_into().unwrap());
        let calldata = Bytes::copy_from_slice(&input[68..]);

        let is_static = self.is_static || scheme == CallScheme::StaticCall;
        // Read-only calls are not payable (nitro `api.go:142`, geth `opCall`).
        if self.is_static && value > U256::ZERO {
            return (vec![CALL_STATUS_FAILURE], Vec::new(), 0);
        }
        let base_cost = match self.call_base_cost(addr, value, gas_left) {
            Some(cost) => cost,
            // The tally blew the caller's budget; nitro burns all of it.
            None => return (vec![CALL_STATUS_FAILURE], Vec::new(), gas_left),
        };
        // 63/64 of what survives the base cost, capped by the request. Written
        // as nitro does — `x * 63 / 64`, which is not `x - x / 64`.
        let start_gas = gas_left.saturating_sub(base_cost).saturating_mul(63) / 64;
        let mut call_gas = gas_req.min(start_gas);
        if value > U256::ZERO {
            // The stipend rides on top of the 63/64 split, and unlike geth's
            // opCall nitro bills whatever the callee spends of it.
            call_gas = call_gas.saturating_add(self.gas_params.call_stipend());
        }

        let (target_address, bytecode_address, caller, call_value) = match scheme {
            CallScheme::Call => (addr, addr, self.contract, CallValue::Transfer(value)),
            CallScheme::StaticCall => (addr, addr, self.contract, CallValue::Transfer(U256::ZERO)),
            CallScheme::DelegateCall => (
                self.contract,
                addr,
                self.delegate_caller,
                CallValue::Apparent(self.delegate_value),
            ),
            CallScheme::CallCode => (self.contract, addr, self.contract, CallValue::Transfer(value)),
        };

        let inputs = CallInputs {
            input: CallInput::Bytes(calldata),
            return_memory_offset: 0..0,
            gas_limit: call_gas,
            bytecode_address,
            known_bytecode: None,
            target_address,
            caller,
            value: call_value,
            scheme,
            is_static,
        };

        match drive_subframe(self.evm, FrameInput::Call(Box::new(inputs))) {
            Some(FrameResult::Call(outcome)) => {
                let returned = outcome.gas().remaining();
                let status = if outcome.instruction_result().is_ok() {
                    CALL_STATUS_SUCCESS
                } else {
                    CALL_STATUS_FAILURE
                };
                let output = outcome.output().to_vec();
                let cost = base_cost.saturating_add(call_gas.saturating_sub(returned));
                (vec![status], output, cost)
            }
            _ => (
                vec![CALL_STATUS_FAILURE],
                Vec::new(),
                base_cost.saturating_add(call_gas),
            ),
        }
    }

    /// Create1 (`gas[8] ++ endowment[32] ++ code`) / Create2 (`... ++ salt[32]
    /// ++ code`). Response: `1 ++ addr[20]` on success (returndata on raw_data),
    /// else `0` with the revert data on raw_data.
    /// TODO(Phase 4): exact create gas (CreateGas + keccak word cost for CREATE2)
    /// and the precise error-response encoding — diff against writer.
    fn create(&mut self, input: &[u8], is_create2: bool) -> (Vec<u8>, Vec<u8>, u64) {
        let header = if is_create2 { 72 } else { 40 };
        // The requested gas doubles as the cost charged on the refusal paths,
        // where nitro burns the whole budget (`api.go:425`).
        let gas = if input.len() >= 8 {
            u64::from_be_bytes(input[0..8].try_into().unwrap())
        } else {
            0
        };
        if self.is_static {
            return (create_error(WRITE_PROTECTION_ERROR), Vec::new(), gas);
        }
        if input.len() < header {
            return (create_error(MALFORMED_REQUEST_ERROR), Vec::new(), gas);
        }
        let endowment = U256::from_be_slice(&input[8..40]);
        let scheme = if is_create2 {
            CreateScheme::Create2 {
                salt: U256::from_be_slice(&input[40..72]),
            }
        } else {
            CreateScheme::Create
        };
        let init_code = Bytes::copy_from_slice(&input[header..]);
        // nitro `api.go:217-233`: CreateGas, plus CREATE2's keccak word cost.
        let base_cost = if is_create2 {
            self.gas_params.create2_cost(init_code.len())
        } else {
            self.gas_params.create_cost()
        };
        if gas < base_cost {
            return (create_error(OUT_OF_GAS_ERROR), Vec::new(), gas);
        }
        // EIP-150 keeps a 64th back for the caller — note nitro splits this as
        // `gas -= gas / 64` *after* the base cost, unlike the call path.
        let after_base = gas - base_cost;
        let one_64th = after_base / 64;
        let child_gas = after_base - one_64th;
        let inputs = CreateInputs::new(self.contract, scheme, endowment, init_code, child_gas);

        match drive_subframe(self.evm, FrameInput::Create(Box::new(inputs))) {
            Some(FrameResult::Create(outcome)) => {
                let returned = outcome.gas().remaining();
                // The withheld 64th goes back to the caller (`api.go:260`).
                let cost = gas.saturating_sub(returned.saturating_add(one_64th));
                // A failed deploy still answers "success" carrying the zero
                // address, exactly as the EVM's CREATE pushes 0 on failure.
                let address = outcome.address.unwrap_or(Address::ZERO);
                let mut resp = Vec::with_capacity(21);
                resp.push(1);
                resp.extend_from_slice(address.as_slice());
                // Return data survives a revert only (nitro `api.go:257-258`).
                let output = if matches!(outcome.instruction_result(), InstructionResult::Revert) {
                    outcome.output().to_vec()
                } else {
                    Vec::new()
                };
                (resp, output, cost)
            }
            _ => (create_error(b"create frame failed"), Vec::new(), gas),
        }
    }

    /// EmitLog: `topics[4] ++ topic[32]*n ++ data`. Gas is charged Rust-side
    /// (`pay_for_evm_log`), so the wire cost is 0.
    /// An *empty* response means success here; any non-empty response is read as
    /// an error string and fails the program (`arbutil` `req.rs:266`) — the
    /// inverse of the `apiStatus` convention.
    fn emit_log(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static {
            return (WRITE_PROTECTION_ERROR.to_vec(), Vec::new(), 0);
        }
        if input.len() < 4 {
            return (MALFORMED_REQUEST_ERROR.to_vec(), Vec::new(), 0);
        }
        let num_topics = u32::from_be_bytes([input[0], input[1], input[2], input[3]]) as usize;
        let topics_end = 4 + num_topics * 32;
        if input.len() < topics_end {
            return (MALFORMED_REQUEST_ERROR.to_vec(), Vec::new(), 0);
        }
        let topics = (0..num_topics)
            .map(|i| B256::from_slice(&input[4 + i * 32..4 + (i + 1) * 32]))
            .collect::<Vec<_>>();
        let data = Bytes::copy_from_slice(&input[topics_end..]);
        let contract = self.contract;
        self.ctx()
            .journal_mut()
            .log(Log::new_unchecked(contract, topics, data));
        (Vec::new(), Vec::new(), 0)
    }

    fn account_balance(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 20 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let (balance, is_cold) = match self.ctx().journal_mut().load_account(addr) {
            Ok(load) => (load.data.info.balance, load.is_cold),
            Err(_) => (U256::ZERO, false),
        };
        (
            balance.to_be_bytes::<32>().to_vec(),
            Vec::new(),
            self.account_touch_cost(is_cold),
        )
    }

    fn account_code_hash(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 20 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let (hash, is_cold) = match self.ctx().journal_mut().code_hash(addr) {
            Ok(load) => (load.data, load.is_cold),
            Err(_) => (B256::ZERO, false),
        };
        (hash.0.to_vec(), Vec::new(), self.account_touch_cost(is_cold))
    }

    /// AccountCode: `addr[20] ++ gasLeft[8]`. Code goes on the `raw_data`
    /// channel; the response stays empty.
    fn account_code(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 28 {
            return (Vec::new(), Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let gas_left = u64::from_be_bytes(input[20..28].try_into().unwrap());
        let (code, is_cold) = match self.ctx().journal_mut().code(addr) {
            Ok(load) => (load.data.to_vec(), load.is_cold),
            Err(_) => (Vec::new(), false),
        };
        let cost = self.account_code_cost(is_cold);
        // nitro still bills the full cost but hands back no code when the
        // program cannot afford the load, which then runs it out of ink
        // (`api.go:289-299`).
        if gas_left < cost {
            return (Vec::new(), Vec::new(), cost);
        }
        (Vec::new(), code, cost)
    }

    /// AddPages: `pages[2]` (u16). Charges nitro `MemoryModel.GasCost` and tracks
    /// open pages on the execution context.
    /// TODO(Phase 4): enforceStylusPageLimit penalty (breach -> OOG).
    fn add_pages(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 2 {
            return (Vec::new(), Vec::new(), 0);
        }
        let new_pages = u16::from_be_bytes([input[0], input[1]]);
        let open = self.ctx().chain().stylus_pages_open();
        let ever = self.ctx().chain().stylus_pages_ever();
        let cost = memory_gas_cost(new_pages, open, ever, self.free_pages, self.page_gas);
        self.ctx()
            .chain_mut()
            .set_stylus_pages_open(open.saturating_add(new_pages));
        (Vec::new(), Vec::new(), cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::precompile::ArbitrumPrecompileEnv;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::database::{in_memory_db::CacheDB, EmptyDB};

    type TestDb = CacheDB<EmptyDB>;

    fn test_evm() -> ArbitrumEvm<TestDb, ()> {
        ArbitrumEvm::new(
            BlockEnv::default(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            CacheDB::new(EmptyDB::default()),
            (),
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        )
    }

    fn test_hostio(evm: &mut ArbitrumEvm<TestDb, ()>, is_static: bool) -> StylusHostio<'_, TestDb, ()> {
        let gas_params = evm.inner.ctx.cfg().gas_params().clone();
        // revm expects an account to be journal-loaded before its storage is
        // touched, which in production the frame setup has already done.
        let _ = evm.inner.ctx.journal_mut().load_account(Address::ZERO);
        StylusHostio {
            evm,
            contract: Address::ZERO,
            is_static,
            delegate_caller: Address::ZERO,
            delegate_value: U256::ZERO,
            free_pages: 2,
            page_gas: 1000,
            gas_params,
            max_code_size: DEFAULT_MAX_CODE_SIZE,
            arbos_version: 61,
            refund: 0,
        }
    }

    #[test]
    fn transient_store_always_answers_with_a_status_byte() {
        // An empty response makes the Stylus runtime abort the whole program
        // with "empty result!" (`arbutil` req.rs:174), so every path answers.
        let mut evm = test_evm();
        let (response, raw, cost) = test_hostio(&mut evm, false).set_transient(&[7u8; 64]);
        assert_eq!(response, vec![API_STATUS_SUCCESS]);
        assert!(raw.is_empty());
        assert_eq!(cost, 0);

        let mut evm = test_evm();
        let (response, _, _) = test_hostio(&mut evm, true).set_transient(&[7u8; 64]);
        assert_eq!(response, vec![API_STATUS_WRITE_PROTECTION]);

        let mut evm = test_evm();
        let (response, _, _) = test_hostio(&mut evm, false).set_transient(&[0u8; 8]);
        assert_eq!(response, vec![API_STATUS_FAILURE]);
    }

    #[test]
    fn emit_log_answers_empty_on_success_and_an_error_string_otherwise() {
        // Inverse of the status-byte convention: empty means success, and any
        // non-empty response is read as an error string (`req.rs:266`).
        let mut input = 0u32.to_be_bytes().to_vec(); // zero topics
        input.extend_from_slice(b"payload");

        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, false).emit_log(&input);
        assert!(response.is_empty(), "success must answer empty");
        assert_eq!(cost, 0, "log gas is charged runtime-side");

        let mut evm = test_evm();
        let (response, _, _) = test_hostio(&mut evm, true).emit_log(&input);
        assert_eq!(response, WRITE_PROTECTION_ERROR);

        let mut evm = test_evm();
        let (response, _, _) = test_hostio(&mut evm, false).emit_log(&[0u8; 2]);
        assert_eq!(response, MALFORMED_REQUEST_ERROR);
    }

    #[test]
    fn return_data_is_priced_like_evm_memory() {
        // nitro `evmMemoryCost`: words*MemoryGas + words^2/QuadCoeffDiv.
        assert_eq!(evm_memory_cost(0), 0);
        assert_eq!(evm_memory_cost(32), 3);
        assert_eq!(evm_memory_cost(33), 6);
        assert_eq!(evm_memory_cost(32 * 512), 512 * 3 + 512);
    }

    #[test]
    fn account_code_bills_the_worst_case_extcodesize() {
        let mut evm = test_evm();
        let hostio = test_hostio(&mut evm, false);
        // 700 for the unknown code length, plus the 2929 access cost.
        assert_eq!(hostio.account_code_cost(true), 700 + 2600);
        assert_eq!(hostio.account_code_cost(false), 700 + 100);
    }

    #[test]
    fn set_trie_slots_stops_at_its_own_budget() {
        let mut slot = 10u64.to_be_bytes().to_vec(); // budget far below an SSTORE
        slot.extend_from_slice(&[1u8; 32]);
        slot.extend_from_slice(&[2u8; 32]);

        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, false).set_trie_slots(&slot);
        assert_eq!(response, vec![API_STATUS_OUT_OF_GAS]);
        assert_eq!(cost, 10, "an exhausted budget is burned whole");

        let mut affordable = 1_000_000u64.to_be_bytes().to_vec();
        affordable.extend_from_slice(&slot[8..]);
        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, false).set_trie_slots(&affordable);
        assert_eq!(response, vec![API_STATUS_SUCCESS]);
        assert!(cost > 0 && cost < 1_000_000);
    }

    #[test]
    fn payable_calls_are_refused_in_a_static_context() {
        let mut input = Address::with_last_byte(9).to_vec();
        input.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
        input.extend_from_slice(&100_000u64.to_be_bytes()); // gasLeft
        input.extend_from_slice(&100_000u64.to_be_bytes()); // gasReq

        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, true).contract_call(&input, CallScheme::Call);
        assert_eq!(response, vec![CALL_STATUS_FAILURE]);
        assert_eq!(cost, 0);
    }

    #[test]
    fn create_below_its_base_cost_is_out_of_gas() {
        let mut input = 1_000u64.to_be_bytes().to_vec(); // below CreateGas
        input.extend_from_slice(&[0u8; 32]);

        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, false).create(&input, false);
        assert_eq!(response[0], 0);
        assert_eq!(&response[1..], OUT_OF_GAS_ERROR);
        assert_eq!(cost, 1_000);
    }

    #[test]
    fn create_refusals_are_error_strings_that_burn_the_budget() {
        // Only the paths nitro refuses outright answer with a 0-led error
        // string; a failed deploy instead reports the zero address so the
        // program keeps running (covered by writer diffing, not here).
        let mut input = 4_242u64.to_be_bytes().to_vec();
        input.extend_from_slice(&[0u8; 32]); // endowment

        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, true).create(&input, false);
        assert_eq!(response[0], 0);
        assert_eq!(&response[1..], WRITE_PROTECTION_ERROR);
        assert_eq!(cost, 4_242, "nitro burns the requested gas on refusal");

        let mut evm = test_evm();
        let (response, _, cost) = test_hostio(&mut evm, false).create(&input, true);
        assert_eq!(response[0], 0, "a create2 request is short of its salt");
        assert_eq!(&response[1..], MALFORMED_REQUEST_ERROR);
        assert_eq!(cost, 4_242);
    }

    #[test]
    fn memory_model_matches_nitro() {
        // Exp table endpoints (pins the 129-value transcription).
        assert_eq!(memory_exp(0), 1);
        assert_eq!(memory_exp(128), 31_873_999);
        assert_eq!(memory_exp(129), u64::MAX); // beyond the table

        // GasCost(new, open, ever, free_pages, page_gas) — nitro memory.go.
        // Within the free window -> 0.
        assert_eq!(memory_gas_cost(1, 0, 0, 2, 1000), 0);
        // 3 pages, 2 free: 1 linear page * 1000, exp(3)-exp(0) = 1-1 = 0.
        assert_eq!(memory_gas_cost(3, 0, 0, 2, 1000), 1000);
        // 10 pages, 2 free: 8 linear * 1000 + (exp(10)-exp(0)) = 8000 + (3-1).
        assert_eq!(memory_gas_cost(10, 0, 0, 2, 1000), 8002);
        // Re-growing already-open pages is linear only (exp keyed on high-water).
        assert_eq!(memory_gas_cost(0, 10, 10, 2, 1000), 0);
    }

    #[test]
    fn dispatches_only_on_stylus_prefixes() {
        // The three activated-program prefixes (with a non-empty body) dispatch.
        assert!(is_stylus_code(&[0xef, 0xf0, 0x00, 0x01]));
        assert!(is_stylus_code(&[0xef, 0xf0, 0x01, 0x2a]));
        assert!(is_stylus_code(&[0xef, 0xf0, 0x02, 0xff, 0x00]));
    }

    #[test]
    fn does_not_dispatch_on_non_stylus_code() {
        // Ordinary EVM bytecode, empty code, and near-miss prefixes must not
        // dispatch (no false positives — see design §8.2.1).
        assert!(!is_stylus_code(&[]));
        assert!(!is_stylus_code(&[0x60, 0x80, 0x60, 0x40])); // PUSH1 0x80 ...
        assert!(!is_stylus_code(&[0xef])); // bare 0xEF
        assert!(!is_stylus_code(&[0xef, 0x00, 0x01, 0x02])); // EOF magic 0xEF00
        assert!(!is_stylus_code(&[0xef, 0xf0, 0x03, 0x00])); // unknown 4th prefix byte
        assert!(!is_stylus_code(&[0xef, 0xf0, 0x00])); // prefix only, no body
    }
}
