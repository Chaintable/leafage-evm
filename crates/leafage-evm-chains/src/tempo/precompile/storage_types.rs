//! Storable type system for EVM storage (leafage-evm adaptation).
//!
//! Ported from Tempo's `storage/types/` and `storage/packing.rs`. These are mostly
//! pure computation (keccak256 slot calculation, bit packing) with minimal revm dependency.
//!
//! Provides:
//! - [`Layout`], [`LayoutCtx`] -- storage layout descriptors
//! - [`StorableType`], [`Storable`], [`FromWord`], [`Packable`] -- core traits
//! - [`StorageKey`] -- mapping key trait (keccak256-based slot computation)
//! - [`Slot`] -- type-safe single storage slot accessor
//! - [`Mapping`] -- type-safe storage mapping accessor
//! - Packing helpers: [`FieldLocation`], [`PackedSlot`], extract/insert/delete operations
//! - Primitive implementations for `bool`, `Address`, `u8`..`u128`, `U256`

use alloy::primitives::{keccak256, Address, Bytes, FixedBytes, U256};
use std::{
    cell::RefCell,
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    ops::{Index, IndexMut},
};

use super::error::{Result, TempoPrecompileError};
use super::storage::{StorageCtx, StorageOps};

// ===========================================================================
// Layout types
// ===========================================================================

/// Describes how a type is laid out in EVM storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Single slot, N bytes (1-32). Can be packed with other fields if N < 32.
    Bytes(usize),
    /// Occupies N full slots (each 32 bytes). Cannot be packed.
    Slots(usize),
}

impl Layout {
    /// Returns true if this field can be packed with adjacent fields.
    pub const fn is_packable(&self) -> bool {
        match self {
            Self::Bytes(n) => *n < 32,
            Self::Slots(_) => false,
        }
    }

    /// Returns the number of storage slots this type occupies.
    pub const fn slots(&self) -> usize {
        match self {
            Self::Bytes(_) => 1,
            Self::Slots(n) => *n,
        }
    }

    /// Returns the number of bytes this type occupies.
    pub const fn bytes(&self) -> usize {
        match self {
            Self::Bytes(n) => *n,
            Self::Slots(n) => {
                let (mut i, mut result) = (0, 0);
                while i < *n {
                    result += 32;
                    i += 1;
                }
                result
            }
        }
    }
}

/// Describes the context in which a storable value is being loaded or stored.
///
/// Determines whether the value occupies an entire storage slot or is packed
/// with other values at a specific byte offset within a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct LayoutCtx(usize);

impl LayoutCtx {
    /// Load/store the entire value at a given slot.
    pub const FULL: Self = Self(usize::MAX);

    /// Load/store a packed primitive at the given byte offset within a slot.
    pub const fn packed(offset: usize) -> Self {
        debug_assert!(offset < 32);
        Self(offset)
    }

    /// Get the packed offset, returns `None` for `Full`.
    #[inline]
    pub const fn packed_offset(&self) -> Option<usize> {
        if self.0 == usize::MAX {
            None
        } else {
            Some(self.0)
        }
    }
}

// ===========================================================================
// Core traits
// ===========================================================================

/// Helper trait to access storage layout information.
pub trait StorableType {
    /// Describes how this type is laid out in storage.
    const LAYOUT: Layout;

    /// Number of storage slots this type takes.
    const SLOTS: usize = Self::LAYOUT.slots();

    /// Number of bytes this type takes.
    const BYTES: usize = Self::LAYOUT.bytes();

    /// Whether this type can be packed with adjacent fields.
    const IS_PACKABLE: bool = Self::LAYOUT.is_packable();

    /// Whether this type stores its data in its base slot or not.
    const IS_DYNAMIC: bool = false;

    /// The handler type that provides storage access for this type.
    type Handler;

    /// Creates a handler for this type at the given storage location.
    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler;
}

/// Abstracts reading, writing, and deleting values for [`Storable`] types.
pub trait Handler<T: Storable> {
    /// Reads the value from storage.
    fn read(&self) -> Result<T>;

    /// Writes the value to storage.
    fn write(&mut self, value: T) -> Result<()>;

    /// Deletes the value from storage (sets to zero).
    fn delete(&mut self) -> Result<()>;

    /// Reads the value from transient storage.
    fn t_read(&self) -> Result<T>;

    /// Writes the value to transient storage.
    fn t_write(&mut self, value: T) -> Result<()>;

    /// Deletes the value from transient storage (sets to zero).
    fn t_delete(&mut self) -> Result<()>;
}

/// High-level storage operations for storable types.
pub trait Storable: StorableType + Sized {
    /// Load this type from storage at the given slot.
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self>;

    /// Store this type to storage at the given slot.
    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()>;

    /// Delete this type from storage (set to zero).
    fn delete<S: StorageOps>(storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        match ctx.packed_offset() {
            None => {
                for offset in 0..Self::SLOTS {
                    storage.store(slot + U256::from(offset), U256::ZERO)?;
                }
                Ok(())
            }
            Some(offset) => {
                let bytes = Self::BYTES;
                let current = storage.load(slot)?;
                let cleared = packing::delete_from_word(current, offset, bytes)?;
                storage.store(slot, cleared)
            }
        }
    }
}

/// Private module to seal the `Packable` trait.
#[allow(unnameable_types)]
pub(crate) mod sealed {
    /// Marker trait to prevent external implementations of `Packable`.
    pub trait OnlyPrimitives {}
}

/// Trait for types that can be packed into EVM storage slots.
pub trait Packable: FromWord + StorableType {}

/// Trait for primitive types that fit into a single EVM storage slot.
///
/// Implementations must produce right-aligned U256 values.
pub trait FromWord: sealed::OnlyPrimitives {
    /// Encode this type to a single U256 word.
    fn to_word(&self) -> U256;

    /// Decode this type from a single U256 word.
    fn from_word(word: U256) -> Result<Self>
    where
        Self: Sized;
}

