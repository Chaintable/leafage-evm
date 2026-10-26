//! Metered storage access and the CAS20 storage layout.
//!
//! Port of go-bsc `core/vm/cas20_storage.go` (and `chargeStorageWrite` from `cas20_gas.go`).
//! Only the metered form exists here: everything leafage runs is reached from a precompile
//! frame. Unlike Base's B20, deriving a mapping slot is a metered keccak.

use alloy::primitives::{keccak256, Address, B256, U256};

use super::frame::{gas, keccak, Frame};

/// `bsc.cas20`
pub(crate) const CORE_NAMESPACE: &str = "bsc.cas20";

pub(crate) const SLOT_NAME: u64 = 0;
pub(crate) const SLOT_SYMBOL: u64 = 1;
pub(crate) const SLOT_CONTRACT_URI: u64 = 2;
pub(crate) const SLOT_TOTAL_SUPPLY: u64 = 3;
pub(crate) const SLOT_BALANCES: u64 = 4;
pub(crate) const SLOT_ALLOWANCES: u64 = 5;
pub(crate) const SLOT_ROLES: u64 = 6;
pub(crate) const SLOT_ROLE_ADMINS: u64 = 7;
pub(crate) const SLOT_ADMIN_COUNT: u64 = 8;
pub(crate) const SLOT_TRANSFER_POLICIES: u64 = 9;
pub(crate) const SLOT_MINT_POLICY: u64 = 10;
pub(crate) const SLOT_PAUSED: u64 = 11;
pub(crate) const SLOT_SUPPLY_CAP: u64 = 12;
pub(crate) const SLOT_NONCES: u64 = 13;
pub(crate) const SLOT_SEIZE_POLICIES: u64 = 14;

pub(crate) const OFF_TRANSFER_SENDER: u32 = 0;
pub(crate) const OFF_TRANSFER_RECEIVER: u32 = 8;
pub(crate) const OFF_TRANSFER_EXECUTOR: u32 = 16;
pub(crate) const OFF_MINT_RECEIVER: u32 = 0;
pub(crate) const OFF_SEIZE_EXEMPT: u32 = 0;
pub(crate) const OFF_SEIZE_RECEIVER: u32 = 8;

/// `cas20MaxStringLen`.
const MAX_STRING_LEN: u64 = 1 << 24;

/// `erc7201Root`: `keccak256(keccak256(namespace) - 1) & ~0xff`.
pub(crate) fn erc7201_root(namespace: &str) -> B256 {
    let inner = U256::from_be_bytes(keccak256(namespace.as_bytes()).0) - U256::from(1);
    let mut root = keccak256(inner.to_be_bytes::<32>());
    root[31] = 0;
    root
}

pub(crate) fn offset_slot(root: B256, offset: u64) -> B256 {
    B256::from(U256::from_be_bytes(root.0).wrapping_add(U256::from(offset)))
}

/// `slotAt`: a field of the core layout.
pub(crate) fn slot_at(offset: u64) -> B256 {
    static ROOT: std::sync::OnceLock<B256> = std::sync::OnceLock::new();
    offset_slot(*ROOT.get_or_init(|| erc7201_root(CORE_NAMESPACE)), offset)
}

/// `mappingSlot`: Solidity's `keccak256(key ++ base)`.
pub(crate) fn mapping_slot(base: B256, key: B256) -> B256 {
    keccak(&[key.as_slice(), base.as_slice()])
}

/// `addrKey`.
pub(crate) fn addr_key(a: Address) -> B256 {
    a.into_word()
}

pub(crate) fn u256(w: B256) -> U256 {
    U256::from_be_bytes(w.0)
}

/// Low 64 bits, as go `uint256.Int.Uint64`.
pub(crate) fn low_u64(v: U256) -> u64 {
    v.as_limbs()[0]
}

