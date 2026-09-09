//! Storage layout of the staking contract (`staking_contract.hpp`
//! `Variables`, `util/{val_execution,delegator,consensus_view}.hpp`) and the
//! journal access used to read and write it.
//!
//! `StorageVariable<T>` stores `T` by `memcpy` into consecutive slots starting
//! at the key: values are *left aligned* in their slot (a `u64_be` occupies
//! bytes `0..8`) and structs are laid out field after field without padding.
//! Every accessor below spells out the byte offsets so the layout stays
//! auditable against the C++ headers.

use super::error::{Failure, StakingResult};
use crate::monad::precompile::STAKING_CONTRACT_ADDRESS;
use revm::context_interface::journaled_state::TransferError;
use revm::context_interface::JournalTr;
use revm::primitives::{Address, Log, U256};

// Single slot constants under namespace 0x0.
const ADDRESS_EPOCH: U256 = U256::from_limbs([1, 0, 0, 0]);
const ADDRESS_IN_BOUNDARY: U256 = U256::from_limbs([2, 0, 0, 0]);
const ADDRESS_LAST_VAL_ID: U256 = U256::from_limbs([3, 0, 0, 0]);
const ADDRESS_PROPOSER_VAL_ID: U256 = U256::from_limbs([4, 0, 0, 0]);

// Working valsets get namespaces 0x1, 0x2, 0x3 (first byte of the key).
const ADDRESS_VALSET_EXECUTION: U256 = U256::from_limbs([0, 0, 0, 0x01 << 56]);
const ADDRESS_VALSET_CONSENSUS: U256 = U256::from_limbs([0, 0, 0, 0x02 << 56]);
const ADDRESS_VALSET_SNAPSHOT: U256 = U256::from_limbs([0, 0, 0, 0x03 << 56]);

/// `Variables::Namespace`.
mod ns {
    pub(super) const CONSENSUS_STAKE: u8 = 0x04;
    pub(super) const SNAPSHOT_STAKE: u8 = 0x05;
    pub(super) const VAL_ID_SECP: u8 = 0x06;
    pub(super) const VAL_ID_BLS: u8 = 0x07;
    pub(super) const VAL_BITSET: u8 = 0x08;
    pub(super) const VAL_EXECUTION: u8 = 0x09;
    pub(super) const ACCUMULATOR: u8 = 0x0A;
    pub(super) const DELEGATOR: u8 = 0x0B;
    pub(super) const WITHDRAWAL_REQUEST: u8 = 0x0C;
}

fn key_from_bytes(bytes: [u8; 32]) -> U256 {
    U256::from_be_bytes(bytes)
}

/// `struct { u8_be ns; Address address; uint8_t slots[11]; }`
fn key_ns_address(namespace: u8, address: Address) -> U256 {
    let mut key = [0u8; 32];
    key[0] = namespace;
    key[1..21].copy_from_slice(address.as_slice());
    key_from_bytes(key)
}

/// `struct { u8_be ns; u64_be id; uint8_t slots[23]; }`
fn key_ns_u64(namespace: u8, id: u64) -> U256 {
    let mut key = [0u8; 32];
    key[0] = namespace;
    key[1..9].copy_from_slice(&id.to_be_bytes());
    key_from_bytes(key)
}

/// `struct { u8_be ns; u64_be val_id; Address address; uint8_t slots[3]; }`
fn key_delegator(val_id: u64, address: Address) -> U256 {
    let mut key = [0u8; 32];
    key[0] = ns::DELEGATOR;
    key[1..9].copy_from_slice(&val_id.to_be_bytes());
    key[9..29].copy_from_slice(address.as_slice());
    key_from_bytes(key)
}

/// `struct { u8_be ns; u64_be val_id; Address address; u8_be withdrawal_id; uint8_t slots[2]; }`
fn key_withdrawal_request(val_id: u64, address: Address, withdrawal_id: u8) -> U256 {
    let mut key = [0u8; 32];
    key[0] = ns::WITHDRAWAL_REQUEST;
    key[1..9].copy_from_slice(&val_id.to_be_bytes());
    key[9..29].copy_from_slice(address.as_slice());
    key[29] = withdrawal_id;
    key_from_bytes(key)
}