/// Blanket implementation of `Storable` for all `Packable` types.
impl<T: Packable> Storable for T {
    #[inline]
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self> {
        match ctx.packed_offset() {
            None => storage.load(slot).and_then(Self::from_word),
            Some(offset) => {
                let slot_value = storage.load(slot)?;
                packing::extract_from_word(slot_value, offset, Self::BYTES)
            }
        }
    }

    #[inline]
    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        match ctx.packed_offset() {
            None => storage.store(slot, self.to_word()),
            Some(offset) => {
                let current = storage.load(slot)?;
                let updated = packing::insert_into_word(current, self, offset, Self::BYTES)?;
                storage.store(slot, updated)
            }
        }
    }
}

/// Trait for types that can be used as storage mapping keys.
pub trait StorageKey: sealed::OnlyPrimitives {
    /// Returns key bytes for storage slot computation.
    fn as_storage_bytes(&self) -> impl AsRef<[u8]>;

    /// Compute storage slot for a mapping with this key.
    ///
    /// `keccak256(left_pad_32(key) || big_endian_32(slot))`
    fn mapping_slot(&self, slot: U256) -> U256 {
        let key_bytes = self.as_storage_bytes();
        let key_bytes = key_bytes.as_ref();
        debug_assert!(key_bytes.len() <= 32);

        let mut buf = [0u8; 64];
        buf[32 - key_bytes.len()..32].copy_from_slice(key_bytes);
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());

        U256::from_be_bytes(keccak256(buf).0)
    }
}

// ===========================================================================
// Packing utilities
// ===========================================================================

pub mod packing {
    //! Shared utilities for packing and unpacking values in EVM storage slots.

    use super::*;

    /// A helper struct for in-memory slot value manipulation during packing.
    pub struct PackedSlot(pub U256);

    impl StorageOps for PackedSlot {
        fn load(&self, _slot: U256) -> Result<U256> {
            Ok(self.0)
        }

        fn store(&mut self, _slot: U256, value: U256) -> Result<()> {
            self.0 = value;
            Ok(())
        }
    }

    /// Location information for a packed field within a storage slot.
    #[derive(Debug, Clone, Copy)]
    pub struct FieldLocation {
        /// Offset in slots from the base slot.
        pub offset_slots: usize,
        /// Offset in bytes within the target slot.
        pub offset_bytes: usize,
        /// Size of the field in bytes.
        pub size: usize,
    }

    impl FieldLocation {
        /// Create a new field location.
        #[inline]
        pub const fn new(offset_slots: usize, offset_bytes: usize, size: usize) -> Self {
            Self {
                offset_slots,
                offset_bytes,
                size,
            }
        }
    }

    /// Create a bit mask for a value of the given byte size.
    #[inline]
    pub fn create_element_mask(byte_count: usize) -> U256 {
        if byte_count >= 32 {
            U256::MAX
        } else {
            (U256::ONE << (byte_count * 8)) - U256::ONE
        }
    }

    /// Extract a packed value from a storage slot at a given byte offset.
    #[inline]
    pub fn extract_from_word<T: FromWord + StorableType>(
        slot_value: U256,
        offset: usize,
        bytes: usize,
    ) -> Result<T> {
        if offset + bytes > 32 {
            return Err(TempoPrecompileError::Fatal(format!(
                "Value of {} bytes at offset {} would span slot boundary (max offset: {})",
                bytes,
                offset,
                32 - bytes
            )));
        }

        let shift_bits = offset * 8;
        let mask = create_element_mask(bytes);
        T::from_word((slot_value >> shift_bits) & mask)
    }

    /// Insert a packed value into a storage slot at a given byte offset.
    #[inline]
    pub fn insert_into_word<T: FromWord + StorableType>(
        current: U256,
        value: &T,
        offset: usize,
        bytes: usize,
    ) -> Result<U256> {
        if offset + bytes > 32 {
            return Err(TempoPrecompileError::Fatal(format!(
                "Value of {} bytes at offset {} would span slot boundary (max offset: {})",
                bytes,
                offset,
                32 - bytes
            )));
        }

        let field_value = value.to_word();
        let shift_bits = offset * 8;
        let mask = create_element_mask(bytes);

        let clear_mask = !(mask << shift_bits);
        let cleared = current & clear_mask;

        let positioned = (field_value & mask) << shift_bits;
        Ok(cleared | positioned)
    }

    /// Zero out a packed value in a storage slot at a given byte offset.
    #[inline]
    pub fn delete_from_word(current: U256, offset: usize, bytes: usize) -> Result<U256> {
        if offset + bytes > 32 {
            return Err(TempoPrecompileError::Fatal(format!(
                "Value of {} bytes at offset {} would span slot boundary (max offset: {})",
                bytes,
                offset,
                32 - bytes
            )));
        }

        let mask = create_element_mask(bytes);
        let shifted_mask = mask << (offset * 8);
        Ok(current & !shifted_mask)
    }

    /// Calculate which slot an array element at index `idx` starts in.
    #[inline]
    pub const fn calc_element_slot(idx: usize, elem_bytes: usize) -> usize {
        let elems_per_slot = 32 / elem_bytes;
        idx / elems_per_slot
    }

    /// Calculate the byte offset within a slot for an array element at index `idx`.
    #[inline]
    pub const fn calc_element_offset(idx: usize, elem_bytes: usize) -> usize {
        let elems_per_slot = 32 / elem_bytes;
        (idx % elems_per_slot) * elem_bytes
    }

    /// Calculate the element location within a slot for an array element at index `idx`.
    #[inline]
    pub const fn calc_element_loc(idx: usize, elem_bytes: usize) -> FieldLocation {
        FieldLocation::new(
            calc_element_slot(idx, elem_bytes),
            calc_element_offset(idx, elem_bytes),
            elem_bytes,
        )
    }

    /// Calculate the total number of slots needed for an array.
    #[inline]
    pub const fn calc_packed_slot_count(n: usize, elem_bytes: usize) -> usize {
        let elems_per_slot = 32 / elem_bytes;
        n.div_ceil(elems_per_slot)
    }
}

// ===========================================================================
// Slot<T>
// ===========================================================================