/// `packedLane`.
pub(crate) fn packed_lane(word: U256, byte_off: u32) -> u64 {
    low_u64(word >> (byte_off as usize * 8))
}

/// go-bsc `cas20Storage` bound to an account. Every access is charged to the frame.
#[derive(Clone, Copy)]
pub(crate) struct Store {
    pub address: Address,
}

impl Store {
    pub fn new(address: Address) -> Self {
        Self { address }
    }

    /// `mapSlot`: a refused charge yields the zero slot, harmless since the frame is out of
    /// gas by then and every access through it is refused too.
    pub fn map_slot(&self, f: &mut Frame<'_>, base: B256, key: B256) -> B256 {
        if !f.charge_keccak(64) {
            return B256::ZERO;
        }
        mapping_slot(base, key)
    }

    /// `strMapSlot`: Solidity hashes a string key's raw bytes ++ base.
    pub fn str_map_slot(&self, f: &mut Frame<'_>, base: B256, key: &[u8]) -> B256 {
        if !f.charge_keccak(key.len() + 32) {
            return B256::ZERO;
        }
        keccak(&[key, base.as_slice()])
    }

    fn charge_read(&self, f: &mut Frame<'_>, slot: B256) -> bool {
        if f.host.access_slot(self.address, slot) {
            return f.charge_gas(gas::WARM_STORAGE_READ);
        }
        f.charge_gas(gas::COLD_SLOAD)
    }

    /// `getWordChecked`: for reads whose value decides a branch.
    pub fn get_word_checked(&self, f: &mut Frame<'_>, slot: B256) -> Option<B256> {
        if !self.charge_read(f, slot) {
            return None;
        }
        Some(f.host.state(self.address, slot))
    }

    pub fn get_word(&self, f: &mut Frame<'_>, slot: B256) -> B256 {
        self.get_word_checked(f, slot).unwrap_or_default()
    }

    /// `chargeStorageWrite`: mirrors go-bsc `makeGasSStoreFunc`, arms in the same order.
    fn charge_storage_write(&self, f: &mut Frame<'_>, slot: B256, value: B256) -> bool {
        if f.read_only {
            f.mark_write_protected();
            return false;
        }
        if !f.sstore_sentry() {
            f.mark_out_of_gas();
            return false;
        }
        // go-bsc reads the values first and then consults the access list; the order is
        // swapped because revm's journal warms a slot on its first read. The values do not
        // depend on warmth, so the charge is the same.
        let cost = if f.host.access_slot(self.address, slot) {
            0
        } else {
            gas::COLD_SLOAD
        };
        let (current, original) = f.host.state_and_committed(self.address, slot);
        let clearing_refund = gas::SSTORE_CLEARS_SCHEDULE_REFUND;
        if current == value {
            // noop (1)
            return f.charge_gas(cost + gas::WARM_STORAGE_READ);
        }
        if original == current {
            if original.is_zero() {
                // create slot (2.1.1)
                return f.charge_gas(cost + gas::SSTORE_SET);
            }
            if value.is_zero() {
                // delete slot (2.1.2b)
                f.add_refund(clearing_refund);
            }
            // write existing slot (2.1.2)
            return f.charge_gas(cost + (gas::SSTORE_RESET - gas::COLD_SLOAD));
        }
        // dirty slot (2.2)
        if !original.is_zero() {
            if current.is_zero() {
                // recreate slot (2.2.1.1)
                f.sub_refund(clearing_refund);
            } else if value.is_zero() {
                // delete slot (2.2.1.2)
                f.add_refund(clearing_refund);
            }
        }
        if original == value {
            if original.is_zero() {
                // reset to original inexistent slot (2.2.2.1)
                f.add_refund(gas::SSTORE_SET - gas::WARM_STORAGE_READ);
            } else {
                // reset to original existing slot (2.2.2.2)
                f.add_refund((gas::SSTORE_RESET - gas::COLD_SLOAD) - gas::WARM_STORAGE_READ);
            }
        }
        // dirty update (2.2)
        f.charge_gas(cost + gas::WARM_STORAGE_READ)
    }

