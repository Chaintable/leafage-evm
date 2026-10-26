//! Celo's permissioned native transfer, adapted from celo-org/celo-kona@9a608fbb
//! (crates/celo-revm/src/precompiles/transfer.rs) to Leafage's revm version.

use crate::celo::CELO_TOKEN;
use op_revm::{precompiles::OpPrecompiles, OpSpecId};
use revm::{
    context::{Cfg, ContextTr, JournalTr},
    context_interface::{journaled_state::account::JournaledAccountTr, ContextError},
    handler::PrecompileProvider,
    interpreter::{CallInputs, Gas, InstructionResult, InterpreterResult},
    primitives::{address, Address, AddressSet, Bytes, U256},
    Database,
};

const TRANSFER_ADDRESS: Address = address!("00000000000000000000000000000000000000fd");
const TRANSFER_GAS: u64 = 9_000;

pub struct CeloPrecompiles {
    inner: OpPrecompiles,
    warm_addresses: AddressSet,
}

impl CeloPrecompiles {
    pub fn new(spec: OpSpecId) -> Self {
        let inner = OpPrecompiles::new_with_spec(spec);
        let mut warm_addresses = inner.precompiles().addresses_set().clone();
        warm_addresses.insert(TRANSFER_ADDRESS);
        Self {
            inner,
            warm_addresses,
        }
    }
}

impl<CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>> PrecompileProvider<CTX> for CeloPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: OpSpecId) -> bool {
        let changed = PrecompileProvider::<CTX>::set_spec(&mut self.inner, spec);
        if changed {
            self.warm_addresses = self.inner.precompiles().addresses_set().clone();
            self.warm_addresses.insert(TRANSFER_ADDRESS);
        }
        changed
    }

    fn warm_addresses(&self) -> &AddressSet {
        &self.warm_addresses
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        if inputs.bytecode_address != TRANSFER_ADDRESS {
            return self.inner.run(context, inputs);
        }
        let input = inputs.input.bytes(context);
        let outcome = if inputs.is_static {
            Ok(InstructionResult::PrecompileError)
        } else if inputs.gas_limit < TRANSFER_GAS {
            Ok(InstructionResult::PrecompileOOG)
        } else if inputs.caller != CELO_TOKEN || input.len() != 96 {
            Ok(InstructionResult::PrecompileError)
        } else {
            transfer(context, &input)
        };
        let mut gas = Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
        let result = match outcome {
            Ok(InstructionResult::Return) => {
                let _ = gas.record_regular_cost(TRANSFER_GAS);
                InstructionResult::Return
            }
            Ok(failure) => {
                gas.spend_all();
                failure
            }
            Err(error) => {
                *context.error() = Err(ContextError::Db(error));
                InstructionResult::FatalExternalError
            }
        };
        Ok(Some(InterpreterResult {
            result,
            gas,
            output: Bytes::new(),
        }))
    }
}

