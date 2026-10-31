use super::{
    consensus::{self, CeloChainData},
    evm::{self, CeloL1Context},
    pure, L1Fork,
};
use num_bigint::BigUint;
use revm::interpreter::{
    CallInputs, CallScheme, CallValue, Gas, InstructionResult, InterpreterResult,
};
use revm::primitives::{address, hardfork::SpecId, keccak256, Address, AddressSet, Bytes, U256};
use revm::{
    context_interface::{journaled_state::account::JournaledAccountTr, JournalTr},
    handler::{EthPrecompiles, PrecompileProvider},
    Database, Inspector,
};

#[derive(Debug)]
pub(crate) enum Failure {
    Invalid,
    Node(String),
}

pub struct L1Precompiles<'a> {
    inner: EthPrecompiles,
    fork: L1Fork,
    chain: &'a dyn CeloChainData,
    warm: AddressSet,
}
impl<'a> L1Precompiles<'a> {
    pub fn new(fork: L1Fork, chain: &'a dyn CeloChainData) -> Self {
        let inner = EthPrecompiles::new(fork.spec());
        let mut warm = inner.warm_addresses().clone();
        for id in 0xe1..=0xfd {
            if fork.precompile_active(id) {
                warm.insert(Address::with_last_byte(id));
            }
        }
        Self {
            inner,
            fork,
            chain,
            warm,
        }
    }