    pub fn set_word(&self, f: &mut Frame<'_>, slot: B256, value: B256) -> bool {
        if !self.charge_storage_write(f, slot, value) {
            return false;
        }
        f.host.set_state(self.address, slot, value);
        true
    }

    pub fn get_u256(&self, f: &mut Frame<'_>, offset: u64) -> U256 {
        u256(self.get_word(f, slot_at(offset)))
    }

    pub fn set_u256(&self, f: &mut Frame<'_>, offset: u64, v: U256) {
        self.set_word(f, slot_at(offset), B256::from(v));
    }

    pub fn get_u256_at(&self, f: &mut Frame<'_>, slot: B256) -> U256 {
        u256(self.get_word(f, slot))
    }

    pub fn set_u256_at(&self, f: &mut Frame<'_>, slot: B256, v: U256) {
        self.set_word(f, slot, B256::from(v));
    }

    pub fn total_supply(&self, f: &mut Frame<'_>) -> U256 {
        self.get_u256(f, SLOT_TOTAL_SUPPLY)
    }
    pub fn set_total_supply(&self, f: &mut Frame<'_>, v: U256) {
        self.set_u256(f, SLOT_TOTAL_SUPPLY, v)
    }
    pub fn supply_cap(&self, f: &mut Frame<'_>) -> U256 {
        self.get_u256(f, SLOT_SUPPLY_CAP)
    }
    pub fn set_supply_cap(&self, f: &mut Frame<'_>, v: U256) {
        self.set_u256(f, SLOT_SUPPLY_CAP, v)
    }
    pub fn admin_count(&self, f: &mut Frame<'_>) -> U256 {
        self.get_u256(f, SLOT_ADMIN_COUNT)
    }
    pub fn set_admin_count(&self, f: &mut Frame<'_>, v: U256) {
        self.set_u256(f, SLOT_ADMIN_COUNT, v)
    }

    pub fn paused_checked(&self, f: &mut Frame<'_>) -> Option<U256> {
        self.get_word_checked(f, slot_at(SLOT_PAUSED)).map(u256)
    }
    pub fn paused(&self, f: &mut Frame<'_>) -> U256 {
        self.get_u256(f, SLOT_PAUSED)
    }
    pub fn set_paused(&self, f: &mut Frame<'_>, v: U256) {
        self.set_u256(f, SLOT_PAUSED, v)
    }

    // --- balances / allowances / nonces -------------------------------------------------

    pub fn balance_slot(&self, f: &mut Frame<'_>, a: Address) -> B256 {
        self.map_slot(f, slot_at(SLOT_BALANCES), addr_key(a))
    }

    pub fn allowance_slot(&self, f: &mut Frame<'_>, owner: Address, spender: Address) -> B256 {
        let inner = self.map_slot(f, slot_at(SLOT_ALLOWANCES), addr_key(owner));
        self.map_slot(f, inner, addr_key(spender))
    }

    pub fn balance_of(&self, f: &mut Frame<'_>, a: Address) -> U256 {
        let slot = self.balance_slot(f, a);
        self.get_u256_at(f, slot)
    }

    pub fn allowance(&self, f: &mut Frame<'_>, owner: Address, spender: Address) -> U256 {
        let slot = self.allowance_slot(f, owner, spender);
        self.get_u256_at(f, slot)
    }

    pub fn set_allowance(&self, f: &mut Frame<'_>, owner: Address, spender: Address, v: U256) {
        let slot = self.allowance_slot(f, owner, spender);
        self.set_u256_at(f, slot, v)
    }

    pub fn nonce(&self, f: &mut Frame<'_>, owner: Address) -> U256 {
        let slot = self.map_slot(f, slot_at(SLOT_NONCES), addr_key(owner));
        u256(self.get_word(f, slot))
    }

