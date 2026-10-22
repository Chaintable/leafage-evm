use leafage_evm_types::{hex, BlockId, BlockNumberOrTag, DebankErrorCode};

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

/// Constructs a `-39005 DataBaseFailed` JSON-RPC error for a failed state or
/// block read.
pub(crate) fn database_rpc_err(
    msg: impl Into<String>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(DebankErrorCode::DataBaseFailed as i32, msg, None)
}

/// Error for a failed state or block lookup by `block_id`. Every backend
/// rejects the `safe`/`finalized`/`earliest` tags before reading anything,
/// so those are a bad parameter (`-32602`); any other failure is a read
/// error (`-39005`).
pub(crate) fn block_lookup_err(
    block_id: BlockId,
    err: impl std::fmt::Display,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    match block_id {
        BlockId::Number(
            BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized | BlockNumberOrTag::Earliest,
        ) => invalid_params_rpc_err(err.to_string()),
        _ => database_rpc_err(err.to_string()),
    }
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
