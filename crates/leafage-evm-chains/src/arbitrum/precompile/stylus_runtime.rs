use super::state::{StylusParams, WasmActivation};
use alloy::primitives::{Address, B256, Bytes, U256};
use libloading::Library;
use moka::sync::Cache;
use once_cell::sync::OnceCell;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::sync::{Arc, Condvar, Mutex};

const STYLUS_RUNTIME_ENV: &str = "LEAFAGE_ARB_STYLUS_LIB";
const STYLUS_CACHE_MB_ENV: &str = "LEAFAGE_ARB_STYLUS_CACHE_MB";
const STYLUS_COMPILE_CONCURRENCY_ENV: &str = "LEAFAGE_ARB_STYLUS_COMPILE_CONCURRENCY";
const DEFAULT_STYLUS_CACHE_MB: u64 = 64;
const DEFAULT_STYLUS_COMPILE_CONCURRENCY: usize = 2;

static PROCESS_STYLUS_RUNTIME: OnceCell<Arc<StylusRuntime>> = OnceCell::new();
static PROCESS_STYLUS_INIT: OnceCell<StylusRuntimeInit> = OnceCell::new();

type StylusActivateFn = unsafe extern "C" fn(
    GoSliceData,
    u16,
    u16,
    u64,
    bool,
    *mut RustBytes,
    *const Bytes32,
    *mut Bytes32,
    *mut StylusData,
    *mut u64,
) -> u8;
type FreeRustBytesFn = unsafe extern "C" fn(RustBytes);
type StylusCompileFn = unsafe extern "C" fn(
    GoSliceData,    // wasm
    u16,            // stylus version
    bool,           // debug
    GoSliceData,    // target name (empty => native host target)
    bool,           // cranelift (false => singlepass)
    *mut RustBytes, // out: native asm on success, error string otherwise
) -> u8;

#[derive(Clone, Debug)]
pub(crate) enum StylusRuntimeError {
    Unconfigured,
    Configuration {
        variable: &'static str,
        value: String,
        reason: String,
    },
    Load {
        path: PathBuf,
        error: String,
    },
    Symbol {
        path: PathBuf,
        symbol: &'static str,
        error: String,
    },
    PathConflict {
        loaded: PathBuf,
        requested: PathBuf,
    },
    Activation {
        status: u8,
        message: String,
    },
    Compile {
        status: u8,
        message: String,
    },
    OutOfInk,
    OutOfStack,
    NativeStackOverflow,
}

