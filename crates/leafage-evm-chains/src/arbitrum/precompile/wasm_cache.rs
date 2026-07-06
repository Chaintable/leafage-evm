use super::abi::{IArbWasm, IArbWasmCache};
use super::state::{ArbStorage, StylusProgramError, WasmProgram};
use super::util::{dispatch, empty_revert, finish_call, topic_address};
use super::{ArbPrecompileInput, ArbitrumContext, ARB_WASM_CACHE_ADDRESS};
use alloy::primitives::{keccak256, Address, Bytes, Log, B256};
use alloy::sol_types::{SolError, SolValue};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::Block;
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::{Database, DatabaseRef};

pub(super) struct ArbWasmCache;

const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_FIXES: u64 = 31;
const COPY_GAS: u64 = 3;
const LOG_GAS: u64 = 375;
const LOG_TOPIC_GAS: u64 = 375;
const LOG_DATA_GAS: u64 = 8;

impl ArbWasmCache {
    pub(super) fn run<DB: Database + DatabaseRef>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let is_static = input.is_static;
        let context = input.context;
        dispatch::<IArbWasmCache::IArbWasmCacheCalls>(data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            let arbos_version = storage.arbos_version()?;
            if arbos_version < ARBOS_VERSION_STYLUS {
                return empty_revert(gas_limit, gas_limit);
            }
            match call {
                IArbWasmCache::IArbWasmCacheCalls::isCacheManager(call) => {
                    let managers_key = storage.wasm_cache_manager_key();
                    let ret = storage.address_set_contains(&managers_key, call.manager)?;
                    finish_call::<IArbWasmCache::isCacheManagerCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbWasmCache::IArbWasmCacheCalls::allCacheManagers(_) => {
                    let managers_key = storage.wasm_cache_manager_key();
                    let ret = storage.address_set_members(&managers_key)?;
                    finish_call::<IArbWasmCache::allCacheManagersCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbWasmCache::IArbWasmCacheCalls::codehashIsCached(call) => {
                    let ret = storage.wasm_program_cached(call.codehash)?;
                    finish_call::<IArbWasmCache::codehashIsCachedCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbWasmCache::IArbWasmCacheCalls::cacheCodehash(call) => Self::set_cached(
                    &mut storage,
                    gas_limit,
                    caller,
                    is_static,
                    arbos_version <= ARBOS_VERSION_STYLUS,
                    call.codehash,
                    true,
                ),
                IArbWasmCache::IArbWasmCacheCalls::cacheProgram(call) => {
                    if arbos_version < ARBOS_VERSION_STYLUS_FIXES {
                        return empty_revert(gas_limit, gas_limit);
                    }
                    let code_hash = storage.account_code_hash(call.addr)?;
                    Self::set_cached(
                        &mut storage,
                        gas_limit,
                        caller,
                        is_static,
                        true,
                        code_hash,
                        true,
                    )
                }
                IArbWasmCache::IArbWasmCacheCalls::evictCodehash(call) => Self::set_cached(
                    &mut storage,
                    gas_limit,
                    caller,
                    is_static,
                    true,
                    call.codehash,
                    false,
                ),
            }
        })
    }

    fn set_cached<DB: Database + DatabaseRef>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        caller: Address,
        is_static: bool,
        method_available: bool,
        code_hash: B256,
        cached: bool,
    ) -> PrecompileResult {
        if !method_available {
            return empty_revert(gas_limit, gas_limit);
        }
        if is_static {
            return empty_revert(gas_limit, gas_limit);
        }
        if !Self::has_access(storage, caller)? {
            storage.burn_out();
            return empty_revert(gas_limit, gas_limit);
        }

        let program = match Self::program_for_cache(storage, code_hash, cached) {
            Ok(program) => program,
            Err(error) => return Self::handle_program_error(storage, gas_limit, error),
        };
        if program.cached != cached {
            Self::emit(storage, caller, code_hash, cached)?;
            storage.burn(u64::from(program.init_cost))?;
            if cached {
                match Self::ensure_cacheable_code(storage, code_hash) {
                    Ok(()) => {}
                    Err(error) => return Self::handle_precompile_error(storage, gas_limit, error),
                }
            }
            storage.set_wasm_program_cached(code_hash, cached)?;
        }

        if cached {
            finish_call::<IArbWasmCache::cacheCodehashCall>(gas_limit, storage.gas_used, ().into())
        } else {
            finish_call::<IArbWasmCache::evictCodehashCall>(gas_limit, storage.gas_used, ().into())
        }
    }

