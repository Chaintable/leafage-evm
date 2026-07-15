//! Stylus/WASM execution seam. When a CALL lands on a contract whose bytecode
//! starts with a Stylus prefix (`0xEFF0xx`), `frame_run` runs the WASM body via
//! the native runtime instead of the EVM opcode loop, then feeds a synthetic
//! `InterpreterAction::Return` back through the stock `process_next_action` so
//! journal commit/revert, `CallOutcome` wrapping, and parent gas/return wiring
//! stay identical to an EVM callee. See `docs/stylus-execution-impl-plan.md`.
//!
//! Verification status: dispatch, decode, compile, execute, storage/account/log
//! hostio, and subcall driving are wired. **Gas/trace parity is NOT verified** —
//! the exact nitro pre-charge (memory model + RecentWasms), the exact hostio
//! gas/refund (EIP-2929/2200 via nitro `Wasm*Cost`), the subcall base-cost and
//! status encoding, create (hostio 7/8), capture-hostio (14), and the L1
//! block-number / paid-gas-price EvmData fields are TODO(Phase 4) and must be
//! diffed against a writer / Arb One traced RPC before shipping.

use super::ArbitrumEvm;
use crate::arbitrum::arbos_state::ArbStateReader;
use crate::arbitrum::precompile::{
    ArbWasm, ArbitrumContext, HostioHandler, PreparedStylusProgram, StylusExecInput, StylusOutcome,
    StylusRuntime,
};
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use revm::context::{ContextTr, JournalTr};
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
    let (code, code_hash, calldata, contract, caller, value, is_static, gas_limit) = {
        let frame = evm.inner.frame_stack.get();
        let code = frame.interpreter.bytecode.original_byte_slice().to_vec();
        let code_hash = keccak256(&code);
        let calldata = frame.interpreter.input.input.bytes(&evm.inner.ctx);
        let contract = frame.interpreter.input.target_address;
        let caller = frame.interpreter.input.caller_address;
        let value = frame.interpreter.input.call_value;
        let is_static = frame.interpreter.runtime_flag.is_static;
        let gas_limit = frame.interpreter.gas.remaining();
        (
            code, code_hash, calldata, contract, caller, value, is_static, gas_limit,
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
        // TODO(Phase 4): nitro's paid gas price (GasPriceOp), not the raw tx price.
        tx_gas_price: U256::from(evm.inner.ctx.tx().gas_price()),
        tx_origin: evm.inner.ctx.tx().caller(),
        reentrant: 0, // TODO(Phase 3): depth-based reentrancy flag.
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
    let mut hostio = StylusHostio {
        evm,
        contract,
        is_static,
        delegate_caller: caller,
        delegate_value: value,
        free_pages: prepared.free_pages,
        page_gas: prepared.page_gas,
        refund: 0,
    };
    let result = StylusRuntime::call_from_env(&asm, &calldata, input, &mut hostio, &mut call_gas);
    let refund = hostio.refund;
    // Release the footprint page reservation (nitro's deferred SetStylusPagesOpen);
    // the high-water `ever` set during the call is retained.
    evm.inner.ctx.chain_mut().set_stylus_pages_open(open);

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
        // TODO(Phase 4): nitro WasmStateLoadCost; approximate EIP-2929 SLOAD.
        let cost = if is_cold { 2100 } else { 100 };
        (value.to_be_bytes::<32>().to_vec(), Vec::new(), cost)
    }

    fn set_trie_slots(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static {
            return (vec![3], Vec::new(), 0); // WriteProtection
        }
        let contract = self.contract;
        let mut cost = 0u64;
        let mut offset = if input.len() >= 8 { 8 } else { input.len() };
        while input.len() >= offset + 64 {
            let key = U256::from_be_slice(&input[offset..offset + 32]);
            let value = U256::from_be_slice(&input[offset + 32..offset + 64]);
            match self.ctx().journal_mut().sstore(contract, key, value) {
                Ok(load) => {
                    // TODO(Phase 4): exact EIP-2200 cost + refund from SStoreResult.
                    cost = cost.saturating_add(if load.is_cold { 2200 } else { 100 });
                }
                Err(_) => return (vec![1], Vec::new(), cost), // Failure
            }
            offset += 64;
        }
        (vec![0], Vec::new(), cost) // Success
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

    fn set_transient(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static || input.len() < 64 {
            return (Vec::new(), Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let value = U256::from_be_slice(&input[32..64]);
        let contract = self.contract;
        self.ctx().journal_mut().tstore(contract, key, value);
        (Vec::new(), Vec::new(), 0)
    }

    /// ContractCall / DelegateCall / StaticCall: `addr[20] ++ value[32] ++
    /// gasLeft[8] ++ gasReq[8] ++ calldata`. Response: `status[1]`, returndata
    /// on `raw_data`.
    /// TODO(Phase 4): exact base cost (nitro `WasmCallCost`), stipend, 63/64,
    /// and status encoding — diff against writer.
    fn contract_call(&mut self, input: &[u8], scheme: CallScheme) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 68 {
            return (vec![2], Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let value = U256::from_be_slice(&input[20..52]);
        let gas_left = u64::from_be_bytes(input[52..60].try_into().unwrap());
        let gas_req = u64::from_be_bytes(input[60..68].try_into().unwrap());
        let calldata = Bytes::copy_from_slice(&input[68..]);

        let is_static = self.is_static || scheme == CallScheme::StaticCall;
        let one_64th = gas_left / 64;
        let forwarded = gas_req.min(gas_left.saturating_sub(one_64th));
        let stipend = if value > U256::ZERO && scheme == CallScheme::Call {
            2300
        } else {
            0
        };
        let call_gas = forwarded.saturating_add(stipend);

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
                    0u8
                } else {
                    2u8
                };
                let output = outcome.output().to_vec();
                // Program is charged the gas the subcall consumed. TODO(Phase 4):
                // + nitro base call cost.
                let cost = call_gas.saturating_sub(returned);
                (vec![status], output, cost)
            }
            _ => (vec![2], Vec::new(), call_gas),
        }
    }

    /// Create1 (`gas[8] ++ endowment[32] ++ code`) / Create2 (`... ++ salt[32]
    /// ++ code`). Response: `1 ++ addr[20]` on success (returndata on raw_data),
    /// else `0` with the revert data on raw_data.
    /// TODO(Phase 4): exact create gas (CreateGas + keccak word cost for CREATE2)
    /// and the precise error-response encoding — diff against writer.
    fn create(&mut self, input: &[u8], is_create2: bool) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static {
            return (vec![0u8], Vec::new(), 0);
        }
        let header = if is_create2 { 72 } else { 40 };
        if input.len() < header {
            return (vec![0u8], Vec::new(), 0);
        }
        let gas = u64::from_be_bytes(input[0..8].try_into().unwrap());
        let endowment = U256::from_be_slice(&input[8..40]);
        let scheme = if is_create2 {
            CreateScheme::Create2 {
                salt: U256::from_be_slice(&input[40..72]),
            }
        } else {
            CreateScheme::Create
        };
        let init_code = Bytes::copy_from_slice(&input[header..]);
        let inputs = CreateInputs::new(self.contract, scheme, endowment, init_code, gas);

        match drive_subframe(self.evm, FrameInput::Create(Box::new(inputs))) {
            Some(FrameResult::Create(outcome)) => {
                let returned = outcome.gas().remaining();
                let cost = gas.saturating_sub(returned);
                match (outcome.instruction_result().is_ok(), outcome.address) {
                    (true, Some(addr)) => {
                        let mut resp = Vec::with_capacity(21);
                        resp.push(1);
                        resp.extend_from_slice(addr.as_slice());
                        (resp, outcome.output().to_vec(), cost)
                    }
                    _ => (vec![0u8], outcome.output().to_vec(), cost),
                }
            }
            _ => (vec![0u8], Vec::new(), gas),
        }
    }

    /// EmitLog: `topics[4] ++ topic[32]*n ++ data`. Gas is charged Rust-side
    /// (`pay_for_evm_log`), so the wire cost is 0.
    fn emit_log(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static || input.len() < 4 {
            return (Vec::new(), Vec::new(), 0);
        }
        let num_topics = u32::from_be_bytes([input[0], input[1], input[2], input[3]]) as usize;
        let topics_end = 4 + num_topics * 32;
        if input.len() < topics_end {
            return (Vec::new(), Vec::new(), 0);
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
            touch_cost(is_cold),
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
        (hash.0.to_vec(), Vec::new(), touch_cost(is_cold))
    }

    /// AccountCode: `addr[20] ++ gas[8]`. Code goes on the `raw_data` channel.
    fn account_code(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 20 {
            return (Vec::new(), Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let (code, is_cold) = match self.ctx().journal_mut().code(addr) {
            Ok(load) => (load.data.to_vec(), load.is_cold),
            Err(_) => (Vec::new(), false),
        };
        (Vec::new(), code, touch_cost(is_cold))
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

/// EIP-2929 account-touch cost approximation (nitro `WasmAccountTouchCost`).
/// TODO(Phase 4): match nitro's exact cold/warm accounting.
fn touch_cost(is_cold: bool) -> u64 {
    if is_cold {
        2600
    } else {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::{is_stylus_code, memory_exp, memory_gas_cost};

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
