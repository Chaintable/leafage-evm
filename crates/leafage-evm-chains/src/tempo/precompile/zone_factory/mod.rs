//! Native ZoneFactory precompile for TIP-1091 (T10+).

mod portal;

use std::collections::{HashMap, HashSet};

use alloy::primitives::{Address, B256, IntoLogData, U256, keccak256};
use alloy::sol_types::{SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_types::{Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType};
use super::tip20::TIP20Token;
use super::tip20_factory::TIP20Factory;
use super::tip403_registry::TIP403Registry;
use super::{
    Precompile, ZONE_FACTORY_ADDRESS, ZONE_MESSENGER_ADDRESS, ZONE_VERIFIER_ADDRESS, dispatch_call,
    input_cost, mutate, mutate_void, view,
};
use crate::tempo::address::TempoAddressExt;

pub use portal::{PortalTokenConfig, ZONE_PORTAL_PROXY_RUNTIME, ZonePortalStorage};

/// Minimum gas consumed by a successful zone creation attempt.
pub const ZONE_CREATION_GAS: u64 = 15_000_000;
/// Maximum number of equal sequencers in a zone settlement set.
pub const MAX_SEQUENCERS: usize = 8;
const MAX_TOKEN_METADATA_BYTES: usize = 31;

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    struct ZoneInfo {
        uint32 zoneId;
        address portal;
        bool accessMode;
        bool gatewayMode;
        address admin;
        address[] sequencers;
        uint8 threshold;
        address verifier;
        string rpcUrl;
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(abi)]
    interface IZoneFactory {
        struct CreateZoneParams {
            address initialToken;
            bool accessMode;
            bool gatewayMode;
            address[] allowedAccounts;
            address[] zoneGateways;
            address admin;
            address[] sequencers;
            uint8 threshold;
            string rpcUrl;
        }

        event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
        event ZoneCreated(
            uint32 indexed zoneId,
            address indexed portal,
            address initialToken,
            bool accessMode,
            bool gatewayMode,
            address admin,
            address[] sequencers,
            uint8 threshold,
            address verifier
        );

        error InvalidToken();
        error TokenTransferPolicyNotSet();
        error InvalidClosedLoopConfig();
        error NotOwner();
        error InvalidAdmin();
        error InvalidSequencerSet();
        error AlreadyInitialized();
        error TokenMetadataTooLong();

        function owner() external view returns (address);
        function transferOwnership(address newOwner) external;
        function createZone(CreateZoneParams calldata params)
            external
            returns (uint32 zoneId, address portal);
        function nextZoneId() external view returns (uint32);
        function zones(uint32 id) external view returns (ZoneInfo memory info);
        function isZonePortal(address portal) external view returns (bool);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(abi)]
    interface IZonePortal {
        enum Role {
            None,
            Sequencer,
            Account,
            CallbackGateway,
            PauseGuardian
        }

        enum Capability {
            PausePortal,
            AccessPolicy
        }

        event SequencerSetUpdated(uint64 indexed nonce, uint8 threshold, address[] sequencers);
        event TokenEnabled(address indexed token, string name, string symbol, string currency);
        event RoleUpdated(address indexed account, Role prev, Role next);
        event EnforcementModesUpdated(bool accessMode, bool gatewayMode);
        event LeaderUpdated(
            address indexed previousLeader,
            address indexed newLeader,
            uint64 indexed leaderEpoch,
            uint64 leaderActivationTempoBlock
        );
    }
}

pub(super) fn revert(error: impl SolError) -> TempoPrecompileError {
    TempoPrecompileError::Revert(error.abi_encode().into())
}

/// Solidity-compatible storage representation of `ZoneInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ZoneInfoStorage {
    zone_id: u32,
    portal: Address,
    access_mode: bool,
    gateway_mode: bool,
    admin: Address,
    sequencers: Vec<Address>,
    threshold: u8,
    verifier: Address,
    rpc_url: String,
}