/// Type-safe wrapper for a single EVM storage slot.
#[derive(Debug, Clone)]
pub struct Slot<T> {
    slot: U256,
    ctx: LayoutCtx,
    address: Address,
    _ty: PhantomData<T>,
}

impl<T> Slot<T> {
    /// Creates a new `Slot` with the given slot number and address.
    #[inline]
    pub fn new(slot: U256, address: Address) -> Self {
        Self {
            slot,
            ctx: LayoutCtx::FULL,
            address,
            _ty: PhantomData,
        }
    }

    /// Creates a new `Slot` with the given slot number, layout context, and address.
    #[inline]
    pub fn new_with_ctx(slot: U256, ctx: LayoutCtx, address: Address) -> Self {
        Self {
            slot,
            ctx,
            address,
            _ty: PhantomData,
        }
    }

    /// Creates a new `Slot` at a given offset from a base slot.
    #[inline]
    pub fn new_at_offset(base_slot: U256, offset_slots: usize, address: Address) -> Self {
        Self {
            slot: base_slot.saturating_add(U256::from_limbs([offset_slots as u64, 0, 0, 0])),
            ctx: LayoutCtx::FULL,
            address,
            _ty: PhantomData,
        }
    }

    /// Creates a new `Slot` from a `FieldLocation` (for packed struct fields).
    #[inline]
    pub fn new_at_loc(base_slot: U256, loc: packing::FieldLocation, address: Address) -> Self
    where
        T: StorableType,
    {
        debug_assert!(
            T::IS_PACKABLE,
            "`fn new_at_loc` can only be used with packable types"
        );
        Self {
            slot: base_slot.saturating_add(U256::from_limbs([loc.offset_slots as u64, 0, 0, 0])),
            ctx: LayoutCtx::packed(loc.offset_bytes),
            address,
            _ty: PhantomData,
        }
    }

    /// Returns the storage slot number.
    #[inline]
    pub const fn slot(&self) -> U256 {
        self.slot
    }

    /// Returns the byte offset within the slot (for packed fields).
    #[inline]
    pub const fn offset(&self) -> Option<usize> {
        self.ctx.packed_offset()
    }
}

impl<T> StorageOps for Slot<T> {
    fn load(&self, slot: U256) -> Result<U256> {
        let storage = StorageCtx;
        storage.sload(self.address, slot)
    }

    fn store(&mut self, slot: U256, value: U256) -> Result<()> {
        let mut storage = StorageCtx;
        storage.sstore(self.address, slot, value)
    }
}

/// Wrapper that routes storage operations through transient storage (TLOAD/TSTORE).
struct TransientOps {
    address: Address,
}

impl StorageOps for TransientOps {
    fn load(&self, slot: U256) -> Result<U256> {
        let storage = StorageCtx;
        storage.tload(self.address, slot)
    }

    fn store(&mut self, slot: U256, value: U256) -> Result<()> {
        let mut storage = StorageCtx;
        storage.tstore(self.address, slot, value)
    }
}

impl<T: Storable> Slot<T> {
    /// Returns a transient storage operations wrapper for this slot's address.
    fn transient(&self) -> TransientOps {
        TransientOps {
            address: self.address,
        }
    }
}

impl<T: Storable> Handler<T> for Slot<T> {
    #[inline]
    fn read(&self) -> Result<T> {
        T::load(self, self.slot, self.ctx)
    }

    #[inline]
    fn write(&mut self, value: T) -> Result<()> {
        value.store(self, self.slot, self.ctx)
    }

    #[inline]
    fn delete(&mut self) -> Result<()> {
        T::delete(self, self.slot, self.ctx)
    }

    #[inline]
    fn t_read(&self) -> Result<T> {
        T::load(&self.transient(), self.slot, self.ctx)
    }

    #[inline]
    fn t_write(&mut self, value: T) -> Result<()> {
        value.store(&mut self.transient(), self.slot, self.ctx)
    }

    #[inline]
    fn t_delete(&mut self) -> Result<()> {
        T::delete(&mut self.transient(), self.slot, self.ctx)
    }
}

// ===========================================================================
// Mapping<K, V>
// ===========================================================================

/// Type-safe access wrapper for EVM storage mappings (hash-based key-value storage).
#[derive(Debug, Clone)]
pub struct Mapping<K, V: StorableType> {
    base_slot: U256,
    address: Address,
    cache: HandlerCache<K, V::Handler>,
}

impl<K, V: StorableType> Mapping<K, V> {
    /// Creates a new `Mapping` with the given base slot number and address.
    #[inline]
    pub fn new(base_slot: U256, address: Address) -> Self {
        Self {
            base_slot,
            address,
            cache: HandlerCache::new(),
        }
    }

    /// Returns the U256 base storage slot number for this mapping.
    #[inline]
    pub const fn slot(&self) -> U256 {
        self.base_slot
    }