    fn has_access<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
    ) -> Result<bool, PrecompileError> {
        let managers_key = storage.wasm_cache_manager_key();
        if storage.address_set_contains(&managers_key, caller)? {
            return Ok(true);
        }
        let owners_key = storage.chain_owner_key();
        storage.address_set_contains(&owners_key, caller)
    }

    fn program_for_cache<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code_hash: B256,
        cached: bool,
    ) -> Result<WasmProgram, StylusProgramError> {
        let params = storage.stylus_params()?;
        let program = storage.wasm_program(code_hash)?;
        if !cached {
            return Ok(program);
        }

        if program.version != params.version {
            return Err(StylusProgramError::ProgramNeedsUpgrade {
                version: program.version,
                stylus_version: params.version,
            });
        }

        let timestamp = storage.context.block().timestamp().to::<u64>();
        let age = storage.wasm_program_age(timestamp, program);
        let expiry = u64::from(params.expiry_days) * 24 * 60 * 60;
        if age > expiry {
            return Err(StylusProgramError::ProgramExpired { age });
        }

        Ok(program)
    }

    fn ensure_cacheable_code<DB: Database + DatabaseRef>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        code_hash: B256,
    ) -> Result<(), PrecompileError> {
        if storage.code_by_hash(code_hash)?.is_empty() {
            return Err(PrecompileError::other(format!(
                "code not found for codeHash: {code_hash:?}"
            )));
        }
        Ok(())
    }

    fn handle_precompile_error<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        error: PrecompileError,
    ) -> PrecompileResult {
        match error {
            PrecompileError::OutOfGas => {
                storage.burn_out();
                empty_revert(gas_limit, gas_limit)
            }
            PrecompileError::Other(_) => empty_revert(gas_limit, storage.gas_used),
            error => Err(error),
        }
    }

    fn handle_program_error<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        error: StylusProgramError,
    ) -> PrecompileResult {
        match error {
            StylusProgramError::Precompile(error) => {
                Self::handle_precompile_error(storage, gas_limit, error)
            }
            StylusProgramError::ProgramNotActivated => {
                Self::custom_error(storage, gas_limit, IArbWasm::ProgramNotActivated {})
            }
            StylusProgramError::ProgramNeedsUpgrade {
                version,
                stylus_version,
            } => Self::custom_error(
                storage,
                gas_limit,
                IArbWasm::ProgramNeedsUpgrade {
                    version,
                    stylusVersion: stylus_version,
                },
            ),
            StylusProgramError::ProgramExpired { age } => Self::custom_error(
                storage,
                gas_limit,
                IArbWasm::ProgramExpired { ageInSeconds: age },
            ),
            StylusProgramError::ProgramKeepaliveTooSoon { age } => Self::custom_error(
                storage,
                gas_limit,
                IArbWasm::ProgramKeepaliveTooSoon { ageInSeconds: age },
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
            return empty_revert(gas_limit, gas_limit);
        }
        Ok(revm::precompile::PrecompileOutput::new_reverted(
            storage.gas_used,
            bytes,
        ))
    }

    fn copy_gas(byte_count: usize) -> u64 {
        COPY_GAS.saturating_mul((byte_count as u64).div_ceil(32))
    }

    fn emit<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        manager: Address,
        code_hash: B256,
        cached: bool,
    ) -> Result<(), PrecompileError> {
        storage.burn(Self::update_program_cache_event_gas())?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_WASM_CACHE_ADDRESS,
            vec![
                keccak256("UpdateProgramCache(address,bytes32,bool)"),
                topic_address(manager),
                code_hash,
            ],
            Bytes::from((cached,).abi_encode()),
        ));
        Ok(())
    }

    fn update_program_cache_event_gas() -> u64 {
        LOG_GAS + 3 * LOG_TOPIC_GAS + 32 * LOG_DATA_GAS
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::StylusParams;
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::U256;
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::bytecode::Bytecode;
    use revm::context::{ContextTr, JournalTr};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::precompile::PrecompileOutput;
    use revm::state::AccountInfo;
    use revm::{Context, MainContext};

    fn stylus_params() -> StylusParams {
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
            max_wasm_size: 128 * 1024,
            max_fragment_count: 1,
        }
    }

    fn wasm_program() -> WasmProgram {
        WasmProgram {
            version: 1,
            init_cost: 7,
            cached_cost: 0,
            footprint: 0,
            activated_at: 0,
            asm_estimate_kb: 0,
            cached: false,
        }
    }

    fn account_info_with_code(code: Bytes) -> (AccountInfo, B256) {
        let bytecode = Bytecode::new_legacy(code);
        let code_hash = bytecode.hash_slow();
        (
            AccountInfo {
                code_hash,
                code: Some(bytecode),
                ..Default::default()
            },
            code_hash,
        )
    }

    fn context_with_program(
        caller: Address,
        code_hash: B256,
        db: CacheDB<EmptyDB>,
    ) -> ArbitrumContext<CacheDB<EmptyDB>> {
        context_with_program_at_version(caller, code_hash, db, ARBOS_VERSION_STYLUS)
    }

    fn context_with_program_at_version(
        caller: Address,
        code_hash: B256,
        db: CacheDB<EmptyDB>,
        arbos_version: u64,
    ) -> ArbitrumContext<CacheDB<EmptyDB>> {
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
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(
                    &[],
                    arbos_state::ARBOS_VERSION_OFFSET,
                    U256::from(arbos_version),
                )
                .expect("write ArbOS version");
            storage
                .save_stylus_params(stylus_params())
                .expect("write stylus params");
            let owners_key = storage.chain_owner_key();
            storage
                .address_set_add(&owners_key, caller)
                .expect("write chain owner");
            storage
                .save_wasm_program(code_hash, wasm_program())
                .expect("write wasm program");
        }
        context
    }

    fn run_cache_codehash(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        caller: Address,
        code_hash: B256,
    ) -> PrecompileOutput {
        let data = IArbWasmCache::cacheCodehashCall {
            codehash: code_hash,
        }
        .abi_encode();
        ArbWasmCache::run(ArbPrecompileInput {
            data: &data,
            gas: 1_000_000,
            caller,
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
            context,
        })
        .expect("cache codehash call")
    }

    fn run_cache_program(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        caller: Address,
        addr: Address,
    ) -> PrecompileOutput {
        let data = IArbWasmCache::cacheProgramCall { addr }.abi_encode();
        ArbWasmCache::run(ArbPrecompileInput {
            data: &data,
            gas: 1_000_000,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: ARBOS_VERSION_STYLUS_FIXES,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        })
        .expect("cache program call")
    }

    fn wasm_program_cached(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        code_hash: B256,
    ) -> bool {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        storage
            .wasm_program_cached(code_hash)
            .expect("read wasm program cache flag")
    }

    #[test]
    fn cache_codehash_reverts_when_code_hash_is_missing() {
        let caller = Address::from([0x11; 20]);
        let code_address = Address::from([0x33; 20]);
        let (account, code_hash) = account_info_with_code(Bytes::from_static(&[0x60, 0x00]));
        let mut context = context_with_program(caller, code_hash, CacheDB::new(EmptyDB::default()));

        let output = run_cache_codehash(&mut context, caller, code_hash);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert!(output.gas_used < 1_000_000);
        assert!(!wasm_program_cached(&mut context, code_hash));

        context.db_mut().insert_account_info(code_address, account);

        let output = run_cache_codehash(&mut context, caller, code_hash);

        assert!(!output.reverted);
        assert!(wasm_program_cached(&mut context, code_hash));
    }

    #[test]
    fn cache_codehash_sets_cached_when_code_hash_exists() {
        let caller = Address::from([0x11; 20]);
        let code_address = Address::from([0x33; 20]);
        let (account, code_hash) = account_info_with_code(Bytes::from_static(&[0x60, 0x00]));
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(code_address, account);
        let mut context = context_with_program(caller, code_hash, db);

        let output = run_cache_codehash(&mut context, caller, code_hash);

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert!(wasm_program_cached(&mut context, code_hash));
    }

    #[test]
    fn cache_program_sets_cached_when_account_code_exists() {
        let caller = Address::from([0x11; 20]);
        let code_address = Address::from([0x33; 20]);
        let (account, code_hash) = account_info_with_code(Bytes::from_static(&[0x60, 0x00]));
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(code_address, account);
        let mut context =
            context_with_program_at_version(caller, code_hash, db, ARBOS_VERSION_STYLUS_FIXES);

        let output = run_cache_program(&mut context, caller, code_address);

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert!(wasm_program_cached(&mut context, code_hash));
    }

    #[test]
    fn cache_program_reverts_when_account_is_selfdestructed() {
        let caller = Address::from([0x11; 20]);
        let code_address = Address::from([0x33; 20]);
        let (account, code_hash) = account_info_with_code(Bytes::from_static(&[0x60, 0x00]));
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(code_address, account);
        let mut context =
            context_with_program_at_version(caller, code_hash, db, ARBOS_VERSION_STYLUS_FIXES);
        context
            .journal_mut()
            .load_account(code_address)
            .expect("load code account");
        context
            .journal_mut()
            .inner
            .state
            .get_mut(&code_address)
            .expect("loaded code account")
            .mark_selfdestruct();

        let output = run_cache_program(&mut context, caller, code_address);

        assert!(output.reverted);
        assert!(!wasm_program_cached(&mut context, code_hash));
    }
}
