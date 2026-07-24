//! Stylus/WASM execution seam. When a CALL lands on a contract whose bytecode
//! starts with a Stylus prefix (`0xEFF0xx`), `frame_run` runs the WASM body via
//! the native runtime instead of the EVM opcode loop, then feeds a synthetic
//! `InterpreterAction::Return` back through the stock `process_next_action` so
//! journal commit/revert, `CallOutcome` wrapping, and parent gas/return wiring
//! stay identical to an EVM callee.
//!
//! Verified gas-for-gas against the Arb One writer: dispatch, native-asm
//! compile, the pre-charge, read_args, storage and transient-storage hostios,
//! keccak, subcalls (including the call tree), successful returns, program
//! panics and pre-charge-only reverts.
//!
//! Not yet exercised against a writer, because Arb One's handful of Stylus
//! programs never reach these paths: create, logs, account access, storage
//! *writes* (and with them the SSTORE refund), and Stylus calling Stylus.
//!
//! Full `CaptureHostIO` opcode tracing remains deliberately absent: request 14
//! stays a no-op because the registered API only consumes logs and SSTOREs.

use super::ArbitrumEvm;
use crate::arbitrum::arbos_state::read_blockhashes_l1_block_number;
use crate::arbitrum::precompile::{
    ArbWasm, ArbitrumContext, HostioHandler, NativeAsmCacheKey, PreparedStylusProgram,
    StylusCompiler, StylusExecInput, StylusOutcome, StylusRuntime,
};
use crate::arbitrum::stylus_prefix::{
    is_stylus_classic, is_stylus_component, is_stylus_deployable, is_stylus_fragment,
    is_stylus_root,
};
use alloy::primitives::{Address, B256, Bytes, Log, U256, keccak256};
use core::marker::PhantomData;
use revm::bytecode::{Bytecode, opcode};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::cfg::gas_params::GasParams;
use revm::context_interface::context::{ContextError, take_error};
use revm::context_interface::{Block, Cfg, CreateScheme, Transaction};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::{EthFrame, EvmTr, FrameData, FrameInitOrResult, FrameResult, ItemOrResult};
use revm::inspector::{Inspector, InspectorEvmTr};
use revm::interpreter::interpreter::{EthInterpreter, ExtBytecode, Interpreter};
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::{
    CallInput, CallInputs, CallScheme, CallValue, CreateInputs, FrameInput, Gas, InputsImpl,
    InstructionResult, InterpreterAction, InterpreterResult, SharedMemory,
};
use revm::{Database, DatabaseRef};

