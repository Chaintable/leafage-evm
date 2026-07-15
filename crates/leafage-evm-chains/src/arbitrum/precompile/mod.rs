mod abi;
mod address_table;
mod aggregator;
mod arb_bls;
mod arb_info;
mod arb_sys;
mod arbos_acts;
mod arbos_test;
mod chain_config;
mod debug;
mod env;
mod filtered_transactions;
mod function_table;
mod gas_info;
mod native_token_manager;
mod owner;
mod owner_public;
mod registry;
mod retryable_tx;
mod state;
mod statistics;
mod stylus_dictionary;
mod stylus_runtime;
mod util;
mod wasm;
mod wasm_cache;

use self::env::ArbPrecompileInput;
pub use self::env::ArbitrumPrecompileEnv;
pub(crate) use self::stylus_runtime::{HostioHandler, StylusExecInput, StylusOutcome, StylusRuntime};
pub(crate) use self::wasm::{ArbWasm, PreparedStylusProgram};
use self::filtered_transactions::ArbFilteredTransactionsManager;
use self::registry::ArbitrumPrecompile;
use self::util::{
    charge_precompile_context_gas, decode_revert, empty_revert, to_interpreter_result,
};
use crate::arbitrum::evm::ArbitrumExecutionContext;
use crate::arbitrum::hardforks::ArbitrumHardfork;
use crate::arbitrum::tx::ArbitrumTxEnv;
use alloy::primitives::{address, Address, Bytes, U256};
use leafage_evm_types::{BlockEnv, CfgEnv};
use once_cell::race::OnceBox;
use revm::context::{ContextTr, LocalContextTr};
use revm::handler::{EthPrecompiles, PrecompileProvider};
use revm::interpreter::{CallInput, CallInputs, CallScheme, InterpreterResult};
use revm::precompile::{secp256r1, PrecompileResult, Precompiles};
use revm::primitives::Address as RevmAddress;
use revm::{Context, Journal};
use revm::{Database, DatabaseRef};
use std::boxed::Box;

pub const ARB_SYS_ADDRESS: Address = address!("0000000000000000000000000000000000000064");
pub const ARB_INFO_ADDRESS: Address = address!("0000000000000000000000000000000000000065");
pub const ARB_ADDRESS_TABLE_ADDRESS: Address = address!("0000000000000000000000000000000000000066");
pub const ARB_BLS_ADDRESS: Address = address!("0000000000000000000000000000000000000067");
pub const ARB_FUNCTION_TABLE_ADDRESS: Address =
    address!("0000000000000000000000000000000000000068");
pub const ARBOS_TEST_ADDRESS: Address = address!("0000000000000000000000000000000000000069");
pub const ARB_OWNER_PUBLIC_ADDRESS: Address = address!("000000000000000000000000000000000000006b");
pub const ARB_GAS_INFO_ADDRESS: Address = address!("000000000000000000000000000000000000006c");
pub const ARB_AGGREGATOR_ADDRESS: Address = address!("000000000000000000000000000000000000006d");
pub const ARB_RETRYABLE_TX_ADDRESS: Address = address!("000000000000000000000000000000000000006e");
pub const ARB_STATISTICS_ADDRESS: Address = address!("000000000000000000000000000000000000006f");
pub const ARB_OWNER_ADDRESS: Address = address!("0000000000000000000000000000000000000070");
pub const ARB_WASM_ADDRESS: Address = address!("0000000000000000000000000000000000000071");
pub const ARB_WASM_CACHE_ADDRESS: Address = address!("0000000000000000000000000000000000000072");
pub const ARB_NATIVE_TOKEN_MANAGER_ADDRESS: Address =
    address!("0000000000000000000000000000000000000073");
pub const ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS: Address =
    address!("0000000000000000000000000000000000000074");
pub const NODE_INTERFACE_ADDRESS: Address = address!("00000000000000000000000000000000000000c8");
pub const NODE_INTERFACE_DEBUG_ADDRESS: Address =
    address!("00000000000000000000000000000000000000c9");
pub const ARB_DEBUG_ADDRESS: Address = address!("00000000000000000000000000000000000000ff");
pub const ARBOS_ACTS_ADDRESS: Address = address!("00000000000000000000000000000000000a4b05");
pub(crate) const L1_PRICER_FUNDS_POOL_ADDRESS: Address =
    address!("A4B00000000000000000000000000000000000f6");