/// `struct { u8_be ns; u64_be epoch; u64_be val_id; uint8_t slots[15]; }`
fn key_accumulator(epoch: u64, val_id: u64) -> U256 {
    let mut key = [0u8; 32];
    key[0] = ns::ACCUMULATOR;
    key[1..9].copy_from_slice(&epoch.to_be_bytes());
    key[9..17].copy_from_slice(&val_id.to_be_bytes());
    key_from_bytes(key)
}

fn slot_u64(slot: U256) -> u64 {
    u64::from_be_bytes(slot.to_be_bytes::<32>()[..8].try_into().unwrap())
}

fn u64_slot(value: u64) -> U256 {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&value.to_be_bytes());
    U256::from_be_bytes(bytes)
}

/// `ValExecution::AddressFlags` (one slot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AddressFlags {
    pub auth_address: Address,
    pub flags: u64,
}

impl AddressFlags {
    fn decode(slot: U256) -> Self {
        let bytes = slot.to_be_bytes::<32>();
        Self {
            auth_address: Address::from_slice(&bytes[..20]),
            flags: u64::from_be_bytes(bytes[20..28].try_into().unwrap()),
        }
    }

    fn encode(self) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[..20].copy_from_slice(self.auth_address.as_slice());
        bytes[20..28].copy_from_slice(&self.flags.to_be_bytes());
        U256::from_be_bytes(bytes)
    }
}

/// `ValExecution::KeysPacked` (three slots: 33 + 48 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeysPacked {
    pub secp_pubkey: [u8; 33],
    pub bls_pubkey: [u8; 48],
}

impl KeysPacked {
    fn decode(bytes: [u8; 96]) -> Self {
        Self {
            secp_pubkey: bytes[..33].try_into().unwrap(),
            bls_pubkey: bytes[33..81].try_into().unwrap(),
        }
    }

    fn encode(self) -> [u8; 96] {
        let mut bytes = [0u8; 96];
        bytes[..33].copy_from_slice(&self.secp_pubkey);
        bytes[33..81].copy_from_slice(&self.bls_pubkey);
        bytes
    }
}

/// `Delegator::Epochs` (one slot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Epochs {
    pub delta_epoch: u64,
    pub next_delta_epoch: u64,
}

impl Epochs {
    fn decode(slot: U256) -> Self {
        let bytes = slot.to_be_bytes::<32>();
        Self {
            delta_epoch: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            next_delta_epoch: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
        }
    }

    fn encode(self) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&self.delta_epoch.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.next_delta_epoch.to_be_bytes());
        U256::from_be_bytes(bytes)
    }
}

/// `Delegator::ListNode` (two slots: 8 + 8 + 20 + 20 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ListNode {
    pub inext: u64,
    pub iprev: u64,
    pub anext: Address,
    pub aprev: Address,
}

impl ListNode {
    fn decode(bytes: [u8; 64]) -> Self {
        Self {
            inext: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            iprev: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
            anext: Address::from_slice(&bytes[16..36]),
            aprev: Address::from_slice(&bytes[36..56]),
        }
    }

    fn encode(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[..8].copy_from_slice(&self.inext.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.iprev.to_be_bytes());
        bytes[16..36].copy_from_slice(self.anext.as_slice());
        bytes[36..56].copy_from_slice(self.aprev.as_slice());
        bytes
    }
}

/// `StakingContract::WithdrawalRequest` (three slots: 32 + 32 + 8 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WithdrawalRequest {
    pub amount: U256,
    pub acc: U256,
    pub epoch: u64,
}

impl WithdrawalRequest {
    fn decode(slots: [U256; 3]) -> Self {
        Self {
            amount: slots[0],
            acc: slots[1],
            epoch: slot_u64(slots[2]),
        }
    }

    fn encode(self) -> [U256; 3] {
        [self.amount, self.acc, u64_slot(self.epoch)]
    }
}

/// `StakingContract::RefCountedAccumulator` (two slots).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RefCountedAccumulator {
    pub value: U256,
    pub refcount: U256,
}

/// Slot keys of a `ValExecution` entry.
#[derive(Clone, Copy)]
pub(crate) struct ValExecution {
    base: U256,
}