    /// Returns a `Handler` for the given key.
    pub fn at(&self, key: &K) -> &V::Handler
    where
        K: StorageKey + Hash + Eq + Clone,
    {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert(key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }

    /// Returns a mutable `Handler` for the given key.
    pub fn at_mut(&mut self, key: &K) -> &mut V::Handler
    where
        K: StorageKey + Hash + Eq + Clone,
    {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert_mut(key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }
}

impl<K, V: StorableType> Default for Mapping<K, V> {
    fn default() -> Self {
        Self::new(U256::ZERO, Address::ZERO)
    }
}

impl<K, V: StorableType> Index<K> for Mapping<K, V>
where
    K: StorageKey + Hash + Eq + Clone,
{
    type Output = V::Handler;

    fn index(&self, key: K) -> &Self::Output {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert(&key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }
}

impl<K, V: StorableType> IndexMut<K> for Mapping<K, V>
where
    K: StorageKey + Hash + Eq + Clone,
{
    fn index_mut(&mut self, key: K) -> &mut Self::Output {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert_mut(&key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }
}

impl<K, V> StorableType for Mapping<K, V>
where
    V: StorableType,
{
    const LAYOUT: Layout = Layout::Slots(1);
    type Handler = Self;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Self::new(slot, address)
    }
}

// ===========================================================================
// HandlerCache
// ===========================================================================

/// Cache for computed handlers with stable references.
#[derive(Debug, Default)]
struct HandlerCache<K, H> {
    inner: RefCell<HashMap<K, Box<H>>>,
}

impl<K, H> HandlerCache<K, H> {
    #[inline]
    fn new() -> Self {
        Self {
            inner: RefCell::new(HashMap::new()),
        }
    }
}

impl<K, H> Clone for HandlerCache<K, H> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Clone, H> HandlerCache<K, H> {
    #[inline]
    fn get_or_insert(&self, key: &K, f: impl FnOnce() -> H) -> &H {
        let mut cache = self.inner.borrow_mut();
        if let Some(boxed) = cache.get(key) {
            // SAFETY: Box provides stable heap address. Cache is append-only.
            return unsafe { &*(boxed.as_ref() as *const H) };
        }
        let boxed = cache.entry(key.clone()).or_insert_with(|| Box::new(f()));
        // SAFETY: Box provides stable heap address. Cache is append-only.
        unsafe { &*(boxed.as_ref() as *const H) }
    }

    #[inline]
    fn get_or_insert_mut(&mut self, key: &K, f: impl FnOnce() -> H) -> &mut H {
        let mut cache = self.inner.borrow_mut();
        if let Some(boxed) = cache.get_mut(key) {
            // SAFETY: Box provides stable heap address. Cache is append-only. &mut self ensures exclusive.
            return unsafe { &mut *(boxed.as_mut() as *mut H) };
        }
        let boxed = cache.entry(key.clone()).or_insert_with(|| Box::new(f()));
        // SAFETY: Box provides stable heap address. Cache is append-only. &mut self ensures exclusive.
        unsafe { &mut *(boxed.as_mut() as *mut H) }
    }
}

// ===========================================================================
// Bytes-like types (String, Bytes)
// ===========================================================================

/// Handler for bytes-like types (`Bytes`, `String`) with efficient length queries.
#[derive(Debug, Clone)]
pub struct BytesLikeHandler<T> {
    base_slot: U256,
    address: Address,
    _ty: PhantomData<T>,
}

impl<T: Storable> BytesLikeHandler<T> {
    /// Creates a new handler for the bytes-like value at the given base slot.
    #[inline]
    pub fn new(base_slot: U256, address: Address) -> Self {
        Self {
            base_slot,
            address,
            _ty: PhantomData,
        }
    }

    #[inline]
    fn as_slot(&self) -> Slot<T> {
        Slot::new(self.base_slot, self.address)
    }

    /// Returns the byte length without loading all data (only reads base slot).
    #[inline]
    pub fn len(&self) -> Result<usize> {
        let base_value = Slot::<U256>::new(self.base_slot, self.address).read()?;
        let is_long = is_long_string(base_value);
        Ok(calc_string_length(base_value, is_long))
    }

    /// Returns whether the stored value is empty.
    #[inline]
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

impl<T: Storable> Handler<T> for BytesLikeHandler<T> {
    #[inline]
    fn read(&self) -> Result<T> {
        self.as_slot().read()
    }

    #[inline]
    fn write(&mut self, value: T) -> Result<()> {
        self.as_slot().write(value)
    }

    #[inline]
    fn delete(&mut self) -> Result<()> {
        self.as_slot().delete()
    }

    #[inline]
    fn t_read(&self) -> Result<T> {
        self.as_slot().t_read()
    }

    #[inline]
    fn t_write(&mut self, value: T) -> Result<()> {
        self.as_slot().t_write(value)
    }

    #[inline]
    fn t_delete(&mut self) -> Result<()> {
        self.as_slot().t_delete()
    }
}

/// Returns true if the base slot value indicates a long string (bit 0 set).
#[inline]
fn is_long_string(base_value: U256) -> bool {
    !(base_value & U256::from(1u64)).is_zero()
}

/// Calculates the string/bytes length from the base slot value.
#[inline]
fn calc_string_length(base_value: U256, is_long: bool) -> usize {
    if is_long {
        // Long: base_slot = length * 2 + 1
        let len_u256: U256 = (base_value - U256::from(1u64)) >> 1;
        // Safe: string length fits in usize
        len_u256.try_into().unwrap_or(usize::MAX)
    } else {
        // Short: LSB byte = length * 2
        let lsb = base_value.byte(0); // least significant byte
        (lsb / 2) as usize
    }
}

// ===========================================================================
// Primitive type implementations
// ===========================================================================

// -- bool --

impl sealed::OnlyPrimitives for bool {}
impl Packable for bool {}

impl StorableType for bool {
    const LAYOUT: Layout = Layout::Bytes(1);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl FromWord for bool {
    #[inline]
    fn to_word(&self) -> U256 {
        if *self {
            U256::ONE
        } else {
            U256::ZERO
        }
    }

    #[inline]
    fn from_word(word: U256) -> Result<Self> {
        Ok(!word.is_zero())
    }
}

impl StorageKey for bool {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        if *self {
            [1u8]
        } else {
            [0u8]
        }
    }
}

// -- Address --

impl sealed::OnlyPrimitives for Address {}
impl Packable for Address {}

impl StorableType for Address {
    const LAYOUT: Layout = Layout::Bytes(20);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl FromWord for Address {
    #[inline]
    fn to_word(&self) -> U256 {
        use revm::interpreter::instructions::utility::IntoU256;
        (*self).into_u256()
    }

    #[inline]
    fn from_word(word: U256) -> Result<Self> {
        use revm::interpreter::instructions::utility::IntoAddress;
        Ok(word.into_address())
    }
}

impl StorageKey for Address {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.as_slice()
    }
}

// -- U256 --

impl sealed::OnlyPrimitives for U256 {}
impl Packable for U256 {}

impl StorableType for U256 {
    const LAYOUT: Layout = Layout::Bytes(32);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl FromWord for U256 {
    #[inline]
    fn to_word(&self) -> U256 {
        *self
    }

    #[inline]
    fn from_word(word: U256) -> Result<Self> {
        Ok(word)
    }
}

impl StorageKey for U256 {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.to_be_bytes::<32>()
    }
}

// -- Rust integer types --

macro_rules! impl_uint_storable {
    ($($ty:ty, $bytes:expr);+ $(;)?) => {
        $(
            impl sealed::OnlyPrimitives for $ty {}
            impl Packable for $ty {}

            impl StorableType for $ty {
                const LAYOUT: Layout = Layout::Bytes($bytes);
                type Handler = Slot<Self>;

                fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
                    Slot::new_with_ctx(slot, ctx, address)
                }
            }

            impl FromWord for $ty {
                #[inline]
                fn to_word(&self) -> U256 {
                    U256::from(*self)
                }

                #[inline]
                fn from_word(word: U256) -> Result<Self> {
                    word.try_into().map_err(|_| {
                        TempoPrecompileError::Fatal(
                            format!("U256 value too large for {}", stringify!($ty))
                        )
                    })
                }
            }
        )+
    };
}

impl_uint_storable! {
    u8,  1;
    u16, 2;
    u32, 4;
    u64, 8;
    u128, 16;
}

impl StorageKey for u64 {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.to_be_bytes()
    }
}

impl StorageKey for u128 {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.to_be_bytes()
    }
}

impl sealed::OnlyPrimitives for i16 {}

impl StorageKey for i16 {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.to_be_bytes()
    }
}

