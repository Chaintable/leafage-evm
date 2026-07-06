use super::abi::IArbWasm;
use super::state::{ArbStorage, StylusParams, StylusProgramError, WasmActivation, WasmProgram};
use super::stylus_dictionary::{program_dictionary_owned, PROGRAM_DICTIONARY_ID};
use super::stylus_runtime::{ActivatedWasm, StylusRuntime, StylusRuntimeError};
use super::util::{dispatch, empty_revert, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext, ARB_WASM_ADDRESS};
use crate::arbitrum::arbos_state;
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use alloy::sol_types::{SolError, SolValue};
use revm::context::ContextTr;
use revm::context_interface::{Block, Cfg};
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;
use std::io::{Cursor, Read};

const PAGE_RAMP: u64 = 620_674_314;
const ARBITRUM_START_TIME: u64 = 1_421_388_000;
const MIN_INIT_GAS_UNITS: u64 = 128;
const MIN_CACHED_GAS_UNITS: u64 = 32;
const COST_SCALAR_PERCENT: u64 = 2;
const COPY_GAS: u64 = 3;
const COLD_ACCOUNT_ACCESS_GAS: u64 = 2_600;
const WARM_STORAGE_READ_GAS: u64 = 100;
const LOG_GAS: u64 = 375;
const LOG_TOPIC_GAS: u64 = 375;
const LOG_DATA_GAS: u64 = 8;
const ACTIVATION_FIXED_GAS: u64 = 1_659_168;
const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_CHARGING_FIXES: u64 = 32;
const ARBOS_VERSION_WASM_ACTIVATION_GAS: u64 = 59;
const ARBOS_VERSION_STYLUS_CONTRACT_LIMIT: u64 = 60;
const STYLUS_CLASSIC_PREFIX: &[u8] = &[0xef, 0xf0, 0x00];
const STYLUS_FRAGMENT_PREFIX: &[u8] = &[0xef, 0xf0, 0x01];
const STYLUS_ROOT_PREFIX: &[u8] = &[0xef, 0xf0, 0x02];
const STYLUS_HEADER_LEN: usize = 4;
const STYLUS_EMPTY_DICTIONARY: u8 = 0;
const STYLUS_PROGRAM_DICTIONARY: u8 = PROGRAM_DICTIONARY_ID;

pub(super) struct ArbWasm;

#[derive(Debug)]
struct StylusRoot {
    dictionary: u8,
    decompressed_len: u32,
    fragments: Vec<Address>,
}

#[derive(Debug)]
enum ArbWasmError {
    Precompile(PrecompileError),
    Program(StylusProgramError),
    ActivationRuntime(StylusRuntimeError),
    NonSolidityError,
    ProgramNotWasm,
    ProgramUpToDate,
    ProgramInsufficientValue { have: U256, want: U256 },
}

impl From<PrecompileError> for ArbWasmError {
    fn from(error: PrecompileError) -> Self {
        Self::Precompile(error)
    }
}

impl From<StylusProgramError> for ArbWasmError {
    fn from(error: StylusProgramError) -> Self {
        Self::Program(error)
    }
}

