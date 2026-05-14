//! TIP-1022 virtual address registry precompile (T3+).
//!
//! Ported from Tempo writer `crates/precompiles/src/address_registry/`.
//!
//! Provides on-chain registration of virtual-address masters and resolution of
//! virtual addresses back to their registered master address. Registration
//! requires a 32-bit proof-of-work to prevent squatting.
//!
//! ## Storage layout
//!
//! | Slot | Field | Type                                    |
//! |------|-------|-----------------------------------------|
//! |  0   | data  | Mapping<bytes4 masterId, RegistryData>  |
//!
//! `RegistryData` packs into a single 32-byte word:
//! `master_address(20)` @ packed offset 0 || `reserved(11)` @ offset 20 || `ty(1)` @ offset 31

use alloy::primitives::{keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_types::{
    Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType,
};
use super::{
    dispatch_call, input_cost, mutate, unknown_selector, view, Precompile,
    ADDRESS_REGISTRY_ADDRESS,
};
use crate::tempo::address::{MasterId, TempoAddressExt, UserTag};

// ===========================================================================
// Solidity ABI
// ===========================================================================

alloy::sol! {
    interface IAddressRegistry {
        function registerVirtualMaster(bytes32 salt) external returns (bytes4 masterId);
        function getMaster(bytes4 masterId) external view returns (address);
        function resolveRecipient(address to) external view returns (address effectiveRecipient);
        function resolveVirtualAddress(address virtualAddr) external view returns (address master);
        function isVirtualAddress(address addr) external pure returns (bool);
        function decodeVirtualAddress(address addr) external pure returns (bool isVirtual, bytes4 masterId, bytes6 userTag);

        event MasterRegistered(bytes4 indexed masterId, address indexed masterAddress);

        error MasterIdCollision(address master);
        error InvalidMasterAddress();
        error ProofOfWorkFailed();
        error VirtualAddressUnregistered();
    }
}

// ===========================================================================
// RegistryData (single packed slot)
// ===========================================================================

/// On-chain record for a registered virtual-address master. Packed into one 32-byte slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryData {
    /// EOA or contract that owns this `masterId`.
    pub master_address: Address,
    /// Reserved for future use; currently always zero.
    pub reserved: FixedBytes<11>,
    /// Master type discriminator; currently always zero.
    pub ty: u8,
}

impl RegistryData {
    /// Returns the master address, or `None` if the record is empty (`address(0)`).
    fn master_address(&self) -> Option<Address> {
        if self.master_address.is_zero() {
            None
        } else {
            Some(self.master_address)
        }
    }
}

