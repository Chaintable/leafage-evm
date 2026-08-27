//! B20 storage layout: ERC-7201 namespaced slot derivation and typed field access.
//!
//! Transcribed from Base reth's `#[contract]`/`#[derive(Storable)]` generated code
//! (`base/crates/common/precompiles/src/common/core_storage.rs`,
//! `b20_asset/storage.rs`, `b20_stablecoin/storage.rs`) plus the packing rules in
//! `base/crates/common/precompile-storage/src/packing.rs`.
//!
//! Every read/write goes through [`B20Port`], so each one is metered exactly as the
//! equivalent `SLOAD`/`SSTORE` would be. Slot arithmetic itself is free — Base does not
//! charge keccak gas for mapping derivation.

use alloy::primitives::{keccak256, Address, B256, U256};

use super::error::{B20Error, Result};
use super::port::B20Port;

// --- ERC-7201 namespace roots (verified against Base reth) ---

/// `base.b20` core storage root.
pub const ROOT_B20: U256 = U256::from_limbs([
    0xbb5f01ed48434000,
    0x4c938c3196430e10,
    0x4aff64ea9b247419,
    0xc78b71fee795ddd7,
]);
/// `base.b20.asset` extension storage root.
pub const ROOT_ASSET: U256 = U256::from_limbs([
    0x6fd277585e374b00,
    0x2ec9b89a90e104f2,
    0xe4d9facdbf0fb50d,
    0xfdc6d4552d1286ad,
]);
/// `base.b20.stablecoin` extension storage root.
pub const ROOT_STABLECOIN: U256 = U256::from_limbs([
    0xf09e73d0943d6200,
    0x45d0ca58e30b7693,
    0x367ea3129b19441d,
    0x35827975a06ca0e9,
]);

// --- `base.b20` core field offsets ---

const OFF_NAME: u64 = 0;
const OFF_SYMBOL: u64 = 1;
const OFF_CONTRACT_URI: u64 = 2;
const OFF_TOTAL_SUPPLY: u64 = 3;
const OFF_BALANCES: u64 = 4;
const OFF_ALLOWANCES: u64 = 5;
const OFF_ROLES: u64 = 6;
const OFF_ROLE_ADMINS: u64 = 7;
const OFF_ADMIN_COUNT: u64 = 8;
/// Slot 9 packs the three transfer policy IDs at byte offsets 0 / 8 / 16.
const OFF_TRANSFER_POLICIES: u64 = 9;
/// Slot 10 holds the mint-receiver policy ID at byte offset 0.
const OFF_MINT_POLICY: u64 = 10;
const OFF_PAUSED: u64 = 11;
const OFF_SUPPLY_CAP: u64 = 12;
const OFF_NONCES: u64 = 13;

// --- `base.b20.asset` field offsets ---

const OFF_ASSET_DECIMALS: u64 = 0;
const OFF_ASSET_MULTIPLIER: u64 = 1;
const OFF_ASSET_USED_ANNOUNCEMENT_IDS: u64 = 2;
const OFF_ASSET_EXTRA_METADATA: u64 = 3;

// --- `base.b20.stablecoin` field offsets ---

const OFF_STABLECOIN_CURRENCY: u64 = 0;

/// WAD fixed-point precision (1e18) for the asset multiplier.
pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// Minimum (and default) decimals for an asset token.
pub const ASSET_MIN_DECIMALS: u8 = 6;
/// Fixed decimals for a stablecoin token.
pub const STABLECOIN_DECIMALS: u8 = 6;

/// Byte offsets of the three packed transfer policy IDs within slot 9.
const POLICY_SENDER_BYTES: usize = 0;
const POLICY_RECEIVER_BYTES: usize = 8;
const POLICY_EXECUTOR_BYTES: usize = 16;
const POLICY_MINT_BYTES: usize = 0;