impl ArbWasm {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let value = input.value;
        let is_static = input.is_static;
        let context = input.context;
        dispatch::<IArbWasm::IArbWasmCalls>(data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            if storage.arbos_version()? < ARBOS_VERSION_STYLUS {
                return empty_revert(gas_limit, gas_limit);
            }
            match call {
                IArbWasm::IArbWasmCalls::activateProgram(call) => {
                    if is_static {
                        return empty_revert(gas_limit, gas_limit);
                    }
                    match Self::activate_program(&mut storage, caller, value, call.program) {
                        Ok((version, data_fee)) => finish_call::<IArbWasm::activateProgramCall>(
                            gas_limit,
                            storage.gas_used,
                            (version, data_fee).into(),
                        ),
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::codehashKeepalive(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    match Self::codehash_keepalive(&mut storage, caller, value, call.codehash) {
                        Ok(()) => finish_call::<IArbWasm::codehashKeepaliveCall>(
                            gas_limit,
                            storage.gas_used,
                            ().into(),
                        ),
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::stylusVersion(_) => {
                    let ret = storage.stylus_params()?.version;
                    finish_call::<IArbWasm::stylusVersionCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::inkPrice(_) => {
                    let ret = storage.stylus_params()?.ink_price;
                    finish_call::<IArbWasm::inkPriceCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::maxStackDepth(_) => {
                    let ret = storage.stylus_params()?.max_stack_depth;
                    finish_call::<IArbWasm::maxStackDepthCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::freePages(_) => {
                    let ret = storage.stylus_params()?.free_pages;
                    finish_call::<IArbWasm::freePagesCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::pageGas(_) => {
                    let ret = storage.stylus_params()?.page_gas;
                    finish_call::<IArbWasm::pageGasCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::pageRamp(_) => {
                    storage.stylus_params()?;
                    finish_call::<IArbWasm::pageRampCall>(gas_limit, storage.gas_used, PAGE_RAMP)
                }
                IArbWasm::IArbWasmCalls::pageLimit(_) => {
                    let ret = storage.stylus_params()?.page_limit;
                    finish_call::<IArbWasm::pageLimitCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::minInitGas(_) => {
                    let params = storage.stylus_params()?;
                    if storage.arbos_version()? < ARBOS_VERSION_STYLUS_CHARGING_FIXES {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    let ret = (
                        u64::from(params.min_init_gas) * MIN_INIT_GAS_UNITS,
                        u64::from(params.min_cached_init_gas) * MIN_CACHED_GAS_UNITS,
                    );
                    finish_call::<IArbWasm::minInitGasCall>(gas_limit, storage.gas_used, ret.into())
                }
                IArbWasm::IArbWasmCalls::initCostScalar(_) => {
                    let ret =
                        u64::from(storage.stylus_params()?.init_cost_scalar) * COST_SCALAR_PERCENT;
                    finish_call::<IArbWasm::initCostScalarCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::expiryDays(_) => {
                    let ret = storage.stylus_params()?.expiry_days;
                    finish_call::<IArbWasm::expiryDaysCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::keepaliveDays(_) => {
                    let ret = storage.stylus_params()?.keepalive_days;
                    finish_call::<IArbWasm::keepaliveDaysCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::blockCacheSize(_) => {
                    let ret = storage.stylus_params()?.block_cache_size;
                    finish_call::<IArbWasm::blockCacheSizeCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::activationGas(_) => {
                    if storage.arbos_version()? < ARBOS_VERSION_WASM_ACTIVATION_GAS {
                        return empty_revert(gas_limit, gas_limit);
                    }
                    let ret = storage.wasm_activation_gas()?;
                    finish_call::<IArbWasm::activationGasCall>(gas_limit, storage.gas_used, ret)
                }
                IArbWasm::IArbWasmCalls::codehashVersion(call) => {
                    match Self::active_program(&mut storage, call.codehash) {
                        Ok(program) => finish_call::<IArbWasm::codehashVersionCall>(
                            gas_limit,
                            storage.gas_used,
                            program.version,
                        ),
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::codehashAsmSize(call) => {
                    match Self::active_program(&mut storage, call.codehash) {
                        Ok(program) => finish_call::<IArbWasm::codehashAsmSizeCall>(
                            gas_limit,
                            storage.gas_used,
                            program.asm_estimate_kb.saturating_mul(1024),
                        ),
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::programVersion(call) => {
                    let code_hash = storage.account_code_hash(call.program)?;
                    match Self::active_program(&mut storage, code_hash) {
                        Ok(program) => finish_call::<IArbWasm::programVersionCall>(
                            gas_limit,
                            storage.gas_used,
                            program.version,
                        ),
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::programInitGas(call) => {
                    let code_hash = storage.account_code_hash(call.program)?;
                    let params = storage.stylus_params()?;
                    match Self::active_program_with_params(&mut storage, code_hash, params) {
                        Ok(program) => {
                            let ret = Self::program_init_gas(program, params);
                            finish_call::<IArbWasm::programInitGasCall>(
                                gas_limit,
                                storage.gas_used,
                                ret.into(),
                            )
                        }
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::programMemoryFootprint(call) => {
                    let code_hash = storage.account_code_hash(call.program)?;
                    match Self::active_program(&mut storage, code_hash) {
                        Ok(program) => finish_call::<IArbWasm::programMemoryFootprintCall>(
                            gas_limit,
                            storage.gas_used,
                            program.footprint,
                        ),
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
                IArbWasm::IArbWasmCalls::programTimeLeft(call) => {
                    let code_hash = storage.account_code_hash(call.program)?;
                    let params = storage.stylus_params()?;
                    match Self::active_program_with_params(&mut storage, code_hash, params) {
                        Ok(program) => {
                            let expiry = u64::from(params.expiry_days) * 24 * 60 * 60;
                            let ret = expiry.saturating_sub(Self::program_age(&storage, program));
                            finish_call::<IArbWasm::programTimeLeftCall>(
                                gas_limit,
                                storage.gas_used,
                                ret,
                            )
                        }
                        Err(error) => Self::handle_error(&mut storage, gas_limit, error),
                    }
                }
            }
        })
    }

    fn activate_program<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        value: U256,
        program: Address,
    ) -> Result<(u16, U256), ArbWasmError> {
        let activation_gas = storage.wasm_activation_gas()?;
        storage.burn(activation_gas)?;
        storage.burn(ACTIVATION_FIXED_GAS)?;

        let params = storage.stylus_params()?;
        let (code, code_hash) = storage.account_code_and_hash(program)?;
        let current_program = storage.wasm_program(code_hash)?;
        if Self::program_is_up_to_date(storage, current_program, params) {
            return Err(ArbWasmError::ProgramUpToDate);
        }

        let wasm = Self::decode_stylus_wasm(storage, params, &code)?;

        let activated = Self::activate_program_runtime(storage, program, code_hash, &wasm, params)?;
        let ret = Self::finish_activation(
            storage,
            caller,
            value,
            program,
            code_hash,
            params,
            current_program.cached,
            activated.activation,
        )?;
        storage
            .context
            .chain_mut()
            .insert_activated_wasm_module(activated.activation.module_hash, activated.module);
        Ok(ret)
    }

    fn activate_program_runtime<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        _program: Address,
        code_hash: B256,
        wasm: &[u8],
        params: StylusParams,
    ) -> Result<ActivatedWasm, ArbWasmError> {
        let arbos_version = storage.arbos_version()?;
        let page_limit = storage
            .context
            .chain()
            .remaining_stylus_page_limit(params.page_limit);
        let supplied_gas = storage.gas_left();
        let mut gas_left = supplied_gas;
        let result = StylusRuntime::activate_from_env(
            wasm,
            code_hash,
            params,
            page_limit,
            arbos_version,
            &mut gas_left,
        );
        if gas_left < supplied_gas {
            storage.burn(supplied_gas - gas_left)?;
        }
        match result {
            Ok(activation) => Ok(activation),
            Err(StylusRuntimeError::OutOfInk) => Err(PrecompileError::OutOfGas.into()),
            Err(error) => Err(ArbWasmError::ActivationRuntime(error)),
        }
    }

    fn finish_activation<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        value: U256,
        program: Address,
        code_hash: B256,
        params: StylusParams,
        cached: bool,
        activation: WasmActivation,
    ) -> Result<(u16, U256), ArbWasmError> {
        let timestamp = storage.context.block().timestamp().to::<u64>();
        let module_hash = activation.module_hash;
        let data_fee = storage
            .save_activated_wasm_program(code_hash, params, activation, timestamp, cached)?;
        Self::pay_activation_data_fee(storage, caller, value, data_fee)?;
        Self::emit_program_activated(
            storage,
            code_hash,
            module_hash,
            program,
            data_fee,
            params.version,
        )?;
        Ok((params.version, data_fee))
    }

    fn active_program<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code_hash: B256,
    ) -> Result<WasmProgram, ArbWasmError> {
        let params = storage.stylus_params()?;
        Self::active_program_with_params(storage, code_hash, params)
    }

    fn active_program_with_params<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code_hash: B256,
        params: StylusParams,
    ) -> Result<WasmProgram, ArbWasmError> {
        let timestamp = storage.context.block().timestamp().to::<u64>();
        storage
            .active_wasm_program(code_hash, timestamp, params)
            .map_err(Into::into)
    }

    fn program_is_up_to_date<DB: Database>(
        storage: &ArbStorage<'_, ArbitrumContext<DB>>,
        program: WasmProgram,
        params: StylusParams,
    ) -> bool {
        if program.version == 0 || program.version != params.version {
            return false;
        }
        let expiry = u64::from(params.expiry_days) * 24 * 60 * 60;
        Self::program_age(storage, program) <= expiry
    }

    fn codehash_keepalive<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        value: U256,
        code_hash: B256,
    ) -> Result<(), ArbWasmError> {
        let params = storage.stylus_params()?;
        let timestamp = storage.context.block().timestamp().to::<u64>();
        let data_fee = storage.keepalive_wasm_program(code_hash, timestamp, params)?;
        Self::pay_activation_data_fee(storage, caller, value, data_fee)?;
        Self::emit_program_lifetime_extended(storage, code_hash, data_fee)?;
        Ok(())
    }

    fn pay_activation_data_fee<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        value: U256,
        data_fee: U256,
    ) -> Result<(), ArbWasmError> {
        if value < data_fee {
            return Err(ArbWasmError::ProgramInsufficientValue {
                have: value,
                want: data_fee,
            });
        }
        let network = storage.read_address(&[], arbos_state::NETWORK_FEE_ACCOUNT_OFFSET)?;
        storage.transfer_balance(ARB_WASM_ADDRESS, network, data_fee)?;
        storage
            .transfer_balance(ARB_WASM_ADDRESS, caller, value - data_fee)
            .map_err(Into::into)
    }

    fn emit_program_lifetime_extended<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code_hash: B256,
        data_fee: U256,
    ) -> Result<(), PrecompileError> {
        storage.burn(Self::program_lifetime_extended_event_gas())?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_WASM_ADDRESS,
            vec![
                keccak256("ProgramLifetimeExtended(bytes32,uint256)"),
                code_hash,
            ],
            data_fee.abi_encode().into(),
        ));
        Ok(())
    }

    fn emit_program_activated<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code_hash: B256,
        module_hash: B256,
        program: Address,
        data_fee: U256,
        version: u16,
    ) -> Result<(), PrecompileError> {
        storage.burn(Self::program_activated_event_gas())?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_WASM_ADDRESS,
            vec![
                keccak256("ProgramActivated(bytes32,bytes32,address,uint256,uint16)"),
                code_hash,
            ],
            (module_hash, program, data_fee, version)
                .abi_encode()
                .into(),
        ));
        Ok(())
    }

    fn program_activated_event_gas() -> u64 {
        LOG_GAS + 2 * LOG_TOPIC_GAS + 4 * 32 * LOG_DATA_GAS
    }

    fn program_lifetime_extended_event_gas() -> u64 {
        LOG_GAS + 2 * LOG_TOPIC_GAS + 32 * LOG_DATA_GAS
    }

    fn handle_error<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        error: ArbWasmError,
    ) -> PrecompileResult {
        match error {
            ArbWasmError::Precompile(PrecompileError::OutOfGas)
            | ArbWasmError::Program(StylusProgramError::Precompile(PrecompileError::OutOfGas)) => {
                storage.burn_out();
                Self::empty_revert(gas_limit, gas_limit)
            }
            ArbWasmError::Precompile(PrecompileError::Other(_))
            | ArbWasmError::Program(StylusProgramError::Precompile(PrecompileError::Other(_))) => {
                Self::empty_revert(gas_limit, storage.gas_used)
            }
            ArbWasmError::Precompile(error)
            | ArbWasmError::Program(StylusProgramError::Precompile(error)) => Err(error),
            ArbWasmError::ActivationRuntime(error) => {
                let _ = error.message();
                storage.burn_out();
                Self::empty_revert(gas_limit, gas_limit)
            }
            ArbWasmError::NonSolidityError => Self::empty_revert(gas_limit, storage.gas_used),
            ArbWasmError::Program(StylusProgramError::ProgramNotActivated) => {
                Self::custom_error(storage, gas_limit, IArbWasm::ProgramNotActivated {})
            }
            ArbWasmError::ProgramNotWasm => {
                Self::custom_error(storage, gas_limit, IArbWasm::ProgramNotWasm {})
            }
            ArbWasmError::ProgramUpToDate => {
                Self::custom_error(storage, gas_limit, IArbWasm::ProgramUpToDate {})
            }
            ArbWasmError::Program(StylusProgramError::ProgramNeedsUpgrade {
                version,
                stylus_version,
            }) => Self::custom_error(
                storage,
                gas_limit,
                IArbWasm::ProgramNeedsUpgrade {
                    version,
                    stylusVersion: stylus_version,
                },
            ),
            ArbWasmError::Program(StylusProgramError::ProgramExpired { age }) => {
                Self::custom_error(
                    storage,
                    gas_limit,
                    IArbWasm::ProgramExpired { ageInSeconds: age },
                )
            }
            ArbWasmError::Program(StylusProgramError::ProgramKeepaliveTooSoon { age }) => {
                Self::custom_error(
                    storage,
                    gas_limit,
                    IArbWasm::ProgramKeepaliveTooSoon { ageInSeconds: age },
                )
            }
            ArbWasmError::ProgramInsufficientValue { have, want } => Self::custom_error(
                storage,
                gas_limit,
                IArbWasm::ProgramInsufficientValue { have, want },
            ),
        }
    }

    fn custom_error<DB: Database, T: SolError>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        error: T,
    ) -> PrecompileResult {
        Self::revert_bytes(storage, gas_limit, error.abi_encode().into())
    }

    fn revert_bytes<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        bytes: Bytes,
    ) -> PrecompileResult {
        if storage.burn(Self::copy_gas(bytes.len())).is_err() {
            storage.burn_out();
            return Self::empty_revert(gas_limit, gas_limit);
        }
        Ok(revm::precompile::PrecompileOutput::new_reverted(
            storage.gas_used,
            bytes,
        ))
    }

    fn empty_revert(gas_limit: u64, gas_used: u64) -> PrecompileResult {
        if gas_used > gas_limit {
            return Err(PrecompileError::OutOfGas);
        }
        Ok(revm::precompile::PrecompileOutput::new_reverted(
            gas_used,
            Bytes::new(),
        ))
    }

    fn copy_gas(byte_count: usize) -> u64 {
        COPY_GAS.saturating_mul((byte_count as u64).div_ceil(32))
    }

    fn program_init_gas(program: WasmProgram, params: StylusParams) -> (u64, u64) {
        let cached = u64::from(params.min_cached_init_gas) * MIN_CACHED_GAS_UNITS
            + div_ceil_u64(
                u64::from(program.cached_cost)
                    .saturating_mul(u64::from(params.cached_cost_scalar) * COST_SCALAR_PERCENT),
                100,
            );
        let mut init = u64::from(params.min_init_gas) * MIN_INIT_GAS_UNITS
            + div_ceil_u64(
                u64::from(program.init_cost)
                    .saturating_mul(u64::from(params.init_cost_scalar) * COST_SCALAR_PERCENT),
                100,
            );
        if params.version > 1 {
            init = init.saturating_add(cached);
        }
        (init, cached)
    }

    fn program_age<DB: Database>(
        storage: &ArbStorage<'_, ArbitrumContext<DB>>,
        program: WasmProgram,
    ) -> u64 {
        let seconds = u64::from(program.activated_at).saturating_mul(3600);
        let activated_at = ARBITRUM_START_TIME.saturating_add(seconds);
        storage
            .context
            .block()
            .timestamp()
            .to::<u64>()
            .saturating_sub(activated_at)
    }

    fn decode_stylus_wasm<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        params: StylusParams,
        code: &[u8],
    ) -> Result<Vec<u8>, ArbWasmError> {
        if code.is_empty() {
            return Err(ArbWasmError::ProgramNotWasm);
        }
        if Self::has_stylus_prefix(code, STYLUS_CLASSIC_PREFIX) {
            return Self::check_classic_stylus_code(code, params.max_wasm_size);
        }

        let arbos_version = storage.arbos_version()?;
        if arbos_version < ARBOS_VERSION_STYLUS_CONTRACT_LIMIT {
            return Err(ArbWasmError::NonSolidityError);
        }

        if Self::has_stylus_prefix(code, STYLUS_ROOT_PREFIX) {
            return Self::check_stylus_root(storage, code, params);
        }
        if Self::has_stylus_prefix(code, STYLUS_FRAGMENT_PREFIX) {
            return Err(ArbWasmError::NonSolidityError);
        }
        Err(ArbWasmError::ProgramNotWasm)
    }

    fn has_stylus_prefix(code: &[u8], prefix: &[u8]) -> bool {
        code.len() > prefix.len() && code.starts_with(prefix)
    }

    fn check_stylus_root<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code: &[u8],
        params: StylusParams,
    ) -> Result<Vec<u8>, ArbWasmError> {
        let root = Self::parse_stylus_root(code, params)?;
        let max_code_size = storage.context.cfg().max_code_size();
        let compressed = Self::read_stylus_fragments(storage, &root.fragments, max_code_size)?;
        Self::check_stylus_dictionary(root.dictionary)?;
        let wasm =
            Self::decompress_stylus_payload(&compressed, root.dictionary, root.decompressed_len)?;
        if wasm.len() != root.decompressed_len as usize {
            return Err(ArbWasmError::NonSolidityError);
        }
        Ok(wasm)
    }

    fn parse_stylus_root(code: &[u8], params: StylusParams) -> Result<StylusRoot, ArbWasmError> {
        if code.len() < 8 {
            return Err(ArbWasmError::NonSolidityError);
        }

        let decompressed_len = u32::from_be_bytes([code[4], code[5], code[6], code[7]]);
        let address_bytes = code.len() - 8;
        if address_bytes % 20 != 0 {
            return Err(ArbWasmError::NonSolidityError);
        }

        let fragment_count = address_bytes / 20;
        if decompressed_len > params.max_wasm_size {
            return Err(ArbWasmError::NonSolidityError);
        }
        if fragment_count > usize::from(params.max_fragment_count) {
            return Err(ArbWasmError::NonSolidityError);
        }
        if fragment_count == 0 {
            return Err(ArbWasmError::NonSolidityError);
        }

        let dictionary = code[3];
        let fragments = code[8..]
            .chunks_exact(20)
            .map(Address::from_slice)
            .collect();
        Ok(StylusRoot {
            dictionary,
            decompressed_len,
            fragments,
        })
    }

    fn read_stylus_fragments<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        fragments: &[Address],
        max_code_size: usize,
    ) -> Result<Vec<u8>, ArbWasmError> {
        let mut compressed = Vec::new();
        for fragment in fragments {
            let is_cold = !storage.account_is_warm(*fragment);
            Self::ensure_can_read_max_fragment(storage, is_cold, max_code_size)?;
            let (code, _) = storage.account_code(*fragment)?;
            storage.burn(Self::fragment_read_gas(is_cold, code.len()))?;
            compressed.extend_from_slice(Self::stylus_fragment_payload(&code)?);
        }
        Ok(compressed)
    }

    fn ensure_can_read_max_fragment<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        is_cold: bool,
        max_code_size: usize,
    ) -> Result<(), ArbWasmError> {
        if storage.gas_left() < Self::fragment_read_gas(is_cold, max_code_size) {
            storage.burn_out();
            return Err(PrecompileError::OutOfGas.into());
        }
        Ok(())
    }

    fn stylus_fragment_payload(code: &[u8]) -> Result<&[u8], ArbWasmError> {
        if Self::has_stylus_prefix(code, STYLUS_FRAGMENT_PREFIX) {
            return Ok(&code[STYLUS_FRAGMENT_PREFIX.len()..]);
        }
        Err(ArbWasmError::NonSolidityError)
    }

    fn fragment_read_gas(is_cold: bool, code_size: usize) -> u64 {
        let access_gas = if is_cold {
            COLD_ACCOUNT_ACCESS_GAS
        } else {
            WARM_STORAGE_READ_GAS
        };
        access_gas.saturating_add(Self::copy_gas(code_size))
    }

    fn check_stylus_dictionary(dictionary: u8) -> Result<(), ArbWasmError> {
        match dictionary {
            STYLUS_EMPTY_DICTIONARY | STYLUS_PROGRAM_DICTIONARY => Ok(()),
            _ => Err(ArbWasmError::NonSolidityError),
        }
    }

    fn check_classic_stylus_code(code: &[u8], max_wasm_size: u32) -> Result<Vec<u8>, ArbWasmError> {
        let dictionary = code[3];
        Self::check_stylus_dictionary(dictionary)?;
        Self::decompress_stylus_payload(&code[STYLUS_HEADER_LEN..], dictionary, max_wasm_size)
    }

    fn decompress_stylus_payload(
        payload: &[u8],
        dictionary: u8,
        max_size: u32,
    ) -> Result<Vec<u8>, ArbWasmError> {
        let max_size = max_size as usize;
        let mut output = Vec::with_capacity(max_size.min(payload.len().saturating_mul(2)));
        let mut decoder = match dictionary {
            STYLUS_EMPTY_DICTIONARY => brotli::Decompressor::new(Cursor::new(payload), 4096),
            STYLUS_PROGRAM_DICTIONARY => brotli::Decompressor::new_with_custom_dict(
                Cursor::new(payload),
                4096,
                program_dictionary_owned().into(),
            ),
            _ => return Err(ArbWasmError::NonSolidityError),
        };
        decoder
            .by_ref()
            .take(max_size.saturating_add(1) as u64)
            .read_to_end(&mut output)
            .map_err(|_| ArbWasmError::NonSolidityError)?;

        if output.len() > max_size {
            return Err(ArbWasmError::NonSolidityError);
        }

        Ok(output)
    }
}

fn div_ceil_u64(lhs: u64, rhs: u64) -> u64 {
    lhs / rhs + u64::from(lhs % rhs != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::precompile::stylus_dictionary::PROGRAM_DICTIONARY_BYTES;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::sol_types::SolCall;
    use brotli::enc::BrotliEncoderParams;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::bytecode::Bytecode;
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::state::AccountInfo;
    use revm::{Context, MainContext};

    fn brotli_compress(input: &[u8]) -> Vec<u8> {
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        brotli::BrotliCompress(&mut reader, &mut output, &BrotliEncoderParams::default())
            .expect("test brotli compression");
        output
    }

    fn brotli_compress_with_dictionary(input: &[u8], dictionary: &[u8]) -> Vec<u8> {
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        let mut input_buffer = [0; 4096];
        let mut output_buffer = [0; 4096];
        let params = BrotliEncoderParams {
            quality: 11,
            lgwin: 22,
            ..Default::default()
        };
        let mut callback =
            |_: &mut brotli::interface::PredictionModeContextMap<brotli::InputReferenceMut>,
             _: &mut [brotli::interface::Command<brotli::SliceOffset>],
             _: brotli::InputPair,
             _: &mut brotli::enc::reader::StandardAlloc| {};

        brotli::BrotliCompressCustomIoCustomDict(
            &mut brotli::IoReaderWrapper(&mut reader),
            &mut brotli::IoWriterWrapper(&mut output),
            &mut input_buffer,
            &mut output_buffer,
            &params,
            brotli::enc::reader::StandardAlloc::default(),
            &mut callback,
            dictionary,
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Unexpected EOF"),
        )
        .expect("test brotli dictionary compression");
        output
    }

    fn test_stylus_params(max_wasm_size: u32, max_fragment_count: u8) -> StylusParams {
        StylusParams {
            version: 1,
            ink_price: 1,
            max_stack_depth: 1,
            free_pages: 1,
            page_gas: 1,
            page_limit: 1,
            min_init_gas: 1,
            min_cached_init_gas: 1,
            init_cost_scalar: 1,
            cached_cost_scalar: 1,
            expiry_days: 1,
            keepalive_days: 1,
            block_cache_size: 1,
            max_wasm_size,
            max_fragment_count,
        }
    }

    fn account_info_with_code(code: Vec<u8>) -> AccountInfo {
        let code = Bytecode::new_legacy(Bytes::from(code));
        AccountInfo {
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        }
    }

    fn stylus_root(dictionary: u8, decompressed_len: u32, fragments: &[Address]) -> Vec<u8> {
        let mut code = vec![0xef, 0xf0, 0x02, dictionary];
        code.extend_from_slice(&decompressed_len.to_be_bytes());
        for fragment in fragments {
            code.extend_from_slice(fragment.as_slice());
        }
        code
    }

    fn classic_stylus(dictionary: u8, payload: &[u8]) -> Vec<u8> {
        let mut code = STYLUS_CLASSIC_PREFIX.to_vec();
        code.push(dictionary);
        code.extend_from_slice(payload);
        code
    }

    fn stylus_fragment(payload: &[u8]) -> Vec<u8> {
        let mut code = STYLUS_FRAGMENT_PREFIX.to_vec();
        code.extend_from_slice(payload);
        code
    }

    fn with_storage<T>(
        gas_limit: u64,
        max_code_size: usize,
        accounts: &[(Address, Vec<u8>)],
        test: impl FnOnce(&mut ArbStorage<'_, ArbitrumContext<CacheDB<EmptyDB>>>) -> T,
    ) -> T {
        let mut db = CacheDB::new(EmptyDB::default());
        for (address, code) in accounts {
            db.insert_account_info(*address, account_info_with_code(code.clone()));
        }

        let mut cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Prague);
        cfg.limit_contract_code_size = Some(max_code_size);
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(cfg)
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        let mut storage = ArbStorage::new_with_initial_gas(&mut context, gas_limit, 0);
        test(&mut storage)
    }

    fn context_with_stylus_params(params: StylusParams) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(
                    &[],
                    arbos_state::ARBOS_VERSION_OFFSET,
                    U256::from(ARBOS_VERSION_STYLUS),
                )
                .expect("write ArbOS version");
            storage
                .save_stylus_params(params)
                .expect("write stylus params");
        }
        context
    }

    fn context_with_activation_state(
        params: StylusParams,
        timestamp: u64,
        network_fee_account: Address,
        precompile_balance: U256,
    ) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            ARB_WASM_ADDRESS,
            AccountInfo {
                balance: precompile_balance,
                ..Default::default()
            },
        );
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                timestamp: U256::from(timestamp),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(
                    &[],
                    arbos_state::ARBOS_VERSION_OFFSET,
                    U256::from(ARBOS_VERSION_STYLUS_CONTRACT_LIMIT),
                )
                .expect("write ArbOS version");
            storage
                .write_address(
                    &[],
                    arbos_state::NETWORK_FEE_ACCOUNT_OFFSET,
                    network_fee_account,
                )
                .expect("write network fee account");
            storage
                .save_stylus_params(params)
                .expect("write stylus params");

            let data_pricer_key = storage.wasm_data_pricer_key();
            storage
                .write(&data_pricer_key, 2, U256::from(timestamp))
                .expect("write wasm data pricer last update");
            storage
                .write(&data_pricer_key, 3, U256::from(2))
                .expect("write wasm data pricer min price");
            storage
                .write(&data_pricer_key, 4, U256::from(1_000_000_000u64))
                .expect("write wasm data pricer inertia");
        }
        context
    }

    #[test]
    fn decompress_stylus_payload_accepts_empty_dictionary_brotli() {
        let wasm = b"\0asm\x01\0\0\0";
        let compressed = brotli_compress(wasm);

        let decompressed = ArbWasm::decompress_stylus_payload(
            &compressed,
            STYLUS_EMPTY_DICTIONARY,
            wasm.len() as u32,
        )
        .unwrap();

        assert_eq!(decompressed, wasm);
    }

    #[test]
    fn decompress_stylus_payload_accepts_program_dictionary_brotli() {
        let wasm = &PROGRAM_DICTIONARY_BYTES[1024..1152];
        let compressed = brotli_compress_with_dictionary(wasm, PROGRAM_DICTIONARY_BYTES);

        assert!(ArbWasm::decompress_stylus_payload(
            &compressed,
            STYLUS_EMPTY_DICTIONARY,
            wasm.len() as u32,
        )
        .is_err());

        let decompressed = ArbWasm::decompress_stylus_payload(
            &compressed,
            STYLUS_PROGRAM_DICTIONARY,
            wasm.len() as u32,
        )
        .unwrap();

        assert_eq!(decompressed, wasm);
    }

    #[test]
    fn decompress_stylus_payload_rejects_oversized_output() {
        let compressed = brotli_compress(b"\0asm\x01\0\0\0");

        let err = ArbWasm::decompress_stylus_payload(&compressed, STYLUS_EMPTY_DICTIONARY, 1)
            .unwrap_err();

        assert!(matches!(err, ArbWasmError::NonSolidityError));
    }

    #[test]
    fn parse_stylus_root_accepts_fragment_addresses() {
        let fragments = [Address::from([0x11; 20]), Address::from([0x22; 20])];
        let code = stylus_root(STYLUS_EMPTY_DICTIONARY, 8, &fragments);

        let root = ArbWasm::parse_stylus_root(&code, test_stylus_params(8, 2)).unwrap();

        assert_eq!(root.dictionary, STYLUS_EMPTY_DICTIONARY);
        assert_eq!(root.decompressed_len, 8);
        assert_eq!(root.fragments, fragments);
    }

    #[test]
    fn parse_stylus_root_rejects_invalid_fragment_address_len() {
        let mut code = stylus_root(STYLUS_EMPTY_DICTIONARY, 8, &[Address::from([0x11; 20])]);
        code.push(0);

        let err = ArbWasm::parse_stylus_root(&code, test_stylus_params(8, 2)).unwrap_err();

        assert!(matches!(err, ArbWasmError::NonSolidityError));
    }

    #[test]
    fn parse_stylus_root_rejects_zero_fragments() {
        let code = stylus_root(STYLUS_EMPTY_DICTIONARY, 8, &[]);

        let err = ArbWasm::parse_stylus_root(&code, test_stylus_params(8, 2)).unwrap_err();

        assert!(matches!(err, ArbWasmError::NonSolidityError));
    }

    #[test]
    fn parse_stylus_root_rejects_fragment_count_over_limit() {
        let fragments = [Address::from([0x11; 20]), Address::from([0x22; 20])];
        let code = stylus_root(STYLUS_EMPTY_DICTIONARY, 8, &fragments);

        let err = ArbWasm::parse_stylus_root(&code, test_stylus_params(8, 1)).unwrap_err();

        assert!(matches!(err, ArbWasmError::NonSolidityError));
    }

    #[test]
    fn stylus_fragment_payload_accepts_fragment_prefix() {
        let code = [0xef, 0xf0, 0x01, 0xaa, 0xbb];

        let payload = ArbWasm::stylus_fragment_payload(&code).unwrap();

        assert_eq!(payload, [0xaa, 0xbb]);
    }

    #[test]
    fn stylus_fragment_payload_rejects_direct_classic_program() {
        let code = [0xef, 0xf0, 0x00, STYLUS_EMPTY_DICTIONARY, 0xaa];

        let err = ArbWasm::stylus_fragment_payload(&code).unwrap_err();

        assert!(matches!(err, ArbWasmError::NonSolidityError));
    }

    #[test]
    fn fragment_read_gas_uses_access_and_copy_cost() {
        assert_eq!(
            ArbWasm::fragment_read_gas(false, 33),
            WARM_STORAGE_READ_GAS + 2 * COPY_GAS
        );
        assert_eq!(
            ArbWasm::fragment_read_gas(true, 33),
            COLD_ACCOUNT_ACCESS_GAS + 2 * COPY_GAS
        );
    }

    #[test]
    fn page_ramp_charges_stylus_params_read() {
        let mut context = context_with_stylus_params(test_stylus_params(128 * 1024, 0));
        let data = IArbWasm::pageRampCall {}.abi_encode();

        let output = ArbWasm::run(ArbPrecompileInput {
            data: &data,
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: ARBOS_VERSION_STYLUS,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("pageRamp call");

        assert!(!output.reverted);
        assert_eq!(
            IArbWasm::pageRampCall::abi_decode_returns(output.bytes.as_ref())
                .expect("decode pageRamp return"),
            PAGE_RAMP
        );
        assert_eq!(
            output.gas_used,
            super::super::BASE_PRECOMPILE_GAS + WARM_STORAGE_READ_GAS + COPY_GAS
        );
    }

    #[test]
    fn activate_program_selfdestructed_target_reverts_without_burnout() {
        let program = Address::from([0x55; 20]);
        let gas_limit = 2_000_000;
        let mut context = context_with_stylus_params(test_stylus_params(128 * 1024, 0));
        context
            .db_mut()
            .insert_account_info(program, account_info_with_code(vec![0x60, 0x00]));
        context
            .journal_mut()
            .load_account(program)
            .expect("load program account");
        context
            .journal_mut()
            .inner
            .state
            .get_mut(&program)
            .expect("loaded program account")
            .mark_selfdestruct();

        let data = IArbWasm::activateProgramCall { program }.abi_encode();
        let output = ArbWasm::run(ArbPrecompileInput {
            data: &data,
            gas: gas_limit,
            caller: Address::from([0x11; 20]),
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: ARBOS_VERSION_STYLUS,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("activateProgram call");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert!(output.gas_used < gas_limit);
    }

    #[test]
    fn finish_activation_saves_program_state_pays_fee_and_emits_event() {
        let params = test_stylus_params(128 * 1024, 0);
        let timestamp = ARBITRUM_START_TIME + 7 * 3600;
        let caller = Address::from([0x44; 20]);
        let program = Address::from([0x55; 20]);
        let network = Address::from([0x99; 20]);
        let code_hash = B256::from([0x11; 32]);
        let module_hash = B256::from([0x22; 32]);
        let activation = WasmActivation {
            module_hash,
            init_cost: 17,
            cached_cost: 5,
            footprint: 9,
            asm_estimate: 123,
        };
        let mut context =
            context_with_activation_state(params, timestamp, network, U256::from(300));

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let ret = ArbWasm::finish_activation(
                &mut storage,
                caller,
                U256::from(300),
                program,
                code_hash,
                params,
                false,
                activation,
            )
            .expect("finish activation");

            assert_eq!(ret, (params.version, U256::from(246)));
        }

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let module_hashes_key = storage.wasm_module_hashes_key();
            assert_eq!(
                storage
                    .read_key(&module_hashes_key, code_hash.0)
                    .expect("read module hash"),
                U256::from_be_bytes(module_hash.0)
            );

            let saved = storage.wasm_program(code_hash).expect("read wasm program");
            assert_eq!(saved.version, params.version);
            assert_eq!(saved.init_cost, activation.init_cost);
            assert_eq!(saved.cached_cost, activation.cached_cost);
            assert_eq!(saved.footprint, activation.footprint);
            assert_eq!(saved.asm_estimate_kb, 1);
            assert_eq!(saved.activated_at, 7);
            assert!(!saved.cached);

            let data_pricer_key = storage.wasm_data_pricer_key();
            assert_eq!(
                storage
                    .read_u64(&data_pricer_key, 0)
                    .expect("read data pricer demand"),
                123
            );
            assert_eq!(
                storage
                    .read_u64(&data_pricer_key, 2)
                    .expect("read data pricer last update"),
                timestamp
            );
        }

        assert_eq!(
            context
                .journal_mut()
                .load_account(network)
                .expect("load network fee account")
                .data
                .info
                .balance,
            U256::from(246)
        );
        assert_eq!(
            context
                .journal_mut()
                .load_account(caller)
                .expect("load caller")
                .data
                .info
                .balance,
            U256::from(54)
        );

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, ARB_WASM_ADDRESS);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("ProgramActivated(bytes32,bytes32,address,uint256,uint16)")
        );
        assert_eq!(logs[0].data.topics()[1], code_hash);
        assert_eq!(
            logs[0].data.data,
            Bytes::from((module_hash, program, U256::from(246), params.version).abi_encode())
        );
    }

    #[test]
    fn finish_activation_preserves_cached_flag() {
        let params = test_stylus_params(128 * 1024, 0);
        let timestamp = ARBITRUM_START_TIME + 3 * 3600;
        let code_hash = B256::from([0x33; 32]);
        let activation = WasmActivation {
            module_hash: B256::from([0x44; 32]),
            init_cost: 1,
            cached_cost: 2,
            footprint: 3,
            asm_estimate: 1,
        };
        let mut context = context_with_activation_state(
            params,
            timestamp,
            Address::from([0x99; 20]),
            U256::from(2),
        );

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            ArbWasm::finish_activation(
                &mut storage,
                Address::from([0x11; 20]),
                U256::from(2),
                Address::from([0x22; 20]),
                code_hash,
                params,
                true,
                activation,
            )
            .expect("finish activation");
        }

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        assert!(
            storage
                .wasm_program(code_hash)
                .expect("read wasm program")
                .cached
        );
    }

    #[test]
    fn root_stylus_program_reads_fragments_and_decompresses() {
        let wasm = b"\0asm\x01\0\0\0";
        let compressed = brotli_compress(wasm);
        let split = compressed.len() / 2;
        let first = Address::from([0x11; 20]);
        let second = Address::from([0x22; 20]);
        let root = stylus_root(STYLUS_EMPTY_DICTIONARY, wasm.len() as u32, &[first, second]);

        with_storage(
            20_000,
            24_576,
            &[
                (first, stylus_fragment(&compressed[..split])),
                (second, stylus_fragment(&compressed[split..])),
            ],
            |storage| {
                let decoded = ArbWasm::check_stylus_root(
                    storage,
                    &root,
                    test_stylus_params(wasm.len() as u32, 2),
                )
                .unwrap();
                assert_eq!(decoded, wasm);

                let expected_gas =
                    ArbWasm::fragment_read_gas(true, STYLUS_FRAGMENT_PREFIX.len() + split)
                        + ArbWasm::fragment_read_gas(
                            true,
                            STYLUS_FRAGMENT_PREFIX.len() + compressed.len() - split,
                        );
                assert_eq!(storage.gas_used, expected_gas);
            },
        );
    }

    #[test]
    fn root_stylus_program_accepts_program_dictionary() {
        let wasm = &PROGRAM_DICTIONARY_BYTES[1024..1152];
        let compressed = brotli_compress_with_dictionary(wasm, PROGRAM_DICTIONARY_BYTES);
        let fragment = Address::from([0x11; 20]);
        let root = stylus_root(STYLUS_PROGRAM_DICTIONARY, wasm.len() as u32, &[fragment]);

        with_storage(
            20_000,
            24_576,
            &[(fragment, stylus_fragment(&compressed))],
            |storage| {
                let decoded = ArbWasm::check_stylus_root(
                    storage,
                    &root,
                    test_stylus_params(wasm.len() as u32, 1),
                )
                .unwrap();
                assert_eq!(decoded, wasm);

                assert_eq!(
                    storage.gas_used,
                    ArbWasm::fragment_read_gas(
                        true,
                        STYLUS_FRAGMENT_PREFIX.len() + compressed.len()
                    )
                );
            },
        );
    }

    #[test]
    fn classic_stylus_program_accepts_program_dictionary() {
        let wasm = &PROGRAM_DICTIONARY_BYTES[1024..1152];
        let compressed = brotli_compress_with_dictionary(wasm, PROGRAM_DICTIONARY_BYTES);
        let code = classic_stylus(STYLUS_PROGRAM_DICTIONARY, &compressed);

        let decoded = ArbWasm::check_classic_stylus_code(&code, wasm.len() as u32).unwrap();
        assert_eq!(decoded, wasm);
    }

    #[test]
    fn read_stylus_fragments_charges_repeated_fragment_as_warm() {
        let fragment = Address::from([0x11; 20]);
        let code = stylus_fragment(&[0xaa, 0xbb]);

        with_storage(20_000, 24_576, &[(fragment, code.clone())], |storage| {
            let payload =
                ArbWasm::read_stylus_fragments(storage, &[fragment, fragment], 24_576).unwrap();

            assert_eq!(payload, [0xaa, 0xbb, 0xaa, 0xbb]);
            assert_eq!(
                storage.gas_used,
                ArbWasm::fragment_read_gas(true, code.len())
                    + ArbWasm::fragment_read_gas(false, code.len())
            );
        });
    }

    #[test]
    fn read_stylus_fragments_requires_max_fragment_read_reserve_before_loading_code() {
        let fragment = Address::from([0x11; 20]);
        let code = stylus_fragment(&[0xaa]);
        let max_code_size = 24_576;
        let gas_limit = ArbWasm::fragment_read_gas(true, max_code_size) - 1;

        with_storage(gas_limit, max_code_size, &[(fragment, code)], |storage| {
            assert!(!storage.account_is_warm(fragment));

            let err =
                ArbWasm::read_stylus_fragments(storage, &[fragment], max_code_size).unwrap_err();

            assert!(matches!(
                err,
                ArbWasmError::Precompile(PrecompileError::OutOfGas)
            ));
            assert_eq!(storage.gas_used, gas_limit);
            assert!(!storage.account_is_warm(fragment));
        });
    }

    #[test]
    fn root_program_dictionary_decompress_failure_still_reads_fragments_first() {
        let fragment = Address::from([0x11; 20]);
        let code = stylus_fragment(&[0xaa]);
        let root = stylus_root(STYLUS_PROGRAM_DICTIONARY, 1, &[fragment]);

        with_storage(20_000, 24_576, &[(fragment, code.clone())], |storage| {
            let err =
                ArbWasm::check_stylus_root(storage, &root, test_stylus_params(1, 1)).unwrap_err();

            assert!(matches!(err, ArbWasmError::NonSolidityError));
            assert_eq!(
                storage.gas_used,
                ArbWasm::fragment_read_gas(true, code.len())
            );
        });
    }

    #[test]
    fn root_empty_dictionary_rejects_invalid_fragment_prefix_after_read_charge() {
        let fragment = Address::from([0x11; 20]);
        let code = [0xef, 0xf0, 0x00, STYLUS_EMPTY_DICTIONARY, 0xaa].to_vec();
        let root = stylus_root(STYLUS_EMPTY_DICTIONARY, 1, &[fragment]);

        with_storage(20_000, 24_576, &[(fragment, code.clone())], |storage| {
            let err =
                ArbWasm::check_stylus_root(storage, &root, test_stylus_params(1, 1)).unwrap_err();

            assert!(matches!(err, ArbWasmError::NonSolidityError));
            assert_eq!(
                storage.gas_used,
                ArbWasm::fragment_read_gas(true, code.len())
            );
        });
    }

    #[test]
    fn classic_stylus_program_dictionary_rejects_invalid_payload() {
        let code = [0xef, 0xf0, 0x00, STYLUS_PROGRAM_DICTIONARY, 0x00];

        let err = ArbWasm::check_classic_stylus_code(&code, 1).unwrap_err();

        assert!(matches!(err, ArbWasmError::NonSolidityError));
    }
}