impl ValExecution {
    pub(crate) fn stake(self) -> U256 {
        self.base
    }
    pub(crate) fn accumulated_reward_per_token(self) -> U256 {
        self.base + U256::from(1)
    }
    pub(crate) fn commission(self) -> U256 {
        self.base + U256::from(2)
    }
    pub(crate) fn keys(self) -> U256 {
        self.base + U256::from(3)
    }
    pub(crate) fn address_flags(self) -> U256 {
        self.base + U256::from(6)
    }
    pub(crate) fn unclaimed_rewards(self) -> U256 {
        self.base + U256::from(7)
    }
}

/// Slot keys of a `Delegator` entry.
#[derive(Clone, Copy)]
pub(crate) struct Delegator {
    base: U256,
}

impl Delegator {
    pub(crate) fn stake(self) -> U256 {
        self.base
    }
    pub(crate) fn accumulated_reward_per_token(self) -> U256 {
        self.base + U256::from(1)
    }
    pub(crate) fn rewards(self) -> U256 {
        self.base + U256::from(2)
    }
    pub(crate) fn delta_stake(self) -> U256 {
        self.base + U256::from(3)
    }
    pub(crate) fn next_delta_stake(self) -> U256 {
        self.base + U256::from(4)
    }
    pub(crate) fn epochs(self) -> U256 {
        self.base + U256::from(5)
    }
    pub(crate) fn list_node(self) -> U256 {
        self.base + U256::from(6)
    }
}

/// Slot keys of a `ConsensusView` / `SnapshotView` entry.
#[derive(Clone, Copy)]
pub(crate) struct ConsensusView {
    base: U256,
}

impl ConsensusView {
    pub(crate) fn stake(self) -> U256 {
        self.base
    }
    pub(crate) fn commission(self) -> U256 {
        self.base + U256::from(1)
    }
}

/// `StorageArray<u64_be>`: length at `base`, element `i` at `base + 1 + i`.
#[derive(Clone, Copy)]
pub(crate) struct Valset {
    base: U256,
}

/// Journal backed storage of the staking contract account.
pub(crate) struct StakingState<'a, J: ?Sized> {
    journal: &'a mut J,
}

impl<'a, J: JournalTr + ?Sized> StakingState<'a, J> {
    pub(crate) fn new(journal: &'a mut J) -> Self {
        Self { journal }
    }

    fn fatal<E: core::fmt::Display>(error: E) -> Failure {
        Failure::Fatal(error.to_string())
    }

    // -- raw slots --------------------------------------------------------

    pub(crate) fn load(&mut self, key: U256) -> StakingResult<U256> {
        self.journal
            .sload(STAKING_CONTRACT_ADDRESS, key)
            .map(|load| load.data)
            .map_err(Self::fatal)
    }

    pub(crate) fn store(&mut self, key: U256, value: U256) -> StakingResult<()> {
        self.journal
            .sstore(STAKING_CONTRACT_ADDRESS, key, value)
            .map(|_| ())
            .map_err(Self::fatal)
    }

    fn load_slots<const N: usize>(&mut self, key: U256) -> StakingResult<[U256; N]> {
        let mut slots = [U256::ZERO; N];
        for (i, slot) in slots.iter_mut().enumerate() {
            *slot = self.load(key + U256::from(i))?;
        }
        Ok(slots)
    }

    fn store_slots<const N: usize>(&mut self, key: U256, slots: [U256; N]) -> StakingResult<()> {
        for (i, slot) in slots.into_iter().enumerate() {
            self.store(key + U256::from(i), slot)?;
        }
        Ok(())
    }

    fn load_bytes<const N: usize, const BYTES: usize>(
        &mut self,
        key: U256,
    ) -> StakingResult<[u8; BYTES]> {
        debug_assert_eq!(N * 32, BYTES);
        let slots = self.load_slots::<N>(key)?;
        let mut bytes = [0u8; BYTES];
        for (i, slot) in slots.iter().enumerate() {
            bytes[i * 32..(i + 1) * 32].copy_from_slice(&slot.to_be_bytes::<32>());
        }
        Ok(bytes)
    }

    fn store_bytes<const N: usize, const BYTES: usize>(
        &mut self,
        key: U256,
        bytes: [u8; BYTES],
    ) -> StakingResult<()> {
        debug_assert_eq!(N * 32, BYTES);
        let mut slots = [U256::ZERO; N];
        for (i, slot) in slots.iter_mut().enumerate() {
            *slot = U256::from_be_slice(&bytes[i * 32..(i + 1) * 32]);
        }
        self.store_slots(key, slots)
    }