fn transfer<CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>>(
    context: &mut CTX,
    input: &[u8],
) -> Result<InstructionResult, <CTX::Db as Database>::Error> {
    let from = Address::from_slice(&input[12..32]);
    let to = Address::from_slice(&input[44..64]);
    let amount = U256::from_be_slice(&input[64..96]);
    let preserve_cold = !context.cfg().spec().is_enabled_in(OpSpecId::JOVIAN);
    let journal = context.journal_mut();
    let from_cold = preserve_cold && journal.load_account(from)?.is_cold;
    let to_cold = preserve_cold && journal.load_account(to)?.is_cold;
    let failure = journal.transfer(from, to, amount)?;
    // Pre-Jovian transfers must not warm previously cold accounts.
    for (address, was_cold) in [(from, from_cold), (to, to_cold)] {
        if was_cold {
            journal.load_account_mut(address)?.unsafe_mark_cold();
        }
    }
    Ok(if failure.is_some() {
        InstructionResult::PrecompileError
    } else {
        InstructionResult::Return
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_revm::{DefaultOp, OpContext};
    use revm::{
        context::CfgEnv,
        database::InMemoryDB,
        interpreter::{CallInput, CallScheme, CallValue},
        state::AccountInfo,
        Context,
    };

    fn setup(spec: OpSpecId) -> (OpContext<InMemoryDB>, CallInputs) {
        let from = Address::repeat_byte(0x11);
        let to = Address::repeat_byte(0x22);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            from,
            AccountInfo {
                balance: U256::from(10),
                ..Default::default()
            },
        );
        let ctx = Context::op()
            .with_cfg(CfgEnv::new_with_spec(spec))
            .with_db(db);
        let mut data = vec![0; 96];
        data[12..32].copy_from_slice(from.as_slice());
        data[44..64].copy_from_slice(to.as_slice());
        data[95] = 3;
        (
            ctx,
            CallInputs {
                input: CallInput::Bytes(data.into()),
                return_memory_offset: 0..0,
                gas_limit: TRANSFER_GAS + 100,
                reservoir: 0,
                bytecode_address: TRANSFER_ADDRESS,
                known_bytecode: Default::default(),
                target_address: TRANSFER_ADDRESS,
                caller: CELO_TOKEN,
                value: CallValue::Transfer(U256::ZERO),
                scheme: CallScheme::Call,
                is_static: false,
                charged_new_account_state_gas: false,
            },
        )
    }

    #[test]
    fn transfer_preserves_fork_warmth_and_frame_rollback() {
        for spec in [OpSpecId::ISTHMUS, OpSpecId::JOVIAN] {
            let (mut ctx, inputs) = setup(spec);
            let checkpoint = ctx.journal_mut().checkpoint();
            let mut provider = CeloPrecompiles::new(spec);
            let result = provider.run(&mut ctx, &inputs).unwrap().unwrap();
            assert_eq!(result.result, InstructionResult::Return);
            assert_eq!(result.gas.total_gas_spent(), TRANSFER_GAS);
            assert!(result.output.is_empty());
            for (address, balance) in [
                (Address::repeat_byte(0x11), 7),
                (Address::repeat_byte(0x22), 3),
            ] {
                let account = ctx.journal_mut().load_account(address).unwrap();
                assert_eq!(account.info.balance, U256::from(balance));
                assert_eq!(account.is_cold, spec == OpSpecId::ISTHMUS);
            }
            ctx.journal_mut().checkpoint_revert(checkpoint);
            assert_eq!(
                ctx.journal_mut()
                    .load_account(Address::repeat_byte(0x11))
                    .unwrap()
                    .info
                    .balance,
                U256::from(10)
            );
            assert_eq!(
                ctx.journal_mut()
                    .load_account(Address::repeat_byte(0x22))
                    .unwrap()
                    .info
                    .balance,
                U256::ZERO
            );
        }
    }

    #[test]
    fn invalid_transfers_fail_without_moving_balances() {
        for case in 0..5 {
            let (mut ctx, mut inputs) = setup(OpSpecId::JOVIAN);
            match case {
                0 => inputs.caller = Address::ZERO,
                1 => inputs.is_static = true,
                2 => inputs.gas_limit = TRANSFER_GAS - 1,
                3 => inputs.input = CallInput::Bytes(Bytes::new()),
                _ => {
                    let mut data = inputs.input.bytes(&ctx).to_vec();
                    data[95] = 11;
                    inputs.input = CallInput::Bytes(data.into());
                }
            }
            let result = CeloPrecompiles::new(OpSpecId::JOVIAN)
                .run(&mut ctx, &inputs)
                .unwrap()
                .unwrap();
            assert_eq!(
                result.result,
                if case == 2 {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                }
            );
            assert_eq!(
                ctx.journal_mut()
                    .load_account(Address::repeat_byte(0x11))
                    .unwrap()
                    .info
                    .balance,
                U256::from(10)
            );
            assert_eq!(
                ctx.journal_mut()
                    .load_account(Address::repeat_byte(0x22))
                    .unwrap()
                    .info
                    .balance,
                U256::ZERO
            );
        }
    }
}