/// Returns the ArbOS version when the current call frame contains a directly
/// executable Stylus program. CREATE initcode and fragment components always
/// stay on the EVM path.
pub(super) fn frame_stylus_version<DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
) -> Result<Option<u64>, ContextDbError<ArbitrumContext<DB>>>
where
    DB: Database + DatabaseRef,
{
    let code = {
        let frame = evm.inner.frame_stack.get();
        if !matches!(frame.data, FrameData::Call(_)) {
            return Ok(None);
        }
        let code = frame.interpreter.bytecode.original_byte_slice();
        if !(is_stylus_classic(code) || is_stylus_fragment(code) || is_stylus_root(code)) {
            return Ok(None);
        }
        code.to_vec()
    };
    let arbos_version = ArbWasm::arbos_version_for_execution(&mut evm.inner.ctx)?;
    Ok(is_stylus_deployable(&code, arbos_version).then_some(arbos_version))
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

/// ArbOS version from which `StylusParams.PageLimit` is a consensus cap on the
/// open-page total (`api.go:507-512`).
const ARBOS_PAGE_LIMIT: u64 = 59;
const ARBOS_RECENT_WASMS: u64 = 60;

#[derive(Debug, Eq, PartialEq)]
enum StylusFrameDisposition {
    Complete {
        result: InstructionResult,
        output: Bytes,
        spend_all: bool,
    },
    Infrastructure(&'static str),
}

fn stylus_frame_disposition(outcome: StylusOutcome, output: Vec<u8>) -> StylusFrameDisposition {
    match outcome {
        StylusOutcome::Success => StylusFrameDisposition::Complete {
            result: InstructionResult::Return,
            output: Bytes::from(output),
            spend_all: false,
        },
        StylusOutcome::Revert => StylusFrameDisposition::Complete {
            result: InstructionResult::Revert,
            output: Bytes::from(output),
            spend_all: false,
        },
        StylusOutcome::Failure => StylusFrameDisposition::Complete {
            result: InstructionResult::Revert,
            output: Bytes::new(),
            spend_all: false,
        },
        StylusOutcome::OutOfInk => StylusFrameDisposition::Complete {
            result: InstructionResult::OutOfGas,
            output: Bytes::new(),
            spend_all: true,
        },
        // Nitro maps userOutOfStack to vm.ErrDepth. In revm that frame result
        // is CallTooDeep; it is not an EVM operand-stack overflow.
        StylusOutcome::OutOfStack => StylusFrameDisposition::Complete {
            result: InstructionResult::CallTooDeep,
            output: Bytes::new(),
            spend_all: true,
        },
        StylusOutcome::NativeStackOverflow => StylusFrameDisposition::Infrastructure(
            "Stylus native stack overflow during off-chain execution",
        ),
    }
}

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
pub(super) fn run_stylus_frame<D, DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
    arbos_version: u64,
) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>>
where
    D: FrameDriver<DB, I>,
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

    // 2. Read Programs state. An inactive, stale, or expired program is an
    //    exceptional child failure in Nitro: it rolls back and consumes all
    //    forwarded gas. Concrete database errors propagate to the handler.
    let timestamp = evm.inner.ctx.block().timestamp().saturating_to::<u64>();
    let prepared = match ArbWasm::prepare_stylus_program(
        &mut evm.inner.ctx,
        code_hash,
        timestamp,
        arbos_version,
    )? {
        Some(prepared) => prepared,
        None => {
            let mut gas = Gas::new(gas_limit);
            gas.spend_all();
            return finish_frame(evm, InstructionResult::NotActivated, Bytes::new(), gas);
        }
    };
    if prepared.module_hash == B256::ZERO {
        return Err(ContextError::Custom(
            "active Stylus program has no module hash".to_owned(),
        ));
    }

    // 3. Gas pre-charge, mirroring nitro `CallProgram` order: memory-init cost
    //    for the program footprint, then program init/cached cost.
    //
    //    From ArbOS 60 every active call inserts its code hash into RecentWasms
    //    before the gas gate. The LRU is not journaled, so even an OOG/reverted
    //    call warms a later call in the same transaction. Cross-transaction
    //    block replay requires a request-level seed and remains separate.
    let recent_hit = prepared.arbos_version >= ARBOS_RECENT_WASMS
        && evm
            .inner
            .ctx
            .chain_mut()
            .insert_recent_wasm(code_hash, prepared.block_cache_size);
    let cached_for_gas = prepared.cached || recent_hit;
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
    if cached_for_gas || prepared.version > 1 {
        precharge = precharge.saturating_add(cached_gas(&prepared));
    }
    if !cached_for_gas {
        precharge = precharge.saturating_add(init_gas(&prepared));
    }
    // Consensus page cap: nitro saturates callCost so the burn below fails,
    // which surfaces as out-of-gas rather than a revert (`api.go:507-512`). The
    // node-level MaxOpenPages cap is deliberately not mirrored — it is policy,
    // not consensus, and its default equals PageLimit anyway.
    let new_open = open.saturating_add(prepared.footprint);
    let over_page_limit = prepared.arbos_version >= ARBOS_PAGE_LIMIT
        && prepared.page_limit > 0
        && new_open > prepared.page_limit;
    if over_page_limit || !gas.record_cost(precharge) {
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

    // 4. Native asm: lookup happens only after the consensus pre-charge and
    // page-limit gate. A hit avoids decoding root fragments altogether; a miss
    // decodes without charging or warming addresses, then enters the cache's
    // singleflight compile path.
    let cache_key = NativeAsmCacheKey::native(
        prepared.module_hash,
        prepared.version,
        StylusCompiler::Singlepass,
        false,
    );
    let asm = match StylusRuntime::cached_asm_from_env(&cache_key) {
        Ok(Some(asm)) => asm,
        Ok(None) => {
            let wasm = match ArbWasm::decode_stylus_wasm_for_execution(
                &mut evm.inner.ctx,
                &code,
                prepared.max_wasm_size,
                prepared.arbos_version,
            ) {
                Ok(Some(wasm)) => wasm,
                Ok(None) => {
                    evm.inner.ctx.chain_mut().set_stylus_pages_open(open);
                    return Err(ContextError::Custom(
                        "active Stylus program has invalid execution code".to_owned(),
                    ));
                }
                Err(error) => {
                    evm.inner.ctx.chain_mut().set_stylus_pages_open(open);
                    return Err(error.into());
                }
            };
            match StylusRuntime::compile_cached_from_env(cache_key, &wasm) {
                Ok(asm) => asm,
                Err(error) => {
                    evm.inner.ctx.chain_mut().set_stylus_pages_open(open);
                    return Err(ContextError::Custom(error.message()));
                }
            }
        }
        Err(error) => {
            evm.inner.ctx.chain_mut().set_stylus_pages_open(open);
            return Err(ContextError::Custom(error.message()));
        }
    };

    // `frame_init` has already counted this non-delegate frame. Nitro reports
    // reentrancy when the same acting address has more than one open span.
    let reentrant = evm.inner.ctx.chain().contract_is_reentrant(contract);

    // EvmData.block_number is the ArbOS-recorded L1 block number (what the
    // NUMBER opcode returns on Arbitrum, per PR #184's BLOCKHASH work). A
    // database failure aborts execution instead of substituting the L2 number.
    let l1_block_number =
        read_blockhashes_l1_block_number(evm.inner.ctx.db_mut()).map_err(ContextError::Db)?;

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
        // Left false on purpose: it only makes the runtime emit CaptureHostIO,
        // which nothing here consumes. See the hostio dispatch.
        tracing: false,
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
    let mut hostio = StylusHostio::<D, _, _> {
        evm,
        driver: PhantomData,
        contract,
        is_static,
        delegate_caller: caller,
        delegate_value: value,
        free_pages: prepared.free_pages,
        page_gas: prepared.page_gas,
        page_limit: prepared.page_limit,
        gas_params,
        max_code_size,
        arbos_version: prepared.arbos_version,
        refund: 0,
        fatal_error: None,
    };
    let result =
        StylusRuntime::call_from_env(asm.as_ref(), &calldata, input, &mut hostio, &mut call_gas);
    let refund = hostio.refund;
    let fatal_error = hostio.fatal_error.take();
    // Release the footprint page reservation (nitro's deferred SetStylusPagesOpen);
    // the high-water `ever` set during the call is retained.
    evm.inner.ctx.chain_mut().set_stylus_pages_open(open);
    if let Some(error) = fatal_error {
        return Err(error);
    }

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => return Err(ContextError::Custom(error.message())),
    };

    // 7. Thread gas + refund back onto the frame result.
    let wasm_used = supplied.saturating_sub(call_gas);
    let _ = gas.record_cost(wasm_used);
    gas.record_refund(refund);
    let (result, output) = match stylus_frame_disposition(outcome.outcome, outcome.output) {
        StylusFrameDisposition::Complete {
            result,
            output,
            spend_all,
        } => {
            if spend_all {
                gas.spend_all();
            }
            (result, output)
        }
        StylusFrameDisposition::Infrastructure(message) => {
            return Err(ContextError::Custom(message.to_owned()));
        }
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
    process_next_action(context, frame, action).inspect(|item| {
        if item.is_result() {
            frame.set_finished(true);
        }
    })
}

/// Processes a frame action with Nitro's CREATE exception to EIP-3541. The
/// stock revm path remains responsible for size checks, code-deposit gas,
/// checkpoint handling, and result construction; only its optional EIP-3541
/// flag is changed for the duration of this call.
pub(super) fn process_next_action<DB: Database>(
    context: &mut ArbitrumContext<DB>,
    frame: &mut EthFrame,
    action: InterpreterAction,
) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
    let create_output = match (&frame.data, &action) {
        (FrameData::Create(_), InterpreterAction::Return(result))
            if result.result.is_ok() && result.output.first() == Some(&0xef) =>
        {
            Some(result.output.as_ref())
        }
        _ => None,
    };
    let Some(output) = create_output else {
        return frame.process_next_action(context, action);
    };

    let arbos_version = ArbWasm::arbos_version_for_execution(context)?;
    let previous = context.cfg.disable_eip3541;
    context.cfg.disable_eip3541 = is_stylus_component(output, arbos_version);
    let result = frame.process_next_action(context, action);
    context.cfg.disable_eip3541 = previous;
    result
}

/// Which pair of revm entry points a Stylus frame uses to drive its children.
///
/// revm runs traced and untraced execution through different calls —
/// `inspect_frame_init`/`inspect_frame_run` versus `frame_init`/`frame_run`,
/// with `frame_return_result` shared — and a Stylus frame drives its subtree by
/// hand rather than yielding to the run loop, so it has to pick the matching
/// pair itself. Going through the plain pair while traced is what made a Stylus
/// program's subcalls vanish from the trace: nitro reports the full tree, we
/// reported only the top frame.
///
/// The `Inspector` bound lives on the `Traced` impl instead of the trait, so the
/// untraced path never acquires it and `ArbitrumEvm`'s own bounds are untouched.
pub(super) trait FrameDriver<DB: Database + DatabaseRef, I> {
    fn init(
        evm: &mut ArbitrumEvm<DB, I>,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<ArbitrumContext<DB>>>;

    fn run(
        evm: &mut ArbitrumEvm<DB, I>,
    ) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>>;

    fn record_log(_evm: &mut ArbitrumEvm<DB, I>, _log: Log) {}

    fn start_sstore_step(
        _evm: &mut ArbitrumEvm<DB, I>,
        _contract: Address,
        _key: U256,
        _value: U256,
    ) -> Option<Interpreter<EthInterpreter>> {
        None
    }

    fn end_sstore_step(
        _evm: &mut ArbitrumEvm<DB, I>,
        _interpreter: &mut Option<Interpreter<EthInterpreter>>,
    ) {
    }
}

/// Untraced execution.
pub(super) struct Plain;

impl<DB, I> FrameDriver<DB, I> for Plain
where
    DB: Database + DatabaseRef,
{
    fn init(
        evm: &mut ArbitrumEvm<DB, I>,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
        evm.frame_init(frame_init)
    }

    fn run(
        evm: &mut ArbitrumEvm<DB, I>,
    ) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
        evm.frame_run()
    }
}