    /// `StorageVariable<T>::clear`.
    pub(crate) fn clear<const N: usize>(&mut self, key: U256) -> StakingResult<()> {
        self.store_slots(key, [U256::ZERO; N])
    }

    /// `StorageVariable<T>::load_checked().has_value()`.
    pub(crate) fn has_data<const N: usize>(&mut self, key: U256) -> StakingResult<bool> {
        Ok(self
            .load_slots::<N>(key)?
            .iter()
            .any(|slot| !slot.is_zero()))
    }

    // -- typed slots ------------------------------------------------------

    pub(crate) fn load_u64(&mut self, key: U256) -> StakingResult<u64> {
        Ok(slot_u64(self.load(key)?))
    }

    pub(crate) fn store_u64(&mut self, key: U256, value: u64) -> StakingResult<()> {
        self.store(key, u64_slot(value))
    }

    /// `StorageVariable<bool>::load`: first byte of the slot.
    pub(crate) fn load_bool(&mut self, key: U256) -> StakingResult<bool> {
        Ok(self.load(key)?.to_be_bytes::<32>()[0] != 0)
    }

    pub(crate) fn load_address_flags(&mut self, key: U256) -> StakingResult<AddressFlags> {
        Ok(AddressFlags::decode(self.load(key)?))
    }

    pub(crate) fn store_address_flags(
        &mut self,
        key: U256,
        value: AddressFlags,
    ) -> StakingResult<()> {
        self.store(key, value.encode())
    }

    pub(crate) fn load_keys(&mut self, key: U256) -> StakingResult<KeysPacked> {
        Ok(KeysPacked::decode(self.load_bytes::<3, 96>(key)?))
    }

    pub(crate) fn store_keys(&mut self, key: U256, value: KeysPacked) -> StakingResult<()> {
        self.store_bytes::<3, 96>(key, value.encode())
    }

    pub(crate) fn load_epochs(&mut self, key: U256) -> StakingResult<Epochs> {
        Ok(Epochs::decode(self.load(key)?))
    }

    pub(crate) fn store_epochs(&mut self, key: U256, value: Epochs) -> StakingResult<()> {
        self.store(key, value.encode())
    }

    pub(crate) fn load_list_node(&mut self, key: U256) -> StakingResult<ListNode> {
        Ok(ListNode::decode(self.load_bytes::<2, 64>(key)?))
    }

    pub(crate) fn store_list_node(&mut self, key: U256, value: ListNode) -> StakingResult<()> {
        self.store_bytes::<2, 64>(key, value.encode())
    }

    pub(crate) fn load_withdrawal_request(
        &mut self,
        key: U256,
    ) -> StakingResult<WithdrawalRequest> {
        Ok(WithdrawalRequest::decode(self.load_slots::<3>(key)?))
    }

    pub(crate) fn store_withdrawal_request(
        &mut self,
        key: U256,
        value: WithdrawalRequest,
    ) -> StakingResult<()> {
        self.store_slots(key, value.encode())
    }

    pub(crate) fn load_accumulator(&mut self, key: U256) -> StakingResult<RefCountedAccumulator> {
        let [value, refcount] = self.load_slots::<2>(key)?;
        Ok(RefCountedAccumulator { value, refcount })
    }

    pub(crate) fn store_accumulator(
        &mut self,
        key: U256,
        value: RefCountedAccumulator,
    ) -> StakingResult<()> {
        self.store_slots(key, [value.value, value.refcount])
    }

    // -- Variables --------------------------------------------------------

    pub(crate) fn epoch(&mut self) -> StakingResult<u64> {
        self.load_u64(ADDRESS_EPOCH)
    }

    /// `in_epoch_delay_period.load()`.
    pub(crate) fn in_epoch_delay_period(&mut self) -> StakingResult<bool> {
        self.load_bool(ADDRESS_IN_BOUNDARY)
    }

    /// `in_epoch_delay_period.load_checked().has_value()`.
    pub(crate) fn in_epoch_delay_period_set(&mut self) -> StakingResult<bool> {
        self.has_data::<1>(ADDRESS_IN_BOUNDARY)
    }