pub(super) struct ActivatedWasm {
    pub(super) activation: WasmActivation,
    pub(super) module: Bytes,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[allow(dead_code)]
pub(crate) enum StylusCompiler {
    Singlepass,
    Cranelift,
}

impl StylusCompiler {
    fn uses_cranelift(self) -> bool {
        matches!(self, Self::Cranelift)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct StylusTarget {
    name: Arc<str>,
    fingerprint: Arc<str>,
}

impl StylusTarget {
    /// Native targets are process-local cache entries. This fingerprint only
    /// separates host classes inside the in-memory key; persisted caches would
    /// additionally need the exact CPU feature set and libstylus version.
    pub(crate) fn native() -> Self {
        let endian = if cfg!(target_endian = "little") {
            "little"
        } else {
            "big"
        };
        Self {
            name: Arc::from(""),
            fingerprint: Arc::from(format!(
                "native:{}:{}:{}:{endian}",
                env::consts::ARCH,
                env::consts::OS,
                usize::BITS
            )),
        }
    }
}

/// Complete identity of a node-local serialized native module.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct NativeAsmCacheKey {
    module_hash: B256,
    stylus_version: u16,
    compiler: StylusCompiler,
    target: StylusTarget,
    debug: bool,
}

impl NativeAsmCacheKey {
    pub(crate) fn native(
        module_hash: B256,
        stylus_version: u16,
        compiler: StylusCompiler,
        debug: bool,
    ) -> Self {
        Self {
            module_hash,
            stylus_version,
            compiler,
            target: StylusTarget::native(),
            debug,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StylusRuntimeConfig {
    asm_cache_capacity_bytes: u64,
    compile_concurrency: usize,
}

struct StylusRuntimeInit {
    path: PathBuf,
    config: StylusRuntimeConfig,
}

impl StylusRuntimeConfig {
    fn from_env() -> Result<Self, StylusRuntimeError> {
        let cache_mb = read_env_u64(STYLUS_CACHE_MB_ENV, DEFAULT_STYLUS_CACHE_MB)?;
        let asm_cache_capacity_bytes =
            cache_mb
                .checked_mul(1024 * 1024)
                .ok_or_else(|| StylusRuntimeError::Configuration {
                    variable: STYLUS_CACHE_MB_ENV,
                    value: cache_mb.to_string(),
                    reason: "capacity overflows bytes".to_owned(),
                })?;
        let compile_concurrency = read_env_u64(
            STYLUS_COMPILE_CONCURRENCY_ENV,
            DEFAULT_STYLUS_COMPILE_CONCURRENCY as u64,
        )?;
        let compile_concurrency = usize::try_from(compile_concurrency)
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| StylusRuntimeError::Configuration {
                variable: STYLUS_COMPILE_CONCURRENCY_ENV,
                value: compile_concurrency.to_string(),
                reason: "must be a positive usize".to_owned(),
            })?;
        Ok(Self {
            asm_cache_capacity_bytes,
            compile_concurrency,
        })
    }
}

#[derive(Clone)]
struct NativeAsmCache {
    entries: Cache<NativeAsmCacheKey, Arc<[u8]>>,
}

impl NativeAsmCache {
    fn new(capacity_bytes: u64) -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(capacity_bytes)
                .weigher(|_key: &NativeAsmCacheKey, asm: &Arc<[u8]>| {
                    asm.len().try_into().unwrap_or(u32::MAX)
                })
                .build(),
        }
    }

    fn try_get_with<F, E>(&self, key: NativeAsmCacheKey, init: F) -> Result<Arc<[u8]>, Arc<E>>
    where
        F: FnOnce() -> Result<Vec<u8>, E>,
        E: Send + Sync + 'static,
    {
        self.entries
            .try_get_with(key, || init().map(Arc::<[u8]>::from))
    }

    fn get(&self, key: &NativeAsmCacheKey) -> Option<Arc<[u8]>> {
        self.entries.get(key)
    }
}

struct CompileGate {
    limit: usize,
    active: Mutex<usize>,
    available: Condvar,
}

impl CompileGate {
    fn new(limit: usize) -> Self {
        debug_assert!(limit > 0);
        Self {
            limit,
            active: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    fn acquire(&self) -> CompilePermit<'_> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *active >= self.limit {
            active = self
                .available
                .wait(active)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *active += 1;
        CompilePermit { gate: self }
    }
}

struct CompilePermit<'a> {
    gate: &'a CompileGate,
}

impl Drop for CompilePermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .gate
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active -= 1;
        self.gate.available.notify_one();
    }
}