// -- alloy B256 --

impl sealed::OnlyPrimitives for alloy::primitives::B256 {}
impl Packable for alloy::primitives::B256 {}

impl StorableType for alloy::primitives::B256 {
    const LAYOUT: Layout = Layout::Bytes(32);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl FromWord for alloy::primitives::B256 {
    #[inline]
    fn to_word(&self) -> U256 {
        U256::from_be_bytes(self.0)
    }

    #[inline]
    fn from_word(word: U256) -> Result<Self> {
        Ok(alloy::primitives::B256::from(word.to_be_bytes::<32>()))
    }
}

impl StorageKey for alloy::primitives::B256 {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.as_slice()
    }
}

// -- alloy FixedBytes<4> --
//
// Mirrors writer `crates/precompiles-macros/src/storable_primitives.rs::FixedBytes`
// pattern. Value lives in the LOWER N bytes of the U256 word (left-padded with
// zeros in upper bytes), matching writer's uniform storage scheme rather than
// Solidity ABI right-padded bytes4 semantics. The default `mapping_slot`
// left-pads the key, which diverges from `abi.encode(bytes4)` but is what
// writer's storage layer uses end-to-end (see writer
// `storage/types/mod.rs:349-351` warning).
//
// Needed to instantiate `Set<FixedBytes<4>>` for account_keychain's
// per-target selector set in CallScope.

impl sealed::OnlyPrimitives for FixedBytes<4> {}
impl Packable for FixedBytes<4> {}

impl StorableType for FixedBytes<4> {
    const LAYOUT: Layout = Layout::Bytes(4);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl FromWord for FixedBytes<4> {
    #[inline]
    fn to_word(&self) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[28..32].copy_from_slice(&self.0);
        U256::from_be_bytes(bytes)
    }

    #[inline]
    fn from_word(word: U256) -> Result<Self> {
        let bytes = word.to_be_bytes::<32>();
        let mut fixed = [0u8; 4];
        fixed.copy_from_slice(&bytes[28..32]);
        Ok(Self::from(fixed))
    }
}

impl StorageKey for FixedBytes<4> {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.as_slice()
    }
}

// -- Bytes (dynamic) --

impl StorableType for Bytes {
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler = BytesLikeHandler<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        BytesLikeHandler::new(slot, address)
    }
}

// -- String (dynamic) --

impl StorableType for String {
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler = BytesLikeHandler<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        BytesLikeHandler::new(slot, address)
    }
}

// Bytes Storable implementation (Solidity-compatible layout)
impl Storable for Bytes {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let base_value = storage.load(slot)?;
        let is_long = is_long_string(base_value);
        let len = calc_string_length(base_value, is_long);

        if !is_long {
            // Short: data is in the high bytes of the base slot
            let bytes = base_value.to_be_bytes::<32>();
            Ok(Bytes::copy_from_slice(&bytes[..len]))
        } else {
            // Long: data starts at keccak256(slot)
            let data_start = U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0);
            let num_slots = len.div_ceil(32);
            let mut data = Vec::with_capacity(num_slots * 32);

            for i in 0..num_slots {
                let word = storage.load(data_start + U256::from(i))?;
                data.extend_from_slice(&word.to_be_bytes::<32>());
            }

            data.truncate(len);
            Ok(Bytes::from(data))
        }
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let data = self.as_ref();
        let len = data.len();

        if len < 32 {
            // Short string: pack data + length into one slot
            let mut slot_bytes = [0u8; 32];
            slot_bytes[..len].copy_from_slice(data);
            slot_bytes[31] = (len * 2) as u8;
            storage.store(slot, U256::from_be_bytes(slot_bytes))
        } else {
            // Long string: base slot = length * 2 + 1
            let length_value = U256::from(len * 2 + 1);
            storage.store(slot, length_value)?;

            // Data at keccak256(slot) + i
            let data_start = U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0);
            let num_slots = len.div_ceil(32);

            for i in 0..num_slots {
                let start = i * 32;
                let end = std::cmp::min(start + 32, len);
                let mut word = [0u8; 32];
                word[..end - start].copy_from_slice(&data[start..end]);
                storage.store(data_start + U256::from(i), U256::from_be_bytes(word))?;
            }

            Ok(())
        }
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let base_value = storage.load(slot)?;
        let is_long = is_long_string(base_value);

        if is_long {
            let len = calc_string_length(base_value, true);
            let data_start = U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0);
            let num_slots = len.div_ceil(32);
            for i in 0..num_slots {
                storage.store(data_start + U256::from(i), U256::ZERO)?;
            }
        }

        storage.store(slot, U256::ZERO)
    }
}