pub(crate) const BATCH_POSTER_ADDRESS: Address =
    address!("A4B000000000000000000073657175656e636572");

const STORAGE_READ_GAS: u64 = 800;
const BASE_PRECOMPILE_GAS: u64 = 0;
const STORAGE_WRITE_COST: u64 = 20_000;
const STORAGE_WRITE_ZERO_COST: u64 = 5_000;
const ASSUMED_SIMPLE_TX_SIZE: u64 = 140;
const TX_DATA_NON_ZERO_GAS: u64 = 16;
const RETRYABLE_LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;
const ADDRESS_ALIAS_OFFSET: U256 =
    alloy::primitives::uint!(0x1111000000000000000000000000000000001111_U256);
const MAX_GET_ALL_MEMBERS: u64 = 65_536;
const GAS_CONSTRAINTS_KEY: &[u8] = &[0];
const MULTI_GAS_CONSTRAINTS_KEY: &[u8] = &[1];
const NUM_RESOURCE_KIND: usize = 9;
const RESOURCE_KIND_SINGLE_DIM: usize = 6;
const ARBOS_VERSION_MULTI_GAS_CONSTRAINTS: u64 = 60;
const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_NATIVE_TOKEN: u64 = 41;
const ARBOS_VERSION_BLS_PRECOMPILES: u64 = 50;
const ARBOS_VERSION_TRANSACTION_FILTERING: u64 = 60;

pub type ArbitrumContext<DB> = Context<
    BlockEnv,
    ArbitrumTxEnv,
    CfgEnv<ArbitrumHardfork>,
    DB,
    Journal<DB>,
    ArbitrumExecutionContext,
>;

#[derive(Clone, Debug)]
pub struct ArbitrumPrecompiles {
    eth: EthPrecompiles,
    env: ArbitrumPrecompileEnv,
}

impl ArbitrumPrecompiles {
    pub fn new(spec: ArbitrumHardfork) -> Self {
        Self::new_with_env(spec, ArbitrumPrecompileEnv::default())
    }

    pub fn new_with_env(spec: ArbitrumHardfork, env: ArbitrumPrecompileEnv) -> Self {
        let mut eth = EthPrecompiles::new(spec.into());
        eth.precompiles = Self::eth_precompiles(env.current_arbos_version);
        Self { eth, env }
    }

    fn eth_precompiles(arbos_version: u64) -> &'static Precompiles {
        if arbos_version >= ARBOS_VERSION_BLS_PRECOMPILES {
            Precompiles::osaka()
        } else if arbos_version >= ARBOS_VERSION_STYLUS {
            cancun_with_p256()
        } else {
            Precompiles::berlin()
        }
    }

    fn run_checked_precompile<DB: Database + DatabaseRef>(
        &self,
        precompile: ArbitrumPrecompile,
        context: &mut ArbitrumContext<DB>,
        inputs: &CallInputs,
        data: &[u8],
        is_valid_call_context: bool,
    ) -> PrecompileResult {
        let purity = if matches!(precompile, ArbitrumPrecompile::ArbOwner) {
            None
        } else {
            let Some(purity) = precompile.purity(data) else {
                return decode_revert(inputs.gas_limit, "unknown Arbitrum precompile selector");
            };

            if purity.uses_precompile_context() && !is_valid_call_context {
                return decode_revert(inputs.gas_limit, "invalid Arbitrum precompile call context");
            }

            if purity.mutates_state() && inputs.is_static {
                return decode_revert(inputs.gas_limit, "state-changing staticcall");
            }

            if !inputs.call_value().is_zero() && !purity.accepts_value() {
                return decode_revert(inputs.gas_limit, "non-payable Arbitrum precompile method");
            }

            Some(purity)
        };

        let context_gas = if purity.is_some_and(|purity| purity.uses_precompile_context()) {
            STORAGE_READ_GAS
        } else {
            0
        };
        if context_gas > inputs.gas_limit {
            return Err(revm::precompile::PrecompileError::OutOfGas);
        }
        let precompile_gas_limit = inputs.gas_limit - context_gas;

        let charge = context.chain().current_poster_charge();
        let current_tx_l1_gas_fees = charge
            .map(|charge| charge.poster_fee)
            .unwrap_or(self.env.current_tx_l1_gas_fees);
        let current_tx_l1_gas_units = if charge.is_some() {
            0
        } else {
            self.env.current_tx_l1_gas_units
        };

        let result = precompile.run(ArbPrecompileInput {
            data,
            gas: precompile_gas_limit,
            caller: inputs.caller,
            value: inputs.call_value(),
            is_static: inputs.is_static,
            is_valid_call_context,
            current_arbos_version: self.env.current_arbos_version,
            current_tx_l1_gas_fees,
            current_tx_l1_gas_units,
            current_l1_block_number: self.env.current_l1_block_number,
            current_retryable_ticket: self.env.current_retryable_ticket,
            current_refund_to: self.env.current_refund_to,
            allow_debug_precompiles: self.env.allow_debug_precompiles,
            current_chain_config: self.env.current_chain_config.as_ref().map(Bytes::as_ref),
            context,
        });

        charge_precompile_context_gas(context_gas, inputs.gas_limit, result)
    }
}