impl StorableType for ZoneInfoStorage {
    const LAYOUT: Layout = Layout::Slots(5);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl Storable for ZoneInfoStorage {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        Ok(Self {
            zone_id: u32::load(storage, slot, LayoutCtx::packed(0))?,
            portal: Address::load(storage, slot, LayoutCtx::packed(4))?,
            access_mode: bool::load(storage, slot, LayoutCtx::packed(24))?,
            gateway_mode: bool::load(storage, slot, LayoutCtx::packed(25))?,
            admin: Address::load(storage, slot + U256::ONE, LayoutCtx::packed(0))?,
            sequencers: Vec::<Address>::load(storage, slot + U256::from(2), LayoutCtx::FULL)?,
            threshold: u8::load(storage, slot + U256::from(3), LayoutCtx::packed(0))?,
            verifier: Address::load(storage, slot + U256::from(3), LayoutCtx::packed(1))?,
            rpc_url: String::load(storage, slot + U256::from(4), LayoutCtx::FULL)?,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        self.zone_id.store(storage, slot, LayoutCtx::packed(0))?;
        self.portal.store(storage, slot, LayoutCtx::packed(4))?;
        self.access_mode
            .store(storage, slot, LayoutCtx::packed(24))?;
        self.gateway_mode
            .store(storage, slot, LayoutCtx::packed(25))?;
        self.admin
            .store(storage, slot + U256::ONE, LayoutCtx::packed(0))?;
        self.sequencers
            .store(storage, slot + U256::from(2), LayoutCtx::FULL)?;
        self.threshold
            .store(storage, slot + U256::from(3), LayoutCtx::packed(0))?;
        self.verifier
            .store(storage, slot + U256::from(3), LayoutCtx::packed(1))?;
        self.rpc_url
            .store(storage, slot + U256::from(4), LayoutCtx::FULL)
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        Vec::<Address>::delete(storage, slot + U256::from(2), LayoutCtx::FULL)?;
        String::delete(storage, slot + U256::from(4), LayoutCtx::FULL)?;
        for offset in 0..5 {
            storage.store(slot + U256::from(offset), U256::ZERO)?;
        }
        Ok(())
    }
}

impl From<ZoneInfoStorage> for ZoneInfo {
    fn from(value: ZoneInfoStorage) -> Self {
        Self {
            zoneId: value.zone_id,
            portal: value.portal,
            accessMode: value.access_mode,
            gatewayMode: value.gateway_mode,
            admin: value.admin,
            sequencers: value.sequencers,
            threshold: value.threshold,
            verifier: value.verifier,
            rpcUrl: value.rpc_url,
        }
    }
}

pub struct ZoneFactory {
    next_zone_id: Slot<u32>,
    owner: Slot<Address>,
    zones: Mapping<u32, ZoneInfoStorage>,
    address: Address,
    storage: StorageCtx,
}

impl ZoneFactory {
    pub fn new() -> Self {
        let address = ZONE_FACTORY_ADDRESS;
        Self {
            next_zone_id: Slot::new_with_ctx(U256::ZERO, LayoutCtx::packed(0), address),
            owner: Slot::new_with_ctx(U256::ZERO, LayoutCtx::packed(4), address),
            zones: Mapping::new(U256::ONE, address),
            address,
            storage: StorageCtx,
        }
    }

    pub fn owner(&self) -> Result<Address> {
        self.owner.read()
    }

    pub fn transfer_ownership(
        &mut self,
        msg_sender: Address,
        call: IZoneFactory::transferOwnershipCall,
    ) -> Result<()> {
        let previous_owner = self.owner()?;
        if msg_sender != previous_owner {
            return Err(revert(IZoneFactory::NotOwner {}));
        }
        self.owner.write(call.newOwner)?;
        self.storage.emit_event(
            self.address,
            IZoneFactory::OwnershipTransferred {
                previousOwner: previous_owner,
                newOwner: call.newOwner,
            }
            .into_log_data(),
        )
    }

    pub fn create_zone(
        &mut self,
        msg_sender: Address,
        call: IZoneFactory::createZoneCall,
    ) -> Result<IZoneFactory::createZoneReturn> {
        self.storage.deduct_gas(ZONE_CREATION_GAS)?;

        if msg_sender != self.owner()? {
            return Err(revert(IZoneFactory::NotOwner {}));
        }
        if !TIP20Factory::new().is_tip20(call.params.initialToken)? {
            return Err(revert(IZoneFactory::InvalidToken {}));
        }
        if TIP403Registry::new()
            .registered_token_transfer_policy_id(call.params.initialToken)?
            .is_none()
        {
            return Err(revert(IZoneFactory::TokenTransferPolicyNotSet {}));
        }
        validate_closed_loop_config(
            &call.params.allowedAccounts,
            &call.params.zoneGateways,
            &call.params.sequencers,
        )?;
        if call.params.admin.is_zero() {
            return Err(revert(IZoneFactory::InvalidAdmin {}));
        }
        validate_sequencer_set(&call.params.sequencers, call.params.threshold)?;

        let zone_id = self.next_zone_id()?;
        let portal = portal_address(zone_id);
        let token = TIP20Token::from_address(call.params.initialToken)?;
        let token_name = token.name()?;
        let token_symbol = token.symbol()?;
        let token_currency = token.currency()?;
        validate_token_metadata(&token_name, &token_symbol, &token_currency)?;
        let token_enablement_hash = keccak256(
            (
                B256::ZERO,
                call.params.initialToken,
                token_name.clone(),
                token_symbol.clone(),
                token_currency.clone(),
            )
                .abi_encode_params(),
        );

        self.next_zone_id.write(
            zone_id
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        ZonePortalStorage::new(portal).initialize(zone_id, &call.params, token_enablement_hash)?;
        self.zones[zone_id].write(ZoneInfoStorage {
            zone_id,
            portal,
            access_mode: call.params.accessMode,
            gateway_mode: call.params.gatewayMode,
            admin: call.params.admin,
            sequencers: call.params.sequencers.clone(),
            threshold: call.params.threshold,
            verifier: ZONE_VERIFIER_ADDRESS,
            rpc_url: call.params.rpcUrl.clone(),
        })?;

        self.storage.emit_event(
            portal,
            IZonePortal::EnforcementModesUpdated {
                accessMode: call.params.accessMode,
                gatewayMode: call.params.gatewayMode,
            }
            .into_log_data(),
        )?;
        self.storage.emit_event(
            portal,
            IZonePortal::SequencerSetUpdated {
                nonce: 0,
                threshold: call.params.threshold,
                sequencers: call.params.sequencers.clone(),
            }
            .into_log_data(),
        )?;
        self.storage.emit_event(
            portal,
            IZonePortal::LeaderUpdated {
                previousLeader: Address::ZERO,
                newLeader: call.params.sequencers[0],
                leaderEpoch: 1,
                leaderActivationTempoBlock: self.storage.block_number(),
            }
            .into_log_data(),
        )?;

        let mut emitted_roles = HashMap::new();
        for gateway in &call.params.zoneGateways {
            let previous = emitted_roles
                .insert(*gateway, IZonePortal::Role::CallbackGateway)
                .unwrap_or(IZonePortal::Role::None);
            self.storage.emit_event(
                portal,
                IZonePortal::RoleUpdated {
                    account: *gateway,
                    prev: previous,
                    next: IZonePortal::Role::CallbackGateway,
                }
                .into_log_data(),
            )?;
        }
        for account in &call.params.allowedAccounts {
            let previous = emitted_roles
                .insert(*account, IZonePortal::Role::Account)
                .unwrap_or(IZonePortal::Role::None);
            self.storage.emit_event(
                portal,
                IZonePortal::RoleUpdated {
                    account: *account,
                    prev: previous,
                    next: IZonePortal::Role::Account,
                }
                .into_log_data(),
            )?;
        }
        self.storage.emit_event(
            portal,
            IZonePortal::TokenEnabled {
                token: call.params.initialToken,
                name: token_name,
                symbol: token_symbol,
                currency: token_currency,
            }
            .into_log_data(),
        )?;
        self.storage.emit_event(
            self.address,
            IZoneFactory::ZoneCreated {
                zoneId: zone_id,
                portal,
                initialToken: call.params.initialToken,
                accessMode: call.params.accessMode,
                gatewayMode: call.params.gatewayMode,
                admin: call.params.admin,
                sequencers: call.params.sequencers.clone(),
                threshold: call.params.threshold,
                verifier: ZONE_VERIFIER_ADDRESS,
            }
            .into_log_data(),
        )?;

        Ok(IZoneFactory::createZoneReturn {
            zoneId: zone_id,
            portal,
        })
    }

    pub fn next_zone_id(&self) -> Result<u32> {
        self.next_zone_id.read()
    }

    pub fn zone(&self, zone_id: u32) -> Result<ZoneInfo> {
        Ok(self.zones[zone_id].read()?.into())
    }

    pub fn is_zone_portal(&self, portal: Address) -> Result<bool> {
        let Some(zone_id) = portal.zone_portal_id() else {
            return Ok(false);
        };
        Ok(zone_id < u64::from(self.next_zone_id()?))
    }
}

impl ContractStorage for ZoneFactory {
    fn address(&self) -> Address {
        self.address
    }

    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

impl Precompile for ZoneFactory {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            |data| {
                IZoneFactory::IZoneFactoryCalls::abi_decode_with_config(
                    data,
                    super::abi_decoder_config(),
                )
            },
            |call| match call {
                IZoneFactory::IZoneFactoryCalls::owner(call) => view(call, |_| self.owner()),
                IZoneFactory::IZoneFactoryCalls::transferOwnership(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.transfer_ownership(sender, call)
                    })
                }
                IZoneFactory::IZoneFactoryCalls::createZone(call) => {
                    mutate(call, msg_sender, |sender, call| {
                        self.create_zone(sender, call)
                    })
                }
                IZoneFactory::IZoneFactoryCalls::nextZoneId(call) => {
                    view(call, |_| self.next_zone_id())
                }
                IZoneFactory::IZoneFactoryCalls::zones(call) => {
                    view(call, |call| self.zone(call.id))
                }
                IZoneFactory::IZoneFactoryCalls::isZonePortal(call) => {
                    view(call, |call| self.is_zone_portal(call.portal))
                }
            },
        )
    }
}