// String Storable implementation (delegates to Bytes)
impl Storable for String {
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self> {
        let bytes = Bytes::load(storage, slot, ctx)?;
        String::from_utf8(bytes.to_vec()).map_err(|e| {
            TempoPrecompileError::Fatal(format!("Invalid UTF-8 in storage string: {e}"))
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        let bytes = Bytes::from(self.as_bytes().to_vec());
        bytes.store(storage, slot, ctx)
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        Bytes::delete(storage, slot, ctx)
    }
}

// ===========================================================================
// Vec<T> (Solidity-compatible dynamic array)
// ===========================================================================

/// Computes the data start slot for a dynamic array: `keccak256(len_slot)`.
fn calc_data_slot(len_slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(len_slot.to_be_bytes::<32>()).0)
}

impl<T> StorableType for Vec<T>
where
    T: Storable,
{
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler = VecHandler<T>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        VecHandler::new(slot, address)
    }
}

impl<T> Storable for Vec<T>
where
    T: Storable,
{
    fn load<S: StorageOps>(storage: &S, len_slot: U256, ctx: LayoutCtx) -> Result<Self> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Dynamic arrays cannot be packed");
        let length_value = storage.load(len_slot)?;
        let length = length_value.to::<usize>();

        if length == 0 {
            return Ok(Self::new());
        }

        let data_start = calc_data_slot(len_slot);
        if T::BYTES <= 16 {
            // Packed elements
            let mut result = Vec::with_capacity(length);
            let slots_needed = packing::calc_packed_slot_count(length, T::BYTES);
            for slot_idx in 0..slots_needed {
                let slot_value = storage.load(data_start + U256::from(slot_idx))?;
                let packed = packing::PackedSlot(slot_value);
                let elems_per_slot = 32 / T::BYTES;
                let start_elem = slot_idx * elems_per_slot;
                let end_elem = (start_elem + elems_per_slot).min(length);
                for elem_idx in start_elem..end_elem {
                    let loc = packing::calc_element_loc(elem_idx, T::BYTES);
                    let elem = T::load(&packed, U256::ZERO, LayoutCtx::packed(loc.offset_bytes))?;
                    result.push(elem);
                }
            }
            Ok(result)
        } else {
            // Unpacked (multi-slot) elements
            let mut result = Vec::with_capacity(length);
            for elem_idx in 0..length {
                let elem_slot = data_start + U256::from(elem_idx * T::SLOTS);
                let elem = T::load(storage, elem_slot, LayoutCtx::FULL)?;
                result.push(elem);
            }
            Ok(result)
        }
    }

    fn store<S: StorageOps>(&self, storage: &mut S, len_slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Dynamic arrays cannot be packed");
        storage.store(len_slot, U256::from(self.len()))?;

        if self.is_empty() {
            return Ok(());
        }

        let data_start = calc_data_slot(len_slot);
        if T::BYTES <= 16 {
            let elems_per_slot = 32 / T::BYTES;
            let slots_needed = packing::calc_packed_slot_count(self.len(), T::BYTES);
            for slot_idx in 0..slots_needed {
                let mut packed = packing::PackedSlot(U256::ZERO);
                let start_elem = slot_idx * elems_per_slot;
                let end_elem = (start_elem + elems_per_slot).min(self.len());
                for elem_idx in start_elem..end_elem {
                    let loc = packing::calc_element_loc(elem_idx, T::BYTES);
                    self[elem_idx].store(
                        &mut packed,
                        U256::ZERO,
                        LayoutCtx::packed(loc.offset_bytes),
                    )?;
                }
                storage.store(data_start + U256::from(slot_idx), packed.0)?;
            }
        } else {
            for (elem_idx, elem) in self.iter().enumerate() {
                let elem_slot = data_start + U256::from(elem_idx * T::SLOTS);
                elem.store(storage, elem_slot, LayoutCtx::FULL)?;
            }
        }

        Ok(())
    }

    fn delete<S: StorageOps>(storage: &mut S, len_slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Dynamic arrays cannot be packed");
        let length_value = storage.load(len_slot)?;
        let length = length_value.to::<usize>();
        storage.store(len_slot, U256::ZERO)?;

        if length == 0 {
            return Ok(());
        }

        let data_start = calc_data_slot(len_slot);
        if T::BYTES <= 16 {
            let slot_count = packing::calc_packed_slot_count(length, T::BYTES);
            for slot_idx in 0..slot_count {
                storage.store(data_start + U256::from(slot_idx), U256::ZERO)?;
            }
        } else {
            for elem_idx in 0..length {
                let elem_slot = data_start + U256::from(elem_idx * T::SLOTS);
                T::delete(storage, elem_slot, LayoutCtx::FULL)?;
            }
        }

        Ok(())
    }
}

/// Type-safe handler for accessing `Vec<T>` in storage.
///
/// Provides full-vector operations (read/write/delete) and individual element access
/// via `at(index)` with bounds checking or `[index]` without bounds checking.
#[derive(Debug, Clone)]
pub struct VecHandler<T: Storable> {
    len_slot: U256,
    address: Address,
    cache: HandlerCache<usize, T::Handler>,
}

impl<T> Handler<Vec<T>> for VecHandler<T>
where
    T: Storable,
{
    fn read(&self) -> Result<Vec<T>> {
        self.as_slot().read()
    }
    fn write(&mut self, value: Vec<T>) -> Result<()> {
        self.as_slot().write(value)
    }
    fn delete(&mut self) -> Result<()> {
        self.as_slot().delete()
    }
    fn t_read(&self) -> Result<Vec<T>> {
        self.as_slot().t_read()
    }
    fn t_write(&mut self, value: Vec<T>) -> Result<()> {
        self.as_slot().t_write(value)
    }
    fn t_delete(&mut self) -> Result<()> {
        self.as_slot().t_delete()
    }
}

