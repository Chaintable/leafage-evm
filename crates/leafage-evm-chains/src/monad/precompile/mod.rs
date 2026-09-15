//! Precompile set (`execution/monad/monad_precompiles.cpp`,
//! `execution/ethereum/precompiles.cpp`, `monad_precompiles_gas_cost_impl.cpp`).
//!
//! * Ethereum precompiles follow the EVM revision of the Monad revision. The
//!   P256 precompile (`0x100`, EIP-7951, 6 900 gas) is active from MONAD_FOUR,
//!   one fork before Osaka.
//! * Pricing v1 (MONAD_SEVEN) multiplies the Ethereum gas of ecrecover (x2),
//!   ecadd (x2), ecmul (x5), pairing (x5), blake2f (x2) and point evaluation
//!   (x4).
//! * `0x1000` is the staking contract (callable from MONAD_FOUR, always a
//!   warm "precompile" address) and `0x1001` the reserve balance contract
//!   (MONAD_NINE). Both reject calls with `msg.flags != 0`, which includes
//!   `EVMC_DELEGATED`: an EIP-7702 account delegating to them cannot be
//!   called (all gas consumed). Ethereum precompiles behind a delegation
//!   run as empty code, like in revm.

mod reserve_balance;
mod staking;

use crate::monad::{MonadContext, MonadHardfork};
use alloy_evm::Database;
use once_cell::race::OnceBox;
use revm::bytecode::Bytecode;
use revm::context::{ContextTr, JournalTr, LocalContextTr};
use revm::handler::{EthPrecompiles, PrecompileProvider};
use revm::interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult};
use revm::precompile::{secp256r1, PrecompileError, Precompiles};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{address, Address, Bytes};

/// `staking::STAKING_CA`
pub const STAKING_CONTRACT_ADDRESS: Address = address!("0000000000000000000000000000000000001000");
/// `RESERVE_BALANCE_CA`
pub const RESERVE_BALANCE_CONTRACT_ADDRESS: Address =
    address!("0000000000000000000000000000000000001001");

const ECRECOVER: Address = address!("0000000000000000000000000000000000000001");
const ECADD: Address = address!("0000000000000000000000000000000000000006");
const ECMUL: Address = address!("0000000000000000000000000000000000000007");
const ECPAIRING: Address = address!("0000000000000000000000000000000000000008");
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");
const POINT_EVALUATION: Address = address!("000000000000000000000000000000000000000a");

/// Monad specification §4.3: pricing v1 precompile multipliers.
fn pricing_v1_factor(address: &Address) -> u64 {
    match *address {
        ECRECOVER | ECADD | BLAKE2F => 2,
        ECMUL | ECPAIRING => 5,
        POINT_EVALUATION => 4,
        _ => 1,
    }
}

fn prague_with_p256() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::prague().clone();
        precompiles.extend([secp256r1::P256VERIFY_OSAKA]);
        Box::new(precompiles)
    })
}

/// `resolve_precompile<MonadTraits<rev>>`: the Ethereum precompile table.
pub(crate) fn eth_precompiles(hardfork: MonadHardfork) -> &'static Precompiles {
    match hardfork.evm_spec() {
        SpecId::CANCUN => Precompiles::cancun(),
        SpecId::PRAGUE => prague_with_p256(),
        _ => Precompiles::osaka(),
    }
}

#[derive(Debug, Clone)]
pub struct MonadPrecompiles {
    eth: EthPrecompiles,
    hardfork: MonadHardfork,
}

impl MonadPrecompiles {
    pub fn new(hardfork: MonadHardfork) -> Self {
        Self {
            eth: EthPrecompiles {
                precompiles: eth_precompiles(hardfork),
                spec: hardfork.into(),
            },
            hardfork,
        }
    }

    pub fn hardfork(&self) -> MonadHardfork {
        self.hardfork
    }

    fn monad_addresses(&self) -> impl Iterator<Item = Address> {
        [
            Some(STAKING_CONTRACT_ADDRESS),
            self.hardfork
                .is_reserve_balance_enabled()
                .then_some(RESERVE_BALANCE_CONTRACT_ADDRESS),
        ]
        .into_iter()
        .flatten()
    }

    /// Run an Ethereum precompile with a pricing v1 gas multiplier: the
    /// gas check `msg.gas < factor * cost` is the same as running the
    /// precompile with `gas_limit / factor`, and the charged gas is scaled back.
    fn run_scaled<CTX: ContextTr>(
        &self,
        context: &mut CTX,
        inputs: &CallInputs,
        factor: u64,
    ) -> Result<Option<InterpreterResult>, String> {
        let Some(precompile) = self.eth.precompiles.get(&inputs.bytecode_address) else {
            return Ok(None);
        };
        let input = call_input_bytes(context, inputs);
        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(inputs.gas_limit),
            output: Bytes::new(),
        };
        match precompile.execute(&input, inputs.gas_limit / factor) {
            Ok(output) => {
                let gas_used = output.gas_used.saturating_mul(factor);
                let underflow = result.gas.record_cost(gas_used);
                assert!(underflow, "Gas underflow is not possible");
                result.result = if output.reverted {
                    InstructionResult::Revert
                } else {
                    InstructionResult::Return
                };
                result.output = output.bytes;
            }
            Err(PrecompileError::Fatal(e)) => return Err(e),
            Err(e) => {
                result.result = if e.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                };
                if !e.is_oog() && context.journal().depth() == 1 {
                    context
                        .local_mut()
                        .set_precompile_error_context(e.to_string());
                }
            }
        }
        Ok(Some(result))
    }
}

