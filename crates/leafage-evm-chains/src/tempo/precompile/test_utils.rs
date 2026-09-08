//! Shared in-memory storage provider for Tempo precompile tests.

use std::collections::HashMap;

use alloy::primitives::{Address, LogData, U256};
use revm::context_interface::cfg::{gas_params::GasId, GasParams};
use revm::interpreter::{SStoreResult, StateLoad};
use revm::state::{AccountInfo, Bytecode};

use super::error::{Result, TempoPrecompileError};
use super::storage::{JournalCheckpoint, PrecompileStorageProvider};
use super::storage_credits::{
    account_storage_write, is_non_creditable_slot, AccountingError, StorageCreditsBackend,
};
use crate::tempo::hardfork::TempoHardfork;

#[derive(Clone)]
struct Snapshot {
    storage: HashMap<(Address, U256), U256>,
    transient: HashMap<(Address, U256), U256>,
    accounts: HashMap<Address, AccountInfo>,
    events: HashMap<Address, Vec<LogData>>,
}

/// Write-enabled [`PrecompileStorageProvider`] used by unit and dispatch tests.
pub(crate) struct TestStorageProvider {
    storage: HashMap<(Address, U256), U256>,
    transient: HashMap<(Address, U256), U256>,
    accounts: HashMap<Address, AccountInfo>,
    events: HashMap<Address, Vec<LogData>>,
    snapshots: Vec<Snapshot>,
    chain_id: u64,
    timestamp: U256,
    beneficiary: Address,
    block_number: u64,
    spec: TempoHardfork,
    is_static: bool,
    gas_limit: u64,
    gas_remaining: u64,
    gas_refunded: i64,
    tip1060_storage_credits_enabled: bool,
    tip1060_storage_credit_minting_enabled: bool,
}

impl TestStorageProvider {
    pub(crate) fn new(spec: TempoHardfork) -> Self {
        Self {
            storage: HashMap::new(),
            transient: HashMap::new(),
            accounts: HashMap::new(),
            events: HashMap::new(),
            snapshots: Vec::new(),
            chain_id: 1,
            timestamp: U256::ZERO,
            beneficiary: Address::ZERO,
            block_number: 0,
            spec,
            is_static: false,
            gas_limit: u64::MAX,
            gas_remaining: u64::MAX,
            gas_refunded: 0,
            tip1060_storage_credits_enabled: spec.is_t7(),
            tip1060_storage_credit_minting_enabled: true,
        }
    }

    pub(crate) fn storage(&self, address: Address, slot: U256) -> U256 {
        self.storage
            .get(&(address, slot))
            .copied()
            .unwrap_or(U256::ZERO)
    }

    pub(crate) fn has_storage_entry(&self, address: Address, slot: U256) -> bool {
        self.storage.contains_key(&(address, slot))
    }

    pub(crate) fn storage_len(&self) -> usize {
        self.storage.len()
    }

    pub(crate) fn transient(&self, address: Address, slot: U256) -> U256 {
        self.transient
            .get(&(address, slot))
            .copied()
            .unwrap_or(U256::ZERO)
    }

    pub(crate) fn events(&self, address: Address) -> &[LogData] {
        self.events.get(&address).map(Vec::as_slice).unwrap_or(&[])
    }

    pub(crate) fn account(&self, address: Address) -> Option<&AccountInfo> {
        self.accounts.get(&address)
    }

    pub(crate) fn set_spec(&mut self, spec: TempoHardfork) {
        self.spec = spec;
        self.tip1060_storage_credits_enabled = spec.is_t7();
    }

    pub(crate) fn set_timestamp(&mut self, timestamp: U256) {
        self.timestamp = timestamp;
    }

    pub(crate) fn set_beneficiary(&mut self, beneficiary: Address) {
        self.beneficiary = beneficiary;
    }

    pub(crate) fn set_block_number(&mut self, block_number: u64) {
        self.block_number = block_number;
    }

    pub(crate) fn set_static(&mut self, is_static: bool) {
        self.is_static = is_static;
    }

    pub(crate) fn set_gas_limit(&mut self, gas_limit: u64) {
        self.gas_limit = gas_limit;
        self.gas_remaining = gas_limit;
        self.gas_refunded = 0;
    }
}