fn validate_token_metadata(name: &str, symbol: &str, currency: &str) -> Result<()> {
    if [name, symbol, currency]
        .into_iter()
        .any(|value| value.len() > MAX_TOKEN_METADATA_BYTES)
    {
        return Err(revert(IZoneFactory::TokenMetadataTooLong {}));
    }
    Ok(())
}

fn validate_closed_loop_config(
    allowed_accounts: &[Address],
    zone_gateways: &[Address],
    sequencers: &[Address],
) -> Result<()> {
    if allowed_accounts.contains(&ZONE_MESSENGER_ADDRESS) {
        return Err(revert(IZoneFactory::InvalidClosedLoopConfig {}));
    }

    let mut seen =
        HashSet::with_capacity(allowed_accounts.len().saturating_add(zone_gateways.len()));
    seen.extend(allowed_accounts.iter().copied());
    if zone_gateways.iter().any(|gateway| seen.contains(gateway)) {
        return Err(revert(IZoneFactory::InvalidClosedLoopConfig {}));
    }
    seen.extend(zone_gateways.iter().copied());
    if sequencers.iter().any(|sequencer| seen.contains(sequencer)) {
        return Err(revert(IZoneFactory::InvalidClosedLoopConfig {}));
    }
    Ok(())
}

fn validate_sequencer_set(sequencers: &[Address], threshold: u8) -> Result<()> {
    if sequencers.is_empty()
        || sequencers.len() > MAX_SEQUENCERS
        || threshold == 0
        || usize::from(threshold) > sequencers.len()
    {
        return Err(revert(IZoneFactory::InvalidSequencerSet {}));
    }

    for (index, sequencer) in sequencers.iter().enumerate() {
        if sequencer.is_zero() || sequencers[..index].contains(sequencer) {
            return Err(revert(IZoneFactory::InvalidSequencerSet {}));
        }
    }
    Ok(())
}

