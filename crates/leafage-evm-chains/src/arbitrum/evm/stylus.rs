//! Stylus/WASM execution seam. When a CALL lands on a contract whose bytecode
//! starts with a Stylus prefix (`0xEFF0xx`), `frame_run` runs the WASM body via
//! the native runtime instead of the EVM opcode loop, then feeds a synthetic
//! `InterpreterAction::Return` back through the stock `process_next_action` so
//! journal commit/revert, `CallOutcome` wrapping, and parent gas/return wiring
//! stay identical to an EVM callee. See `docs/stylus-execution-impl-plan.md`.
//!
//! Verification status (Phase 2): dispatch, decode, compile, execute, and
//! return/revert propagation are wired and exercised end to end. Gas is
//! approximate — the exact nitro pre-charge (memory model + RecentWasms), the
//! exact hostio gas/refund (EIP-2929/2200 via nitro `Wasm*Cost`), the L1
//! block-number / paid-gas-price EvmData fields, and subcalls/logs/create
//! (hostio 4-14) are TODO(Phase 3/4) and must be diffed against a writer.

use super::ArbitrumEvm;
use crate::arbitrum::precompile::{
    ArbWasm, ArbitrumContext, HostioHandler, PreparedStylusProgram, StylusExecInput, StylusOutcome,
    StylusRuntime,
};
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::{Block, Cfg, Transaction};
use revm::handler::evm::ContextDbError;
use revm::handler::{EthFrame, FrameInitOrResult};
use revm::interpreter::{Gas, InstructionResult, InterpreterAction, InterpreterResult};
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

/// nitro `Program::initGas` (programs.go): `MinInitGas*128 + ceil(initCost*InitCostScalar*2/100)`.
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
    //    `&mut ctx` (disjoint field of the same `Evm`).
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

    // 4. Gas pre-charge (program init/cached cost). Strategy A for `cached`
    //    (on-chain flag only, no block RecentWasms LRU).
    //    TODO(Phase 4): + memory model, RecentWasms, exact CallProgram order.
    let mut gas = Gas::new(gas_limit);
    let precharge = if prepared.cached {
        cached_gas(&prepared)
    } else {
        let mut cost = init_gas(&prepared);
        if prepared.version > 1 {
            cost = cost.saturating_add(cached_gas(&prepared));
        }
        cost
    };
    if !gas.record_cost(precharge) {
        gas.spend_all();
        return finish_frame(evm, InstructionResult::OutOfGas, Bytes::new(), gas);
    }

    // 5. Assemble EvmData inputs.
    let input = StylusExecInput {
        arbos_version: prepared.arbos_version,
        block_basefee: U256::from(evm.inner.ctx.block().basefee()),
        chainid: evm.inner.ctx.cfg().chain_id(),
        block_coinbase: evm.inner.ctx.block().beneficiary(),
        block_gas_limit: evm.inner.ctx.block().gas_limit(),
        // TODO(Phase 4): should be the ArbOS-recorded L1 block number, not L2.
        block_number: evm.inner.ctx.block().number().saturating_to::<u64>(),
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

    // 6. Execute with the hostio bridge (holds `&mut ctx` for the call only).
    let supplied = gas.remaining();
    let mut call_gas = supplied;
    let mut hostio = StylusHostio {
        ctx: &mut evm.inner.ctx,
        contract,
        is_static,
        refund: 0,
    };
    let result = StylusRuntime::call_from_env(&asm, &calldata, input, &mut hostio, &mut call_gas);
    let refund = hostio.refund;
    drop(hostio);

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

/// Services Stylus hostio requests against revm state. Phase 2 implements the
/// storage set (SLOAD/SSTORE/TLOAD/TSTORE); subcalls, create, logs, account and
/// page requests return safe empty defaults for now (TODO Phase 3).
struct StylusHostio<'a, DB: Database + DatabaseRef> {
    ctx: &'a mut ArbitrumContext<DB>,
    contract: Address,
    is_static: bool,
    refund: i64,
}

impl<DB: Database + DatabaseRef> HostioHandler for StylusHostio<'_, DB> {
    fn handle(&mut self, req_type: u32, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        match req_type {
            0 => self.get_bytes32(input),
            1 => self.set_trie_slots(input),
            2 => self.get_transient(input),
            3 => self.set_transient(input),
            9 => self.emit_log(input),
            10 => self.account_balance(input),
            11 => self.account_code(input),
            12 => self.account_code_hash(input),
            13 => self.add_pages(input),
            // TODO(Phase 3): 4-6 calls, 7-8 create (G1 synchronous subcall
            //   driving); 14 capture-hostio (tracing).
            _ => (Vec::new(), Vec::new(), 0),
        }
    }
}

impl<DB: Database + DatabaseRef> StylusHostio<'_, DB> {
    fn get_bytes32(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 32 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let (value, is_cold) = match self.ctx.journal_mut().sload(self.contract, key) {
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
        let mut rest: &[u8] = if input.len() >= 8 { &input[8..] } else { &[] };
        let mut cost = 0u64;
        while rest.len() >= 64 {
            let key = U256::from_be_slice(&rest[..32]);
            let value = U256::from_be_slice(&rest[32..64]);
            match self.ctx.journal_mut().sstore(self.contract, key, value) {
                Ok(load) => {
                    // TODO(Phase 4): exact EIP-2200 cost + refund from SStoreResult.
                    cost = cost.saturating_add(if load.is_cold { 2200 } else { 100 });
                }
                Err(_) => return (vec![1], Vec::new(), cost), // Failure
            }
            rest = &rest[64..];
        }
        (vec![0], Vec::new(), cost) // Success
    }

    fn get_transient(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 32 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let value = self.ctx.journal_mut().tload(self.contract, key);
        (value.to_be_bytes::<32>().to_vec(), Vec::new(), 0)
    }

    fn set_transient(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.is_static || input.len() < 64 {
            return (Vec::new(), Vec::new(), 0);
        }
        let key = U256::from_be_slice(&input[..32]);
        let value = U256::from_be_slice(&input[32..64]);
        self.ctx.journal_mut().tstore(self.contract, key, value);
        (Vec::new(), Vec::new(), 0)
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
        self.ctx
            .journal_mut()
            .log(Log::new_unchecked(self.contract, topics, data));
        (Vec::new(), Vec::new(), 0)
    }

    fn account_balance(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 20 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let (balance, is_cold) = match self.ctx.journal_mut().load_account(addr) {
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
        let (hash, is_cold) = match self.ctx.journal_mut().code_hash(addr) {
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
        let (code, is_cold) = match self.ctx.journal_mut().code(addr) {
            Ok(load) => (load.data.to_vec(), load.is_cold),
            Err(_) => (Vec::new(), false),
        };
        (Vec::new(), code, touch_cost(is_cold))
    }

    /// AddPages: `pages[2]` (u16). Tracks open pages on the execution context.
    /// TODO(Phase 4): nitro `MemoryModel.GasCost` (exponential table + page
    /// limit); this charges a flat linear approximation.
    fn add_pages(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 2 {
            return (Vec::new(), Vec::new(), 0);
        }
        let new_pages = u16::from_be_bytes([input[0], input[1]]);
        let open = self.ctx.chain().stylus_pages_open();
        self.ctx
            .chain_mut()
            .set_stylus_pages_open(open.saturating_add(new_pages));
        let cost = (new_pages as u64).saturating_mul(1000); // ~InitialPageGas
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
    use super::is_stylus_code;

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
