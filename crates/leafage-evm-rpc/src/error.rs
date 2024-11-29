use leafage_evm_types::hex;
/// Constructs an invalid params JSON-RPC error.
pub(crate) fn invalid_params_rpc_err(
    msg: impl Into<String>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(jsonrpsee::types::error::INVALID_PARAMS_CODE, msg, None)
}

/// Constructs an internal JSON-RPC error.
pub(crate) fn internal_rpc_err(
    msg: impl Into<String>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(jsonrpsee::types::error::INTERNAL_ERROR_CODE, msg, None)
}

#[allow(dead_code)]
/// Constructs an internal JSON-RPC error with data
pub(crate) fn internal_rpc_err_with_data(
    msg: impl Into<String>,
    data: &[u8],
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(
        jsonrpsee::types::error::INTERNAL_ERROR_CODE,
        msg,
        Some(data),
    )
}

#[allow(dead_code)]
/// Constructs an internal JSON-RPC error with code and message
pub(crate) fn rpc_error_with_code(
    code: i32,
    msg: impl Into<String>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(code, msg, None)
}

/// Constructs a JSON-RPC error, consisting of `code`, `message` and optional `data`.
pub(crate) fn rpc_err(
    code: i32,
    msg: impl Into<String>,
    data: Option<&[u8]>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    jsonrpsee::types::error::ErrorObject::owned(
        code,
        msg.into(),
        data.map(|data| {
            jsonrpsee::core::to_json_raw_value(&format!("0x{}", hex::encode(data)))
                .expect("serializing String does fail")
        }),
    )
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebankErrorCode {
    #[allow(dead_code)]
    InvalidJson = -32700,
    #[allow(dead_code)]
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    EvmRevert = -39000,
    GasExhausted = -39001,
    BalanceExhausted = -39002,
    NonceError = -39003,
    EvmFailed = -39004,
    DataBaseFailed = -39005,
    BlockNotFound = -39006,
    #[allow(dead_code)]
    InternalError = -32603,
}
