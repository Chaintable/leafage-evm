use super::state::{StylusParams, WasmActivation};
use alloy::primitives::{Bytes, B256};
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