    pub(crate) fn last_val_id(&mut self) -> StakingResult<u64> {
        self.load_u64(ADDRESS_LAST_VAL_ID)
    }

    pub(crate) fn store_last_val_id(&mut self, value: u64) -> StakingResult<()> {
        self.store_u64(ADDRESS_LAST_VAL_ID, value)
    }

    pub(crate) fn proposer_val_id(&mut self) -> StakingResult<u64> {
        self.load_u64(ADDRESS_PROPOSER_VAL_ID)
    }

    pub(crate) fn valset_execution(&self) -> Valset {
        Valset {
            base: ADDRESS_VALSET_EXECUTION,
        }
    }

    pub(crate) fn valset_consensus(&self) -> Valset {
        Valset {
            base: ADDRESS_VALSET_CONSENSUS,
        }
    }

    pub(crate) fn valset_snapshot(&self) -> Valset {
        Valset {
            base: ADDRESS_VALSET_SNAPSHOT,
        }
    }

    pub(crate) fn valset_length(&mut self, valset: Valset) -> StakingResult<u64> {
        self.load_u64(valset.base)
    }

    pub(crate) fn valset_get(&mut self, valset: Valset, index: u64) -> StakingResult<u64> {
        self.load_u64(valset.base + U256::from(1) + U256::from(index))
    }

    /// `StorageArray::push`.
    pub(crate) fn valset_push(&mut self, valset: Valset, value: u64) -> StakingResult<()> {
        let len = self.valset_length(valset)?;
        self.store_u64(valset.base + U256::from(1) + U256::from(len), value)?;
        self.store_u64(valset.base, len + 1)
    }

    /// `mapping (address => uint64) val_id`, secp derived address.
    pub(crate) fn val_id(&self, secp_eth_address: Address) -> U256 {
        key_ns_address(ns::VAL_ID_SECP, secp_eth_address)
    }

    /// `mapping (address => uint64) val_id_bls`.
    pub(crate) fn val_id_bls(&self, bls_eth_address: Address) -> U256 {
        key_ns_address(ns::VAL_ID_BLS, bls_eth_address)
    }

    /// `mapping(uint64 => uint256) in_valset_bitset`, bucketed by `val_id >> 8`.
    pub(crate) fn val_bitset_bucket(&self, val_id: u64) -> U256 {
        key_ns_u64(ns::VAL_BITSET, val_id >> 8)
    }

    pub(crate) fn val_execution(&self, val_id: u64) -> ValExecution {
        ValExecution {
            base: key_ns_u64(ns::VAL_EXECUTION, val_id),
        }
    }

    pub(crate) fn consensus_view(&self, val_id: u64) -> ConsensusView {
        ConsensusView {
            base: key_ns_u64(ns::CONSENSUS_STAKE, val_id),
        }
    }

    pub(crate) fn snapshot_view(&self, val_id: u64) -> ConsensusView {
        ConsensusView {
            base: key_ns_u64(ns::SNAPSHOT_STAKE, val_id),
        }
    }

    /// `Variables::this_epoch_view`.
    pub(crate) fn this_epoch_view(&mut self, val_id: u64) -> StakingResult<ConsensusView> {
        Ok(if self.in_epoch_delay_period_set()? {
            self.snapshot_view(val_id)
        } else {
            self.consensus_view(val_id)
        })
    }

    pub(crate) fn delegator(&self, val_id: u64, address: Address) -> Delegator {
        Delegator {
            base: key_delegator(val_id, address),
        }
    }

    pub(crate) fn withdrawal_request(
        &self,
        val_id: u64,
        delegator: Address,
        withdrawal_id: u8,
    ) -> U256 {
        key_withdrawal_request(val_id, delegator, withdrawal_id)
    }

    pub(crate) fn accumulated_reward_per_token(&self, epoch: u64, val_id: u64) -> U256 {
        key_accumulator(epoch, val_id)
    }

    // -- balances and logs ------------------------------------------------

    pub(crate) fn balance(&mut self, address: Address) -> StakingResult<U256> {
        self.journal
            .load_account(address)
            .map(|account| account.data.info.balance)
            .map_err(Self::fatal)
    }

    /// `send_tokens`: move `amount` from the staking contract to `to`.
    pub(crate) fn send_tokens(
        &mut self,
        to: Address,
        amount: U256,
    ) -> StakingResult<Option<TransferError>> {
        self.journal
            .transfer(STAKING_CONTRACT_ADDRESS, to, amount)
            .map_err(Self::fatal)
    }

