//! Base precompile provider: op precompiles + the Beryl B20 tokens.
//!
//! `PrecompilesMap`'s blanket `PrecompileProvider` impl did not unify with op-revm's
//! `OpEvm` execution path, so (mirroring tempo's wrapper pattern) we implement
//! `PrecompileProvider` directly for the op context: B20-prefixed addresses are executed by
//! `leafage_evm_chains::base::b20` against the journal, everything else delegates to
//! `OpPrecompiles`.
//!
//! Gas is metered per storage access by [`MeteredB20Port`], so the gas reported here is the
//! gas Base charges. This is what `eth_estimateGas` over a B20 token depends on: the
//! previous flat-fee approach returned a plausible-looking number that was simply wrong.

use leafage_evm_chains::base::b20::{dispatch as b20_dispatch, B20Error, B20Outcome};
use leafage_evm_chains::base::{
    is_asset_variant,
    precompile::{has_b20_prefix, is_forwarded_registry},
};
use leafage_evm_types::{Address, Bytes, CfgEnv, OpSpecId};
use op_revm::{precompiles::OpPrecompiles, L1BlockInfo, OpTransaction};
use revm::context::TxEnv;
use revm::context::{Block, BlockEnv, Cfg, ContextTr, LocalContextTr};
use revm::handler::PrecompileProvider;
use revm::interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult};
use revm::{Context, Database, Journal};

use super::metered::MeteredB20Port;

/// The op execution context leafage builds for `base` (see `evm.rs`).
type BaseCtx<DB> =
    Context<BlockEnv, OpTransaction<TxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Op precompiles wrapped to also serve Beryl B20 tokens.
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
        // UnsupportedPrecompile (-39008) so the proxy forwards to a real Base node. The
        // "unsupported precompile address: " prefix is recognized by the
        // EVMError::Custom -> -39008 mapping in api_impl.
        if is_forwarded_registry(&addr) {
            return Err(format!("unsupported precompile address: {addr}"));
        }

        if !has_b20_prefix(&addr) {
            return PrecompileProvider::<BaseCtx<DB>>::run(&mut self.inner, context, inputs);
        }

        // Calldata bytes (copied so the context borrow is released before the journal is
        // borrowed for state access).
        let data: Vec<u8> = match &inputs.input {
            CallInput::Bytes(bytes) => bytes.to_vec(),
            CallInput::SharedBuffer(range) => context
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|slice| slice.to_vec())
                .unwrap_or_default(),
        };

        let is_asset = is_asset_variant(&addr);
        let gas_limit = inputs.gas_limit;
        let gas_params = context.cfg().gas_params().clone();
        let chain_id = context.cfg().chain_id();
        let timestamp = context.block().timestamp();
        let caller = inputs.caller;
        let call_value = inputs.value.get();
        let is_static = inputs.is_static;

        let journal = context.journal_mut();
        let mut port = MeteredB20Port::new(
            journal,
            gas_limit,
            gas_params,
            caller,
            call_value,
            chain_id,
            timestamp,
            is_static,
        );

        let outcome = b20_dispatch(&mut port, addr, is_asset, &data);
        let spent = port.gas_spent();
        let refunded = port.gas_refunded();

        let mut gas = Gas::new(gas_limit);
        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(gas_limit),
            output: Bytes::new(),
        };

        match outcome {
            Ok(B20Outcome::Return(output)) => {
                // Both arms consume the metered gas: a revert keeps what it burned before
                // reverting, exactly as an EVM call frame does.
                let _ = gas.record_cost(spent);
                gas.record_refund(refunded);
                result.gas = gas;
                result.output = output;
            }
            Ok(B20Outcome::Revert(output)) => {
                let _ = gas.record_cost(spent);
                result.gas = gas;
                result.result = InstructionResult::Revert;
                result.output = output;
            }
            Err(B20Error::OutOfGas) => {
                // Out of gas consumes the entire limit.
                gas.spend_all();
                result.gas = gas;
                result.result = InstructionResult::PrecompileOOG;
            }
            Err(B20Error::StaticCallViolation) => {
                gas.spend_all();
                result.gas = gas;
                result.result = InstructionResult::StateChangeDuringStaticCall;
            }
            Err(err) => {
                // Fatal storage failure: not a revert, abort the call.
                let _ = err;
                gas.spend_all();
                result.gas = gas;
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