impl<DB: Database + DatabaseRef> PrecompileProvider<ArbitrumContext<DB>> for ArbitrumPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: ArbitrumHardfork) -> bool {
        let spec_id = spec.into();
        let precompiles = Self::eth_precompiles(self.env.current_arbos_version);
        let changed = self.eth.spec != spec_id || !std::ptr::eq(self.eth.precompiles, precompiles);
        self.eth.spec = spec_id;
        self.eth.precompiles = precompiles;
        changed
    }

    fn run(
        &mut self,
        context: &mut ArbitrumContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let address = inputs.bytecode_address;
        let Some(precompile) = ArbitrumPrecompile::from_address(address)
            .filter(|precompile| precompile.is_active(self.env.current_arbos_version))
        else {
            return PrecompileProvider::<ArbitrumContext<DB>>::run(&mut self.eth, context, inputs);
        };

        if matches!(precompile, ArbitrumPrecompile::ArbDebug) && !self.env.allow_debug_precompiles {
            let result = empty_revert(inputs.gas_limit, inputs.gas_limit);
            return Ok(Some(to_interpreter_result(inputs.gas_limit, result)?));
        }

        let data = match &inputs.input {
            CallInput::SharedBuffer(range) => {
                let (_, _, _, _, _, local) = context.all_mut();
                local
                    .shared_memory_buffer_slice(range.clone())
                    .map(|slice| Bytes::copy_from_slice(&slice))
                    .unwrap_or_default()
            }
            CallInput::Bytes(bytes) => bytes.clone(),
        };

        let is_valid_call_context = inputs.target_address == address
            && !matches!(
                inputs.scheme,
                CallScheme::CallCode | CallScheme::DelegateCall
            );

        let result = if matches!(
            precompile,
            ArbitrumPrecompile::ArbFilteredTransactionsManager
        ) {
            match ArbFilteredTransactionsManager::wrapper_access(
                context,
                inputs.gas_limit,
                inputs.caller,
            ) {
                Ok((caller_is_filterer, wrapper_gas_used)) => {
                    let result = self.run_checked_precompile(
                        precompile,
                        context,
                        inputs,
                        data.as_ref(),
                        is_valid_call_context,
                    );
                    ArbFilteredTransactionsManager::finish_free_access_call(
                        inputs.gas_limit,
                        result,
                        caller_is_filterer,
                        wrapper_gas_used,
                    )
                }
                Err(_) => empty_revert(inputs.gas_limit, inputs.gas_limit),
            }
        } else {
            self.run_checked_precompile(
                precompile,
                context,
                inputs,
                data.as_ref(),
                is_valid_call_context,
            )
        };

        Ok(Some(to_interpreter_result(inputs.gas_limit, result)?))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = RevmAddress>> {
        let mut addresses: Vec<_> =
            PrecompileProvider::<ArbitrumContext<DB>>::warm_addresses(&self.eth).collect();
        let arbos_version = self.env.current_arbos_version;
        addresses.extend(
            ArbitrumPrecompile::ALL
                .into_iter()
                .filter(move |precompile| precompile.is_active(arbos_version))
                .map(ArbitrumPrecompile::address),
        );
        Box::new(addresses.into_iter())
    }

    fn contains(&self, address: &RevmAddress) -> bool {
        ArbitrumPrecompile::from_address(*address)
            .is_some_and(|precompile| precompile.is_active(self.env.current_arbos_version))
            || PrecompileProvider::<ArbitrumContext<DB>>::contains(&self.eth, address)
    }
}