    pub fn set_nonce(&self, f: &mut Frame<'_>, owner: Address, v: U256) {
        let slot = self.map_slot(f, slot_at(SLOT_NONCES), addr_key(owner));
        self.set_word(f, slot, B256::from(v));
    }

    // --- roles --------------------------------------------------------------------------

    pub fn has_role(&self, f: &mut Frame<'_>, role: B256, a: Address) -> bool {
        let inner = self.map_slot(f, slot_at(SLOT_ROLES), role);
        let slot = self.map_slot(f, inner, addr_key(a));
        !self.get_word(f, slot).is_zero()
    }

    pub fn set_role(&self, f: &mut Frame<'_>, role: B256, a: Address, enabled: bool) {
        let inner = self.map_slot(f, slot_at(SLOT_ROLES), role);
        let slot = self.map_slot(f, inner, addr_key(a));
        let mut v = B256::ZERO;
        if enabled {
            v[31] = 1;
        }
        self.set_word(f, slot, v);
    }

    pub fn role_admin(&self, f: &mut Frame<'_>, role: B256) -> B256 {
        let slot = self.map_slot(f, slot_at(SLOT_ROLE_ADMINS), role);
        self.get_word(f, slot)
    }

    pub fn set_role_admin(&self, f: &mut Frame<'_>, role: B256, admin: B256) {
        let slot = self.map_slot(f, slot_at(SLOT_ROLE_ADMINS), role);
        self.set_word(f, slot, admin);
    }

    // --- packed policy ids --------------------------------------------------------------

    pub fn get_packed_u64(&self, f: &mut Frame<'_>, offset: u64, byte_off: u32) -> u64 {
        packed_lane(u256(self.get_word(f, slot_at(offset))), byte_off)
    }

    pub fn set_packed_u64(&self, f: &mut Frame<'_>, offset: u64, byte_off: u32, v: u64) {
        let slot = slot_at(offset);
        let shift = byte_off as usize * 8;
        let mut word = u256(self.get_word(f, slot));
        let lane = U256::from(u64::MAX) << shift;
        word &= !lane;
        word |= U256::from(v) << shift;
        self.set_word(f, slot, B256::from(word));
    }

    pub fn transfer_policies(&self, f: &mut Frame<'_>) -> (u64, u64, u64) {
        let w = self.get_u256_at(f, slot_at(SLOT_TRANSFER_POLICIES));
        (
            packed_lane(w, OFF_TRANSFER_SENDER),
            packed_lane(w, OFF_TRANSFER_RECEIVER),
            packed_lane(w, OFF_TRANSFER_EXECUTOR),
        )
    }

    pub fn seize_policies(&self, f: &mut Frame<'_>) -> (u64, u64) {
        let w = self.get_u256_at(f, slot_at(SLOT_SEIZE_POLICIES));
        (
            packed_lane(w, OFF_SEIZE_EXEMPT),
            packed_lane(w, OFF_SEIZE_RECEIVER),
        )
    }

    pub fn mint_receiver_policy(&self, f: &mut Frame<'_>) -> u64 {
        self.get_packed_u64(f, SLOT_MINT_POLICY, OFF_MINT_RECEIVER)
    }

    // --- strings (Solidity storage encoding) --------------------------------------------

    pub fn get_string(&self, f: &mut Frame<'_>, offset: u64) -> Option<Vec<u8>> {
        self.get_string_at(f, slot_at(offset))
    }

    pub fn set_string(&self, f: &mut Frame<'_>, offset: u64, s: &[u8]) -> bool {
        self.set_string_at(f, slot_at(offset), s)
    }

    /// `stringDataRoot`: `keccak256(slot)`, also the base of a dynamic array.
    pub fn string_data_root(&self, f: &mut Frame<'_>, slot: B256) -> U256 {
        if !f.charge_keccak(32) {
            return U256::ZERO;
        }
        U256::from_be_bytes(keccak256(slot).0)
    }

