mod address_set;
mod address_table;
mod balance;
mod filtered_transactions;
mod merkle;
mod pricing;
mod retryable;
mod stylus;

use super::util::{address_from_word, address_key, signed_diff};
use super::{
    ArbitrumContext, ASSUMED_SIMPLE_TX_SIZE, GAS_CONSTRAINTS_KEY, L1_PRICER_FUNDS_POOL_ADDRESS,
    MAX_GET_ALL_MEMBERS, MULTI_GAS_CONSTRAINTS_KEY, NUM_RESOURCE_KIND, RESOURCE_KIND_SINGLE_DIM,
    RETRYABLE_LIFETIME_SECONDS, STORAGE_READ_GAS, STORAGE_WRITE_COST, STORAGE_WRITE_ZERO_COST,
    TX_DATA_NON_ZERO_GAS,
};
use crate::arbitrum::arbos_state;
use alloy::primitives::{keccak256, Address, Bytes, B256, I256, U256};
use alloy_rlp::{Decodable, Encodable, Header};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::{journaled_state::account::JournaledAccountTr, Block};
use revm::precompile::PrecompileError;
use revm::primitives::KECCAK_EMPTY;
use revm::{Database, DatabaseRef};

const ARBOS_VERSION_4: u64 = 4;
const ARBOS_VERSION_60: u64 = 60;
const ARBITRUM_START_TIME: u64 = 1_421_388_000;
const MAX_UINT24: u32 = 0x00ff_ffff;
const WARM_STORAGE_READ_GAS: u64 = 100;