fn cancun_with_p256() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::cancun().clone();
        precompiles.extend([secp256r1::P256VERIFY]);
        Box::new(precompiles)
    })
}

#[cfg(test)]
mod tests {
    use super::util::{alias_l1_address, copy_gas, inverse_alias_l1_address, signed_diff};
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::evm::ArbPosterCharge;
    use alloy::sol_types::SolCall;
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::handler::PrecompileProvider;
    use revm::interpreter::{CallValue, InstructionResult};
    use revm::precompile::u64_to_address;
    use revm::{Context, MainContext};

    fn context() -> ArbitrumContext<CacheDB<EmptyDB>> {
        let db = CacheDB::new(EmptyDB::default());
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        context
    }

    #[test]
    fn registers_phase_one_precompile_addresses() {
        let precompiles = ArbitrumPrecompiles::new_with_env(
            ArbitrumHardfork::Prague,
            ArbitrumPrecompileEnv {
                current_arbos_version: 60,
                ..Default::default()
            },
        );

        for address in ArbitrumPrecompile::ALL.map(ArbitrumPrecompile::address) {
            assert!(
                PrecompileProvider::<ArbitrumContext<EmptyDB>>::contains(&precompiles, &address),
                "missing Arbitrum precompile {address:?}"
            );
        }
    }

    #[test]
    fn active_precompiles_match_nitro_arbos_version_bands() {
        let contains = |arbos_version, address| {
            let precompiles = ArbitrumPrecompiles::new_with_env(
                ArbitrumHardfork::Prague,
                ArbitrumPrecompileEnv {
                    current_arbos_version: arbos_version,
                    ..Default::default()
                },
            );
            PrecompileProvider::<ArbitrumContext<EmptyDB>>::contains(&precompiles, &address)
        };
        let is_warm = |arbos_version, address| {
            let precompiles = ArbitrumPrecompiles::new_with_env(
                ArbitrumHardfork::Prague,
                ArbitrumPrecompileEnv {
                    current_arbos_version: arbos_version,
                    ..Default::default()
                },
            );
            let contains =
                PrecompileProvider::<ArbitrumContext<EmptyDB>>::warm_addresses(&precompiles)
                    .any(|warm| warm == address);
            contains
        };

        assert!(!contains(29, ARB_WASM_ADDRESS));
        assert!(!is_warm(29, ARB_WASM_ADDRESS));
        assert!(contains(30, ARB_WASM_ADDRESS));
        assert!(is_warm(30, ARB_WASM_ADDRESS));
        assert!(!contains(29, ARB_WASM_CACHE_ADDRESS));
        assert!(!is_warm(29, ARB_WASM_CACHE_ADDRESS));
        assert!(contains(30, ARB_WASM_CACHE_ADDRESS));
        assert!(is_warm(30, ARB_WASM_CACHE_ADDRESS));
        assert!(!contains(40, ARB_NATIVE_TOKEN_MANAGER_ADDRESS));
        assert!(!is_warm(40, ARB_NATIVE_TOKEN_MANAGER_ADDRESS));
        assert!(contains(41, ARB_NATIVE_TOKEN_MANAGER_ADDRESS));
        assert!(is_warm(41, ARB_NATIVE_TOKEN_MANAGER_ADDRESS));
        assert!(!contains(59, ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS));
        assert!(!is_warm(59, ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS));
        assert!(contains(60, ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS));
        assert!(is_warm(60, ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS));
    }