fn call_input_bytes<CTX: ContextTr>(context: &mut CTX, inputs: &CallInputs) -> Bytes {
    match &inputs.input {
        CallInput::SharedBuffer(range) => context
            .local()
            .shared_memory_buffer_slice(range.clone())
            .map(|slice| Bytes::copy_from_slice(&slice))
            .unwrap_or_default(),
        CallInput::Bytes(bytes) => bytes.clone(),
    }
}

/// `check_call_monad_precompile`: `msg.kind != EVMC_CALL || msg.flags != 0`
/// is `EVMC_REJECTED`, all gas of the call is consumed.
pub(crate) fn rejected(gas_limit: u64) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::PrecompileError,
        gas: Gas::new_spent(gas_limit),
        output: Bytes::new(),
    }
}

impl MonadPrecompiles {
    fn is_monad_address(&self, address: &Address) -> bool {
        self.monad_addresses().any(|a| a == *address)
    }

    /// The call target is an EIP-7702 account delegating to a Monad
    /// precompile (`EVMC_DELEGATED` set on `code_address`).
    fn delegates_to_monad_precompile<DB: Database>(
        &self,
        context: &mut MonadContext<DB>,
        inputs: &CallInputs,
    ) -> Result<bool, String> {
        if !self.hardfork.is_staking_enabled() || inputs.known_bytecode.is_none() {
            return Ok(false);
        }
        // The account was loaded by the CALL instruction, this is a cache hit.
        let account = context
            .journal_mut()
            .load_account_with_code(inputs.bytecode_address)
            .map_err(|e| e.to_string())?;
        Ok(account
            .info
            .code
            .as_ref()
            .and_then(Bytecode::eip7702_address)
            .is_some_and(|delegate| self.is_monad_address(&delegate)))
    }
}

impl<DB: Database> PrecompileProvider<MonadContext<DB>> for MonadPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: MonadHardfork) -> bool {
        if spec == self.hardfork {
            return false;
        }
        *self = Self::new(spec);
        true
    }

    fn run(
        &mut self,
        context: &mut MonadContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let address = inputs.bytecode_address;
        if address == STAKING_CONTRACT_ADDRESS {
            if !self.hardfork.is_staking_enabled() {
                return Ok(None);
            }
            let input = call_input_bytes(context, inputs);
            return staking::run(context, inputs, &input, self.hardfork).map(Some);
        }
        if address == RESERVE_BALANCE_CONTRACT_ADDRESS {
            if !self.hardfork.is_reserve_balance_enabled() {
                return Ok(None);
            }
            let input = call_input_bytes(context, inputs);
            return reserve_balance::run(context, inputs, &input).map(Some);
        }
        if self.eth.precompiles.contains(&address) {
            let factor = if self.hardfork.is_pricing_v1_enabled() {
                pricing_v1_factor(&address)
            } else {
                1
            };
            if factor == 1 {
                return PrecompileProvider::<MonadContext<DB>>::run(&mut self.eth, context, inputs);
            }
            return self.run_scaled(context, inputs, factor);
        }
        if self.delegates_to_monad_precompile(context, inputs)? {
            return Ok(Some(rejected(inputs.gas_limit)));
        }
        Ok(None)
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        Box::new(
            self.eth
                .precompiles
                .addresses()
                .cloned()
                .chain(self.monad_addresses()),
        )
    }

    fn contains(&self, address: &Address) -> bool {
        self.eth.precompiles.contains(address) || self.is_monad_address(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::api::MonadContext;
    use revm::database_interface::EmptyDB;
    use revm::precompile::u64_to_address;

    fn provider(hardfork: MonadHardfork) -> MonadPrecompiles {
        MonadPrecompiles::new(hardfork)
    }

    fn contains(hardfork: MonadHardfork, address: u64) -> bool {
        PrecompileProvider::<MonadContext<EmptyDB>>::contains(
            &provider(hardfork),
            &u64_to_address(address),
        )
    }

    #[test]
    fn precompile_tables_follow_monad_revisions() {
        // Cancun: 0x01..0x0a, no BLS, no P256
        let cancun = eth_precompiles(MonadHardfork::MonadThree);
        assert_eq!(cancun.len(), 10);
        assert!(!cancun.contains(&u64_to_address(0x100)));

        // Prague: BLS (0x0b..0x11) and P256 (0x100) at 6900 gas
        let prague = eth_precompiles(MonadHardfork::MonadSix);
        assert_eq!(prague.len(), 18);
        assert_eq!(
            prague
                .get(&u64_to_address(0x100))
                .unwrap()
                .execute(&[], u64::MAX)
                .unwrap()
                .gas_used,
            6_900
        );

        // Osaka: same set
        let osaka = eth_precompiles(MonadHardfork::MonadNine);
        assert_eq!(osaka.len(), 18);
        assert!(osaka.contains(&u64_to_address(0x100)));
    }

    #[test]
    fn monad_contracts_are_precompile_addresses() {
        assert!(contains(MonadHardfork::MonadThree, 0x1000));
        assert!(!contains(MonadHardfork::MonadEight, 0x1001));
        assert!(contains(MonadHardfork::MonadNine, 0x1001));
        assert!(contains(MonadHardfork::MonadTen, 0x1000));
    }

    #[test]
    fn pricing_v1_factors() {
        assert_eq!(pricing_v1_factor(&u64_to_address(1)), 2);
        assert_eq!(pricing_v1_factor(&u64_to_address(6)), 2);
        assert_eq!(pricing_v1_factor(&u64_to_address(7)), 5);
        assert_eq!(pricing_v1_factor(&u64_to_address(8)), 5);
        assert_eq!(pricing_v1_factor(&u64_to_address(9)), 2);
        assert_eq!(pricing_v1_factor(&u64_to_address(0xa)), 4);
        assert_eq!(pricing_v1_factor(&u64_to_address(2)), 1);
        assert_eq!(pricing_v1_factor(&u64_to_address(0x100)), 1);
    }
}