/// Traced execution — children run through the inspected loop so they show up
/// in the trace.
pub(super) struct Traced;

impl<DB, I> FrameDriver<DB, I> for Traced
where
    DB: Database + DatabaseRef,
    I: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    fn init(
        evm: &mut ArbitrumEvm<DB, I>,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
        evm.inspect_frame_init(frame_init)
    }

    fn run(
        evm: &mut ArbitrumEvm<DB, I>,
    ) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
        evm.inspect_frame_run()
    }

    fn record_log(evm: &mut ArbitrumEvm<DB, I>, log: Log) {
        evm.inner.inspector.log(&mut evm.inner.ctx, log);
    }

    fn start_sstore_step(
        evm: &mut ArbitrumEvm<DB, I>,
        contract: Address,
        key: U256,
        value: U256,
    ) -> Option<Interpreter<EthInterpreter>> {
        let bytecode = Bytecode::new_raw(Bytes::from(vec![opcode::SSTORE, opcode::STOP]));
        let input = InputsImpl {
            target_address: contract,
            bytecode_address: Some(contract),
            ..Default::default()
        };
        let mut interpreter = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(bytecode),
            input,
            false,
            (*evm.inner.ctx.cfg().spec()).into(),
            u64::MAX,
        );
        // SSTORE pops key first and value second, so key is the stack top.
        let _ = interpreter.stack.push(value);
        let _ = interpreter.stack.push(key);
        evm.inner
            .inspector
            .step(&mut interpreter, &mut evm.inner.ctx);
        Some(interpreter)
    }

    fn end_sstore_step(
        evm: &mut ArbitrumEvm<DB, I>,
        interpreter: &mut Option<Interpreter<EthInterpreter>>,
    ) {
        if let Some(interpreter) = interpreter {
            evm.inner
                .inspector
                .step_end(interpreter, &mut evm.inner.ctx);
        }
    }
}

/// Drives a Stylus subcall's child frame subtree to completion synchronously
/// (WASM can't yield to revm's run loop — the G1 problem), returning the direct
/// child's `CallOutcome`. The child is pushed above the Stylus parent; grandchild
/// frames resolve via the stock `frame_return_result`, but the DIRECT child's
/// result is captured + popped manually so it never reaches the Stylus parent's
/// `return_result` (which would corrupt the parent's interpreter gas/stack).
fn drive_subframe<D, DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
    frame_input: FrameInput,
) -> Result<FrameResult, ContextDbError<ArbitrumContext<DB>>>
where
    D: FrameDriver<DB, I>,
    DB: Database + DatabaseRef,
{
    let parent_index = evm.inner.frame_stack.index().ok_or_else(|| {
        ContextError::Custom("Stylus subframe started without a parent frame".to_owned())
    })?;
    // Build the child FrameInit from the current (Stylus parent) frame. This
    // reserves a child context on the parent's shared memory.
    let child_init = {
        let parent = evm.inner.frame_stack.get();
        FrameInit {
            depth: parent.depth + 1,
            memory: parent.interpreter.memory.new_child_context(),
            frame_input,
        }
    };
    // The Stylus parent's index; the direct child lands one above it.
    let child_index = parent_index + 1;

    let outcome = run_subframe::<D, _, _>(evm, child_init, child_index);

    // revm releases the child context on its normal return path, which popping
    // the child by hand bypasses. Without this the *second* subcall from the
    // same Stylus frame hits `new_child_context was already called without
    // freeing child context` and panics. Guarded on the stack having unwound
    // back to the parent, so an error path that left a frame behind does not
    // free the wrong frame's memory.
    if evm.inner.frame_stack.index() == Some(parent_index) {
        evm.inner
            .frame_stack
            .get()
            .interpreter
            .memory
            .free_child_context();
    }
    outcome
}