    #[test]
    fn standard_precompiles_match_nitro_arbos_version_bands() {
        let contains = |arbos_version, address| {
            let precompiles = ArbitrumPrecompiles::new_with_env(
                ArbitrumHardfork::Prague,
                ArbitrumPrecompileEnv {
                    current_arbos_version: arbos_version,
                    ..Default::default()
                },
            );
            PrecompileProvider::<ArbitrumContext<EmptyDB>>::contains(&precompiles, &address)
        };
        let is_warm = |arbos_version, address| {
            let precompiles = ArbitrumPrecompiles::new_with_env(
                ArbitrumHardfork::Prague,
                ArbitrumPrecompileEnv {
                    current_arbos_version: arbos_version,
                    ..Default::default()
                },
            );
            let contains =
                PrecompileProvider::<ArbitrumContext<EmptyDB>>::warm_addresses(&precompiles)
                    .any(|warm| warm == address);
            contains
        };

        let ecrecover = u64_to_address(0x01);
        let blake2f = u64_to_address(0x09);
        let kzg = u64_to_address(0x0a);
        let bls_g1_add = u64_to_address(0x0b);
        let bls_map_g2 = u64_to_address(0x11);
        let p256 = u64_to_address(0x100);

        assert!(contains(11, ecrecover));
        assert!(contains(11, blake2f));
        assert!(!contains(11, kzg));
        assert!(!is_warm(11, kzg));
        assert!(!contains(11, p256));
        assert!(!is_warm(11, p256));
        assert!(!contains(11, bls_g1_add));

        assert!(contains(30, kzg));
        assert!(is_warm(30, kzg));
        assert!(contains(30, p256));
        assert!(is_warm(30, p256));
        assert!(!contains(30, bls_g1_add));
        assert!(!is_warm(30, bls_g1_add));

        assert!(contains(49, p256));
        assert!(!contains(49, bls_map_g2));

        assert!(contains(50, kzg));
        assert!(contains(50, p256));
        assert!(contains(50, bls_g1_add));
        assert!(contains(50, bls_map_g2));
    }

    #[test]
    fn disabled_standard_precompile_is_not_executed() {
        let mut context = context();
        let mut precompiles = ArbitrumPrecompiles::new_with_env(
            ArbitrumHardfork::Prague,
            ArbitrumPrecompileEnv {
                current_arbos_version: 11,
                ..Default::default()
            },
        );
        let kzg = u64_to_address(0x0a);
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            bytecode_address: kzg,
            known_bytecode: None,
            target_address: kzg,
            caller: Address::from([1; 20]),
            value: CallValue::default(),
            scheme: CallScheme::Call,
            is_static: false,
        };

        let result = PrecompileProvider::<ArbitrumContext<CacheDB<EmptyDB>>>::run(
            &mut precompiles,
            &mut context,
            &inputs,
        )
        .expect("provider run should not fail");