    pub(crate) fn emit_log(&mut self, log: Log) {
        self.journal.log(log);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::address;

    #[test]
    fn keys_match_cpp_bit_cast_layout() {
        let addr = address!("1111111111111111111111111111111111111111");
        let mut expected = [0u8; 32];
        expected[0] = 0x0B;
        expected[1..9].copy_from_slice(&0x0102u64.to_be_bytes());
        expected[9..29].copy_from_slice(addr.as_slice());
        assert_eq!(key_delegator(0x0102, addr), U256::from_be_bytes(expected));

        expected[29] = 7;
        expected[0] = 0x0C;
        assert_eq!(
            key_withdrawal_request(0x0102, addr, 7),
            U256::from_be_bytes(expected)
        );

        let mut expected = [0u8; 32];
        expected[0] = 0x0A;
        expected[1..9].copy_from_slice(&5u64.to_be_bytes());
        expected[9..17].copy_from_slice(&9u64.to_be_bytes());
        assert_eq!(key_accumulator(5, 9), U256::from_be_bytes(expected));

        let mut expected = [0u8; 32];
        expected[0] = 0x09;
        expected[1..9].copy_from_slice(&9u64.to_be_bytes());
        assert_eq!(
            key_ns_u64(ns::VAL_EXECUTION, 9),
            U256::from_be_bytes(expected)
        );

        let mut expected = [0u8; 32];
        expected[0] = 0x06;
        expected[1..21].copy_from_slice(addr.as_slice());
        assert_eq!(
            key_ns_address(ns::VAL_ID_SECP, addr),
            U256::from_be_bytes(expected)
        );

        assert_eq!(ADDRESS_VALSET_EXECUTION.to_be_bytes::<32>()[0], 0x01);
        assert_eq!(ADDRESS_VALSET_SNAPSHOT >> 248, U256::from(3));
        assert_eq!(ADDRESS_EPOCH, U256::from(1));
    }

    #[test]
    fn values_are_left_aligned_in_their_slot() {
        let slot = u64_slot(0x1122);
        assert_eq!(slot.to_be_bytes::<32>()[..8], 0x1122u64.to_be_bytes());
        assert_eq!(slot_u64(slot), 0x1122);

        let af = AddressFlags {
            auth_address: address!("2222222222222222222222222222222222222222"),
            flags: 3,
        };
        let bytes = af.encode().to_be_bytes::<32>();
        assert_eq!(&bytes[..20], af.auth_address.as_slice());
        assert_eq!(bytes[27], 3);
        assert_eq!(AddressFlags::decode(af.encode()), af);

        let e = Epochs {
            delta_epoch: 1,
            next_delta_epoch: 2,
        };
        let bytes = e.encode().to_be_bytes::<32>();
        assert_eq!(bytes[7], 1);
        assert_eq!(bytes[15], 2);
        assert_eq!(Epochs::decode(e.encode()), e);
    }

    #[test]
    fn multi_slot_structs_round_trip() {
        let node = ListNode {
            inext: 1,
            iprev: 2,
            anext: address!("3333333333333333333333333333333333333333"),
            aprev: address!("4444444444444444444444444444444444444444"),
        };
        let bytes = node.encode();
        assert_eq!(&bytes[16..36], node.anext.as_slice());
        assert_eq!(&bytes[36..56], node.aprev.as_slice());
        assert_eq!(ListNode::decode(bytes), node);

        let keys = KeysPacked {
            secp_pubkey: [0xaa; 33],
            bls_pubkey: [0xbb; 48],
        };
        let bytes = keys.encode();
        assert_eq!(bytes[32], 0xaa);
        assert_eq!(bytes[33], 0xbb);
        assert_eq!(bytes[80], 0xbb);
        assert_eq!(bytes[81], 0);
        assert_eq!(KeysPacked::decode(bytes), keys);

        let req = WithdrawalRequest {
            amount: U256::from(5),
            acc: U256::from(6),
            epoch: 7,
        };
        assert_eq!(WithdrawalRequest::decode(req.encode()), req);
        assert_eq!(req.encode()[2].to_be_bytes::<32>()[7], 7);
    }
}