impl<T> VecHandler<T>
where
    T: Storable,
{
    /// Creates a new handler for the vector at the given base slot.
    pub fn new(len_slot: U256, address: Address) -> Self {
        Self {
            len_slot,
            address,
            cache: HandlerCache::new(),
        }
    }

    const fn max_index() -> usize {
        if T::BYTES <= 16 {
            u32::MAX as usize / T::BYTES
        } else {
            u32::MAX as usize / T::SLOTS
        }
    }

    fn as_slot(&self) -> Slot<Vec<T>> {
        Slot::new(self.len_slot, self.address)
    }

    /// Returns the data start slot for this array.
    pub fn data_slot(&self) -> U256 {
        calc_data_slot(self.len_slot)
    }

    /// Returns the length of the vector (reads from storage).
    pub fn len(&self) -> Result<usize> {
        let slot = Slot::<U256>::new(self.len_slot, self.address);
        Ok(slot.read()?.to::<usize>())
    }

    /// Returns whether the vector is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    fn compute_handler(data_start: U256, address: Address, index: usize) -> T::Handler {
        let (slot, layout_ctx) = if T::BYTES <= 16 {
            let location = packing::calc_element_loc(index, T::BYTES);
            (
                data_start + U256::from(location.offset_slots),
                LayoutCtx::packed(location.offset_bytes),
            )
        } else {
            (data_start + U256::from(index * T::SLOTS), LayoutCtx::FULL)
        };
        T::handle(slot, layout_ctx, address)
    }

    /// Returns a `Handler` for the element at the given index with bounds checking.
    pub fn at(&self, index: usize) -> Result<Option<&T::Handler>> {
        if index >= self.len()? {
            return Ok(None);
        }
        let (data_start, address) = (self.data_slot(), self.address);
        Ok(Some(self.cache.get_or_insert(&index, || {
            Self::compute_handler(data_start, address, index)
        })))
    }

    /// Pushes a new element to the end of the vector.
    pub fn push(&self, value: T) -> Result<()>
    where
        T::Handler: Handler<T>,
    {
        let length = self.len()?;
        if length >= Self::max_index() {
            return Err(TempoPrecompileError::Fatal("Vec is at max capacity".into()));
        }
        let mut elem_slot = Self::compute_handler(self.data_slot(), self.address, length);
        elem_slot.write(value)?;
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address);
        length_slot.write(U256::from(length + 1))
    }

    /// Removes and discards the last element of the vector.
    ///
    /// Decrements the length by one. The storage slot of the removed element is NOT zeroed --
    /// callers are responsible for clearing the element's data before calling `pop` if needed.
    pub fn pop(&self) -> Result<()> {
        let length = self.len()?;
        if length == 0 {
            return Err(TempoPrecompileError::Fatal("Vec is empty".into()));
        }
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address);
        length_slot.write(U256::from(length - 1))
    }
}

impl<T> Index<usize> for VecHandler<T>
where
    T: Storable,
{
    type Output = T::Handler;

    fn index(&self, index: usize) -> &Self::Output {
        let (data_start, address) = (self.data_slot(), self.address);
        self.cache.get_or_insert(&index, || {
            Self::compute_handler(data_start, address, index)
        })
    }
}

impl<T> IndexMut<usize> for VecHandler<T>
where
    T: Storable,
{
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        let (data_start, address) = (self.data_slot(), self.address);
        self.cache.get_or_insert_mut(&index, || {
            Self::compute_handler(data_start, address, index)
        })
    }
}

// ===========================================================================
// Set<T> -- OpenZeppelin EnumerableSet for EVM storage
// ===========================================================================
//
// Storage layout (mirrors writer crates/precompiles/src/storage/types/set.rs):
//   base_slot: length (U256) + values array data at keccak256(base_slot)
//   base_slot + 1: positions mapping (T -> u32, 1-indexed; 0 = not present)
//
// Read path is the leafage hot path (state diffs from writer populate storage,
// eth_call reads). Write paths are simplified relative to writer:
//   - `insert` and `remove` are implemented (used by setCallScopes full-replace)
//   - swap-and-pop on remove follows OZ semantics

/// Read-only in-memory snapshot of an [`SetHandler`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Set<T>(Vec<T>);

impl<T> Set<T> {
    #[inline]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    #[inline]
    pub fn into_inner(self) -> Vec<T> {
        self.0
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

impl<T> From<Set<T>> for Vec<T> {
    #[inline]
    fn from(set: Set<T>) -> Self {
        set.0
    }
}

impl<T: Eq + Hash + Clone> From<Vec<T>> for Set<T> {
    /// Creates a set from a vector, deduplicating while preserving first-occurrence order.
    fn from(vec: Vec<T>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let mut deduped = Vec::with_capacity(vec.len());
        for item in vec {
            if seen.insert(item.clone()) {
                deduped.push(item);
            }
        }
        Self(deduped)
    }
}

impl<T> IntoIterator for Set<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Handler for storage operations on a `Set<T>`.
///
/// Layout (`base_slot` is the U256 reserved for the set):
/// - `base_slot`: vec length; values data at `keccak256(base_slot)` (via `VecHandler`)
/// - `base_slot + 1`: `Mapping<T, u32>` for OZ EnumerableSet positions (1-indexed, 0 = absent)
pub struct SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
{
    values: VecHandler<T>,
    positions: Mapping<T, u32>,
    base_slot: U256,
    address: Address,
}

/// Set occupies 2 reserved slots; values + positions are then placed at hashed
/// derived slots. Layout-equivalent to writer `crates/precompiles/src/storage/types/set.rs`.
impl<T> StorableType for Set<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
{
    const LAYOUT: Layout = Layout::Slots(2);
    const IS_DYNAMIC: bool = true;
    type Handler = SetHandler<T>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        SetHandler::new(slot, address)
    }
}

impl<T> Storable for Set<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
    T::Handler: Handler<T>,
{
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let values: Vec<T> = Vec::load(storage, slot, LayoutCtx::FULL)?;
        Ok(Self(values))
    }

    /// Writes the set's values vector and length. The positions mapping at
    /// `slot + 1` is NOT updated here — it is only used by single-element
    /// `contains` / `insert` / `remove` paths. Full-replace writes via this
    /// `store` (e.g. nested via parent struct `Storable::store`) skip them;
    /// callers that need `contains` correctness afterwards must use
    /// `SetHandler::write` which keeps positions in sync.
    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        Vec::store(&self.0, storage, slot, LayoutCtx::FULL)
    }
}

