use super::state::{StylusParams, WasmActivation};
use alloy::primitives::{Address, Bytes, B256, U256};
use libloading::{Library, Symbol};
use std::env;
use std::path::PathBuf;
use std::ptr;
use std::slice;

const STYLUS_RUNTIME_ENV: &str = "LEAFAGE_ARB_STYLUS_LIB";

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

#[derive(Debug)]
pub(super) enum StylusRuntimeError {
    Unconfigured,
    Load {
        path: PathBuf,
        error: String,
    },
    Symbol {
        path: PathBuf,
        symbol: &'static str,
        error: String,
    },
    Activation {
        status: u8,
        message: String,
    },
    Compile {
        status: u8,
        message: String,
    },
    Call {
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

pub(super) struct StylusRuntime {
    path: PathBuf,
    library: Library,
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
    GoSliceData,          // module (native asm from stylus_compile)
    GoSliceData,          // calldata
    StylusConfig,
    NativeRequestHandler,
    EvmData,
    bool,                 // debug
    *mut RustBytes,       // out: return/revert data or error string
    *mut u64,             // gas INOUT (supplied in, remaining written back)
    u32,                  // long_term_tag (0 = no long-term module cache)
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
    pub(super) fn activate_from_env(
        wasm: &[u8],
        code_hash: B256,
        params: StylusParams,
        page_limit: u16,
        arbos_version: u64,
        gas_left: &mut u64,
    ) -> Result<ActivatedWasm, StylusRuntimeError> {
        let Some(path) = env::var_os(STYLUS_RUNTIME_ENV) else {
            return Err(StylusRuntimeError::Unconfigured);
        };
        Self::from_path(path).and_then(|runtime| {
            runtime.activate(wasm, code_hash, params, page_limit, arbos_version, gas_left)
        })
    }

    fn from_path(path: impl Into<PathBuf>) -> Result<Self, StylusRuntimeError> {
        let path = path.into();
        let library = unsafe { Library::new(&path) }.map_err(|error| StylusRuntimeError::Load {
            path: path.clone(),
            error: error.to_string(),
        })?;
        Ok(Self { path, library })
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
        let activate = self.symbol::<StylusActivateFn>("stylus_activate")?;
        let free_output = *self.symbol::<FreeRustBytesFn>("free_rust_bytes")?;
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
            activate(
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
                let module = unsafe { rust_bytes_to_vec(free_output, output) };
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
                    free_output(output);
                }
                Err(StylusRuntimeError::OutOfInk)
            }
            4 => {
                unsafe {
                    free_output(output);
                }
                Err(StylusRuntimeError::OutOfStack)
            }
            5 => {
                unsafe {
                    free_output(output);
                }
                Err(StylusRuntimeError::NativeStackOverflow)
            }
            status => {
                let message = unsafe { rust_bytes_to_vec(free_output, output) };
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
    pub(super) fn compile_from_env(
        wasm: &[u8],
        version: u16,
    ) -> Result<Vec<u8>, StylusRuntimeError> {
        let Some(path) = env::var_os(STYLUS_RUNTIME_ENV) else {
            return Err(StylusRuntimeError::Unconfigured);
        };
        Self::from_path(path).and_then(|runtime| runtime.compile(wasm, version))
    }

    fn compile(&self, wasm: &[u8], version: u16) -> Result<Vec<u8>, StylusRuntimeError> {
        let compile = self.symbol::<StylusCompileFn>("stylus_compile")?;
        let free_output = *self.symbol::<FreeRustBytesFn>("free_rust_bytes")?;
        let wasm = GoSliceData {
            ptr: if wasm.is_empty() {
                ptr::null()
            } else {
                wasm.as_ptr()
            },
            len: wasm.len(),
        };
        let target = GoSliceData {
            ptr: ptr::null(),
            len: 0,
        };
        let mut output = RustBytes {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        let status = unsafe { compile(wasm, version, false, target, false, &mut output) };
        let bytes = unsafe { rust_bytes_to_vec(free_output, output) };
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
        let Some(path) = env::var_os(STYLUS_RUNTIME_ENV) else {
            return Err(StylusRuntimeError::Unconfigured);
        };
        Self::from_path(path)?.call(module, calldata, input, handler, gas)
    }

    fn call(
        &self,
        module: &[u8],
        calldata: &[u8],
        input: StylusExecInput,
        handler: &mut dyn HostioHandler,
        gas: &mut u64,
    ) -> Result<StylusCallResult, StylusRuntimeError> {
        let call = self.symbol::<StylusCallFn>("stylus_call")?;
        let free_output = *self.symbol::<FreeRustBytesFn>("free_rust_bytes")?;

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
            call(
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
        let out_bytes = unsafe { rust_bytes_to_vec(free_output, output) };
        let outcome = match status {
            0 => StylusOutcome::Success,
            1 => StylusOutcome::Revert,
            2 => StylusOutcome::Failure,
            3 => StylusOutcome::OutOfInk,
            4 => StylusOutcome::OutOfStack,
            5 => StylusOutcome::NativeStackOverflow,
            other => {
                return Err(StylusRuntimeError::Call {
                    status: other,
                    message: String::from_utf8_lossy(&out_bytes).into_owned(),
                });
            }
        };
        Ok(StylusCallResult {
            outcome,
            output: out_bytes,
        })
    }

    fn symbol<T>(&self, name: &'static str) -> Result<Symbol<'_, T>, StylusRuntimeError> {
        unsafe { self.library.get(name.as_bytes()) }.map_err(|error| StylusRuntimeError::Symbol {
            path: self.path.clone(),
            symbol: name,
            error: error.to_string(),
        })
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
    pub(super) fn message(&self) -> String {
        match self {
            Self::Unconfigured => format!("{STYLUS_RUNTIME_ENV} is not set"),
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
            Self::Activation { status, message } => {
                format!("stylus activation failed with status {status}: {message}")
            }
            Self::Compile { status, message } => {
                format!("stylus compile failed with status {status}: {message}")
            }
            Self::Call { status, message } => {
                format!("stylus call failed with status {status}: {message}")
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
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn loading_missing_library_reports_path() {
        let path = OsString::from("/definitely/missing/libstylus.dylib");
        let err = match StylusRuntime::from_path(path) {
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
}