pub(super) struct ArbStorage<'a, CTX> {
    pub(super) context: &'a mut CTX,
    gas_limit: u64,
    pub(super) gas_used: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StylusParams {
    pub(super) version: u16,
    pub(super) ink_price: u32,
    pub(super) max_stack_depth: u32,
    pub(super) free_pages: u16,
    pub(super) page_gas: u16,
    pub(super) page_limit: u16,
    pub(super) min_init_gas: u8,
    pub(super) min_cached_init_gas: u8,
    pub(super) init_cost_scalar: u8,
    pub(super) cached_cost_scalar: u8,
    pub(super) expiry_days: u16,
    pub(super) keepalive_days: u16,
    pub(super) block_cache_size: u16,
    pub(super) max_wasm_size: u32,
    pub(super) max_fragment_count: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WasmProgram {
    pub(super) version: u16,
    pub(super) init_cost: u16,
    pub(super) cached_cost: u16,
    pub(super) footprint: u16,
    pub(super) activated_at: u32,
    pub(super) asm_estimate_kb: u32,
    pub(super) cached: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WasmActivation {
    pub(super) module_hash: B256,
    pub(super) init_cost: u16,
    pub(super) cached_cost: u16,
    pub(super) footprint: u16,
    pub(super) asm_estimate: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MerkleUpdate {
    pub(super) level: u64,
    pub(super) num_leaves: u64,
    pub(super) hash: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RetryableRedeemInfo {
    pub(super) nonce: u64,
    pub(super) from: Address,
    pub(super) to: Option<Address>,
    pub(super) value: U256,
    pub(super) data: Bytes,
}

#[derive(Debug)]
pub(super) enum StylusProgramError {
    Precompile(PrecompileError),
    ProgramNotActivated,
    ProgramNeedsUpgrade { version: u16, stylus_version: u16 },
    ProgramExpired { age: u64 },
    ProgramKeepaliveTooSoon { age: u64 },
}

impl From<PrecompileError> for StylusProgramError {
    fn from(error: PrecompileError) -> Self {
        Self::Precompile(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MultiGasPricingConstraint {
    pub(super) resources: [u64; NUM_RESOURCE_KIND],
    pub(super) adjustment_window_secs: u32,
    pub(super) target_per_sec: u64,
    pub(super) backlog: u64,
}

impl MultiGasPricingConstraint {
    pub(super) fn max_weight(&self) -> u64 {
        self.resources.iter().copied().max().unwrap_or_default()
    }
}

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(super) fn new_with_initial_gas(
        context: &'a mut ArbitrumContext<DB>,
        gas_limit: u64,
        initial_gas: u64,
    ) -> Self {
        Self {
            context,
            gas_limit,
            gas_used: initial_gas,
        }
    }

    pub(super) fn burn(&mut self, gas: u64) -> Result<(), PrecompileError> {
        self.gas_used = self.gas_used.saturating_add(gas);
        if self.gas_used > self.gas_limit {
            return Err(PrecompileError::OutOfGas);
        }
        Ok(())
    }

    pub(super) fn burn_out(&mut self) {
        self.gas_used = self.gas_limit;
    }

    pub(super) fn gas_left(&self) -> u64 {
        self.gas_limit.saturating_sub(self.gas_used)
    }

    pub(in crate::arbitrum::precompile) fn current_l2_block_number(&self) -> U256 {
        self.context
            .chain()
            .current_l2_block_number()
            .unwrap_or_else(|| self.context.block().number())
    }

    pub(in crate::arbitrum::precompile) fn current_l2_block_number_u64(&self) -> u64 {
        self.current_l2_block_number()
            .try_into()
            .unwrap_or_default()
    }

    pub(in crate::arbitrum::precompile) fn current_l2_basefee(&self) -> u64 {
        self.context
            .chain()
            .current_l2_basefee()
            .unwrap_or_else(|| self.context.block().basefee())
    }

    fn load_account(&mut self, account: Address) -> Result<(), PrecompileError> {
        self.context
            .journal_mut()
            .load_account(account)
            .map(|_| ())
            .map_err(|e| PrecompileError::other(format!("{e:?}")))
    }

    pub(super) fn read_key(
        &mut self,
        storage_key: &[u8],
        key: [u8; 32],
    ) -> Result<U256, PrecompileError> {
        self.read_account_key(arbos_state::ARBOS_STATE_ADDRESS, storage_key, key)
    }

    pub(super) fn read_account_key(
        &mut self,
        account: Address,
        storage_key: &[u8],
        key: [u8; 32],
    ) -> Result<U256, PrecompileError> {
        self.burn(STORAGE_READ_GAS)?;
        self.load_account(account)?;
        self.context
            .journal_mut()
            .sload(account, arbos_state::slot_for_key(storage_key, key))
            .map(|slot| slot.data)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))
    }

    pub(in crate::arbitrum::precompile::state) fn read_account_key_unmetered(
        &mut self,
        account: Address,
        storage_key: &[u8],
        key: [u8; 32],
    ) -> Result<U256, PrecompileError> {
        self.load_account(account)?;
        self.context
            .journal_mut()
            .sload(account, arbos_state::slot_for_key(storage_key, key))
            .map(|slot| slot.data)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))
    }

    pub(super) fn read(
        &mut self,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<U256, PrecompileError> {
        self.read_key(storage_key, U256::from(offset).to_be_bytes())
    }

    pub(super) fn write_key(
        &mut self,
        storage_key: &[u8],
        key: [u8; 32],
        value: U256,
    ) -> Result<(), PrecompileError> {
        self.write_account_key(arbos_state::ARBOS_STATE_ADDRESS, storage_key, key, value)
    }

    pub(super) fn write_account_key(
        &mut self,
        account: Address,
        storage_key: &[u8],
        key: [u8; 32],
        value: U256,
    ) -> Result<(), PrecompileError> {
        self.burn(if value.is_zero() {
            STORAGE_WRITE_ZERO_COST
        } else {
            STORAGE_WRITE_COST
        })?;
        self.load_account(account)?;
        self.context
            .journal_mut()
            .sstore(account, arbos_state::slot_for_key(storage_key, key), value)
            .map(|_| ())
            .map_err(|e| PrecompileError::other(format!("{e:?}")))
    }

    pub(in crate::arbitrum::precompile::state) fn write_account_key_unmetered(
        &mut self,
        account: Address,
        storage_key: &[u8],
        key: [u8; 32],
        value: U256,
    ) -> Result<(), PrecompileError> {
        self.load_account(account)?;
        self.context
            .journal_mut()
            .sstore(account, arbos_state::slot_for_key(storage_key, key), value)
            .map(|_| ())
            .map_err(|e| PrecompileError::other(format!("{e:?}")))
    }

    pub(super) fn write(
        &mut self,
        storage_key: &[u8],
        offset: u64,
        value: U256,
    ) -> Result<(), PrecompileError> {
        self.write_key(storage_key, U256::from(offset).to_be_bytes(), value)
    }

    pub(super) fn write_address(
        &mut self,
        storage_key: &[u8],
        offset: u64,
        value: Address,
    ) -> Result<(), PrecompileError> {
        self.write(storage_key, offset, U256::from_be_slice(value.as_slice()))
    }

    pub(super) fn root(&mut self, offset: u64) -> Result<U256, PrecompileError> {
        self.read(&[], offset)
    }

    pub(super) fn read_u64(
        &mut self,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<u64, PrecompileError> {
        self.read_u64_with_metering(storage_key, offset, true)
    }

    pub(in crate::arbitrum::precompile::state) fn read_u64_with_metering(
        &mut self,
        storage_key: &[u8],
        offset: u64,
        metered: bool,
    ) -> Result<u64, PrecompileError> {
        let key = U256::from(offset).to_be_bytes();
        let value = if metered {
            self.read_key(storage_key, key)?
        } else {
            self.read_account_key_unmetered(arbos_state::ARBOS_STATE_ADDRESS, storage_key, key)?
        };
        Ok(value.to::<u64>())
    }

    pub(in crate::arbitrum::precompile::state) fn write_with_metering(
        &mut self,
        storage_key: &[u8],
        offset: u64,
        value: U256,
        metered: bool,
    ) -> Result<(), PrecompileError> {
        let key = U256::from(offset).to_be_bytes();
        if metered {
            self.write_key(storage_key, key, value)
        } else {
            self.write_account_key_unmetered(
                arbos_state::ARBOS_STATE_ADDRESS,
                storage_key,
                key,
                value,
            )
        }
    }

    pub(super) fn read_address(
        &mut self,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<Address, PrecompileError> {
        Ok(address_from_word(self.read(storage_key, offset)?))
    }

    pub(in crate::arbitrum::precompile::state) fn read_address_or_nil(
        &mut self,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<Option<Address>, PrecompileError> {
        let value = self.read(storage_key, offset)?;
        if value == (U256::from(1u8) << 255) {
            return Ok(None);
        }
        Ok(Some(address_from_word(value)))
    }

    pub(super) fn arbos_version(&mut self) -> Result<u64, PrecompileError> {
        self.arbos_version_unmetered()
    }

    fn arbos_version_unmetered(&mut self) -> Result<u64, PrecompileError> {
        let slot = arbos_state::slot_for_key(
            &[],
            U256::from(arbos_state::ARBOS_VERSION_OFFSET).to_be_bytes(),
        );
        self.load_account(arbos_state::ARBOS_STATE_ADDRESS)?;
        self.context
            .journal_mut()
            .sload(arbos_state::ARBOS_STATE_ADDRESS, slot)
            .map(|slot| slot.data.to::<u64>())
            .map_err(|e| PrecompileError::other(format!("{e:?}")))
    }

    pub(super) fn l1_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::L1_PRICING_SUBSPACE)
    }

    pub(super) fn l2_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE)
    }

    pub(super) fn address_table_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::ADDRESS_TABLE_SUBSPACE)
    }

    pub(super) fn chain_owner_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::CHAIN_OWNER_SUBSPACE)
    }

    pub(super) fn send_merkle_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::SEND_MERKLE_SUBSPACE)
    }

    pub(super) fn chain_config_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::CHAIN_CONFIG_SUBSPACE)
    }

    pub(super) fn retryable_key(&self, ticket_id: B256) -> [u8; 32] {
        let retryables_key = arbos_state::child_key(&[], arbos_state::RETRYABLE_SUBSPACE);
        arbos_state::child_key(&retryables_key, ticket_id.as_slice())
    }

    pub(super) fn read_bytes(&mut self, storage_key: &[u8]) -> Result<Bytes, PrecompileError> {
        let size = self.read_u64(storage_key, 0)?;
        let mut bytes = Vec::new();
        let mut bytes_left = size;
        let mut offset = 1;

        while bytes_left >= 32 {
            let word = self.read(storage_key, offset)?;
            bytes.extend_from_slice(&word.to_be_bytes::<32>());
            bytes_left -= 32;
            offset += 1;
        }

        let word = self.read(storage_key, offset)?;
        if bytes_left > 0 {
            let encoded = word.to_be_bytes::<32>();
            bytes.extend_from_slice(&encoded[32 - bytes_left as usize..]);
        }

        Ok(bytes.into())
    }

    pub(super) fn write_bytes(
        &mut self,
        storage_key: &[u8],
        value: &[u8],
    ) -> Result<(), PrecompileError> {
        self.clear_bytes(storage_key)?;
        self.write(storage_key, 0, U256::from(value.len()))?;

        let mut offset = 1;
        let mut chunks = value.chunks_exact(32);
        for chunk in &mut chunks {
            self.write(storage_key, offset, U256::from_be_slice(chunk))?;
            offset += 1;
        }
        self.write(storage_key, offset, U256::from_be_slice(chunks.remainder()))?;
        Ok(())
    }

    fn clear_bytes(&mut self, storage_key: &[u8]) -> Result<(), PrecompileError> {
        let mut bytes_left = self.read_u64(storage_key, 0)?;
        let mut offset = 1;

        while bytes_left > 0 {
            self.write(storage_key, offset, U256::ZERO)?;
            offset += 1;
            bytes_left = bytes_left.saturating_sub(32);
        }
        self.write(storage_key, 0, U256::ZERO)
    }

    pub(in crate::arbitrum::precompile::state) fn retryable_calldata(
        &mut self,
        retryable_key: &[u8],
    ) -> Result<Bytes, PrecompileError> {
        let calldata_key = arbos_state::child_key(retryable_key, &[1]);
        self.read_bytes(&calldata_key)
    }

    pub(super) fn native_token_owner_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::NATIVE_TOKEN_OWNER_SUBSPACE)
    }

    pub(super) fn transaction_filterer_key(&self) -> [u8; 32] {
        arbos_state::child_key(&[], arbos_state::TRANSACTION_FILTERER_SUBSPACE)
    }

    pub(super) fn stylus_params_key(&self) -> [u8; 32] {
        let programs_key = arbos_state::child_key(&[], arbos_state::PROGRAMS_SUBSPACE);
        arbos_state::child_key(&programs_key, arbos_state::STYLUS_PARAMS_KEY)
    }

    pub(super) fn batch_poster_table_key(&self) -> [u8; 32] {
        let l1_key = self.l1_key();
        arbos_state::child_key(&l1_key, arbos_state::BATCH_POSTER_TABLE_SUBSPACE)
    }

    pub(super) fn batch_poster_addresses_key(&self) -> [u8; 32] {
        let table_key = self.batch_poster_table_key();
        arbos_state::child_key(&table_key, arbos_state::BATCH_POSTER_ADDRS_KEY)
    }

    pub(super) fn batch_poster_info_key(&self, poster: Address) -> [u8; 32] {
        let table_key = self.batch_poster_table_key();
        let info_key = arbos_state::child_key(&table_key, arbos_state::BATCH_POSTER_INFO_KEY);
        arbos_state::child_key(&info_key, poster.as_slice())
    }

    pub(super) fn wasm_cache_manager_key(&self) -> [u8; 32] {
        let programs_key = arbos_state::child_key(&[], arbos_state::PROGRAMS_SUBSPACE);
        arbos_state::child_key(&programs_key, &[4])
    }

    pub(super) fn wasm_data_pricer_key(&self) -> [u8; 32] {
        let programs_key = arbos_state::child_key(&[], arbos_state::PROGRAMS_SUBSPACE);
        arbos_state::child_key(&programs_key, &[3])
    }

    pub(super) fn wasm_activation_gas_key(&self) -> [u8; 32] {
        let programs_key = arbos_state::child_key(&[], arbos_state::PROGRAMS_SUBSPACE);
        arbos_state::child_key(&programs_key, &[5])
    }

    pub(super) fn wasm_module_hashes_key(&self) -> [u8; 32] {
        let programs_key = arbos_state::child_key(&[], arbos_state::PROGRAMS_SUBSPACE);
        arbos_state::child_key(&programs_key, &[2])
    }

    pub(super) fn wasm_programs_key(&self) -> [u8; 32] {
        let programs_key = arbos_state::child_key(&[], arbos_state::PROGRAMS_SUBSPACE);
        arbos_state::child_key(&programs_key, &[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::{Context, MainContext};

    fn context_without_loaded_account(basefee: u64) -> ArbitrumContext<CacheDB<EmptyDB>> {
        Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                basefee,
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default())
    }

    fn context(basefee: u64) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let mut context = context_without_loaded_account(basefee);
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        context
    }

    #[test]
    fn arbos_version_read_is_unmetered() {
        let mut context = context(0);
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(&[], arbos_state::ARBOS_VERSION_OFFSET, U256::from(60))
                .expect("write ArbOS version");
        }

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 123);

        assert_eq!(storage.arbos_version().expect("read ArbOS version"), 60);
        assert_eq!(storage.gas_used, 123);
    }

    #[test]
    fn arbos_version_read_loads_account_before_sload() {
        let mut context = context_without_loaded_account(0);
        let slot = arbos_state::slot_for_key(
            &[],
            U256::from(arbos_state::ARBOS_VERSION_OFFSET).to_be_bytes(),
        );
        context
            .db_mut()
            .insert_account_storage(arbos_state::ARBOS_STATE_ADDRESS, slot, U256::from(60))
            .expect("seed ArbOS version");

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 123);

        assert_eq!(storage.arbos_version().expect("read ArbOS version"), 60);
        assert_eq!(storage.gas_used, 123);
    }

    #[test]
    fn storage_access_loads_account_and_preserves_journal_writes() {
        let mut context = context_without_loaded_account(0);
        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);

        storage
            .write(&[], arbos_state::ARBOS_VERSION_OFFSET, U256::from(60))
            .expect("write ArbOS version");

        assert_eq!(
            storage
                .read(&[], arbos_state::ARBOS_VERSION_OFFSET)
                .expect("read ArbOS version"),
            U256::from(60)
        );
    }

    #[test]
    fn stylus_params_read_charges_warm_storage_read() {
        let mut context = context(0);
        let params = StylusParams {
            version: 1,
            ink_price: 2,
            max_stack_depth: 3,
            free_pages: 4,
            page_gas: 5,
            page_limit: 6,
            min_init_gas: 7,
            min_cached_init_gas: 8,
            init_cost_scalar: 9,
            cached_cost_scalar: 10,
            expiry_days: 11,
            keepalive_days: 12,
            block_cache_size: 13,
            max_wasm_size: 14,
            max_fragment_count: 15,
        };
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(&[], arbos_state::ARBOS_VERSION_OFFSET, U256::from(60))
                .expect("write ArbOS version");
            storage
                .save_stylus_params(params)
                .expect("write stylus params");
        }

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 7);

        assert_eq!(storage.stylus_params().expect("read stylus params"), params);
        assert_eq!(storage.gas_used, 7 + WARM_STORAGE_READ_GAS);
    }

    #[test]
    fn gas_prices_use_legacy_formula_before_arbos_4() {
        let mut context = context(100);
        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let l1_key = storage.l1_key();
        let l2_key = storage.l2_key();
        storage
            .write(
                &l1_key,
                arbos_state::L1_PRICE_PER_UNIT_OFFSET,
                U256::from(10),
            )
            .expect("write l1 price");
        storage
            .write(
                &l2_key,
                arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET,
                U256::from(25),
            )
            .expect("write min base fee");

        let legacy_wei = storage.gas_prices_in_wei(3).expect("legacy wei prices");
        let current_wei = storage.gas_prices_in_wei(4).expect("current wei prices");
        assert_eq!(legacy_wei.3, U256::from(100));
        assert_eq!(legacy_wei.4, U256::ZERO);
        assert_eq!(current_wei.3, U256::from(25));
        assert_eq!(current_wei.4, U256::from(75));

        let legacy_gas = storage
            .gas_prices_in_arb_gas(3)
            .expect("legacy arb gas prices");
        let current_gas = storage
            .gas_prices_in_arb_gas(4)
            .expect("current arb gas prices");
        assert_eq!(legacy_gas.0, U256::from(ASSUMED_SIMPLE_TX_SIZE));
        assert_eq!(current_gas.0, U256::from(224));
        assert_eq!(legacy_gas.1, current_gas.1);
    }

    #[test]
    fn multi_gas_base_fee_fallback_uses_state_base_fee() {
        let mut context = context(500);
        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let l2_key = storage.l2_key();
        let fees_key = storage.multi_gas_base_fees_key();
        storage
            .write(
                &l2_key,
                arbos_state::L2_BASE_FEE_WEI_OFFSET,
                U256::from(123),
            )
            .expect("write l2 base fee");
        storage
            .write(&fees_key, NUM_RESOURCE_KIND as u64, U256::from(777))
            .expect("write resource fee");
        storage
            .write(
                &fees_key,
                NUM_RESOURCE_KIND as u64 + RESOURCE_KIND_SINGLE_DIM as u64,
                U256::from(999),
            )
            .expect("write single-dimensional fee");

        let fees = storage
            .multi_gas_current_base_fees()
            .expect("multi-gas base fees");
        assert_eq!(fees[0], U256::from(777));
        assert_eq!(fees[1], U256::from(123));
        assert_eq!(fees[RESOURCE_KIND_SINGLE_DIM], U256::from(123));
    }
}