/// Which packed policy field to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySlot {
    /// Transfer sender policy, slot 9 byte 0.
    TransferSender,
    /// Transfer receiver policy, slot 9 byte 8.
    TransferReceiver,
    /// Transfer executor policy, slot 9 byte 16.
    TransferExecutor,
    /// Mint receiver policy, slot 10 byte 0.
    MintReceiver,
}

impl PolicySlot {
    const fn location(self) -> (u64, usize) {
        match self {
            Self::TransferSender => (OFF_TRANSFER_POLICIES, POLICY_SENDER_BYTES),
            Self::TransferReceiver => (OFF_TRANSFER_POLICIES, POLICY_RECEIVER_BYTES),
            Self::TransferExecutor => (OFF_TRANSFER_POLICIES, POLICY_EXECUTOR_BYTES),
            Self::MintReceiver => (OFF_MINT_POLICY, POLICY_MINT_BYTES),
        }
    }
}

// --- Slot derivation ---

/// `root + offset` with 256-bit wrapping, matching Base's sequential namespace layout.
#[inline]
pub fn field_slot(root: U256, offset: u64) -> U256 {
    root.wrapping_add(U256::from(offset))
}

/// Solidity value-key mapping slot: `keccak256(pad32(key) ++ pad32(slot))`.
#[inline]
pub fn mapping_slot(slot: U256, key_word: B256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key_word.as_slice());
    buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Solidity string/bytes-key mapping slot: `keccak256(bytes(key) ++ pad32(slot))`.
///
/// Unlike value keys the string bytes are *not* padded — see Base's
/// `precompile-storage/src/types/bytes_like.rs`.
#[inline]
pub fn string_mapping_slot(slot: U256, key: &str) -> U256 {
    let mut buf = Vec::with_capacity(key.len() + 32);
    buf.extend_from_slice(key.as_bytes());
    buf.extend_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Packs a `u64` into a slot word at `offset_bytes` from the low-order end.
#[inline]
fn insert_u64(current: U256, value: u64, offset_bytes: usize) -> U256 {
    let shift = offset_bytes * 8;
    let mask = U256::from(u64::MAX) << shift;
    (current & !mask) | ((U256::from(value) << shift) & mask)
}

/// Extracts a `u64` from a slot word at `offset_bytes` from the low-order end.
#[inline]
fn extract_u64(word: U256, offset_bytes: usize) -> u64 {
    let shift = offset_bytes * 8;
    ((word >> shift) & U256::from(u64::MAX)).to::<u64>()
}

// --- Typed store ---

/// Typed access to one B20 token's storage through a metered port.
pub struct B20Store<'a, P: B20Port> {
    port: &'a mut P,
    address: Address,
    is_asset: bool,
}

impl<'a, P: B20Port> B20Store<'a, P> {
    /// Binds a store to `address`, dispatching variant-specific fields via `is_asset`.
    pub fn new(port: &'a mut P, address: Address, is_asset: bool) -> Self {
        Self { port, address, is_asset }
    }

    /// The token address backing this store.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Whether this token is the asset variant.
    pub fn is_asset(&self) -> bool {
        self.is_asset
    }

    /// Borrows the underlying port (for guards that need raw access).
    pub fn port(&mut self) -> &mut P {
        self.port
    }

    // --- raw slot access ---

    fn load(&mut self, slot: U256) -> Result<U256> {
        self.port.sload(self.address, slot)
    }

    fn store(&mut self, slot: U256, value: U256) -> Result<()> {
        self.port.sstore(self.address, slot, value)
    }

    // --- initialization marker ---

    /// Whether the token has been created: Base deploys marker bytecode at the token
    /// address on creation, so an account with no code is an uncreated token.
    pub fn is_initialized(&mut self) -> Result<bool> {
        let address = self.address;
        self.port.has_code(address)
    }

    // --- balances / supply ---

    /// Raw stored balance of `account`.
    pub fn balance_of(&mut self, account: Address) -> Result<U256> {
        let slot = mapping_slot(field_slot(ROOT_B20, OFF_BALANCES), account.into_word());
        self.load(slot)
    }