#[inline]
fn checked_position(index: usize) -> Result<u32> {
    u32::try_from(index)
        .ok()
        .and_then(|i| i.checked_add(1))
        .ok_or_else(|| TempoPrecompileError::Fatal("Set position overflow".into()))
}

impl<T> SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
{
    pub fn new(base_slot: U256, address: Address) -> Self {
        Self {
            values: VecHandler::new(base_slot, address),
            positions: Mapping::new(base_slot + U256::ONE, address),
            base_slot,
            address,
        }
    }

    /// Returns the base storage slot.
    #[inline]
    pub fn base_slot(&self) -> U256 {
        self.base_slot
    }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> Result<usize> {
        self.values.len()
    }

    /// Returns whether the set is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Returns true if `value` is in the set.
    pub fn contains(&self, value: &T) -> Result<bool> {
        Ok(self.positions.at(value).read()? != 0)
    }

    /// Returns the value at the given index, or `None` if OOB.
    pub fn at(&self, index: usize) -> Result<Option<T>>
    where
        T::Handler: Handler<T>,
    {
        if index >= self.len()? {
            return Ok(None);
        }
        Ok(Some(self.values[index].read()?))
    }

    /// Inserts `value`. Returns `true` if newly added, `false` if already present.
    /// Mirrors writer single-element `Set::insert` behaviour (OZ EnumerableSet).
    pub fn insert(&mut self, value: T) -> Result<bool>
    where
        T::Handler: Handler<T>,
    {
        if self.contains(&value)? {
            return Ok(false);
        }
        let len = self.len()?;
        self.values.push(value.clone())?;
        self.positions
            .at_mut(&value)
            .write(checked_position(len)?)?;
        Ok(true)
    }

    /// Removes `value` via OZ EnumerableSet swap-and-pop. Returns `true` if it
    /// was present, `false` otherwise. Mirrors writer `Set::remove`.
    pub fn remove(&mut self, value: &T) -> Result<bool>
    where
        T::Handler: Handler<T>,
    {
        let pos = self.positions.at(value).read()?;
        if pos == 0 {
            return Ok(false);
        }
        let to_remove_idx = (pos - 1) as usize;
        let last_idx = self.len()?.saturating_sub(1);

        if to_remove_idx != last_idx {
            let last_value = self.values[last_idx].read()?;
            self.values[to_remove_idx].write(last_value.clone())?;
            self.positions
                .at_mut(&last_value)
                .write(checked_position(to_remove_idx)?)?;
        }

        self.values[last_idx].delete()?;
        self.values.pop()?;
        self.positions.at_mut(value).delete()?;
        Ok(true)
    }
}

impl<T> Handler<Set<T>> for SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
    T::Handler: Handler<T>,
{
    fn read(&self) -> Result<Set<T>> {
        let len = self.len()?;
        let mut vec = Vec::with_capacity(len);
        for i in 0..len {
            vec.push(self.values[i].read()?);
        }
        Ok(Set(vec))
    }

    fn write(&mut self, value: Set<T>) -> Result<()> {
        let old_len = self.values.len()?;
        let new_vec: Vec<T> = value.into();
        let new_len = new_vec.len();

        // Clear old positions.
        for i in 0..old_len {
            let old_value = self.values[i].read()?;
            self.positions.at_mut(&old_value).delete()?;
        }

        // Write new values + positions (1-indexed).
        for (index, new_value) in new_vec.into_iter().enumerate() {
            self.positions
                .at_mut(&new_value)
                .write(checked_position(index)?)?;
            self.values[index].write(new_value)?;
        }

        // Update length.
        Slot::<U256>::new(self.base_slot, self.address).write(U256::from(new_len))?;

        // Clear leftover value slots if shrinking.
        for i in new_len..old_len {
            self.values[i].delete()?;
        }

        Ok(())
    }

    fn delete(&mut self) -> Result<()> {
        let len = self.len()?;
        for i in 0..len {
            let value = self.values[i].read()?;
            self.positions.at_mut(&value).delete()?;
        }
        self.values.delete()
    }

    fn t_read(&self) -> Result<Set<T>> {
        Err(TempoPrecompileError::Fatal(
            "Set does not support transient storage".into(),
        ))
    }
    fn t_write(&mut self, _value: Set<T>) -> Result<()> {
        Err(TempoPrecompileError::Fatal(
            "Set does not support transient storage".into(),
        ))
    }
    fn t_delete(&mut self) -> Result<()> {
        Err(TempoPrecompileError::Fatal(
            "Set does not support transient storage".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_bytes_4_word_roundtrip() {
        let value = FixedBytes::<4>::from([0xde, 0xad, 0xbe, 0xef]);
        let word = value.to_word();
        assert_eq!(word, U256::from(0xdead_beefu32));
        let recovered = FixedBytes::<4>::from_word(word).unwrap();
        assert_eq!(recovered, value);
    }

    #[test]
    fn fixed_bytes_4_packing_at_offset() {
        let value = FixedBytes::<4>::from([0xab, 0xcd, 0xef, 0x01]);
        let mut packed = packing::PackedSlot(U256::ZERO);

        value
            .store(&mut packed, U256::ZERO, LayoutCtx::packed(8))
            .unwrap();
        let expected = U256::from(0xabcd_ef01u32) << (8 * 8);
        assert_eq!(packed.0, expected, "packed bytes at offset 8");

        let recovered =
            FixedBytes::<4>::load(&packed, U256::ZERO, LayoutCtx::packed(8)).unwrap();
        assert_eq!(recovered, value, "round-trip from packed offset");
    }

    #[test]
    fn fixed_bytes_4_mapping_slot_matches_left_padded_keccak() {
        let key = FixedBytes::<4>::from([0xde, 0xad, 0xbe, 0xef]);
        let slot = U256::from(7u8);

        let computed = key.mapping_slot(slot);

        let mut buf = [0u8; 64];
        buf[28..32].copy_from_slice(&key.0);
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        let expected = U256::from_be_bytes(keccak256(buf).0);

        assert_eq!(computed, expected);
    }
}