/// Returns the deterministic TIP-1091 portal address for `zone_id`.
pub fn portal_address(zone_id: u32) -> Address {
    let mut bytes = [0u8; 20];
    bytes[..12].copy_from_slice(&Address::ZONE_PORTAL_PREFIX);
    bytes[12..].copy_from_slice(&u64::from(zone_id).to_be_bytes());
    Address::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::PATH_USD_ADDRESS;
    use crate::tempo::precompile::StorageKey;
    use crate::tempo::precompile::test_utils::TestStorageProvider;
    use alloy::primitives::address;
    use alloy::sol_types::{SolCall, SolError};

    const OWNER: Address = address!("0x0000000000000000000000000000000000000011");
    const ADMIN: Address = address!("0x0000000000000000000000000000000000000022");
    const SEQUENCER_A: Address = address!("0x0000000000000000000000000000000000000033");
    const SEQUENCER_B: Address = address!("0x0000000000000000000000000000000000000044");
    const ALLOWED_ACCOUNT: Address = address!("0x0000000000000000000000000000000000000055");
    const ZONE_GATEWAY: Address = address!("0x0000000000000000000000000000000000000066");
    const TOKEN: Address = address!("0x20c0000000000000000000000000000000000077");
    const CREATION_BLOCK: u64 = 42;

    fn initialize_token(token: Address, name: &str, symbol: &str, currency: &str) -> Result<()> {
        TIP20Token::from_address_unchecked(token).initialize(
            Address::ZERO,
            name,
            symbol,
            currency,
            PATH_USD_ADDRESS,
            ADMIN,
        )
    }

    fn factory_with_owner(owner: Address) -> Result<ZoneFactory> {
        let mut factory = ZoneFactory::new();
        factory.next_zone_id.write(1)?;
        factory.owner.write(owner)?;
        Ok(factory)
    }

    fn create_params(initial_token: Address) -> IZoneFactory::CreateZoneParams {
        IZoneFactory::CreateZoneParams {
            initialToken: initial_token,
            accessMode: true,
            gatewayMode: true,
            allowedAccounts: vec![ALLOWED_ACCOUNT],
            zoneGateways: vec![ZONE_GATEWAY],
            admin: ADMIN,
            sequencers: vec![SEQUENCER_A, SEQUENCER_B],
            threshold: 2,
            rpcUrl: "https://zone.example".to_string(),
        }
    }

    #[test]
    fn portal_address_uses_big_endian_zone_id_suffix() {
        assert_eq!(
            portal_address(1),
            address!("0x5AD0000000000000000000000000000000000001")
        );
        assert_eq!(
            portal_address(0x0102_0304),
            address!("0x5AD0000000000000000000000000000001020304")
        );
        assert_eq!(
            Address::from_slice(&ZONE_PORTAL_PROXY_RUNTIME[10..30]),
            super::super::ZONE_PORTAL_IMPL_ADDRESS
        );
    }

    #[test]
    fn create_zone_selector_matches_tip_1091() {
        assert_eq!(
            IZoneFactory::createZoneCall::SELECTOR,
            [0x89, 0x67, 0x7d, 0x9e]
        );
    }

    #[test]
    fn create_zone_installs_proxy_storage_and_events() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        provider.set_block_number(CREATION_BLOCK);

        let created = StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut factory = factory_with_owner(OWNER)?;
            let created = factory.create_zone(
                OWNER,
                IZoneFactory::createZoneCall {
                    params: create_params(TOKEN),
                },
            )?;

            assert_eq!(created.zoneId, 1);
            assert_eq!(created.portal, portal_address(1));
            assert_eq!(factory.next_zone_id()?, 2);
            assert!(factory.is_zone_portal(created.portal)?);
            assert!(!factory.is_zone_portal(portal_address(2))?);
            assert_eq!(
                factory.zone(1)?,
                ZoneInfo {
                    zoneId: 1,
                    portal: created.portal,
                    accessMode: true,
                    gatewayMode: true,
                    admin: ADMIN,
                    sequencers: vec![SEQUENCER_A, SEQUENCER_B],
                    threshold: 2,
                    verifier: ZONE_VERIFIER_ADDRESS,
                    rpcUrl: "https://zone.example".to_string(),
                }
            );

            let portal = ZonePortalStorage::new(created.portal);
            assert_eq!(portal.admin.read()?, ADMIN);
            assert_eq!(
                portal.token_configs[TOKEN].read()?,
                PortalTokenConfig {
                    enabled: true,
                    deposits_active: true,
                }
            );
            assert_eq!(portal.enabled_tokens.read()?, vec![TOKEN]);
            assert_eq!(portal.rpc_url.read()?, "https://zone.example");
            assert_eq!(portal.zone_id.read()?, 1);
            assert_eq!(portal.messenger.read()?, ZONE_MESSENGER_ADDRESS);
            assert_eq!(portal.verifier.read()?, ZONE_VERIFIER_ADDRESS);
            assert!(portal.initialized.read()?);
            assert_eq!(portal.sequencer_set_version.read()?, 0);
            assert_eq!(portal.sequencer_threshold.read()?, 2);
            assert_eq!(portal.sequencers.read()?, vec![SEQUENCER_A, SEQUENCER_B]);
            assert_eq!(
                portal.role[SEQUENCER_A].read()?,
                u8::from(IZonePortal::Role::Sequencer)
            );
            assert_eq!(
                portal.role[ALLOWED_ACCOUNT].read()?,
                u8::from(IZonePortal::Role::Account)
            );
            assert_eq!(
                portal.role[ZONE_GATEWAY].read()?,
                u8::from(IZonePortal::Role::CallbackGateway)
            );
            assert!(portal.is_access_enforced.read()?);
            assert!(portal.is_gateway_enforced.read()?);
            assert_eq!(portal.max_tempo_gas_rate.read()?, 0);
            assert_eq!(portal.leader.read()?, SEQUENCER_A);
            assert_eq!(portal.leader_epoch.read()?, 1);
            assert_eq!(portal.leader_activation_tempo_block.read()?, CREATION_BLOCK);
            assert_eq!(portal.token_enable_count_block.read()?, CREATION_BLOCK);
            assert_eq!(portal.tokens_enabled_in_current_block.read()?, 1);
            assert_eq!(portal.pause_expiry.read()?, 0);
            assert_eq!(
                portal.abdication_effective_at[1].read()?,
                0,
                "AccessPolicy abdication remains unset"
            );

            Result::<_>::Ok(created)
        })
        .unwrap();

        let code = provider
            .account(created.portal)
            .unwrap()
            .code
            .as_ref()
            .unwrap();
        assert_eq!(
            code.original_bytes().as_ref(),
            ZONE_PORTAL_PROXY_RUNTIME.as_slice()
        );
        assert_eq!(
            provider.storage(created.portal, U256::ZERO),
            U256::from_be_slice(ADMIN.as_slice())
        );
        assert_eq!(
            provider.storage(created.portal, U256::from(15)),
            U256::ONE | (U256::from_be_slice(ZONE_MESSENGER_ADDRESS.as_slice()) << 32)
        );
        assert_eq!(
            provider.storage(created.portal, U256::from(16)),
            U256::from_be_slice(ZONE_VERIFIER_ADDRESS.as_slice())
                | (U256::ONE << 160)
                | (U256::from(2) << 232)
        );
        assert_eq!(
            provider.storage(created.portal, U256::from(18)),
            U256::from(2)
        );
        assert_eq!(provider.storage(created.portal, U256::from(19)), U256::ZERO);
        assert_eq!(
            provider.storage(created.portal, U256::from(21)),
            U256::from(0x0101)
        );
        assert_eq!(
            provider.storage(created.portal, U256::from(23)),
            U256::from_be_slice(SEQUENCER_A.as_slice()) | (U256::ONE << 160)
        );
        assert_eq!(
            provider.storage(created.portal, U256::from(24)),
            U256::from(CREATION_BLOCK) | (U256::from(CREATION_BLOCK) << 192)
        );
        assert_eq!(provider.storage(created.portal, U256::from(25)), U256::ONE);
        assert_eq!(
            provider.storage(created.portal, TOKEN.mapping_slot(U256::from(6))),
            U256::from(0x0101)
        );

        assert_eq!(
            provider.storage(ZONE_FACTORY_ADDRESS, U256::ZERO),
            U256::from(2) | (U256::from_be_slice(OWNER.as_slice()) << 32)
        );
        let zone_slot = 1u32.mapping_slot(U256::ONE);
        assert_eq!(
            provider.storage(ZONE_FACTORY_ADDRESS, zone_slot),
            U256::ONE
                | (U256::from_be_slice(created.portal.as_slice()) << 32)
                | (U256::ONE << 192)
                | (U256::ONE << 200)
        );
        assert_eq!(
            provider.storage(ZONE_FACTORY_ADDRESS, zone_slot + U256::ONE),
            U256::from_be_slice(ADMIN.as_slice())
        );
        assert_eq!(
            provider.storage(ZONE_FACTORY_ADDRESS, zone_slot + U256::from(2)),
            U256::from(2)
        );
        assert_eq!(
            provider.storage(ZONE_FACTORY_ADDRESS, zone_slot + U256::from(3)),
            U256::from(2) | (U256::from_be_slice(ZONE_VERIFIER_ADDRESS.as_slice()) << 8)
        );

        assert_eq!(provider.events(created.portal).len(), 6);
        assert_eq!(
            provider.events(created.portal)[0],
            IZonePortal::EnforcementModesUpdated {
                accessMode: true,
                gatewayMode: true,
            }
            .into_log_data()
        );
        assert_eq!(
            provider.events(created.portal)[5],
            IZonePortal::TokenEnabled {
                token: TOKEN,
                name: "Token".to_string(),
                symbol: "TOK".to_string(),
                currency: "USD".to_string(),
            }
            .into_log_data()
        );
        assert_eq!(provider.events(ZONE_FACTORY_ADDRESS).len(), 1);
        assert_eq!(
            provider.events(ZONE_FACTORY_ADDRESS)[0],
            IZoneFactory::ZoneCreated {
                zoneId: 1,
                portal: created.portal,
                initialToken: TOKEN,
                accessMode: true,
                gatewayMode: true,
                admin: ADMIN,
                sequencers: vec![SEQUENCER_A, SEQUENCER_B],
                threshold: 2,
                verifier: ZONE_VERIFIER_ADDRESS,
            }
            .into_log_data()
        );
    }

    #[test]
    fn ownership_allows_zero_and_rejects_non_owner() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        StorageCtx::enter(&mut provider, || {
            let mut factory = factory_with_owner(OWNER)?;
            let unauthorized = factory.transfer_ownership(
                ADMIN,
                IZoneFactory::transferOwnershipCall { newOwner: ADMIN },
            );
            assert_eq!(unauthorized, Err(revert(IZoneFactory::NotOwner {})));

            factory.transfer_ownership(
                OWNER,
                IZoneFactory::transferOwnershipCall {
                    newOwner: Address::ZERO,
                },
            )?;
            assert_eq!(factory.owner()?, Address::ZERO);
            Result::<()>::Ok(())
        })
        .unwrap();

        assert_eq!(provider.events(ZONE_FACTORY_ADDRESS).len(), 1);
        assert_eq!(
            provider.events(ZONE_FACTORY_ADDRESS)[0],
            IZoneFactory::OwnershipTransferred {
                previousOwner: OWNER,
                newOwner: Address::ZERO,
            }
            .into_log_data()
        );
    }

    #[test]
    fn create_zone_requires_registry_policy_binding() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);
        StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")
        })
        .unwrap();
        provider.set_spec(TempoHardfork::T10);

        StorageCtx::enter(&mut provider, || {
            let mut factory = factory_with_owner(OWNER)?;
            let error = factory
                .create_zone(
                    OWNER,
                    IZoneFactory::createZoneCall {
                        params: create_params(TOKEN),
                    },
                )
                .unwrap_err();
            assert_eq!(error, revert(IZoneFactory::TokenTransferPolicyNotSet {}));
            assert_eq!(factory.next_zone_id()?, 1);

            TIP403Registry::new().set_token_transfer_policy(TOKEN, 1)?;
            factory.create_zone(
                OWNER,
                IZoneFactory::createZoneCall {
                    params: create_params(TOKEN),
                },
            )?;
            assert_eq!(factory.next_zone_id()?, 2);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn create_zone_rejects_invalid_token_and_zero_admin() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        StorageCtx::enter(&mut provider, || {
            let mut factory = factory_with_owner(OWNER)?;
            assert_eq!(
                factory
                    .create_zone(
                        OWNER,
                        IZoneFactory::createZoneCall {
                            params: create_params(Address::repeat_byte(0x77)),
                        },
                    )
                    .unwrap_err(),
                revert(IZoneFactory::InvalidToken {})
            );

            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut params = create_params(TOKEN);
            params.admin = Address::ZERO;
            assert_eq!(
                factory
                    .create_zone(OWNER, IZoneFactory::createZoneCall { params })
                    .unwrap_err(),
                revert(IZoneFactory::InvalidAdmin {})
            );
            assert_eq!(factory.next_zone_id()?, 1);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn duplicate_role_entries_preserve_constructor_event_order() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        let portal = StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut factory = factory_with_owner(OWNER)?;
            let mut params = create_params(TOKEN);
            params.zoneGateways = vec![ZONE_GATEWAY, ZONE_GATEWAY];
            params.allowedAccounts = vec![ALLOWED_ACCOUNT, ALLOWED_ACCOUNT];
            factory
                .create_zone(OWNER, IZoneFactory::createZoneCall { params })
                .map(|created| created.portal)
        })
        .unwrap();

        let events = provider.events(portal);
        assert_eq!(events.len(), 8);
        assert_eq!(
            events[3],
            IZonePortal::RoleUpdated {
                account: ZONE_GATEWAY,
                prev: IZonePortal::Role::None,
                next: IZonePortal::Role::CallbackGateway,
            }
            .into_log_data()
        );
        assert_eq!(
            events[4],
            IZonePortal::RoleUpdated {
                account: ZONE_GATEWAY,
                prev: IZonePortal::Role::CallbackGateway,
                next: IZonePortal::Role::CallbackGateway,
            }
            .into_log_data()
        );
        assert_eq!(
            events[6],
            IZonePortal::RoleUpdated {
                account: ALLOWED_ACCOUNT,
                prev: IZonePortal::Role::Account,
                next: IZonePortal::Role::Account,
            }
            .into_log_data()
        );
    }

    #[test]
    fn failed_portal_initialization_reverts_with_outer_checkpoint() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut factory = factory_with_owner(OWNER)?;
            let portal_address = portal_address(1);
            ZonePortalStorage::new(portal_address)
                .initialized
                .write(true)?;

            let checkpoint = factory.storage.checkpoint();
            let error = factory
                .create_zone(
                    OWNER,
                    IZoneFactory::createZoneCall {
                        params: create_params(TOKEN),
                    },
                )
                .unwrap_err();
            assert_eq!(error, revert(IZoneFactory::AlreadyInitialized {}));
            drop(checkpoint);

            assert_eq!(factory.next_zone_id()?, 1);
            assert_eq!(
                factory.zone(1)?,
                ZoneInfo {
                    zoneId: 0,
                    portal: Address::ZERO,
                    accessMode: false,
                    gatewayMode: false,
                    admin: Address::ZERO,
                    sequencers: Vec::new(),
                    threshold: 0,
                    verifier: Address::ZERO,
                    rpcUrl: String::new(),
                }
            );
            assert!(
                factory
                    .storage
                    .with_account_info(portal_address, |info| Ok(info.is_empty_code_hash()))?
            );
            Result::<()>::Ok(())
        })
        .unwrap();
        assert!(provider.events(portal_address(1)).is_empty());
    }

    #[test]
    fn closed_loop_sets_must_be_pairwise_disjoint() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut factory = factory_with_owner(OWNER)?;

            for (allowed, gateways) in [
                (vec![ZONE_MESSENGER_ADDRESS], vec![ZONE_GATEWAY]),
                (vec![ALLOWED_ACCOUNT], vec![ALLOWED_ACCOUNT]),
                (vec![SEQUENCER_A], vec![ZONE_GATEWAY]),
                (vec![ALLOWED_ACCOUNT], vec![SEQUENCER_A]),
            ] {
                let mut params = create_params(TOKEN);
                params.allowedAccounts = allowed;
                params.zoneGateways = gateways;
                assert_eq!(
                    factory
                        .create_zone(OWNER, IZoneFactory::createZoneCall { params })
                        .unwrap_err(),
                    revert(IZoneFactory::InvalidClosedLoopConfig {})
                );
                assert_eq!(factory.next_zone_id()?, 1);
            }
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn empty_role_sets_open_modes_and_admin_sequencer_are_allowed() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut factory = factory_with_owner(OWNER)?;
            let mut params = create_params(TOKEN);
            params.accessMode = false;
            params.gatewayMode = false;
            params.allowedAccounts.clear();
            params.zoneGateways.clear();
            params.sequencers = vec![ADMIN];
            params.threshold = 1;

            let created = factory.create_zone(OWNER, IZoneFactory::createZoneCall { params })?;
            let info = factory.zone(created.zoneId)?;
            assert!(!info.accessMode);
            assert!(!info.gatewayMode);
            assert_eq!(info.admin, ADMIN);
            assert_eq!(info.sequencers, vec![ADMIN]);

            let portal = ZonePortalStorage::new(created.portal);
            assert!(!portal.is_access_enforced.read()?);
            assert!(!portal.is_gateway_enforced.read()?);
            assert_eq!(portal.sequencers.read()?, vec![ADMIN]);
            assert_eq!(
                portal.role[ADMIN].read()?,
                u8::from(IZonePortal::Role::Sequencer)
            );
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sequencer_validation_covers_count_threshold_zero_and_duplicates() {
        let invalid = [
            (vec![], 1),
            (vec![Address::ZERO], 1),
            (vec![SEQUENCER_A, SEQUENCER_A], 1),
            (vec![SEQUENCER_A], 0),
            (vec![SEQUENCER_A], 2),
            ((1u8..=9).map(Address::with_last_byte).collect(), 1),
        ];
        for (sequencers, threshold) in invalid {
            assert_eq!(
                validate_sequencer_set(&sequencers, threshold),
                Err(revert(IZoneFactory::InvalidSequencerSet {}))
            );
        }
        assert!(validate_sequencer_set(&[SEQUENCER_A], 1).is_ok());
        assert!(
            validate_sequencer_set(
                &(1u8..=8).map(Address::with_last_byte).collect::<Vec<_>>(),
                8,
            )
            .is_ok()
        );
    }

    #[test]
    fn token_metadata_limit_counts_utf8_bytes() {
        assert!(validate_token_metadata(&"x".repeat(31), "s", "c").is_ok());
        assert!(validate_token_metadata("n", &"x".repeat(31), "c").is_ok());
        assert!(validate_token_metadata("n", "s", &"x".repeat(31)).is_ok());
        assert_eq!(
            validate_token_metadata(&"x".repeat(32), "s", "c"),
            Err(revert(IZoneFactory::TokenMetadataTooLong {}))
        );
        assert_eq!(
            validate_token_metadata("n", &"x".repeat(32), "c"),
            Err(revert(IZoneFactory::TokenMetadataTooLong {}))
        );
        assert_eq!(
            validate_token_metadata("n", "s", &"x".repeat(32)),
            Err(revert(IZoneFactory::TokenMetadataTooLong {}))
        );
        assert!(validate_token_metadata(&"界".repeat(10), "s", "c").is_ok());
        assert_eq!(
            validate_token_metadata(&"界".repeat(11), "s", "c"),
            Err(revert(IZoneFactory::TokenMetadataTooLong {}))
        );
    }

    #[test]
    fn create_zone_deducts_fixed_gas_before_validation() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        provider.set_gas_limit(ZONE_CREATION_GAS - 1);
        let result = StorageCtx::enter(&mut provider, || {
            ZoneFactory::new().create_zone(
                Address::ZERO,
                IZoneFactory::createZoneCall {
                    params: create_params(TOKEN),
                },
            )
        });
        assert_eq!(result, Err(TempoPrecompileError::OutOfGas));

        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        provider.set_gas_limit(ZONE_CREATION_GAS);
        StorageCtx::enter(&mut provider, || {
            let error = ZoneFactory::new()
                .create_zone(
                    ADMIN,
                    IZoneFactory::createZoneCall {
                        params: create_params(TOKEN),
                    },
                )
                .unwrap_err();
            assert_eq!(error, revert(IZoneFactory::NotOwner {}));
            assert_eq!(StorageCtx.gas_used(), ZONE_CREATION_GAS);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn dispatch_covers_all_selectors_and_rejects_static_mutation() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T10);
        StorageCtx::enter(&mut provider, || {
            initialize_token(TOKEN, "Token", "TOK", "USD")?;
            let mut factory = factory_with_owner(OWNER)?;
            let owner = factory
                .call(&IZoneFactory::ownerCall {}.abi_encode(), Address::ZERO)
                .unwrap();
            assert!(!owner.reverted);
            assert_eq!(
                IZoneFactory::ownerCall::abi_decode_returns(&owner.bytes).unwrap(),
                OWNER
            );

            for calldata in [
                IZoneFactory::nextZoneIdCall {}.abi_encode(),
                IZoneFactory::zonesCall { id: 99 }.abi_encode(),
                IZoneFactory::isZonePortalCall {
                    portal: portal_address(99),
                }
                .abi_encode(),
                IZoneFactory::createZoneCall {
                    params: create_params(TOKEN),
                }
                .abi_encode(),
                IZoneFactory::transferOwnershipCall { newOwner: ADMIN }.abi_encode(),
            ] {
                let output = factory.call(&calldata, OWNER).unwrap();
                assert!(!output.reverted);
            }
            assert_eq!(factory.next_zone_id()?, 2);
            assert_eq!(factory.owner()?, ADMIN);
            Result::<()>::Ok(())
        })
        .unwrap();

        provider.set_static(true);
        let output = StorageCtx::enter(&mut provider, || {
            ZoneFactory::new().call(
                &IZoneFactory::transferOwnershipCall { newOwner: ADMIN }.abi_encode(),
                OWNER,
            )
        })
        .unwrap();
        assert!(output.reverted);
        super::super::StaticCallNotAllowed::abi_decode(&output.bytes).unwrap();
    }
}
