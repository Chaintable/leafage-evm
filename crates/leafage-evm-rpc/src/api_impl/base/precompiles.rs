//! Base precompile provider: op precompiles + the Beryl B20-token reads.
//!
//! `PrecompilesMap`'s blanket `PrecompileProvider` impl did not unify with
//! op-revm's `OpEvm` execution path, so (mirroring tempo's wrapper pattern) we
//! implement `PrecompileProvider` directly for the op context: B20-prefixed
//! addresses are served by `leafage_evm_chains::base` reading storage through
//! the journal, everything else delegates to `OpPrecompiles`.

use leafage_evm_chains::base::{
    b20_dispatch, is_asset_variant,
    precompile::{has_b20_prefix, is_forwarded_registry},
    B20Outcome,
};
use leafage_evm_types::{Address, Bytes, CfgEnv, OpSpecId, U256};
use op_revm::{precompiles::OpPrecompiles, L1BlockInfo, OpTransaction};
use revm::context::{BlockEnv, ContextTr, JournalTr, LocalContextTr};
use revm::handler::PrecompileProvider;
use revm::interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult};
use revm::{Context, Database, Journal};
use revm::context::TxEnv;

/// Nominal gas charged per B20 view call (leafage disables gas accounting for
/// reads; this only needs to fit within the call's gas limit).
const B20_VIEW_GAS: u64 = 5_000;

/// The op execution context leafage builds for `base` (see `evm.rs`).
type BaseCtx<DB> =
    Context<BlockEnv, OpTransaction<TxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Op precompiles wrapped to also serve Beryl B20-token reads.
pub struct BasePrecompiles {
    inner: OpPrecompiles,
}

impl BasePrecompiles {
    pub fn new(inner: OpPrecompiles) -> Self {
        Self { inner }
    }
}

impl<DB: Database> PrecompileProvider<BaseCtx<DB>> for BasePrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: OpSpecId) -> bool {
        PrecompileProvider::<BaseCtx<DB>>::set_spec(&mut self.inner, spec)
    }

    fn run(
        &mut self,
        context: &mut BaseCtx<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let addr = inputs.bytecode_address;

        // Stateful registries leafage can't reproduce locally: signal
        // UnsupportedPrecompile (-39008) so the proxy forwards to a real Base
        // node. The "unsupported precompile address: " prefix is recognized by
        // the EVMError::Custom -> -39008 mapping in api_impl.
        if is_forwarded_registry(&addr) {
            return Err(format!("unsupported precompile address: {addr}"));
        }

        if !has_b20_prefix(&addr) {
            return PrecompileProvider::<BaseCtx<DB>>::run(&mut self.inner, context, inputs);
        }

        // Calldata bytes (copied so the context borrow is released before the
        // journal is borrowed for storage reads).
        let data: Vec<u8> = match &inputs.input {
            CallInput::Bytes(bytes) => bytes.to_vec(),
            CallInput::SharedBuffer(range) => context
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|slice| slice.to_vec())
                .unwrap_or_default(),
        };

        let is_asset = is_asset_variant(&addr);
        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(inputs.gas_limit),
            output: Bytes::new(),
        };

        let journal = context.journal_mut();
        let sload = |key: U256| -> Result<U256, ()> {
            journal.sload(addr, key).map(|loaded| loaded.data).map_err(|_| ())
        };

        match b20_dispatch(is_asset, &data, sload) {
            Ok(B20Outcome::Return(out)) => {
                let _ = result.gas.record_cost(B20_VIEW_GAS);
                result.output = out;
            }
            Ok(B20Outcome::Revert(out)) => {
                result.result = InstructionResult::Revert;
                result.output = out;
            }
            Err(()) => {
                result.result = InstructionResult::PrecompileError;
            }
        }
        Ok(Some(result))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        PrecompileProvider::<BaseCtx<DB>>::warm_addresses(&self.inner)
    }

    fn contains(&self, address: &Address) -> bool {
        has_b20_prefix(address)
            || is_forwarded_registry(address)
            || PrecompileProvider::<BaseCtx<DB>>::contains(&self.inner, address)
    }
}