    /// Overwrites the stored balance of `account`.
    pub fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
        let slot = mapping_slot(field_slot(ROOT_B20, OFF_BALANCES), account.into_word());
        self.store(slot, balance)
    }

    /// Total supply in circulation.
    pub fn total_supply(&mut self) -> Result<U256> {
        self.load(field_slot(ROOT_B20, OFF_TOTAL_SUPPLY))
    }

    /// Overwrites the total supply.
    pub fn set_total_supply(&mut self, supply: U256) -> Result<()> {
        self.store(field_slot(ROOT_B20, OFF_TOTAL_SUPPLY), supply)
    }

    /// Maximum total supply enforced on mint.
    pub fn supply_cap(&mut self) -> Result<U256> {
        self.load(field_slot(ROOT_B20, OFF_SUPPLY_CAP))
    }

    /// Overwrites the supply cap.
    pub fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
        self.store(field_slot(ROOT_B20, OFF_SUPPLY_CAP), cap)
    }

    // --- allowances ---

    /// Allowance granted by `owner` to `spender`.
    pub fn allowance(&mut self, owner: Address, spender: Address) -> Result<U256> {
        let inner = mapping_slot(field_slot(ROOT_B20, OFF_ALLOWANCES), owner.into_word());
        self.load(mapping_slot(inner, spender.into_word()))
    }

    /// Overwrites the allowance granted by `owner` to `spender`.
    pub fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        let inner = mapping_slot(field_slot(ROOT_B20, OFF_ALLOWANCES), owner.into_word());
        self.store(mapping_slot(inner, spender.into_word()), amount)
    }

    // --- roles ---

    /// Whether `account` holds `role`.
    pub fn has_role(&mut self, role: B256, account: Address) -> Result<bool> {
        let inner = mapping_slot(field_slot(ROOT_B20, OFF_ROLES), role);
        Ok(!self.load(mapping_slot(inner, account.into_word()))?.is_zero())
    }

    /// Sets whether `account` holds `role`.
    pub fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()> {
        let inner = mapping_slot(field_slot(ROOT_B20, OFF_ROLES), role);
        let value = if enabled { U256::ONE } else { U256::ZERO };
        self.store(mapping_slot(inner, account.into_word()), value)
    }

    /// Admin role configured for `role`.
    pub fn role_admin(&mut self, role: B256) -> Result<B256> {
        let slot = mapping_slot(field_slot(ROOT_B20, OFF_ROLE_ADMINS), role);
        Ok(B256::from(self.load(slot)?.to_be_bytes::<32>()))
    }

    /// Overwrites the admin role for `role`.
    pub fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()> {
        let slot = mapping_slot(field_slot(ROOT_B20, OFF_ROLE_ADMINS), role);
        self.store(slot, U256::from_be_bytes(admin_role.0))
    }

    /// Number of accounts holding the default-admin role.
    pub fn admin_count(&mut self) -> Result<U256> {
        self.load(field_slot(ROOT_B20, OFF_ADMIN_COUNT))
    }

    /// Overwrites the default-admin holder count.
    pub fn set_admin_count(&mut self, count: U256) -> Result<()> {
        self.store(field_slot(ROOT_B20, OFF_ADMIN_COUNT), count)
    }

    // --- pause ---

    /// Paused-feature bitmask.
    pub fn paused(&mut self) -> Result<U256> {
        self.load(field_slot(ROOT_B20, OFF_PAUSED))
    }

    /// Overwrites the paused-feature bitmask.
    pub fn set_paused(&mut self, vectors: U256) -> Result<()> {
        self.store(field_slot(ROOT_B20, OFF_PAUSED), vectors)
    }

    // --- policies ---

    /// Policy ID configured for `slot_kind`.
    pub fn policy_id(&mut self, slot_kind: PolicySlot) -> Result<u64> {
        let (offset, byte_offset) = slot_kind.location();
        let word = self.load(field_slot(ROOT_B20, offset))?;
        Ok(extract_u64(word, byte_offset))
    }

    /// Overwrites the policy ID configured for `slot_kind`, preserving the rest of the slot.
    pub fn set_policy_id(&mut self, slot_kind: PolicySlot, policy_id: u64) -> Result<()> {
        let (offset, byte_offset) = slot_kind.location();
        let slot = field_slot(ROOT_B20, offset);
        let current = self.load(slot)?;
        self.store(slot, insert_u64(current, policy_id, byte_offset))
    }

    // --- permit nonces ---

    /// EIP-2612 permit nonce for `owner`.
    pub fn nonce(&mut self, owner: Address) -> Result<U256> {
        let slot = mapping_slot(field_slot(ROOT_B20, OFF_NONCES), owner.into_word());
        self.load(slot)
    }

    /// Overwrites the permit nonce for `owner`.
    pub fn set_nonce(&mut self, owner: Address, nonce: U256) -> Result<()> {
        let slot = mapping_slot(field_slot(ROOT_B20, OFF_NONCES), owner.into_word());
        self.store(slot, nonce)
    }

    // --- metadata strings ---

    /// Token name.
    pub fn name(&mut self) -> Result<String> {
        self.read_string(field_slot(ROOT_B20, OFF_NAME))
    }

    /// Overwrites the token name.
    pub fn set_name(&mut self, value: &str) -> Result<()> {
        self.write_string(field_slot(ROOT_B20, OFF_NAME), value)
    }

    /// Token symbol.
    pub fn symbol(&mut self) -> Result<String> {
        self.read_string(field_slot(ROOT_B20, OFF_SYMBOL))
    }

    /// Overwrites the token symbol.
    pub fn set_symbol(&mut self, value: &str) -> Result<()> {
        self.write_string(field_slot(ROOT_B20, OFF_SYMBOL), value)
    }

    /// ERC-7572 contract metadata URI.
    pub fn contract_uri(&mut self) -> Result<String> {
        self.read_string(field_slot(ROOT_B20, OFF_CONTRACT_URI))
    }

    /// Overwrites the contract metadata URI.
    pub fn set_contract_uri(&mut self, value: &str) -> Result<()> {
        self.write_string(field_slot(ROOT_B20, OFF_CONTRACT_URI), value)
    }

    // --- asset extension ---

    /// Decimals: asset reads its stored slot (defaulting to 6), stablecoin is fixed at 6.
    pub fn decimals(&mut self) -> Result<u8> {
        if !self.is_asset {
            return Ok(STABLECOIN_DECIMALS);
        }
        let word = self.load(field_slot(ROOT_ASSET, OFF_ASSET_DECIMALS))?;
        let raw = word.to_be_bytes::<32>()[31];
        Ok(if raw == 0 { ASSET_MIN_DECIMALS } else { raw })
    }

    /// Asset multiplier at WAD precision.
    ///
    /// An unset slot means WAD (1:1), matching Base's generated accessor
    /// (`precompile-macros/src/accounting.rs:316`) — *not* zero.
    pub fn multiplier(&mut self) -> Result<U256> {
        let raw = self.load(field_slot(ROOT_ASSET, OFF_ASSET_MULTIPLIER))?;
        Ok(if raw.is_zero() { WAD } else { raw })
    }

    /// Overwrites the asset multiplier.
    pub fn set_multiplier(&mut self, multiplier: U256) -> Result<()> {
        self.store(field_slot(ROOT_ASSET, OFF_ASSET_MULTIPLIER), multiplier)
    }

    /// Whether announcement `id` has already been consumed.
    pub fn is_announcement_id_used(&mut self, id: &str) -> Result<bool> {
        let slot =
            string_mapping_slot(field_slot(ROOT_ASSET, OFF_ASSET_USED_ANNOUNCEMENT_IDS), id);
        Ok(!self.load(slot)?.is_zero())
    }

    /// Marks announcement `id` as consumed.
    pub fn mark_announcement_id_used(&mut self, id: &str) -> Result<()> {
        let slot =
            string_mapping_slot(field_slot(ROOT_ASSET, OFF_ASSET_USED_ANNOUNCEMENT_IDS), id);
        self.store(slot, U256::ONE)
    }

    /// Extra-metadata value for `key`, or the empty string when unset.
    pub fn extra_metadata(&mut self, key: &str) -> Result<String> {
        let slot = string_mapping_slot(field_slot(ROOT_ASSET, OFF_ASSET_EXTRA_METADATA), key);
        self.read_string(slot)
    }

    /// Sets (or, with an empty `value`, clears) the extra-metadata entry for `key`.
    pub fn set_extra_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        let slot = string_mapping_slot(field_slot(ROOT_ASSET, OFF_ASSET_EXTRA_METADATA), key);
        self.write_string(slot, value)
    }

    // --- stablecoin extension ---

    /// Stablecoin currency identifier (ISO 4217).
    pub fn currency(&mut self) -> Result<String> {
        self.read_string(field_slot(ROOT_STABLECOIN, OFF_STABLECOIN_CURRENCY))
    }

    // --- Solidity string codec ---

    /// Reads a Solidity `string` at `slot` (short form packed in-slot, long form at
    /// `keccak256(slot)`).
    pub fn read_string(&mut self, slot: U256) -> Result<String> {
        let word = self.load(slot)?;
        let bytes = word.to_be_bytes::<32>();
        let last = bytes[31];
        if last & 1 == 0 {
            let len = (last / 2) as usize;
            return Ok(String::from_utf8_lossy(&bytes[..len]).into_owned());
        }
        let len: usize = ((word - U256::ONE) / U256::from(2u64)).saturating_to();
        let base = U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0);
        let mut out = Vec::with_capacity(len);
        let mut i = 0u64;
        while out.len() < len {
            let chunk = self.load(base.wrapping_add(U256::from(i)))?.to_be_bytes::<32>();
            let take = (len - out.len()).min(32);
            out.extend_from_slice(&chunk[..take]);
            i += 1;
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Writes a Solidity `string` at `slot`, clearing any previous long-form tail.
    pub fn write_string(&mut self, slot: U256, value: &str) -> Result<()> {
        // Length of the value currently stored, so a shrink clears the stale tail words.
        let previous = self.load(slot)?;
        let prev_long = previous.to_be_bytes::<32>()[31] & 1 == 1;
        let prev_len: usize = if prev_long {
            ((previous - U256::ONE) / U256::from(2u64)).saturating_to()
        } else {
            (previous.to_be_bytes::<32>()[31] / 2) as usize
        };

        let bytes = value.as_bytes();
        let data_base = U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0);

        if bytes.len() < 32 {
            let mut word = [0u8; 32];
            word[..bytes.len()].copy_from_slice(bytes);
            word[31] = (bytes.len() * 2) as u8;
            self.store(slot, U256::from_be_bytes(word))?;
        } else {
            self.store(slot, U256::from(bytes.len() * 2 + 1))?;
            for (i, chunk) in bytes.chunks(32).enumerate() {
                let mut word = [0u8; 32];
                word[..chunk.len()].copy_from_slice(chunk);
                self.store(
                    data_base.wrapping_add(U256::from(i as u64)),
                    U256::from_be_bytes(word),
                )?;
            }
        }

        // Zero any tail words the previous longer value occupied.
        if prev_long {
            let prev_words = prev_len.div_ceil(32);
            let new_words = if bytes.len() < 32 { 0 } else { bytes.len().div_ceil(32) };
            for i in new_words..prev_words {
                self.store(data_base.wrapping_add(U256::from(i as u64)), U256::ZERO)?;
            }
        }
        Ok(())
    }
}