pub(crate) struct StylusRuntime {
    path: PathBuf,
    // The library has process lifetime through PROCESS_STYLUS_RUNTIME. Keeping
    // this owner is required because Wasmer installs process-level handlers
    // that call back into libstylus code.
    _library: Library,
    activate_fn: StylusActivateFn,
    compile_fn: StylusCompileFn,
    call_fn: StylusCallFn,
    free_output_fn: FreeRustBytesFn,
    asm_cache: NativeAsmCache,
    compile_gate: CompileGate,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GoSliceData {
    ptr: *const u8,
    len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RustBytes {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes32([u8; 32]);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StylusData {
    ink_left: u32,
    ink_status: u32,
    depth_left: u32,
    init_cost: u16,
    cached_init_cost: u16,
    asm_estimate: u32,
    footprint: u16,
    user_main: u32,
}

/// EvmApiMethod discriminants get this offset added when crossing the FFI
/// boundary (nitro `arbos/programs/api.go` EvmApiMethodReqOffset); the handler
/// subtracts it to recover the raw method.
const EVM_API_METHOD_REQ_OFFSET: u32 = 0x1000_0000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes20([u8; 20]);

/// Read-only view of a Rust-owned buffer the native lib hands to the hostio
/// callback (nitro `prover-ffi` RustSlice; PhantomData dropped, ABI is {ptr,len}).
#[repr(C)]
struct RustSlice {
    ptr: *const u8,
    len: usize,
}

impl RustSlice {
    unsafe fn as_slice<'a>(&self) -> &'a [u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PricingParams {
    ink_price: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StylusConfig {
    version: u16,
    max_depth: u32,
    pricing: PricingParams,
}

/// Mirror of nitro `arbutil::evm::EvmData` (`#[repr(C)]`, field order is
/// load-bearing — do not reorder or pack).
#[repr(C)]
#[derive(Clone, Copy)]
struct EvmData {
    arbos_version: u64,
    block_basefee: Bytes32,
    chainid: u64,
    block_coinbase: Bytes20,
    block_gas_limit: u64,
    block_number: u64,
    block_timestamp: u64,
    contract_address: Bytes20,
    module_hash: Bytes32,
    msg_sender: Bytes20,
    msg_value: Bytes32,
    tx_gas_price: Bytes32,
    tx_origin: Bytes20,
    reentrant: u32,
    return_data_len: u32,
    cached: bool,
    tracing: bool,
}

/// Bare function-pointer + context id (nitro `NativeRequestHandler`; NOT a vtable).
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeRequestHandler {
    handle_request_fptr: unsafe extern "C" fn(
        usize,            // id: the context pointer echoed back
        u32,              // req_type (offset already applied)
        *mut RustSlice,   // in: request payload
        *mut u64,         // out: gas cost charged
        *mut GoSliceData, // out: primary result bytes
        *mut GoSliceData, // out: secondary/raw bytes
    ),
    id: usize,
}

type StylusCallFn = unsafe extern "C" fn(
    GoSliceData, // module (native asm from stylus_compile)
    GoSliceData, // calldata
    StylusConfig,
    NativeRequestHandler,
    EvmData,
    bool,           // debug
    *mut RustBytes, // out: return/revert data or error string
    *mut u64,       // gas INOUT (supplied in, remaining written back)
    u32,            // long_term_tag (0 = no long-term module cache)
) -> u8;

/// Services a Stylus program's hostio requests against revm state. `req_type`
/// is the raw `EvmApiMethod` discriminant (offset already removed). Returns
/// `(response, raw_data, evm_gas_cost)` matching nitro's Go `RequestHandler`.
pub(crate) trait HostioHandler {
    fn handle(&mut self, req_type: u32, input: &[u8]) -> (Vec<u8>, Vec<u8>, u64);
}

/// Semantic inputs for a Stylus call; `call_from_env` marshals these into the
/// `#[repr(C)]` `EvmData`/`StylusConfig` so callers never touch the FFI layout.
pub(crate) struct StylusExecInput {
    pub arbos_version: u64,
    pub block_basefee: U256,
    pub chainid: u64,
    pub block_coinbase: Address,
    pub block_gas_limit: u64,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub contract_address: Address,
    pub module_hash: B256,
    pub msg_sender: Address,
    pub msg_value: U256,
    pub tx_gas_price: U256,
    pub tx_origin: Address,
    pub reentrant: u32,
    pub cached: bool,
    pub tracing: bool,
    pub version: u16,
    pub max_depth: u32,
    pub ink_price: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StylusOutcome {
    Success,
    Revert,
    Failure,
    OutOfInk,
    OutOfStack,
    NativeStackOverflow,
}

fn call_outcome_from_status(status: u8, data: &[u8]) -> StylusOutcome {
    match status {
        0 => StylusOutcome::Success,
        1 => StylusOutcome::Revert,
        2 => StylusOutcome::Failure,
        3 => StylusOutcome::OutOfInk,
        4 => StylusOutcome::OutOfStack,
        5 => StylusOutcome::NativeStackOverflow,
        other => {
            tracing::error!(
                status = other,
                data = %String::from_utf8_lossy(data),
                "Stylus returned an unknown call status"
            );
            // Nitro treats an unknown user status as an execution revert with
            // no returndata. It is not a lib/runtime load failure.
            StylusOutcome::Failure
        }
    }
}

pub(crate) struct StylusCallResult {
    pub outcome: StylusOutcome,
    pub output: Vec<u8>,
}

/// Bridges the C hostio callback to a Rust `HostioHandler`. `arena` keeps the
/// response/raw buffers alive for the whole `stylus_call` (the native lib holds
/// `raw_data` lazily via a `GoSliceData` view).
struct HostioBridge<'a> {
    handler: &'a mut dyn HostioHandler,
    arena: Vec<Vec<u8>>,
}

impl HostioBridge<'_> {
    fn stash(&mut self, bytes: Vec<u8>) -> GoSliceData {
        if bytes.is_empty() {
            return GoSliceData {
                ptr: ptr::null(),
                len: 0,
            };
        }
        let ptr = bytes.as_ptr();
        let len = bytes.len();
        self.arena.push(bytes);
        GoSliceData { ptr, len }
    }
}

unsafe extern "C" fn hostio_trampoline(
    id: usize,
    req_type: u32,
    data: *mut RustSlice,
    gas_cost: *mut u64,
    result: *mut GoSliceData,
    raw_data: *mut GoSliceData,
) {
    let bridge = unsafe { &mut *(id as *mut HostioBridge<'_>) };
    let input = unsafe { (*data).as_slice() };
    let method = req_type.wrapping_sub(EVM_API_METHOD_REQ_OFFSET);
    let (response, raw, cost) = bridge.handler.handle(method, input);
    unsafe {
        *gas_cost = cost;
        *result = bridge.stash(response);
        *raw_data = bridge.stash(raw);
    }
}

impl StylusRuntime {
    /// Lazily initializes the process runtime on the first Arbitrum Stylus
    /// activation, compile, or call. Other chains never read these settings.
    pub(crate) fn initialize_from_env() -> Result<Arc<Self>, StylusRuntimeError> {
        let init = PROCESS_STYLUS_INIT.get_or_try_init(|| {
            let Some(path) = env::var_os(STYLUS_RUNTIME_ENV) else {
                return Err(StylusRuntimeError::Unconfigured);
            };
            Ok(StylusRuntimeInit {
                path: canonicalize_library_path(path.into())?,
                config: StylusRuntimeConfig::from_env()?,
            })
        })?;
        Self::initialize_canonical(init.path.clone(), init.config)
    }

    fn initialize_canonical(
        requested: PathBuf,
        config: StylusRuntimeConfig,
    ) -> Result<Arc<Self>, StylusRuntimeError> {
        let runtime = PROCESS_STYLUS_RUNTIME
            .get_or_try_init(|| Self::load(requested.clone(), config).map(Arc::new))
            .map(Arc::clone)?;
        if runtime.path != requested {
            return Err(StylusRuntimeError::PathConflict {
                loaded: runtime.path.clone(),
                requested,
            });
        }
        Ok(runtime)
    }

    #[cfg(test)]
    fn initialize(
        path: PathBuf,
        config: StylusRuntimeConfig,
    ) -> Result<Arc<Self>, StylusRuntimeError> {
        let requested = canonicalize_library_path(path)?;
        Self::initialize_canonical(requested, config)
    }

    fn load(path: PathBuf, config: StylusRuntimeConfig) -> Result<Self, StylusRuntimeError> {
        let library = unsafe { Library::new(&path) }.map_err(|error| StylusRuntimeError::Load {
            path: path.clone(),
            error: error.to_string(),
        })?;
        let activate_fn = load_symbol(&library, &path, "stylus_activate")?;
        let compile_fn = load_symbol(&library, &path, "stylus_compile")?;
        let call_fn = load_symbol(&library, &path, "stylus_call")?;
        let free_output_fn = load_symbol(&library, &path, "free_rust_bytes")?;
        Ok(Self {
            path,
            _library: library,
            activate_fn,
            compile_fn,
            call_fn,
            free_output_fn,
            asm_cache: NativeAsmCache::new(config.asm_cache_capacity_bytes),
            compile_gate: CompileGate::new(config.compile_concurrency),
        })
    }

    pub(super) fn activate_from_env(
        wasm: &[u8],
        code_hash: B256,
        params: StylusParams,
        page_limit: u16,
        arbos_version: u64,
        gas_left: &mut u64,
    ) -> Result<ActivatedWasm, StylusRuntimeError> {
        Self::initialize_from_env()?.activate(
            wasm,
            code_hash,
            params,
            page_limit,
            arbos_version,
            gas_left,
        )
    }

    fn activate(
        &self,
        wasm: &[u8],
        code_hash: B256,
        params: StylusParams,
        page_limit: u16,
        arbos_version: u64,
        gas_left: &mut u64,
    ) -> Result<ActivatedWasm, StylusRuntimeError> {
        let wasm = GoSliceData {
            ptr: if wasm.is_empty() {
                ptr::null()
            } else {
                wasm.as_ptr()
            },
            len: wasm.len(),
        };
        let code_hash = Bytes32(code_hash.0);
        let mut module_hash = Bytes32([0; 32]);
        let mut stylus_data = StylusData::default();
        let mut output = RustBytes {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        };

        let status = unsafe {
            (self.activate_fn)(
                wasm,
                page_limit,
                params.version,
                arbos_version,
                false,
                &mut output,
                &code_hash,
                &mut module_hash,
                &mut stylus_data,
                gas_left,
            )
        };
        match status {
            0 => {
                let module = unsafe { rust_bytes_to_vec(self.free_output_fn, output) };
                Ok(ActivatedWasm {
                    activation: WasmActivation {
                        module_hash: B256::from(module_hash.0),
                        init_cost: stylus_data.init_cost,
                        cached_cost: stylus_data.cached_init_cost,
                        footprint: stylus_data.footprint,
                        asm_estimate: stylus_data.asm_estimate,
                    },
                    module: Bytes::from(module),
                })
            }
            3 => {
                unsafe {
                    (self.free_output_fn)(output);
                }
                Err(StylusRuntimeError::OutOfInk)
            }
            4 => {
                unsafe {
                    (self.free_output_fn)(output);
                }
                Err(StylusRuntimeError::OutOfStack)
            }
            5 => {
                unsafe {
                    (self.free_output_fn)(output);
                }
                Err(StylusRuntimeError::NativeStackOverflow)
            }
            status => {
                let message = unsafe { rust_bytes_to_vec(self.free_output_fn, output) };
                Err(StylusRuntimeError::Activation {
                    status,
                    message: String::from_utf8_lossy(&message).into_owned(),
                })
            }
        }
    }

    /// Compiles on-chain Stylus wasm to native asm for the host target. The
    /// asm is a node-local derived artifact (not consensus); the moduleHash is
    /// the consensus anchor. An empty target selects the native host target
    /// (`target_cache_get("")` -> `Target::default()`), so no `stylus_target_set`
    /// call is required for single-host execution.
    #[cfg(test)]
    pub(crate) fn compile_from_env(
        wasm: &[u8],
        version: u16,
    ) -> Result<Vec<u8>, StylusRuntimeError> {
        let runtime = Self::initialize_from_env()?;
        runtime.compile(
            wasm,
            version,
            StylusCompiler::Singlepass,
            &StylusTarget::native(),
            false,
        )
    }

    /// Process-cache integration point for the Stylus frame caller. Concurrent
    /// misses for the same complete key share one compile, and failures are not
    /// inserted. The caller must use the module hash read from Programs state.
    pub(crate) fn compile_cached_from_env(
        key: NativeAsmCacheKey,
        wasm: &[u8],
    ) -> Result<Arc<[u8]>, StylusRuntimeError> {
        let runtime = Self::initialize_from_env()?;
        let compile_key = key.clone();
        runtime
            .asm_cache
            .try_get_with(key, || {
                runtime.compile(
                    wasm,
                    compile_key.stylus_version,
                    compile_key.compiler,
                    &compile_key.target,
                    compile_key.debug,
                )
            })
            .map_err(|error| (*error).clone())
    }

    /// Checks the process cache without requiring the caller to decode the
    /// on-chain wasm first. A miss is intentionally separate from compilation
    /// so root-fragment reads can stay behind the consensus gas gate.
    pub(crate) fn cached_asm_from_env(
        key: &NativeAsmCacheKey,
    ) -> Result<Option<Arc<[u8]>>, StylusRuntimeError> {
        Ok(Self::initialize_from_env()?.asm_cache.get(key))
    }

    fn compile(
        &self,
        wasm: &[u8],
        version: u16,
        compiler: StylusCompiler,
        target: &StylusTarget,
        debug: bool,
    ) -> Result<Vec<u8>, StylusRuntimeError> {
        let _permit = self.compile_gate.acquire();
        let wasm = GoSliceData {
            ptr: if wasm.is_empty() {
                ptr::null()
            } else {
                wasm.as_ptr()
            },
            len: wasm.len(),
        };
        let target = GoSliceData {
            ptr: if target.name.is_empty() {
                ptr::null()
            } else {
                target.name.as_ptr()
            },
            len: target.name.len(),
        };
        let mut output = RustBytes {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        let status = unsafe {
            (self.compile_fn)(
                wasm,
                version,
                debug,
                target,
                compiler.uses_cranelift(),
                &mut output,
            )
        };
        let bytes = unsafe { rust_bytes_to_vec(self.free_output_fn, output) };
        match status {
            0 => Ok(bytes),
            status => Err(StylusRuntimeError::Compile {
                status,
                message: String::from_utf8_lossy(&bytes).into_owned(),
            }),
        }
    }

    /// Executes a Stylus program (native asm from `compile`) against revm state.
    /// `gas` is INOUT: supplied gas in, remaining gas written back (the native
    /// lib does the gas<->ink conversion internally via `ink_price`). Hostio
    /// requests are serviced synchronously through `handler`.
    pub(crate) fn call_from_env(
        module: &[u8],
        calldata: &[u8],
        input: StylusExecInput,
        handler: &mut dyn HostioHandler,
        gas: &mut u64,
    ) -> Result<StylusCallResult, StylusRuntimeError> {
        Self::initialize_from_env()?.call(module, calldata, input, handler, gas)
    }

    fn call(
        &self,
        module: &[u8],
        calldata: &[u8],
        input: StylusExecInput,
        handler: &mut dyn HostioHandler,
        gas: &mut u64,
    ) -> Result<StylusCallResult, StylusRuntimeError> {
        let config = StylusConfig {
            version: input.version,
            max_depth: input.max_depth,
            pricing: PricingParams {
                ink_price: input.ink_price,
            },
        };
        let evm_data = EvmData {
            arbos_version: input.arbos_version,
            block_basefee: Bytes32(input.block_basefee.to_be_bytes()),
            chainid: input.chainid,
            block_coinbase: Bytes20(input.block_coinbase.into_array()),
            block_gas_limit: input.block_gas_limit,
            block_number: input.block_number,
            block_timestamp: input.block_timestamp,
            contract_address: Bytes20(input.contract_address.into_array()),
            module_hash: Bytes32(input.module_hash.0),
            msg_sender: Bytes20(input.msg_sender.into_array()),
            msg_value: Bytes32(input.msg_value.to_be_bytes()),
            tx_gas_price: Bytes32(input.tx_gas_price.to_be_bytes()),
            tx_origin: Bytes20(input.tx_origin.into_array()),
            reentrant: input.reentrant,
            return_data_len: 0,
            cached: input.cached,
            tracing: input.tracing,
        };

        let mut bridge = HostioBridge {
            handler,
            arena: Vec::new(),
        };
        let req_handler = NativeRequestHandler {
            handle_request_fptr: hostio_trampoline,
            id: &mut bridge as *mut HostioBridge<'_> as usize,
        };

        let module = GoSliceData {
            ptr: if module.is_empty() {
                ptr::null()
            } else {
                module.as_ptr()
            },
            len: module.len(),
        };
        let calldata = GoSliceData {
            ptr: if calldata.is_empty() {
                ptr::null()
            } else {
                calldata.as_ptr()
            },
            len: calldata.len(),
        };
        let mut output = RustBytes {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        };

        let status = unsafe {
            (self.call_fn)(
                module,
                calldata,
                config,
                req_handler,
                evm_data,
                false,
                &mut output,
                gas,
                0,
            )
        };
        // `bridge` (and its arena) must stay alive until stylus_call returns.
        drop(bridge);
        let out_bytes = unsafe { rust_bytes_to_vec(self.free_output_fn, output) };
        let outcome = call_outcome_from_status(status, &out_bytes);
        Ok(StylusCallResult {
            outcome,
            output: out_bytes,
        })
    }
}

fn canonicalize_library_path(path: PathBuf) -> Result<PathBuf, StylusRuntimeError> {
    fs::canonicalize(&path).map_err(|error| StylusRuntimeError::Load {
        path,
        error: format!("failed to resolve canonical path: {error}"),
    })
}

fn load_symbol<T: Copy>(
    library: &Library,
    path: &PathBuf,
    name: &'static str,
) -> Result<T, StylusRuntimeError> {
    unsafe { library.get::<T>(name.as_bytes()) }
        .map(|symbol| *symbol)
        .map_err(|error| StylusRuntimeError::Symbol {
            path: path.clone(),
            symbol: name,
            error: error.to_string(),
        })
}

fn read_env_u64(name: &'static str, default: u64) -> Result<u64, StylusRuntimeError> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|error| StylusRuntimeError::Configuration {
                variable: name,
                value,
                reason: error.to_string(),
            }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(value)) => Err(StylusRuntimeError::Configuration {
            variable: name,
            value: value.to_string_lossy().into_owned(),
            reason: "value is not valid UTF-8".to_owned(),
        }),
    }
}

unsafe fn rust_bytes_to_vec(free_output: FreeRustBytesFn, output: RustBytes) -> Vec<u8> {
    let bytes = if output.ptr.is_null() || output.len == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(output.ptr, output.len) }.to_vec()
    };
    unsafe {
        free_output(output);
    }
    bytes
}

impl StylusRuntimeError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Unconfigured => format!("{STYLUS_RUNTIME_ENV} is not set"),
            Self::Configuration {
                variable,
                value,
                reason,
            } => format!("invalid {variable}={value:?}: {reason}"),
            Self::Load { path, error } => format!("failed to load {}: {error}", path.display()),
            Self::Symbol {
                path,
                symbol,
                error,
            } => {
                format!(
                    "failed to load symbol {symbol} from {}: {error}",
                    path.display()
                )
            }
            Self::PathConflict { loaded, requested } => format!(
                "libstylus is already initialized from {}; refusing different path {}",
                loaded.display(),
                requested.display()
            ),
            Self::Activation { status, message } => {
                format!("stylus activation failed with status {status}: {message}")
            }
            Self::Compile { status, message } => {
                format!("stylus compile failed with status {status}: {message}")
            }
            Self::OutOfInk => "stylus activation ran out of ink".to_owned(),
            Self::OutOfStack => "stylus activation ran out of stack".to_owned(),
            Self::NativeStackOverflow => "stylus activation hit native stack overflow".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    fn cache_key(module_byte: u8) -> NativeAsmCacheKey {
        NativeAsmCacheKey::native(
            B256::from([module_byte; 32]),
            2,
            StylusCompiler::Singlepass,
            false,
        )
    }

    #[test]
    fn call_statuses_match_nitro_including_unknown_failure() {
        let expected = [
            StylusOutcome::Success,
            StylusOutcome::Revert,
            StylusOutcome::Failure,
            StylusOutcome::OutOfInk,
            StylusOutcome::OutOfStack,
            StylusOutcome::NativeStackOverflow,
        ];
        for (status, outcome) in expected.into_iter().enumerate() {
            assert_eq!(call_outcome_from_status(status as u8, b"detail"), outcome);
        }
        assert_eq!(
            call_outcome_from_status(u8::MAX, b"internal detail"),
            StylusOutcome::Failure
        );
    }

    #[test]
    fn loading_missing_library_reports_path() {
        let path = PathBuf::from("/definitely/missing/libstylus.dylib");
        let err = match canonicalize_library_path(path) {
            Ok(_) => panic!("missing library should not load"),
            Err(err) => err,
        };
        match err {
            StylusRuntimeError::Load { path, .. } => {
                assert_eq!(path, Path::new("/definitely/missing/libstylus.dylib"));
            }
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    #[ignore = "requires libstylus"]
    fn configured_runtime_is_a_process_singleton_and_rejects_another_path() {
        env::var_os(STYLUS_RUNTIME_ENV).expect("LEAFAGE_ARB_STYLUS_LIB must be set");
        let first = StylusRuntime::initialize_from_env().expect("initialize libstylus");
        let second = StylusRuntime::initialize_from_env().expect("reuse libstylus");
        assert!(Arc::ptr_eq(&first, &second));

        let requested = fs::canonicalize(std::env::current_exe().expect("current executable"))
            .expect("canonical executable");
        assert_ne!(first.path, requested);
        let err = match StylusRuntime::initialize(
            requested.clone(),
            StylusRuntimeConfig::from_env().expect("runtime config"),
        ) {
            Ok(_) => panic!("a different canonical path must be rejected"),
            Err(err) => err,
        };
        match err {
            StylusRuntimeError::PathConflict {
                loaded,
                requested: conflict,
            } => {
                assert_eq!(loaded, first.path);
                assert_eq!(conflict, requested);
            }
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    fn native_asm_key_covers_every_compilation_dimension() {
        let key = cache_key(1);

        let mut changed = key.clone();
        changed.module_hash = B256::from([2; 32]);
        assert_ne!(key, changed);

        let mut changed = key.clone();
        changed.stylus_version += 1;
        assert_ne!(key, changed);

        let mut changed = key.clone();
        changed.compiler = StylusCompiler::Cranelift;
        assert_ne!(key, changed);

        let mut changed = key.clone();
        changed.target.fingerprint = Arc::from("another-target");
        assert_ne!(key, changed);

        let mut changed = key.clone();
        changed.debug = true;
        assert_ne!(key, changed);
    }

    #[test]
    fn native_asm_cache_singleflights_concurrent_misses() {
        const THREADS: usize = 20;
        let cache = Arc::new(NativeAsmCache::new(1024));
        let starts = Arc::new(Barrier::new(THREADS));
        let compiles = Arc::new(AtomicUsize::new(0));
        let key = cache_key(3);
        let handles = (0..THREADS)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let starts = Arc::clone(&starts);
                let compiles = Arc::clone(&compiles);
                let key = key.clone();
                thread::spawn(move || {
                    starts.wait();
                    cache
                        .try_get_with(key, || -> Result<Vec<u8>, &'static str> {
                            compiles.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(25));
                            Ok(vec![7; 32])
                        })
                        .expect("compile")
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert_eq!(handle.join().expect("thread").as_ref(), &[7; 32]);
        }
        assert_eq!(compiles.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn native_asm_cache_does_not_store_compile_errors() {
        let cache = NativeAsmCache::new(1024);
        let compiles = AtomicUsize::new(0);
        let key = cache_key(4);

        for _ in 0..2 {
            let error = cache
                .try_get_with(key.clone(), || -> Result<Vec<u8>, &'static str> {
                    compiles.fetch_add(1, Ordering::SeqCst);
                    Err("compile failed")
                })
                .expect_err("compile must fail");
            assert_eq!(*error, "compile failed");
        }
        assert_eq!(compiles.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn native_asm_cache_enforces_its_byte_capacity() {
        let cache = NativeAsmCache::new(8);
        for module_byte in 1..=4 {
            cache
                .try_get_with(
                    cache_key(module_byte),
                    || -> Result<Vec<u8>, &'static str> { Ok(vec![module_byte; 4]) },
                )
                .expect("insert asm");
        }
        cache.entries.run_pending_tasks();

        assert!(cache.entries.weighted_size() <= 8);
        assert!(cache.entries.entry_count() <= 2);
    }

    #[test]
    fn compile_gate_caps_parallel_compiles() {
        const THREADS: usize = 8;
        let gate = Arc::new(CompileGate::new(2));
        let starts = Arc::new(Barrier::new(THREADS));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let handles = (0..THREADS)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let starts = Arc::clone(&starts);
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                thread::spawn(move || {
                    starts.wait();
                    let _permit = gate.acquire();
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(10));
                    active.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().expect("thread");
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }
}