    /// `getStringAt`. `None` means a paid read was refused.
    pub fn get_string_at(&self, f: &mut Frame<'_>, slot: B256) -> Option<Vec<u8>> {
        let word = self.get_word_checked(f, slot)?;
        if word[31] & 1 == 0 {
            let n = (word[31] / 2) as usize;
            if n > 31 {
                return Some(Vec::new());
            }
            return Some(word[..n].to_vec());
        }
        let encoded = u256(word);
        if encoded > U256::from(2 * MAX_STRING_LEN + 1) {
            return Some(Vec::new());
        }
        let length = (low_u64(encoded) - 1) / 2;
        if length < 32 {
            return Some(Vec::new());
        }
        let base = self.string_data_root(f, slot);
        // Grown only behind paid reads, so the allocation is bounded by the gas spent.
        let mut out = Vec::new();
        let mut i = 0u64;
        while i < length {
            let chunk_slot = B256::from(base.wrapping_add(U256::from(i / 32)));
            let chunk = self.get_word_checked(f, chunk_slot)?;
            out.extend_from_slice(chunk.as_slice());
            i += 32;
        }
        out.truncate(length as usize);
        Some(out)
    }

    pub fn set_string_at(&self, f: &mut Frame<'_>, slot: B256, s: &[u8]) -> bool {
        let old_chunks = self.string_chunks(f, slot);
        let new_chunks = if s.len() >= 32 {
            s.len().div_ceil(32) as u64
        } else {
            0
        };

        if s.len() < 32 {
            let mut word = B256::ZERO;
            word[..s.len()].copy_from_slice(s);
            word[31] = (s.len() * 2) as u8;
            if !self.set_word(f, slot, word) {
                return false;
            }
        } else if !self.set_word(f, slot, B256::from(U256::from(s.len() * 2 + 1))) {
            return false;
        }
        if new_chunks == 0 && old_chunks == 0 {
            return true;
        }
        let base = self.string_data_root(f, slot);
        for i in 0..new_chunks {
            if f.out_of_gas() {
                return false;
            }
            let mut chunk = B256::ZERO;
            let start = (i * 32) as usize;
            let end = (start + 32).min(s.len());
            chunk[..end - start].copy_from_slice(&s[start..end]);
            self.set_word(f, B256::from(base.wrapping_add(U256::from(i))), chunk);
        }
        // old_chunks comes from state, not calldata: without this guard a starved frame
        // would do work proportional to the old length and never pay for it.
        for i in new_chunks..old_chunks {
            if f.out_of_gas() {
                return false;
            }
            self.set_word(f, B256::from(base.wrapping_add(U256::from(i))), B256::ZERO);
        }
        true
    }

    fn string_chunks(&self, f: &mut Frame<'_>, slot: B256) -> u64 {
        let word = self.get_word(f, slot);
        if word[31] & 1 == 0 {
            return 0;
        }
        let encoded = u256(word);
        if encoded > U256::from(2 * MAX_STRING_LEN + 1) {
            return 0;
        }
        let length = (low_u64(encoded) - 1) / 2;
        if length < 32 {
            return 0;
        }
        length.div_ceil(32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn erc7201_root_matches_the_formula() {
        // keccak256(abi.encode(uint256(keccak256("bsc.cas20")) - 1)) & ~bytes32(uint256(0xff))
        let root = erc7201_root(CORE_NAMESPACE);
        assert_eq!(root[31], 0);
        let inner = U256::from_be_bytes(keccak256(b"bsc.cas20").0) - U256::from(1);
        let mut expected = keccak256(inner.to_be_bytes::<32>());
        expected[31] = 0;
        assert_eq!(root, expected);
        assert_ne!(
            root,
            b256!("0000000000000000000000000000000000000000000000000000000000000000")
        );
    }
}