/// Checked subtraction that reverts with Solidity's arithmetic panic on underflow.
#[inline]
pub fn checked_sub(a: U256, b: U256) -> Result<U256> {
    a.checked_sub(b).ok_or_else(B20Error::under_overflow)
}

/// Checked addition that reverts with Solidity's arithmetic panic on overflow.
#[inline]
pub fn checked_add(a: U256, b: U256) -> Result<U256> {
    a.checked_add(b).ok_or_else(B20Error::under_overflow)
}

/// Checked multiplication that reverts with Solidity's arithmetic panic on overflow.
#[inline]
pub fn checked_mul(a: U256, b: U256) -> Result<U256> {
    a.checked_mul(b).ok_or_else(B20Error::under_overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erc7201_roots_match_base() {
        assert_eq!(
            format!("{ROOT_B20:#x}"),
            "0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000"
        );
        assert_eq!(
            format!("{ROOT_ASSET:#x}"),
            "0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00"
        );
        assert_eq!(
            format!("{ROOT_STABLECOIN:#x}"),
            "0x35827975a06ca0e9367ea3129b19441d45d0ca58e30b7693f09e73d0943d6200"
        );
    }

    #[test]
    fn wad_is_1e18() {
        assert_eq!(WAD, U256::from(10u64).pow(U256::from(18u64)));
    }

    #[test]
    fn mapping_slot_matches_solidity() {
        let addr = Address::repeat_byte(0x11);
        let base_slot = field_slot(ROOT_B20, OFF_BALANCES);
        let got = mapping_slot(base_slot, addr.into_word());
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(addr.as_slice());
        buf[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
        assert_eq!(got, U256::from_be_bytes(keccak256(buf).0));
    }

    /// String keys are hashed unpadded, unlike value keys.
    #[test]
    fn string_mapping_slot_does_not_pad_the_key() {
        let base_slot = field_slot(ROOT_ASSET, OFF_ASSET_EXTRA_METADATA);
        let got = string_mapping_slot(base_slot, "category");
        let mut buf = Vec::new();
        buf.extend_from_slice(b"category");
        buf.extend_from_slice(&base_slot.to_be_bytes::<32>());
        assert_eq!(got, U256::from_be_bytes(keccak256(buf).0));
    }

    /// Solidity packs from the low-order end: the three transfer policy IDs share slot 9
    /// at byte offsets 0/8/16 and must not disturb one another.
    #[test]
    fn packed_u64_roundtrips_at_each_offset() {
        let mut word = U256::ZERO;
        word = insert_u64(word, 0x1111_1111_1111_1111, POLICY_SENDER_BYTES);
        word = insert_u64(word, 0x2222_2222_2222_2222, POLICY_RECEIVER_BYTES);
        word = insert_u64(word, 0x3333_3333_3333_3333, POLICY_EXECUTOR_BYTES);

        assert_eq!(extract_u64(word, POLICY_SENDER_BYTES), 0x1111_1111_1111_1111);
        assert_eq!(extract_u64(word, POLICY_RECEIVER_BYTES), 0x2222_2222_2222_2222);
        assert_eq!(extract_u64(word, POLICY_EXECUTOR_BYTES), 0x3333_3333_3333_3333);

        // Overwriting one field leaves its neighbours intact.
        let word = insert_u64(word, 0, POLICY_RECEIVER_BYTES);
        assert_eq!(extract_u64(word, POLICY_SENDER_BYTES), 0x1111_1111_1111_1111);
        assert_eq!(extract_u64(word, POLICY_RECEIVER_BYTES), 0);
        assert_eq!(extract_u64(word, POLICY_EXECUTOR_BYTES), 0x3333_3333_3333_3333);
    }

    #[test]
    fn packed_u64_low_order_offset_matches_shift() {
        let word = insert_u64(U256::ZERO, 0xabcd, POLICY_RECEIVER_BYTES);
        assert_eq!(word, U256::from(0xabcdu64) << (POLICY_RECEIVER_BYTES * 8));
    }
}