impl StorableType for RegistryData {
    const LAYOUT: Layout = Layout::Slots(1);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for RegistryData {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        // Packed layout from writer #[derive(Storable)]:
        //   master_address (size 20) @ packed offset 0  → BE bytes[12..32]
        //   reserved       (size 11) @ packed offset 20 → BE bytes[1..12]
        //   ty             (size 1)  @ packed offset 31 → BE bytes[0]
        let word = storage.load(slot)?;
        let bytes = word.to_be_bytes::<32>();
        Ok(Self {
            ty: bytes[0],
            reserved: FixedBytes::<11>::from_slice(&bytes[1..12]),
            master_address: Address::from_slice(&bytes[12..32]),
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[0] = self.ty;
        bytes[1..12].copy_from_slice(self.reserved.as_slice());
        bytes[12..32].copy_from_slice(self.master_address.as_slice());
        storage.store(slot, U256::from_be_bytes(bytes))
    }
}

// ===========================================================================
// AddressRegistry struct
// ===========================================================================

/// TIP-1022 virtual address registry contract.
///
/// `MasterId` (4 bytes) is stored as a Solidity-style mapping key — i.e. the
/// 4 bytes are right-padded with 28 zero bytes before hashing to produce the
/// storage slot. We implement this by storing the right-padded value as a
/// `B256` Mapping key, since `bytes4` ABI encoding pads on the right but the
/// leafage `StorageKey` infrastructure pads on the left (which would diverge
/// from writer / Solidity behaviour for sub-32-byte byte types).
pub struct AddressRegistry {
    pub data: Mapping<B256, RegistryData>,
    pub address: Address,
    pub storage: StorageCtx,
}

impl AddressRegistry {
    pub fn new() -> Self {
        let address = ADDRESS_REGISTRY_ADDRESS;
        Self {
            data: Mapping::new(U256::from(0), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    /// Encode a `MasterId` (4 bytes) as a Solidity `bytes4` mapping key
    /// (right-padded with 28 zero bytes to 32 bytes).
    #[inline]
    fn master_id_key(id: MasterId) -> B256 {
        let mut buf = [0u8; 32];
        buf[..4].copy_from_slice(id.as_slice());
        B256::from(buf)
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    // ────────────────── Registration ──────────────────

    /// Registers `msg_sender` as a virtual-address master and returns the derived
    /// `MasterId`.
    ///
    /// Registration hash is `keccak256(abi.encodePacked(msg.sender, salt))`. The
    /// first 4 bytes MUST be zero (32-bit PoW); `masterId` is bytes `[4..8]`.
    pub fn register_virtual_master(
        &mut self,
        msg_sender: Address,
        call: IAddressRegistry::registerVirtualMasterCall,
    ) -> Result<MasterId> {
        if !msg_sender.is_valid_master() {
            return Err(TempoPrecompileError::Revert(
                IAddressRegistry::InvalidMasterAddress {}.abi_encode().into(),
            ));
        }

        let registration_hash = keccak256((msg_sender, call.salt).abi_encode_packed());

        // 32-bit PoW: first 4 bytes must be zero.
        if registration_hash[0..4] != [0u8; 4] {
            return Err(TempoPrecompileError::Revert(
                IAddressRegistry::ProofOfWorkFailed {}.abi_encode().into(),
            ));
        }

        let master_id = MasterId::from_slice(&registration_hash[4..8]);
        let key = Self::master_id_key(master_id);

        if let Some(existing) = self.data[key].read()?.master_address() {
            return Err(TempoPrecompileError::Revert(
                IAddressRegistry::MasterIdCollision { master: existing }
                    .abi_encode()
                    .into(),
            ));
        }

        self.data[key].write(RegistryData {
            master_address: msg_sender,
            reserved: FixedBytes::ZERO,
            ty: 0,
        })?;

        self.emit_event(IAddressRegistry::MasterRegistered {
            masterId: master_id,
            masterAddress: msg_sender,
        })?;

        Ok(master_id)
    }

    // ────────────────── View Functions ──────────────────

    /// Returns the registered master address for `master_id`, or `None` if unregistered.
    pub fn get_master(&self, master_id: MasterId) -> Result<Option<Address>> {
        let key = Self::master_id_key(master_id);
        Ok(self.data[key].read()?.master_address())
    }

    /// Resolves a transfer recipient through virtual-address semantics.
    ///
    /// - Non-virtual addresses are returned unchanged.
    /// - Pre-T3 (defensive — registration is already gated): returns `to` literally.
    /// - Virtual addresses are resolved to their master; reverts with
    ///   `VirtualAddressUnregistered` if the master is not registered.
    pub fn resolve_recipient(&self, to: Address) -> Result<Address> {
        if !self.storage.spec().is_t3() {
            return Ok(to);
        }
        match to.decode_virtual() {
            None => Ok(to),
            Some((master_id, _)) => self.get_master(master_id)?.ok_or_else(|| {
                TempoPrecompileError::Revert(
                    IAddressRegistry::VirtualAddressUnregistered {}
                        .abi_encode()
                        .into(),
                )
            }),
        }
    }

    /// Pure-view variant of [`Self::resolve_recipient`].
    /// Returns `address(0)` when `addr` is not virtual or the `masterId` is unregistered.
    pub fn resolve_virtual_address(&self, addr: Address) -> Result<Address> {
        match addr.decode_virtual() {
            None => Ok(Address::ZERO),
            Some((master_id, _)) => Ok(self.get_master(master_id)?.unwrap_or(Address::ZERO)),
        }
    }
}

impl ContractStorage for AddressRegistry {
    #[inline]
    fn address(&self) -> Address {
        self.address
    }

    #[inline]
    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

// ===========================================================================
// Dispatch
// ===========================================================================

impl Precompile for AddressRegistry {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        // Defense in depth — registration also gates on spec.is_t3(), but reject
        // again here to catch any caller that bypassed the lookup.
        if !self.storage.spec().is_t3() {
            let selector: [u8; 4] = if calldata.len() >= 4 {
                calldata[..4].try_into().expect("4-byte slice")
            } else {
                [0; 4]
            };
            return unknown_selector(selector, 0);
        }

        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            IAddressRegistry::IAddressRegistryCalls::abi_decode,
            |call| match call {
                IAddressRegistry::IAddressRegistryCalls::registerVirtualMaster(c) => {
                    mutate(c, msg_sender, |sender, c| {
                        self.register_virtual_master(sender, c)
                    })
                }
                IAddressRegistry::IAddressRegistryCalls::getMaster(c) => view(c, |c| {
                    Ok(self.get_master(c.masterId)?.unwrap_or(Address::ZERO))
                }),
                IAddressRegistry::IAddressRegistryCalls::resolveRecipient(c) => {
                    view(c, |c| self.resolve_recipient(c.to))
                }
                IAddressRegistry::IAddressRegistryCalls::resolveVirtualAddress(c) => {
                    view(c, |c| self.resolve_virtual_address(c.virtualAddr))
                }
                IAddressRegistry::IAddressRegistryCalls::isVirtualAddress(c) => {
                    view(c, |c| Ok(c.addr.is_virtual()))
                }
                IAddressRegistry::IAddressRegistryCalls::decodeVirtualAddress(c) => {
                    view(c, |c| {
                        let (is_v, mid, tag) = match c.addr.decode_virtual() {
                            Some((m, t)) => (true, m, t),
                            None => (false, MasterId::ZERO, UserTag::ZERO),
                        };
                        Ok(IAddressRegistry::decodeVirtualAddressReturn {
                            isVirtual: is_v,
                            masterId: mid,
                            userTag: tag,
                        })
                    })
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::storage::with_read_only_storage_ctx;
    use alloy::primitives::address;
    use alloy::sol_types::SolCall;
    use revm::database::EmptyDB;
    use std::collections::HashMap;

    /// In-memory `StorageOps` for unit-testing the Storable layout. No journaling,
    /// no spec — just raw slot read/write.
    struct MockStorage(HashMap<U256, U256>);
    impl MockStorage {
        fn new() -> Self {
            Self(HashMap::new())
        }
    }
    impl StorageOps for MockStorage {
        fn load(&self, slot: U256) -> Result<U256> {
            Ok(self.0.get(&slot).copied().unwrap_or(U256::ZERO))
        }
        fn store(&mut self, slot: U256, value: U256) -> Result<()> {
            self.0.insert(slot, value);
            Ok(())
        }
    }

    #[test]
    fn registry_data_round_trip_packing() {
        let data = RegistryData {
            master_address: address!("0x1234567890123456789012345678901234567890"),
            reserved: FixedBytes::<11>::from_slice(&[0xABu8; 11]),
            ty: 0x42,
        };

        let mut mock = MockStorage::new();
        data.store(&mut mock, U256::from(42), LayoutCtx::FULL).unwrap();
        let loaded = RegistryData::load(&mock, U256::from(42), LayoutCtx::FULL).unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn registry_data_packed_layout_matches_writer() {
        // Verify the precise byte layout: ty @ BE byte 0, reserved @ BE bytes [1..12],
        // master_address @ BE bytes [12..32]. This mirrors the writer's
        // #[derive(Storable)] packed encoding for forward storage-slot
        // compatibility with archive state diffs.
        let data = RegistryData {
            master_address: address!("0x000102030405060708090A0B0C0D0E0F10111213"),
            reserved: FixedBytes::<11>::from_slice(&[
                0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
            ]),
            ty: 0x1F,
        };

        let mut mock = MockStorage::new();
        data.store(&mut mock, U256::from(7), LayoutCtx::FULL).unwrap();

        let stored = mock.load(U256::from(7)).unwrap();
        let bytes = stored.to_be_bytes::<32>();
        assert_eq!(bytes[0], 0x1F, "ty at byte 0");
        assert_eq!(&bytes[1..12], &[0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E]);
        assert_eq!(&bytes[12..32], data.master_address.as_slice());
    }

    #[test]
    fn empty_registry_data_returns_none() {
        let data = RegistryData::default();
        assert_eq!(data.master_address(), None);
    }

    #[test]
    fn master_id_key_right_pads_to_b256() {
        let id = MasterId::from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let key = AddressRegistry::master_id_key(id);
        let bytes = key.as_slice();
        assert_eq!(&bytes[..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&bytes[4..], &[0u8; 28], "must right-pad with zeros");
    }

    #[test]
    fn pre_t3_call_returns_unknown_selector() {
        let calldata = IAddressRegistry::getMasterCall {
            masterId: MasterId::ZERO,
        }
        .abi_encode();

        let result = with_read_only_storage_ctx(&EmptyDB::default(), TempoHardfork::T2, 4217, || {
            AddressRegistry::new().call(&calldata, Address::ZERO)
        });

        let output = result.expect("call ok");
        assert!(output.reverted, "pre-T3 must revert");
    }

    #[test]
    fn t3_resolve_recipient_non_virtual_returns_input() {
        let eoa = address!("0x1234567890123456789012345678901234567890");
        let result = with_read_only_storage_ctx(&EmptyDB::default(), TempoHardfork::T3, 4217, || {
            AddressRegistry::new().resolve_recipient(eoa)
        });
        assert_eq!(result.unwrap(), eoa);
    }

    #[test]
    fn t3_resolve_recipient_unregistered_virtual_reverts() {
        let virt = Address::new_virtual(MasterId::ZERO, UserTag::ZERO);
        let result = with_read_only_storage_ctx(&EmptyDB::default(), TempoHardfork::T3, 4217, || {
            AddressRegistry::new().resolve_recipient(virt)
        });
        assert!(matches!(result, Err(TempoPrecompileError::Revert(_))));
    }

    #[test]
    fn t3_resolve_virtual_address_view_returns_zero_for_non_virtual() {
        let eoa = address!("0x1234567890123456789012345678901234567890");
        let result = with_read_only_storage_ctx(&EmptyDB::default(), TempoHardfork::T3, 4217, || {
            AddressRegistry::new().resolve_virtual_address(eoa)
        });
        assert_eq!(result.unwrap(), Address::ZERO);
    }

    #[test]
    fn t3_resolve_virtual_address_view_returns_zero_for_unregistered() {
        let virt = Address::new_virtual(MasterId::ZERO, UserTag::ZERO);
        let result = with_read_only_storage_ctx(&EmptyDB::default(), TempoHardfork::T3, 4217, || {
            AddressRegistry::new().resolve_virtual_address(virt)
        });
        assert_eq!(result.unwrap(), Address::ZERO);
    }

    #[test]
    fn pre_t3_resolve_recipient_returns_literal_virtual() {
        let virt = Address::new_virtual(MasterId::ZERO, UserTag::ZERO);
        let result = with_read_only_storage_ctx(&EmptyDB::default(), TempoHardfork::T2, 4217, || {
            AddressRegistry::new().resolve_recipient(virt)
        });
        // Pre-T3: returns the literal virtual address (no resolution attempted).
        assert_eq!(result.unwrap(), virt);
    }
}