fn run_subframe<D, DB, I>(
    evm: &mut ArbitrumEvm<DB, I>,
    child_init: FrameInit,
    child_index: usize,
) -> Result<FrameResult, ContextDbError<ArbitrumContext<DB>>>
where
    D: FrameDriver<DB, I>,
    DB: Database + DatabaseRef,
{
    match D::init(evm, child_init)? {
        // Resolved without pushing a frame (precompile / empty code).
        ItemOrResult::Result(frame_result) => return Ok(frame_result),
        ItemOrResult::Item(_frame) => {}
    }

    loop {
        match D::run(evm)? {
            ItemOrResult::Item(init) => match D::init(evm, init)? {
                ItemOrResult::Item(_frame) => continue,
                // This result belongs to the newly requested grandchild, not
                // to the frame currently at `child_index`.
                ItemOrResult::Result(frame_result) => {
                    if evm.frame_return_result(frame_result)?.is_some() {
                        return Err(ContextError::Custom(
                            "Stylus immediate subframe result escaped its parent".to_owned(),
                        ));
                    }
                }
            },
            ItemOrResult::Result(frame_result) => {
                let current_index = evm.inner.frame_stack.index().ok_or_else(|| {
                    ContextError::Custom("Stylus subframe stack became empty".to_owned())
                })?;
                if current_index < child_index {
                    return Err(ContextError::Custom(
                        "Stylus subframe unwound past its direct child".to_owned(),
                    ));
                }

                // Capture the direct child's result without inserting it into
                // the Stylus parent's EVM interpreter.
                if current_index == child_index {
                    if !evm.inner.frame_stack.get().is_finished() {
                        return Err(ContextError::Custom(
                            "Stylus direct child returned before finishing".to_owned(),
                        ));
                    }
                    evm.pop_frame();
                    // `EthFrame::return_result` normally drains errors after a
                    // pop; direct-child capture deliberately bypasses it.
                    take_error::<ContextDbError<ArbitrumContext<DB>>, _>(evm.inner.ctx.error())?;
                    return Ok(frame_result);
                }

                // Deeper frame: pop it and resume its EVM parent.
                if evm.frame_return_result(frame_result)?.is_some() {
                    return Err(ContextError::Custom(
                        "Stylus nested subframe escaped the direct child".to_owned(),
                    ));
                }
            }
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

/// revm returns unused child gas only for success and revert. Exceptional
/// halts consume the entire child allowance even if `Gas` still carries a
/// non-zero remainder.
fn returnable_gas(result: InstructionResult, gas: &Gas) -> u64 {
    if result.is_ok_or_revert() {
        gas.remaining()
    } else {
        0
    }
}

/// Services a Stylus program's hostio requests against revm state. Holds
/// `&mut ArbitrumEvm` so subcalls can drive child frames on the shared stack.
struct StylusHostio<'a, D, DB: Database + DatabaseRef, I> {
    evm: &'a mut ArbitrumEvm<DB, I>,
    /// Selects the frame entry points subcalls drive children through.
    driver: PhantomData<D>,
    contract: Address,
    is_static: bool,
    /// The Stylus frame's caller/value, used for DELEGATECALL context.
    delegate_caller: Address,
    delegate_value: U256,
    /// StylusParams memory model inputs, for AddPages.
    free_pages: u16,
    page_gas: u16,
    page_limit: u16,
    /// revm gas schedule, for exact SLOAD/SSTORE/account-touch cost + refund.
    gas_params: GasParams,
    /// Scales the EXTCODESIZE component of `AccountCode`.
    max_code_size: u64,
    /// Gates `setTrieSlots`' OutOfGas status.
    arbos_version: u64,
    refund: i64,
    /// First execution error raised while synchronously driving an EVM child.
    /// The native ABI cannot return Rust errors, so hostio latches it until the
    /// native call unwinds.
    fatal_error: Option<ContextDbError<ArbitrumContext<DB>>>,
}

impl<D: FrameDriver<DB, I>, DB: Database + DatabaseRef, I> HostioHandler
    for StylusHostio<'_, D, DB, I>
{
    fn handle(&mut self, req_type: u32, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if self.fatal_error.is_some() {
            return (Vec::new(), Vec::new(), 0);
        }
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
            // 14 is CaptureHostIO, which reports every hostio to an
            // opcode-level tracer — nitro turns those into synthetic EVM
            // opcodes for structLogger. leafage exposes no opcode-level trace
            // (`debug_traceCall` and `trace_call` are not served; pre_traceMany
            // and simulateTransactions are call-level, and their subcall frames
            // come from the inspected frame driver, not from here), so nothing
            // would read the output. `EvmData.tracing` is left false, which
            // stops the runtime from ever sending this request — and also skips
            // its ink sampling and calldata clones. This arm is just a guard.
            _ => (Vec::new(), Vec::new(), 0),
        }
    }
}

impl<D: FrameDriver<DB, I>, DB: Database + DatabaseRef, I> StylusHostio<'_, D, DB, I> {
    fn latch_fatal_error(&mut self, error: ContextDbError<ArbitrumContext<DB>>) {
        if self.fatal_error.is_none() {
            self.fatal_error = Some(error);
        }
    }

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
        let account = self
            .ctx()
            .journal_mut()
            .load_account(target)
            .map(|load| (load.is_cold, load.data.is_empty()));
        let (is_cold, is_empty) = match account {
            Ok(account) => account,
            Err(error) => {
                self.latch_fatal_error(ContextError::Db(error));
                return None;
            }
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
        let slot = self
            .ctx()
            .journal_mut()
            .sload(contract, key)
            .map(|load| (load.data, load.is_cold));
        let (value, is_cold) = match slot {
            Ok(slot) => slot,
            Err(error) => {
                let cost = self.storage_load_cost(false);
                self.latch_fatal_error(ContextError::Db(error));
                return (vec![0u8; 32], Vec::new(), cost);
            }
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
            let mut trace_step = D::start_sstore_step(self.evm, contract, key, value);
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
                        D::end_sstore_step(self.evm, &mut trace_step);
                        remaining = 0;
                        out_of_gas = true;
                        break;
                    }
                    self.ctx().journal_mut().checkpoint_commit();
                    self.refund += self.gas_params.sstore_refund(true, &load.data);
                    remaining -= slot_cost;
                    D::end_sstore_step(self.evm, &mut trace_step);
                }
                Err(error) => {
                    self.ctx().journal_mut().checkpoint_revert(checkpoint);
                    D::end_sstore_step(self.evm, &mut trace_step);
                    self.latch_fatal_error(ContextError::Db(error));
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
    /// Base cost, stipend, 63/64 split, child gas, and status follow Nitro's
    /// `doCall` implementation.
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
            CallScheme::CallCode => (
                self.contract,
                addr,
                self.contract,
                CallValue::Transfer(value),
            ),
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

        match drive_subframe::<D, _, _>(self.evm, FrameInput::Call(Box::new(inputs))) {
            Ok(FrameResult::Call(outcome)) => {
                let instruction_result = *outcome.instruction_result();
                let outcome_gas = outcome.gas();
                let returned = returnable_gas(instruction_result, &outcome_gas);
                if instruction_result.is_ok() {
                    self.refund = self.refund.saturating_add(outcome_gas.refunded());
                }
                let status = if instruction_result.is_ok() {
                    CALL_STATUS_SUCCESS
                } else {
                    CALL_STATUS_FAILURE
                };
                let output = outcome.output().to_vec();
                let cost = base_cost.saturating_add(call_gas.saturating_sub(returned));
                (vec![status], output, cost)
            }
            Ok(_) => {
                self.latch_fatal_error(ContextError::Custom(
                    "CALL hostio received a non-call frame result".to_owned(),
                ));
                (
                    vec![CALL_STATUS_FAILURE],
                    Vec::new(),
                    base_cost.saturating_add(call_gas),
                )
            }
            Err(error) => {
                self.latch_fatal_error(error);
                (
                    vec![CALL_STATUS_FAILURE],
                    Vec::new(),
                    base_cost.saturating_add(call_gas),
                )
            }
        }
    }

    /// Create1 (`gas[8] ++ endowment[32] ++ code`) / Create2 (`... ++ salt[32]
    /// ++ code`). Response: `1 ++ addr[20]` on success (returndata on raw_data),
    /// else `0` with the revert data on raw_data.
    /// Gas and response encoding follow Nitro's `create` implementation.
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

        match drive_subframe::<D, _, _>(self.evm, FrameInput::Create(Box::new(inputs))) {
            Ok(FrameResult::Create(outcome)) => {
                let instruction_result = *outcome.instruction_result();
                let returned = returnable_gas(instruction_result, outcome.gas());
                if instruction_result.is_ok() {
                    self.refund = self.refund.saturating_add(outcome.gas().refunded());
                }
                // The withheld 64th goes back to the caller (`api.go:260`).
                let cost = gas.saturating_sub(returned.saturating_add(one_64th));
                // A failed deploy still answers "success" carrying the zero
                // address, exactly as the EVM's CREATE pushes 0 on failure.
                let address = if instruction_result.is_ok() {
                    outcome.address.unwrap_or(Address::ZERO)
                } else {
                    Address::ZERO
                };
                let mut resp = Vec::with_capacity(21);
                resp.push(1);
                resp.extend_from_slice(address.as_slice());
                // Return data survives a revert only (nitro `api.go:257-258`).
                let output = if instruction_result == InstructionResult::Revert {
                    outcome.output().to_vec()
                } else {
                    Vec::new()
                };
                (resp, output, cost)
            }
            Ok(_) => {
                self.latch_fatal_error(ContextError::Custom(
                    "CREATE hostio received a non-create frame result".to_owned(),
                ));
                (create_error(b"create frame failed"), Vec::new(), gas)
            }
            Err(error) => {
                self.latch_fatal_error(error);
                (create_error(b"create frame failed"), Vec::new(), gas)
            }
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
        let log = Log::new_unchecked(contract, topics, data);
        self.ctx().journal_mut().log(log.clone());
        D::record_log(self.evm, log);
        (Vec::new(), Vec::new(), 0)
    }

    fn account_balance(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 20 {
            return (vec![0u8; 32], Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let account = self
            .ctx()
            .journal_mut()
            .load_account(addr)
            .map(|load| (load.data.info.balance, load.is_cold));
        let (balance, is_cold) = match account {
            Ok(account) => account,
            Err(error) => {
                let cost = self.account_touch_cost(false);
                self.latch_fatal_error(ContextError::Db(error));
                return (vec![0u8; 32], Vec::new(), cost);
            }
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
        let code_hash = self
            .ctx()
            .journal_mut()
            .code_hash(addr)
            .map(|load| (load.data, load.is_cold));
        let (hash, is_cold) = match code_hash {
            Ok(code_hash) => code_hash,
            Err(error) => {
                let cost = self.account_touch_cost(false);
                self.latch_fatal_error(ContextError::Db(error));
                return (vec![0u8; 32], Vec::new(), cost);
            }
        };
        (
            hash.0.to_vec(),
            Vec::new(),
            self.account_touch_cost(is_cold),
        )
    }

    /// AccountCode: `addr[20] ++ gasLeft[8]`. Code goes on the `raw_data`
    /// channel; the response stays empty.
    fn account_code(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 28 {
            return (Vec::new(), Vec::new(), 0);
        }
        let addr = Address::from_slice(&input[..20]);
        let gas_left = u64::from_be_bytes(input[20..28].try_into().unwrap());
        let account_code = self
            .ctx()
            .journal_mut()
            .code(addr)
            .map(|load| (load.data.to_vec(), load.is_cold));
        let (code, is_cold) = match account_code {
            Ok(account_code) => account_code,
            Err(error) => {
                let cost = self.account_code_cost(false);
                self.latch_fatal_error(ContextError::Db(error));
                return (Vec::new(), Vec::new(), cost);
            }
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
    fn add_pages(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64) {
        if input.len() < 2 {
            return (Vec::new(), Vec::new(), 0);
        }
        let new_pages = u16::from_be_bytes([input[0], input[1]]);
        let open = self.ctx().chain().stylus_pages_open();
        let ever = self.ctx().chain().stylus_pages_ever();
        // nitro opens the pages before testing the cap (`api.go:305-315`).
        let new_open = open.saturating_add(new_pages);
        self.ctx().chain_mut().set_stylus_pages_open(new_open);
        if self.arbos_version >= ARBOS_PAGE_LIMIT
            && self.page_limit > 0
            && new_open > self.page_limit
        {
            // Priced at MaxUint64 so the runtime runs out of ink.
            return (Vec::new(), Vec::new(), u64::MAX);
        }
        let cost = memory_gas_cost(new_pages, open, ever, self.free_pages, self.page_gas);
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
    use revm::database::{EmptyDB, in_memory_db::CacheDB};
    use revm::interpreter::interpreter_types::Jumps;
    use revm::interpreter::{CallOutcome, CreateOutcome};

    type TestDb = CacheDB<EmptyDB>;

    #[derive(Debug, Eq, PartialEq)]
    struct ExpectedDbError;

    impl core::fmt::Display for ExpectedDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("expected database error")
        }
    }

    impl std::error::Error for ExpectedDbError {}
    impl revm::database_interface::DBErrorMarker for ExpectedDbError {}

    struct FailingStorageDb;

    impl Database for FailingStorageDb {
        type Error = ExpectedDbError;

        fn basic(&mut self, _: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok(Some(revm::state::AccountInfo::default()))
        }

        fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage(&mut self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(ExpectedDbError)
        }

        fn block_hash(&mut self, _: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    impl DatabaseRef for FailingStorageDb {
        type Error = ExpectedDbError;

        fn basic_ref(&self, _: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok(Some(revm::state::AccountInfo::default()))
        }

        fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(ExpectedDbError)
        }

        fn block_hash_ref(&self, _: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    fn test_evm() -> ArbitrumEvm<TestDb, ()> {
        test_evm_with_inspector(())
    }

    #[test]
    fn stylus_outcomes_preserve_nitro_error_semantics() {
        let data = vec![0xaa, 0xbb];
        assert_eq!(
            stylus_frame_disposition(StylusOutcome::Success, data.clone()),
            StylusFrameDisposition::Complete {
                result: InstructionResult::Return,
                output: Bytes::from(data.clone()),
                spend_all: false,
            }
        );
        assert_eq!(
            stylus_frame_disposition(StylusOutcome::Revert, data.clone()),
            StylusFrameDisposition::Complete {
                result: InstructionResult::Revert,
                output: Bytes::from(data),
                spend_all: false,
            }
        );
        assert_eq!(
            stylus_frame_disposition(StylusOutcome::Failure, vec![0xff]),
            StylusFrameDisposition::Complete {
                result: InstructionResult::Revert,
                output: Bytes::new(),
                spend_all: false,
            }
        );
        assert_eq!(
            stylus_frame_disposition(StylusOutcome::OutOfInk, Vec::new()),
            StylusFrameDisposition::Complete {
                result: InstructionResult::OutOfGas,
                output: Bytes::new(),
                spend_all: true,
            }
        );
        assert_eq!(
            stylus_frame_disposition(StylusOutcome::OutOfStack, Vec::new()),
            StylusFrameDisposition::Complete {
                result: InstructionResult::CallTooDeep,
                output: Bytes::new(),
                spend_all: true,
            }
        );
        assert!(matches!(
            stylus_frame_disposition(StylusOutcome::NativeStackOverflow, Vec::new()),
            StylusFrameDisposition::Infrastructure(_)
        ));
    }

    fn test_evm_with_inspector<I>(inspector: I) -> ArbitrumEvm<TestDb, I> {
        ArbitrumEvm::new(
            BlockEnv::default(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            CacheDB::new(EmptyDB::default()),
            inspector,
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        )
    }

    fn failing_storage_evm() -> ArbitrumEvm<FailingStorageDb, ()> {
        ArbitrumEvm::new(
            BlockEnv::default(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            FailingStorageDb,
            (),
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        )
    }

    fn failing_storage_hostio(
        evm: &mut ArbitrumEvm<FailingStorageDb, ()>,
    ) -> StylusHostio<'_, Plain, FailingStorageDb, ()> {
        let gas_params = evm.inner.ctx.cfg().gas_params().clone();
        evm.inner
            .ctx
            .journal_mut()
            .load_account(Address::ZERO)
            .expect("load contract account");
        StylusHostio {
            evm,
            driver: PhantomData,
            contract: Address::ZERO,
            is_static: false,
            delegate_caller: Address::ZERO,
            delegate_value: U256::ZERO,
            free_pages: 2,
            page_gas: 1000,
            page_limit: 128,
            gas_params,
            max_code_size: DEFAULT_MAX_CODE_SIZE,
            arbos_version: 61,
            refund: 0,
            fatal_error: None,
        }
    }

    fn test_hostio(
        evm: &mut ArbitrumEvm<TestDb, ()>,
        is_static: bool,
    ) -> StylusHostio<'_, Plain, TestDb, ()> {
        test_hostio_with_driver(evm, is_static)
    }

    fn test_hostio_with_driver<D, I>(
        evm: &mut ArbitrumEvm<TestDb, I>,
        is_static: bool,
    ) -> StylusHostio<'_, D, TestDb, I> {
        let gas_params = evm.inner.ctx.cfg().gas_params().clone();
        // revm expects an account to be journal-loaded before its storage is
        // touched, which in production the frame setup has already done.
        let _ = evm.inner.ctx.journal_mut().load_account(Address::ZERO);
        StylusHostio {
            evm,
            driver: PhantomData,
            contract: Address::ZERO,
            is_static,
            delegate_caller: Address::ZERO,
            delegate_value: U256::ZERO,
            free_pages: 2,
            page_gas: 1000,
            page_limit: 128,
            gas_params,
            max_code_size: DEFAULT_MAX_CODE_SIZE,
            arbos_version: 61,
            refund: 0,
            fatal_error: None,
        }
    }

    #[derive(Default)]
    struct RecordingInspector {
        steps: Vec<(u8, Address, Vec<U256>)>,
        step_ends: usize,
        logs: Vec<Log>,
    }

    impl Inspector<ArbitrumContext<TestDb>, EthInterpreter> for RecordingInspector {
        fn step(
            &mut self,
            interpreter: &mut Interpreter<EthInterpreter>,
            _context: &mut ArbitrumContext<TestDb>,
        ) {
            self.steps.push((
                interpreter.bytecode.opcode(),
                interpreter.input.target_address,
                interpreter.stack.data().clone(),
            ));
        }

        fn step_end(
            &mut self,
            _interpreter: &mut Interpreter<EthInterpreter>,
            _context: &mut ArbitrumContext<TestDb>,
        ) {
            self.step_ends += 1;
        }

        fn log(&mut self, _context: &mut ArbitrumContext<TestDb>, log: Log) {
            self.logs.push(log);
        }
    }

    fn push_parent(evm: &mut ArbitrumEvm<TestDb, ()>, address: Address) {
        let caller = Address::with_last_byte(1);
        evm.inner
            .ctx
            .journal_mut()
            .load_account(caller)
            .expect("load parent caller");
        evm.inner
            .ctx
            .journal_mut()
            .load_account(address)
            .expect("load parent target");
        let bytecode = Bytecode::new_raw(Bytes::from_static(&[0x00]));
        let code_hash = bytecode.hash_slow();
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 1_000_000,
            bytecode_address: address,
            known_bytecode: Some((code_hash, bytecode)),
            target_address: address,
            caller,
            value: CallValue::Apparent(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
        };
        assert!(matches!(
            evm.frame_init(FrameInit {
                depth: 0,
                memory: SharedMemory::new(),
                frame_input: FrameInput::Call(Box::new(inputs)),
            })
            .expect("initialize parent frame"),
            ItemOrResult::Item(_)
        ));
    }

    fn known_call_inputs(
        target: Address,
        bytecode_address: Address,
        scheme: CallScheme,
        code: Bytes,
    ) -> CallInputs {
        let bytecode = Bytecode::new_raw(code);
        CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            bytecode_address,
            known_bytecode: Some((bytecode.hash_slow(), bytecode)),
            target_address: target,
            caller: Address::with_last_byte(1),
            value: CallValue::Apparent(U256::ZERO),
            scheme,
            is_static: false,
        }
    }

    fn child_frame_init(evm: &mut ArbitrumEvm<TestDb, ()>, frame_input: FrameInput) -> FrameInit {
        let parent = evm.inner.frame_stack.get();
        FrameInit {
            depth: parent.depth + 1,
            memory: parent.interpreter.memory.new_child_context(),
            frame_input,
        }
    }

    struct ImmediateDriver;

    impl<DB, I> FrameDriver<DB, I> for ImmediateDriver
    where
        DB: Database + DatabaseRef,
    {
        fn init(
            evm: &mut ArbitrumEvm<DB, I>,
            frame_init: FrameInit,
        ) -> Result<FrameInitResult<'_, EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
            let frame_result = match frame_init.frame_input {
                FrameInput::Call(inputs) => {
                    let marker = inputs
                        .input
                        .bytes(&evm.inner.ctx)
                        .first()
                        .copied()
                        .unwrap_or_default();
                    if marker == 0xff {
                        return Err(ContextError::Custom("synthetic child failure".to_owned()));
                    }
                    let mut gas = Gas::new(inputs.gas_limit);
                    let (result, output) = match marker {
                        0 => {
                            assert!(gas.record_cost(1_234));
                            gas.record_refund(77);
                            (InstructionResult::Return, Bytes::from_static(b"ok"))
                        }
                        1 => {
                            assert!(gas.record_cost(1_234));
                            gas.record_refund(88);
                            (InstructionResult::Revert, Bytes::from_static(b"revert"))
                        }
                        2 => {
                            gas.record_refund(99);
                            (InstructionResult::OutOfGas, Bytes::new())
                        }
                        _ => unreachable!("unknown call marker"),
                    };
                    FrameResult::Call(CallOutcome::new(
                        InterpreterResult::new(result, output, gas),
                        0..0,
                    ))
                }
                FrameInput::Create(inputs) => {
                    let marker = inputs.init_code().first().copied().unwrap_or_default();
                    if marker == 0xff {
                        return Err(ContextError::Custom("synthetic child failure".to_owned()));
                    }
                    let mut gas = Gas::new(inputs.gas_limit());
                    let (result, output) = match marker {
                        0 => {
                            assert!(gas.record_cost(1_234));
                            gas.record_refund(77);
                            (InstructionResult::Return, Bytes::new())
                        }
                        1 => {
                            assert!(gas.record_cost(1_234));
                            gas.record_refund(88);
                            (InstructionResult::Revert, Bytes::from_static(b"revert"))
                        }
                        2 => {
                            gas.record_refund(99);
                            (InstructionResult::OutOfGas, Bytes::new())
                        }
                        _ => unreachable!("unknown create marker"),
                    };
                    FrameResult::Create(CreateOutcome::new(
                        InterpreterResult::new(result, output, gas),
                        Some(Address::with_last_byte(0x42)),
                    ))
                }
                FrameInput::Empty => unreachable!("empty test frame"),
            };
            Ok(ItemOrResult::Result(frame_result))
        }

        fn run(
            _evm: &mut ArbitrumEvm<DB, I>,
        ) -> Result<FrameInitOrResult<EthFrame>, ContextDbError<ArbitrumContext<DB>>> {
            unreachable!("immediate frames never run")
        }
    }

    fn call_request(marker: u8) -> Vec<u8> {
        let mut input = Address::with_last_byte(0x99).to_vec();
        input.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        input.extend_from_slice(&100_000u64.to_be_bytes());
        input.extend_from_slice(&50_000u64.to_be_bytes());
        input.push(marker);
        input
    }

    fn create_request(marker: u8) -> Vec<u8> {
        let mut input = 100_000u64.to_be_bytes().to_vec();
        input.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        input.push(marker);
        input
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
    fn traced_hostio_reports_each_log_and_sstore_once() {
        let topic = B256::from([0x11; 32]);
        let mut log_input = 1u32.to_be_bytes().to_vec();
        log_input.extend_from_slice(topic.as_slice());
        log_input.extend_from_slice(b"payload");

        let key = U256::from(7);
        let value = U256::from(9);
        let mut slot_input = 1_000_000u64.to_be_bytes().to_vec();
        slot_input.extend_from_slice(&key.to_be_bytes::<32>());
        slot_input.extend_from_slice(&value.to_be_bytes::<32>());

        let mut evm = test_evm_with_inspector(RecordingInspector::default());
        {
            let mut hostio = test_hostio_with_driver::<Traced, _>(&mut evm, false);
            assert!(hostio.emit_log(&log_input).0.is_empty());
            assert_eq!(
                hostio.set_trie_slots(&slot_input).0,
                vec![API_STATUS_SUCCESS]
            );
        }

        assert_eq!(evm.inner.ctx.journal().logs().len(), 1);
        assert_eq!(evm.inner.inspector.logs.len(), 1);
        assert_eq!(evm.inner.inspector.logs[0].address, Address::ZERO);
        assert_eq!(evm.inner.inspector.logs[0].topics(), &[topic]);
        assert_eq!(evm.inner.inspector.logs[0].data.data.as_ref(), b"payload");
        assert_eq!(
            evm.inner.inspector.steps,
            vec![(opcode::SSTORE, Address::ZERO, vec![value, key])]
        );
        assert_eq!(evm.inner.inspector.step_ends, 1);
    }

    #[test]
    fn traced_unaffordable_sstore_still_balances_step_hooks() {
        let mut slot_input = 10u64.to_be_bytes().to_vec();
        slot_input.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
        slot_input.extend_from_slice(&U256::from(2).to_be_bytes::<32>());
        // This second slot is not processed after the first exhausts the budget.
        slot_input.extend_from_slice(&U256::from(3).to_be_bytes::<32>());
        slot_input.extend_from_slice(&U256::from(4).to_be_bytes::<32>());

        let mut evm = test_evm_with_inspector(RecordingInspector::default());
        let response = test_hostio_with_driver::<Traced, _>(&mut evm, false)
            .set_trie_slots(&slot_input)
            .0;

        assert_eq!(response, vec![API_STATUS_OUT_OF_GAS]);
        assert_eq!(evm.inner.inspector.steps.len(), 1);
        assert_eq!(evm.inner.inspector.step_ends, 1);
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
    fn add_pages_prices_a_cap_breach_as_unaffordable() {
        // The harness carries the default 128-page consensus cap.
        let mut evm = test_evm();
        let (_, _, cost) = test_hostio(&mut evm, false).add_pages(&129u16.to_be_bytes());
        assert_eq!(cost, u64::MAX, "a breach has to be unaffordable");

        let mut evm = test_evm();
        let (_, _, cost) = test_hostio(&mut evm, false).add_pages(&10u16.to_be_bytes());
        assert_eq!(cost, memory_gas_cost(10, 0, 0, 2, 1000));
    }

    #[test]
    fn payable_calls_are_refused_in_a_static_context() {
        let mut input = Address::with_last_byte(9).to_vec();
        input.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
        input.extend_from_slice(&100_000u64.to_be_bytes()); // gasLeft
        input.extend_from_slice(&100_000u64.to_be_bytes()); // gasReq

        let mut evm = test_evm();
        let (response, _, cost) =
            test_hostio(&mut evm, true).contract_call(&input, CallScheme::Call);
        assert_eq!(response, vec![CALL_STATUS_FAILURE]);
        assert_eq!(cost, 0);
    }

    #[test]
    fn immediate_grandchild_result_resumes_the_direct_child() {
        let parent = Address::with_last_byte(0x10);
        let child = Address::with_last_byte(0x20);
        let empty_target = Address::with_last_byte(0xee);
        let mut evm = test_evm();
        push_parent(&mut evm, parent);

        // CALL an empty account (resolved by `frame_init` without a pushed
        // frame), then return 0x2a. The direct child must resume after the
        // immediate grandchild result instead of returning that result itself.
        let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x5f, 0x73];
        code.extend_from_slice(empty_target.as_slice());
        code.extend_from_slice(&[
            0x61, 0x27, 0x10, 0xf1, 0x50, 0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ]);

        for _ in 0..2 {
            let inputs = known_call_inputs(
                child,
                child,
                CallScheme::Call,
                Bytes::copy_from_slice(&code),
            );
            let result =
                drive_subframe::<Plain, _, _>(&mut evm, FrameInput::Call(Box::new(inputs)))
                    .expect("drive child frame");
            let FrameResult::Call(outcome) = result else {
                panic!("expected call outcome")
            };
            assert_eq!(*outcome.instruction_result(), InstructionResult::Return);
            assert_eq!(outcome.output().len(), 32);
            assert_eq!(outcome.output()[31], 0x2a);
            assert_eq!(evm.inner.frame_stack.index(), Some(0));
            assert_eq!(evm.ctx().chain().open_contract_frame_count(child), 0);
        }
    }

    #[test]
    fn call_gas_and_refunds_follow_revm_return_rules() {
        let parent = Address::with_last_byte(0x10);

        let mut evm = test_evm();
        push_parent(&mut evm, parent);
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);
        let (response, output, cost) = hostio.contract_call(&call_request(0), CallScheme::Call);
        assert_eq!(response, vec![CALL_STATUS_SUCCESS]);
        assert_eq!(output, b"ok");
        assert_eq!(cost, 2_600 + 1_234);
        assert_eq!(hostio.refund, 77);

        let mut evm = test_evm();
        push_parent(&mut evm, parent);
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);
        let (response, output, cost) = hostio.contract_call(&call_request(1), CallScheme::Call);
        assert_eq!(response, vec![CALL_STATUS_FAILURE]);
        assert_eq!(output, b"revert");
        assert_eq!(cost, 2_600 + 1_234);
        assert_eq!(hostio.refund, 0, "revert must not propagate refunds");

        let mut evm = test_evm();
        push_parent(&mut evm, parent);
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);
        let (response, output, cost) = hostio.contract_call(&call_request(2), CallScheme::Call);
        assert_eq!(response, vec![CALL_STATUS_FAILURE]);
        assert!(output.is_empty());
        assert_eq!(cost, 2_600 + 50_000, "exceptional halt burns child gas");
        assert_eq!(hostio.refund, 0, "exceptional halt must not refund");
    }

    #[test]
    fn create_gas_refund_and_failed_address_follow_revm_rules() {
        let parent = Address::with_last_byte(0x10);

        let mut evm = test_evm();
        push_parent(&mut evm, parent);
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);
        let (response, output, cost) = hostio.create(&create_request(0), false);
        assert_eq!(response[0], 1);
        assert_eq!(&response[1..], Address::with_last_byte(0x42).as_slice());
        assert!(output.is_empty());
        assert_eq!(cost, 32_000 + 1_234);
        assert_eq!(hostio.refund, 77);

        let mut evm = test_evm();
        push_parent(&mut evm, parent);
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);
        let (response, output, cost) = hostio.create(&create_request(1), false);
        assert_eq!(response[0], 1);
        assert_eq!(&response[1..], Address::ZERO.as_slice());
        assert_eq!(output, b"revert");
        assert_eq!(cost, 32_000 + 1_234);
        assert_eq!(hostio.refund, 0, "revert must not propagate refunds");

        let mut evm = test_evm();
        push_parent(&mut evm, parent);
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);
        let (response, output, cost) = hostio.create(&create_request(2), false);
        assert_eq!(response[0], 1);
        assert_eq!(&response[1..], Address::ZERO.as_slice());
        assert!(output.is_empty());
        assert_eq!(
            cost, 98_938,
            "exceptional halt only returns EIP-150 reserve"
        );
        assert_eq!(hostio.refund, 0, "exceptional halt must not refund");
    }

    #[test]
    fn subframe_error_is_latched_and_blocks_later_hostio_mutations() {
        let mut evm = test_evm();
        push_parent(&mut evm, Address::with_last_byte(0x10));
        let mut hostio = test_hostio_with_driver::<ImmediateDriver, _>(&mut evm, false);

        let (response, _, _) = hostio.contract_call(&call_request(0xff), CallScheme::Call);
        assert_eq!(response, vec![CALL_STATUS_FAILURE]);
        assert!(matches!(
            hostio.fatal_error,
            Some(ContextError::Custom(ref message)) if message == "synthetic child failure"
        ));

        let key = U256::from(7);
        let value = U256::from(9);
        let mut mutation = key.to_be_bytes::<32>().to_vec();
        mutation.extend_from_slice(&value.to_be_bytes::<32>());
        let response = hostio.handle(3, &mutation);
        assert_eq!(response, (Vec::new(), Vec::new(), 0));
        assert_eq!(
            hostio.evm.ctx_mut().journal_mut().tload(Address::ZERO, key),
            U256::ZERO
        );
    }

    #[test]
    fn hostio_storage_errors_keep_database_error_provenance() {
        let mut evm = failing_storage_evm();
        let mut hostio = failing_storage_hostio(&mut evm);
        let placeholder_cost = hostio.storage_load_cost(false);
        assert_eq!(
            hostio.get_bytes32(&U256::from(1).to_be_bytes::<32>()),
            (vec![0u8; 32], Vec::new(), placeholder_cost)
        );
        assert_eq!(hostio.fatal_error, Some(ContextError::Db(ExpectedDbError)));

        let mut input = 1_000_000u64.to_be_bytes().to_vec();
        input.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(2).to_be_bytes::<32>());
        let mut evm = failing_storage_evm();
        let mut hostio = failing_storage_hostio(&mut evm);
        assert_eq!(
            hostio.set_trie_slots(&input),
            (vec![API_STATUS_FAILURE], Vec::new(), 0)
        );
        assert_eq!(hostio.fatal_error, Some(ContextError::Db(ExpectedDbError)));
    }

    #[test]
    fn frame_counts_cover_calls_and_creates_but_not_delegate_frames() {
        let acting = Address::with_last_byte(0x10);
        let implementation = Address::with_last_byte(0x11);
        let mut evm = test_evm();
        push_parent(&mut evm, acting);
        assert_eq!(evm.ctx().chain().open_contract_frame_count(acting), 1);

        let call = known_call_inputs(
            acting,
            acting,
            CallScheme::Call,
            Bytes::from_static(&[0x00]),
        );
        let init = child_frame_init(&mut evm, FrameInput::Call(Box::new(call)));
        assert!(matches!(
            evm.frame_init(init).expect("initialize recursive call"),
            ItemOrResult::Item(_)
        ));
        assert_eq!(evm.ctx().chain().open_contract_frame_count(acting), 2);
        assert!(evm.ctx().chain().contract_is_reentrant(acting));
        let ItemOrResult::Result(result) = evm.frame_run().expect("run recursive call") else {
            panic!("STOP should finish the frame")
        };
        assert!(
            evm.frame_return_result(result)
                .expect("return recursive call")
                .is_none()
        );
        assert_eq!(evm.ctx().chain().open_contract_frame_count(acting), 1);

        let delegate = known_call_inputs(
            acting,
            implementation,
            CallScheme::DelegateCall,
            Bytes::from_static(&[0x00]),
        );
        let init = child_frame_init(&mut evm, FrameInput::Call(Box::new(delegate)));
        assert!(matches!(
            evm.frame_init(init).expect("initialize delegate call"),
            ItemOrResult::Item(_)
        ));
        assert_eq!(evm.ctx().chain().open_contract_frame_count(acting), 1);
        let ItemOrResult::Result(result) = evm.frame_run().expect("run delegate call") else {
            panic!("STOP should finish the frame")
        };
        assert!(
            evm.frame_return_result(result)
                .expect("return delegate call")
                .is_none()
        );

        let create = CreateInputs::new(
            acting,
            CreateScheme::Create,
            U256::ZERO,
            Bytes::from_static(&[0x00]),
            100_000,
        );
        let init = child_frame_init(&mut evm, FrameInput::Create(Box::new(create)));
        assert!(matches!(
            evm.frame_init(init).expect("initialize create"),
            ItemOrResult::Item(_)
        ));
        let created = evm
            .inner
            .frame_stack
            .get()
            .data
            .created_address()
            .expect("create address");
        assert_eq!(evm.ctx().chain().open_contract_frame_count(created), 1);
        let ItemOrResult::Result(result) = evm.frame_run().expect("run create") else {
            panic!("STOP should finish init code")
        };
        assert!(
            evm.frame_return_result(result)
                .expect("return create")
                .is_none()
        );
        assert_eq!(evm.ctx().chain().open_contract_frame_count(created), 0);
        assert_eq!(evm.ctx().chain().open_contract_frame_count(acting), 1);

        let ItemOrResult::Result(result) = evm.frame_run().expect("run parent") else {
            panic!("STOP should finish parent")
        };
        assert!(
            evm.frame_return_result(result)
                .expect("return parent")
                .is_some()
        );
        assert_eq!(evm.ctx().chain().open_contract_frame_count(acting), 0);
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
}
