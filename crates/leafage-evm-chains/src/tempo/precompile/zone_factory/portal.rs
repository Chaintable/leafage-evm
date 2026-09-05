//! Solidity-compatible storage initialization for TIP-1091 ZonePortal proxies.
//!
//! ZonePortal is not a native precompile. The factory installs an ERC-1167 proxy and writes the
//! constructor-equivalent state consumed by the shared Solidity implementation.

use alloy::primitives::{Address, B256, Bytes, U256, hex};
use revm::state::Bytecode;

use super::{IZoneFactory, IZonePortal, revert};
use crate::tempo::precompile::error::Result;
use crate::tempo::precompile::storage::StorageOps;
use crate::tempo::precompile::storage_types::{
    BytesLikeHandler, Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType, VecHandler,
};
use crate::tempo::precompile::{ZONE_MESSENGER_ADDRESS, ZONE_VERIFIER_ADDRESS};

/// Exact ERC-1167 runtime installed at every ZonePortal address.
pub const ZONE_PORTAL_PROXY_RUNTIME: [u8; 45] = hex!(
    "363d3d373d3d3d363d735ad10000000000000000000000000000000000005af43d82803e903d91602b57fd5bf3"
);

/// Packed `TokenConfig` stored in the portal token registry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalTokenConfig {
    pub enabled: bool,
    pub deposits_active: bool,
}