    fn run_inner<DB: Database>(
        &mut self,
        context: &mut CeloL1Context<DB>,
        inputs: &CallInputs,
        inspector: Option<&mut dyn Inspector<CeloL1Context<DB>>>,
    ) -> Result<Option<InterpreterResult>, String> {
        // EthFrame has already journaled the plain value transfer. Replace it
        // inside that same checkpoint before running bytecode or a precompile.
        if inputs.scheme == CallScheme::Call {
            if let CallValue::Transfer(value) = inputs.value {
                self.retax_transfer(
                    context,
                    inputs.caller,
                    inputs.target_address,
                    value,
                    inputs.is_static,
                )?;
            }
        }
        let bytes = inputs.bytecode_address.as_slice();
        if bytes[..19].iter().any(|b| *b != 0) || !self.fork.precompile_active(bytes[19]) {
            return self.inner.run(context, inputs);
        }
        let address = bytes[19];
        let input = inputs.input.bytes(context).to_vec();
        let cost = pure::required_gas(address, &input);
        let mut gas = Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
        if inputs.gas_limit < cost {
            gas.spend_all();
            return Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileOOG,
                gas,
                output: Bytes::new(),
            }));
        }
        let result = match address {
            0xfd => self.transfer(context, inputs, &input, inspector),
            0xfa | 0xf9 | 0xf5 | 0xf4 | 0xe1 => consensus::run(
                address,
                &input,
                context.block.number.to::<u64>(),
                self.fork >= L1Fork::Gingerbread,
                self.chain,
            ),
            _ => pure::run(address, &input, self.fork),
        };
        let (result, output) = match result {
            Ok(output) => {
                let _ = gas.record_regular_cost(cost);
                (InstructionResult::Return, output)
            }
            Err(Failure::Invalid) => {
                gas.spend_all();
                (InstructionResult::PrecompileError, Bytes::new())
            }
            Err(Failure::Node(error)) => return Err(error),
        };
        Ok(Some(InterpreterResult {
            result,
            gas,
            output,
        }))
    }

    fn registered<DB: Database>(
        &self,
        context: &mut CeloL1Context<DB>,
        name: &str,
    ) -> Result<Address, Failure> {
        self.registered_inspected(context, name, None)
    }

    fn registered_inspected<DB: Database>(
        &self,
        context: &mut CeloL1Context<DB>,
        name: &str,
        inspector: Option<&mut dyn Inspector<CeloL1Context<DB>>>,
    ) -> Result<Address, Failure> {
        let mut input = keccak256(b"getAddressFor(bytes32)")[..4].to_vec();
        input.extend_from_slice(keccak256(name.as_bytes()).as_slice());
        let result = evm::system_call_inspected(
            context,
            self.fork,
            self.chain,
            Address::ZERO,
            address!("000000000000000000000000000000000000ce10"),
            input.into(),
            100_000,
            true,
            true,
            inspector,
        )
        .map_err(Failure::Node)?;
        if !result.result.is_ok() {
            return Err(Failure::Invalid);
        }
        let output = result.output;
        if output.len() < 32 {
            return Err(Failure::Invalid);
        }
        let address = Address::from_slice(&output[12..32]);
        if address == Address::ZERO {
            return Err(Failure::Invalid);
        }
        Ok(address)
    }

    fn transfer<DB: Database>(
        &self,
        context: &mut CeloL1Context<DB>,
        inputs: &CallInputs,
        input: &[u8],
        inspector: Option<&mut dyn Inspector<CeloL1Context<DB>>>,
    ) -> Result<Bytes, Failure> {
        if self.registered_inspected(context, "GoldToken", inspector)? != inputs.caller {
            return Err(Failure::Invalid);
        }
        if input.len() < 96 || (self.fork >= L1Fork::Gingerbread && input.len() != 96) {
            return Err(Failure::Invalid);
        }
        let from = Address::from_slice(&input[12..32]);
        let to = Address::from_slice(&input[44..64]);
        let amount = U256::from_be_slice(&input[64..96]);
        if from == Address::ZERO {
            let cold = context
                .journaled_state
                .load_account(to)
                .map_err(db_error)?
                .is_cold;
            let mut account = context
                .journaled_state
                .load_account_mut(to)
                .map_err(db_error)?;
            if !account.incr_balance(amount) {
                return Err(Failure::Invalid);
            }
            if cold {
                account.unsafe_mark_cold();
            }
        } else {
            // Native CanTransfer does not warm the source account.
            let cold = context
                .journaled_state
                .load_account(from)
                .map_err(db_error)?
                .is_cold;
            let mut account = context
                .journaled_state
                .load_account_mut(from)
                .map_err(db_error)?;
            let balance = *account.balance();
            if cold {
                account.unsafe_mark_cold();
            }
            if balance < amount {
                return Err(Failure::Invalid);
            }
            let tax = if self.fork < L1Fork::Gingerbread && amount != U256::ZERO {
                self.tobin(context, from, amount, inputs.is_static)
            } else {
                Ok(None)
            }?;
            if let Some((reserve, tax)) = tax {
                native_transfer(context, from, to, amount - tax)?;
                native_transfer(context, from, reserve, tax)?;
            } else {
                native_transfer(context, from, to, amount)?;
            }
        }
        Ok(input.to_vec().into())
    }

    fn tobin<DB: Database>(
        &self,
        context: &mut CeloL1Context<DB>,
        from: Address,
        amount: U256,
        is_static: bool,
    ) -> Result<Option<(Address, U256)>, Failure> {
        let result = (|| {
            let reserve = self.registered(context, "Reserve")?;
            let out = evm::system_call(
                context,
                self.fork,
                self.chain,
                from,
                reserve,
                vec![0x17, 0xf9, 0xa6, 0xf7].into(),
                1_000_000,
                is_static,
                false,
            )?;
            if out.len() != 64 {
                return Err(Failure::Invalid);
            }
            let numerator = U256::from_be_slice(&out[..32]);
            let denominator = U256::from_be_slice(&out[32..]);
            if denominator.is_zero() || numerator > denominator {
                return Err(Failure::Invalid);
            }
            let tax = BigUint::from_bytes_be(&amount.to_be_bytes::<32>())
                * BigUint::from_bytes_be(&numerator.to_be_bytes::<32>())
                / BigUint::from_bytes_be(&denominator.to_be_bytes::<32>());
            Ok((reserve, U256::from_be_slice(&tax.to_bytes_be())))
        })();
        match result {
            Ok(tax) => Ok(Some(tax)),
            Err(Failure::Invalid) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(super) fn retax_transfer<DB: Database>(
        &self,
        context: &mut CeloL1Context<DB>,
        from: Address,
        to: Address,
        amount: U256,
        is_static: bool,
    ) -> Result<(), String> {
        if self.fork >= L1Fork::Gingerbread || amount.is_zero() {
            return Ok(());
        }
        let result = (|| {
            native_transfer(context, to, from, amount)?;
            if let Some((reserve, tax)) = self.tobin(context, from, amount, is_static)? {
                native_transfer(context, from, to, amount - tax)?;
                native_transfer(context, from, reserve, tax)?;
            } else {
                native_transfer(context, from, to, amount)?;
            }
            Ok(())
        })();
        result.map_err(|e| match e {
            Failure::Node(error) => error,
            Failure::Invalid => "Celo value transfer invariant failed".into(),
        })
    }
}

fn db_error(error: impl std::fmt::Display) -> Failure {
    Failure::Node(error.to_string())
}

fn native_transfer<DB: Database>(
    context: &mut CeloL1Context<DB>,
    from: Address,
    to: Address,
    amount: U256,
) -> Result<(), Failure> {
    let from_cold = context
        .journaled_state
        .load_account(from)
        .map_err(db_error)?
        .is_cold;
    let to_cold = from != to
        && context
            .journaled_state
            .load_account(to)
            .map_err(db_error)?
            .is_cold;
    let failure = context
        .journaled_state
        .transfer(from, to, amount)
        .map_err(db_error)?;
    for (address, cold) in [(from, from_cold), (to, to_cold)] {
        if cold {
            context
                .journaled_state
                .load_account_mut(address)
                .map_err(db_error)?
                .unsafe_mark_cold();
        }
    }
    if failure.is_some() {
        Err(Failure::Invalid)
    } else {
        Ok(())
    }
}

macro_rules! provider {
    ($context:ty) => {
        impl<DB: Database> PrecompileProvider<$context> for L1Precompiles<'_> {
            type Output = InterpreterResult;
            fn set_spec(&mut self, _: SpecId) -> bool {
                false
            }
            fn warm_addresses(&self) -> &AddressSet {
                &self.warm
            }
            fn run(
                &mut self,
                context: &mut $context,
                inputs: &CallInputs,
            ) -> Result<Option<Self::Output>, String> {
                self.run_inner(context, inputs, None)
            }
        }
    };
}
provider!(CeloL1Context<DB>);
provider!(&mut CeloL1Context<DB>);

/// Only the GoldToken authorization query is visible to the native tracer.
/// Tobin-tax helper calls remain uninspected, as in Celo's TobinTransfer.
pub(super) struct InspectingPrecompiles<'p, 'a, DB: Database> {
    pub precompiles: &'p mut L1Precompiles<'a>,
    pub inspector: &'p mut dyn Inspector<CeloL1Context<DB>>,
}
macro_rules! inspecting_provider {
    ($context:ty) => {
        impl<DB: Database> PrecompileProvider<$context> for InspectingPrecompiles<'_, '_, DB> {
            type Output = InterpreterResult;
            fn set_spec(&mut self, _: SpecId) -> bool {
                false
            }
            fn warm_addresses(&self) -> &AddressSet {
                &self.precompiles.warm
            }
            fn run(
                &mut self,
                context: &mut $context,
                inputs: &CallInputs,
            ) -> Result<Option<Self::Output>, String> {
                self.precompiles
                    .run_inner(context, inputs, Some(self.inspector))
            }
        }
    };
}
inspecting_provider!(CeloL1Context<DB>);
inspecting_provider!(&mut CeloL1Context<DB>);