        assert!(result.is_none());
    }

    #[test]
    fn current_poster_charge_units_are_not_added_twice() {
        let mut context = context();
        let l1_key = arbos_state::child_key(&[], arbos_state::L1_PRICING_SUBSPACE);
        let units_slot = arbos_state::slot_at(&l1_key, arbos_state::L1_UNITS_SINCE_UPDATE_OFFSET);
        context
            .journal_mut()
            .sstore(
                arbos_state::ARBOS_STATE_ADDRESS,
                units_slot,
                U256::from(1_023),
            )
            .expect("write units since update");
        context
            .chain_mut()
            .set_current_poster_charge(ArbPosterCharge {
                calldata_units: 23,
                poster_fee: U256::from(5),
                ..Default::default()
            });

        let mut precompiles = ArbitrumPrecompiles::new_with_env(
            ArbitrumHardfork::Prague,
            ArbitrumPrecompileEnv {
                current_arbos_version: 20,
                current_tx_l1_gas_units: 23,
                ..Default::default()
            },
        );
        let data = abi::IArbGasInfo::getL1PricingUnitsSinceUpdateCall {}.abi_encode();
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::from(data)),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            bytecode_address: ARB_GAS_INFO_ADDRESS,
            known_bytecode: None,
            target_address: ARB_GAS_INFO_ADDRESS,
            caller: Address::from([1; 20]),
            value: CallValue::default(),
            scheme: CallScheme::Call,
            is_static: false,
        };

        let result = PrecompileProvider::<ArbitrumContext<CacheDB<EmptyDB>>>::run(
            &mut precompiles,
            &mut context,
            &inputs,
        )
        .expect("provider run should not fail")
        .expect("ArbGasInfo should be handled");

        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(
            abi::IArbGasInfo::getL1PricingUnitsSinceUpdateCall::abi_decode_returns(
                result.output.as_ref()
            )
            .expect("decode return"),
            1_023
        );
    }

    #[test]
    fn provider_charges_context_gas_for_view_precompiles() {
        let mut context = context();
        let mut precompiles = ArbitrumPrecompiles::new_with_env(
            ArbitrumHardfork::Prague,
            ArbitrumPrecompileEnv {
                current_arbos_version: 11,
                ..Default::default()
            },
        );
        let data = abi::IArbSys::arbBlockNumberCall {}.abi_encode();
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::from(data)),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            bytecode_address: ARB_SYS_ADDRESS,
            known_bytecode: None,
            target_address: ARB_SYS_ADDRESS,
            caller: Address::from([1; 20]),
            value: CallValue::default(),
            scheme: CallScheme::Call,
            is_static: false,
        };

        let result = PrecompileProvider::<ArbitrumContext<CacheDB<EmptyDB>>>::run(
            &mut precompiles,
            &mut context,
            &inputs,
        )
        .expect("provider run should not fail")
        .expect("ArbSys should be handled");

        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.spent(), STORAGE_READ_GAS + copy_gas(32));
    }

    #[test]
    fn p256_gas_matches_nitro_arbos_transition() {
        let p256_gas = |arbos_version| {
            let mut context = context();
            let mut precompiles = ArbitrumPrecompiles::new_with_env(
                ArbitrumHardfork::Prague,
                ArbitrumPrecompileEnv {
                    current_arbos_version: arbos_version,
                    ..Default::default()
                },
            );
            let p256 = u64_to_address(0x100);
            let inputs = CallInputs {
                input: CallInput::Bytes(Bytes::new()),
                return_memory_offset: 0..0,
                gas_limit: 100_000,
                bytecode_address: p256,
                known_bytecode: None,
                target_address: p256,
                caller: Address::from([1; 20]),
                value: CallValue::default(),
                scheme: CallScheme::Call,
                is_static: false,
            };

            let result = PrecompileProvider::<ArbitrumContext<CacheDB<EmptyDB>>>::run(
                &mut precompiles,
                &mut context,
                &inputs,
            )
            .expect("provider run should not fail")
            .expect("P256 should be active");

            assert_eq!(result.result, InstructionResult::Return);
            result.gas.spent()
        };

        assert_eq!(p256_gas(30), 3_450);
        assert_eq!(p256_gas(49), 3_450);
        assert_eq!(p256_gas(50), 6_900);
    }

    #[test]
    fn provider_checks_arb_owner_access_before_selector_decode() {
        let mut context = context();
        let mut precompiles = ArbitrumPrecompiles::new_with_env(
            ArbitrumHardfork::Prague,
            ArbitrumPrecompileEnv {
                current_arbos_version: 60,
                ..Default::default()
            },
        );
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::copy_from_slice(&[0xff, 0xff, 0xff, 0xff])),
            return_memory_offset: 0..0,
            gas_limit: 10_000_000,
            bytecode_address: ARB_OWNER_ADDRESS,
            known_bytecode: None,
            target_address: ARB_OWNER_ADDRESS,
            caller: Address::from([1; 20]),
            value: CallValue::default(),
            scheme: CallScheme::Call,
            is_static: false,
        };

        let result = PrecompileProvider::<ArbitrumContext<CacheDB<EmptyDB>>>::run(
            &mut precompiles,
            &mut context,
            &inputs,
        )
        .expect("provider run should not fail")
        .expect("ArbOwner should be handled");

        assert_eq!(result.result, InstructionResult::Revert);
        assert!(result.gas.remaining() < inputs.gas_limit);
        assert!(result.gas.spent() < inputs.gas_limit);
    }

    #[test]
    fn l1_aliasing_matches_nitro_offset() {
        assert_eq!(
            alias_l1_address(Address::ZERO),
            address!("1111000000000000000000000000000000001111")
        );
        assert_eq!(
            alias_l1_address(address!("ffffffffffffffffffffffffffffffffffffffff")),
            address!("1111000000000000000000000000000000001110")
        );
        let address = address!("2222000000000000000000000000000000002222");
        assert_eq!(inverse_alias_l1_address(alias_l1_address(address)), address);
    }

    #[test]
    fn signed_diff_encodes_negative_twos_complement() {
        assert_eq!(
            signed_diff(U256::from(9), U256::from(4)).into_raw(),
            U256::from(5)
        );
        assert_eq!(
            signed_diff(U256::from(4), U256::from(9)).into_raw(),
            U256::ZERO.wrapping_sub(U256::from(5))
        );
    }
}