impl StorableType for PortalTokenConfig {
    const LAYOUT: Layout = Layout::Bytes(2);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl Storable for PortalTokenConfig {
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self> {
        let word = match ctx.packed_offset() {
            Some(offset) => crate::tempo::precompile::packing::extract_from_word(
                storage.load(slot)?,
                offset,
                Self::BYTES,
            )?,
            None => storage.load(slot)?,
        };
        Ok(Self {
            enabled: (word & U256::from(0xff)) != U256::ZERO,
            deposits_active: ((word >> 8) & U256::from(0xff)) != U256::ZERO,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        let value =
            U256::from(u8::from(self.enabled)) | (U256::from(u8::from(self.deposits_active)) << 8);
        match ctx.packed_offset() {
            Some(offset) => {
                let current = storage.load(slot)?;
                storage.store(
                    slot,
                    crate::tempo::precompile::packing::insert_into_word(
                        current,
                        &value,
                        offset,
                        Self::BYTES,
                    )?,
                )
            }
            None => storage.store(slot, value),
        }
    }
}

/// Handles the subset of the canonical ZonePortal slots initialized by the native factory.
pub struct ZonePortalStorage {
    pub admin: Slot<Address>,
    pub token_configs: Mapping<Address, PortalTokenConfig>,
    pub enabled_tokens: VecHandler<Address>,
    pub rpc_url: BytesLikeHandler<String>,
    pub zone_id: Slot<u32>,
    pub messenger: Slot<Address>,
    pub verifier: Slot<Address>,
    pub initialized: Slot<bool>,
    pub sequencer_set_version: Slot<u64>,
    pub sequencer_threshold: Slot<u8>,
    pub sequencers: VecHandler<Address>,
    pub role: Mapping<Address, u8>,
    pub is_access_enforced: Slot<bool>,
    pub is_gateway_enforced: Slot<bool>,
    pub max_tempo_gas_rate: Slot<u128>,
    pub leader: Slot<Address>,
    pub leader_epoch: Slot<u64>,
    pub leader_activation_tempo_block: Slot<u64>,
    pub token_enable_count_block: Slot<u64>,
    pub tokens_enabled_in_current_block: Slot<u64>,
    pub pause_expiry: Slot<u64>,
    pub token_enablement_hash: Slot<B256>,
    pub abdication_effective_at: Mapping<u8, u64>,
    pub address: Address,
}

impl ZonePortalStorage {
    pub fn new(address: Address) -> Self {
        Self {
            admin: Slot::new_with_ctx(U256::ZERO, LayoutCtx::packed(0), address),
            token_configs: Mapping::new(U256::from(6), address),
            enabled_tokens: VecHandler::new(U256::from(7), address),
            rpc_url: BytesLikeHandler::new(U256::from(12), address),
            zone_id: Slot::new_with_ctx(U256::from(15), LayoutCtx::packed(0), address),
            messenger: Slot::new_with_ctx(U256::from(15), LayoutCtx::packed(4), address),
            verifier: Slot::new_with_ctx(U256::from(16), LayoutCtx::packed(0), address),
            initialized: Slot::new_with_ctx(U256::from(16), LayoutCtx::packed(20), address),
            sequencer_set_version: Slot::new_with_ctx(
                U256::from(16),
                LayoutCtx::packed(21),
                address,
            ),
            sequencer_threshold: Slot::new_with_ctx(U256::from(16), LayoutCtx::packed(29), address),
            sequencers: VecHandler::new(U256::from(18), address),
            role: Mapping::new(U256::from(20), address),
            is_access_enforced: Slot::new_with_ctx(U256::from(21), LayoutCtx::packed(0), address),
            is_gateway_enforced: Slot::new_with_ctx(U256::from(21), LayoutCtx::packed(1), address),
            max_tempo_gas_rate: Slot::new_with_ctx(U256::from(22), LayoutCtx::packed(0), address),
            leader: Slot::new_with_ctx(U256::from(23), LayoutCtx::packed(0), address),
            leader_epoch: Slot::new_with_ctx(U256::from(23), LayoutCtx::packed(20), address),
            leader_activation_tempo_block: Slot::new_with_ctx(
                U256::from(24),
                LayoutCtx::packed(0),
                address,
            ),
            token_enable_count_block: Slot::new_with_ctx(
                U256::from(24),
                LayoutCtx::packed(24),
                address,
            ),
            tokens_enabled_in_current_block: Slot::new_with_ctx(
                U256::from(25),
                LayoutCtx::packed(0),
                address,
            ),
            pause_expiry: Slot::new_with_ctx(U256::from(25), LayoutCtx::packed(8), address),
            token_enablement_hash: Slot::new(U256::from(26), address),
            abdication_effective_at: Mapping::new(U256::from(27), address),
            address,
        }
    }

    pub fn initialize(
        &mut self,
        zone_id: u32,
        params: &IZoneFactory::CreateZoneParams,
        token_enablement_hash: B256,
    ) -> Result<()> {
        if self.initialized.read()? {
            return Err(revert(IZoneFactory::AlreadyInitialized {}));
        }

        crate::tempo::precompile::StorageCtx.set_code(
            self.address,
            Bytecode::new_legacy(Bytes::from_static(&ZONE_PORTAL_PROXY_RUNTIME)),
        )?;
        self.admin.write(params.admin)?;
        self.token_configs[params.initialToken].write(PortalTokenConfig {
            enabled: true,
            deposits_active: true,
        })?;
        self.enabled_tokens.write(vec![params.initialToken])?;
        self.rpc_url.write(params.rpcUrl.clone())?;
        self.zone_id.write(zone_id)?;
        self.messenger.write(ZONE_MESSENGER_ADDRESS)?;
        self.verifier.write(ZONE_VERIFIER_ADDRESS)?;
        self.initialized.write(true)?;
        self.sequencer_threshold.write(params.threshold)?;
        self.sequencers.write(params.sequencers.clone())?;
        for sequencer in &params.sequencers {
            self.role[*sequencer].write(u8::from(IZonePortal::Role::Sequencer))?;
        }
        self.is_access_enforced.write(params.accessMode)?;
        self.is_gateway_enforced.write(params.gatewayMode)?;
        let leader = *params
            .sequencers
            .first()
            .ok_or_else(|| revert(IZoneFactory::InvalidSequencerSet {}))?;
        self.leader.write(leader)?;
        self.leader_epoch.write(1)?;
        let creation_block = crate::tempo::precompile::StorageCtx.block_number();
        self.leader_activation_tempo_block.write(creation_block)?;
        self.token_enable_count_block.write(creation_block)?;
        self.tokens_enabled_in_current_block.write(1)?;
        self.token_enablement_hash.write(token_enablement_hash)?;
        for gateway in &params.zoneGateways {
            self.role[*gateway].write(u8::from(IZonePortal::Role::CallbackGateway))?;
        }
        for account in &params.allowedAccounts {
            self.role[*account].write(u8::from(IZonePortal::Role::Account))?;
        }
        Ok(())
    }
}