impl PrecompileStorageProvider for TestStorageProvider {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.timestamp
    }

    fn beneficiary(&self) -> Address {
        self.beneficiary
    }

    fn block_number(&self) -> u64 {
        self.block_number
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        let account = self.accounts.entry(address).or_default();
        account.code_hash = code.hash_slow();
        account.code = Some(code);
        Ok(())
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        f(self.accounts.entry(address).or_default());
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(self.storage(address, key))
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(self.transient(address, key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        let present_value = self.storage(address, key);
        self.storage.insert((address, key), value);
        if self.tip1060_storage_credits_enabled {
            let result = StateLoad {
                data: SStoreResult {
                    original_value: present_value,
                    present_value,
                    new_value: value,
                },
                is_cold: false,
            };
            account_storage_write(self, address, Some(key), &result).map_err(|error| match error {
                AccountingError::OutOfGas => TempoPrecompileError::OutOfGas,
                AccountingError::Fatal => {
                    TempoPrecompileError::Fatal("storage credit accounting failed".into())
                }
            })?;
        }
        Ok(())
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        self.transient.insert((address, key), value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        self.events.entry(address).or_default().push(event);
        Ok(())
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        self.gas_remaining = self
            .gas_remaining
            .checked_sub(gas)
            .ok_or(TempoPrecompileError::OutOfGas)?;
        Ok(())
    }

    fn refund_gas(&mut self, gas: i64) {
        self.gas_refunded = self.gas_refunded.saturating_add(gas);
    }

    fn gas_used(&self) -> u64 {
        self.gas_limit - self.gas_remaining
    }

    fn gas_refunded(&self) -> i64 {
        self.gas_refunded
    }

    fn spec(&self) -> TempoHardfork {
        self.spec
    }

    fn is_static(&self) -> bool {
        self.is_static
    }

    fn set_tip1060_storage_credits(&mut self, enabled: bool) {
        self.tip1060_storage_credits_enabled = enabled && self.spec.is_t7();
    }

    fn set_tip1060_storage_credit_minting(&mut self, enabled: bool) {
        self.tip1060_storage_credit_minting_enabled = enabled;
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        let journal_i = self.snapshots.len();
        self.snapshots.push(Snapshot {
            storage: self.storage.clone(),
            transient: self.transient.clone(),
            accounts: self.accounts.clone(),
            events: self.events.clone(),
        });
        JournalCheckpoint {
            log_i: 0,
            journal_i,
            selfdestructed_i: 0,
        }
    }

    fn checkpoint_commit(&mut self, checkpoint: JournalCheckpoint) {
        assert_eq!(checkpoint.journal_i, self.snapshots.len() - 1);
        self.snapshots.pop();
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        assert_eq!(checkpoint.journal_i, self.snapshots.len() - 1);
        let snapshot = self.snapshots.pop().expect("checkpoint snapshot exists");
        self.storage = snapshot.storage;
        self.transient = snapshot.transient;
        self.accounts = snapshot.accounts;
        self.events = snapshot.events;
    }
}

impl StorageCreditsBackend for TestStorageProvider {
    fn gas_params(&self) -> GasParams {
        let mut params = GasParams::new_spec(revm::primitives::hardfork::SpecId::OSAKA);
        if self.spec.is_t7() {
            params.override_gas([
                (GasId::sstore_set_without_load_cost(), 5_000),
                (GasId::sstore_set_refund(), 5_000),
                (GasId::sstore_clearing_slot_refund(), 0),
            ]);
        }
        params
    }

    fn remaining_gas(&self) -> u64 {
        self.gas_remaining
    }

    fn charge_gas(&mut self, gas: u64) -> core::result::Result<(), AccountingError> {
        self.gas_remaining = self
            .gas_remaining
            .checked_sub(gas)
            .ok_or(AccountingError::OutOfGas)?;
        Ok(())
    }

    fn sload_raw(
        &mut self,
        address: Address,
        key: U256,
        _skip_cold: bool,
    ) -> core::result::Result<StateLoad<U256>, AccountingError> {
        Ok(StateLoad {
            data: self.storage(address, key),
            is_cold: false,
        })
    }

    fn sstore_raw(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> core::result::Result<StateLoad<SStoreResult>, AccountingError> {
        let present_value = self.storage(address, key);
        self.storage.insert((address, key), value);
        Ok(StateLoad {
            data: SStoreResult {
                original_value: present_value,
                present_value,
                new_value: value,
            },
            is_cold: false,
        })
    }

    fn tload_raw(&mut self, address: Address, key: U256) -> U256 {
        self.transient(address, key)
    }

    fn tstore_raw(&mut self, address: Address, key: U256, value: U256) {
        self.transient.insert((address, key), value);
    }

    fn is_non_creditable_slot(&self, owner: Address, key: U256) -> bool {
        is_non_creditable_slot(owner, key)
    }

    fn storage_credit_minting_enabled(&self) -> bool {
        self.tip1060_storage_credit_minting_enabled
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Bytes};

    use super::*;
    use crate::tempo::precompile::storage::StorageCtx;

    #[test]
    fn writes_persistent_and_transient_storage() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        let address = address!("0x1111111111111111111111111111111111111111");

        StorageCtx::enter(&mut provider, || {
            StorageCtx.sstore(address, U256::from(1), U256::from(2))?;
            StorageCtx.tstore(address, U256::from(3), U256::from(4))
        })
        .unwrap();

        assert_eq!(provider.storage(address, U256::from(1)), U256::from(2));
        assert_eq!(provider.transient(address, U256::from(3)), U256::from(4));
    }

    #[test]
    fn checkpoint_reverts_storage_code_and_events() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        let address = address!("0x2222222222222222222222222222222222222222");

        StorageCtx::enter(&mut provider, || {
            let mut storage = StorageCtx;
            let checkpoint = storage.checkpoint();
            storage.sstore(address, U256::ZERO, U256::ONE)?;
            storage.set_code(address, Bytecode::new_raw(Bytes::from_static(&[0x00])))?;
            storage.emit_event(address, LogData::empty())?;
            drop(checkpoint);
            Result::<()>::Ok(())
        })
        .unwrap();

        assert_eq!(provider.storage(address, U256::ZERO), U256::ZERO);
        assert!(provider.account(address).is_none());
        assert!(provider.events(address).is_empty());
    }
}
