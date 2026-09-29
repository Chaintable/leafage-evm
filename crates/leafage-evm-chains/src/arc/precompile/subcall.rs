// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Two-phase interface for precompiles that execute a child EVM call frame.

use alloy::primitives::Bytes;
use revm::{handler::FrameResult, interpreter::CallInputs};
use std::{error::Error, fmt};

pub(crate) trait SubcallPrecompile: Send + Sync {
    fn init_subcall(&self, inputs: &CallInputs) -> Result<SubcallInitResult, SubcallError>;

    fn complete_subcall(
        &self,
        continuation_data: SubcallContinuationData,
        child_result: &FrameResult,
    ) -> Result<SubcallCompletionResult, SubcallError>;

    /// Returns the logical child call used to make successful subcalls transparent to inspectors.
    fn trace_child_call(&self, _inputs: &CallInputs) -> Option<CallInputs> {
        None
    }
}

pub(crate) struct SubcallInitResult {
    pub(crate) child_inputs: Box<CallInputs>,
    pub(crate) continuation_data: SubcallContinuationData,
    pub(crate) gas_overhead: u64,
}

pub(crate) struct SubcallContinuationData;

pub(crate) struct SubcallCompletionResult {
    pub(crate) output: Bytes,
    pub(crate) success: bool,
    pub(crate) gas_overhead: u64,
}

#[derive(Debug)]
pub(crate) enum SubcallError {
    AbiDecodeError(String),
    UnexpectedFrameResult,
    InsufficientGas(String),
}

impl fmt::Display for SubcallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AbiDecodeError(message) => write!(f, "ABI decode error: {message}"),
            Self::UnexpectedFrameResult => {
                f.write_str("unexpected frame result type (expected call)")
            }
            Self::InsufficientGas(message) => write!(f, "insufficient gas: {message}"),
        }
    }
}

impl Error for SubcallError {}
