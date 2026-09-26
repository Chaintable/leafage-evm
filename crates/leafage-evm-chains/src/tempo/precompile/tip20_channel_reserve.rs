//! TIP-1034 TIP-20 channel reserve precompile (T5+).

use std::sync::LazyLock;

use alloy::primitives::{aliases::U96, keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolCall, SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::address_registry::AddressRegistry;
use super::error::{Result, TempoPrecompileError};
use super::signature_verifier::SignatureVerifier;
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_credits::StorageCredits;
use super::storage_types::{Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType};
use super::tip20::{is_tip20_prefix, Recipient, TIP20Token, ITIP20};
use super::tip403_registry::AuthRole;
use super::{
    dispatch_call, input_cost, metadata, mutate, mutate_void, unknown_selector, view, Precompile,
    TIP20_CHANNEL_RESERVE_ADDRESS,
};
use crate::tempo::address::TempoAddressExt;
use crate::tempo::hardfork::TempoHardfork;

pub const CLOSE_GRACE_PERIOD: u64 = 15 * 60;
const MAINNET_CHAIN_ID: u64 = 4217;
const MODERATO_CHAIN_ID: u64 = 42431;

static VOUCHER_TYPEHASH: LazyLock<B256> =
    LazyLock::new(|| keccak256(b"Voucher(bytes32 channelId,uint96 cumulativeAmount)"));
static EIP712_DOMAIN_TYPEHASH: LazyLock<B256> = LazyLock::new(|| {
    keccak256(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
});
static NAME_HASH: LazyLock<B256> = LazyLock::new(|| keccak256(b"TIP20 Channel Reserve"));
static VERSION_HASH: LazyLock<B256> = LazyLock::new(|| keccak256(b"1"));
static DOMAIN_SEPARATOR_MAINNET: LazyLock<B256> =
    LazyLock::new(|| domain_separator_inner(MAINNET_CHAIN_ID));
static DOMAIN_SEPARATOR_MODERATO: LazyLock<B256> =
    LazyLock::new(|| domain_separator_inner(MODERATO_CHAIN_ID));

pub use tempo_contracts::precompiles::ITIP20ChannelReserve;

#[inline]
fn revert(error: impl SolError) -> TempoPrecompileError {
    TempoPrecompileError::Revert(error.abi_encode().into())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PackedChannelState {
    settled: U96,
    deposit: U96,
    close_requested_at: u32,
    /// Whether TIP-1028 sees the logical payer during capture (TIP-1095, set by T12 opens).
    /// Channels created before T12 decode it as false and keep the reserve as sender.
    uses_logical_receive_policy_sender: bool,
}

impl PackedChannelState {
    fn exists(self) -> bool {
        !self.deposit.is_zero()
    }

    fn close_requested_at(self) -> Option<u32> {
        (self.close_requested_at != 0).then_some(self.close_requested_at)
    }

    fn receive_policy_sender(self, payer: Address) -> Address {
        if self.uses_logical_receive_policy_sender {
            payer
        } else {
            TIP20_CHANNEL_RESERVE_ADDRESS
        }
    }

    fn to_sol(self) -> ITIP20ChannelReserve::ChannelState {
        ITIP20ChannelReserve::ChannelState {
            settled: self.settled,
            deposit: self.deposit,
            closeRequestedAt: self.close_requested_at,
        }
    }
}

impl StorableType for PackedChannelState {
    const LAYOUT: Layout = Layout::Bytes(29);
    type Handler = Slot<Self>;

    fn handle(slot: U256, ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new_with_ctx(slot, ctx, address)
    }
}

impl Storable for PackedChannelState {
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self> {
        let word = match ctx.packed_offset() {
            Some(offset) => super::storage_types::packing::extract_from_word(
                storage.load(slot)?,
                offset,
                Self::BYTES,
            )?,
            None => storage.load(slot)?,
        };
        let bytes = word.to_be_bytes::<32>();
        Ok(Self {
            uses_logical_receive_policy_sender: bytes[3] != 0,
            close_requested_at: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            deposit: U96::from_be_slice(&bytes[8..20]),
            settled: U96::from_be_slice(&bytes[20..32]),
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
        bytes[3] = u8::from(self.uses_logical_receive_policy_sender);
        bytes[4..8].copy_from_slice(&self.close_requested_at.to_be_bytes());
        bytes[8..20].copy_from_slice(&self.deposit.to_be_bytes::<12>());
        bytes[20..32].copy_from_slice(&self.settled.to_be_bytes::<12>());
        let value = U256::from_be_bytes(bytes);
        match ctx.packed_offset() {
            Some(offset) => {
                let current = storage.load(slot)?;
                storage.store(
                    slot,
                    super::storage_types::packing::insert_into_word(
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

pub struct TIP20ChannelReserve {
    channel_states: Mapping<B256, PackedChannelState>,
    channel_storage_credits: Mapping<Address, u64>,
    opened_this_tx: Mapping<B256, bool>,
    channel_open_context_hash: Slot<B256>,
    address: Address,
    storage: StorageCtx,
}

impl TIP20ChannelReserve {
    pub fn new() -> Self {
        let address = TIP20_CHANNEL_RESERVE_ADDRESS;
        Self {
            channel_states: Mapping::new(U256::ZERO, address),
            channel_storage_credits: Mapping::new(U256::from(1), address),
            opened_this_tx: Mapping::new(U256::from(2), address),
            channel_open_context_hash: Slot::new(U256::from(3), address),
            address,
            storage: StorageCtx,
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        self.storage.set_code(
            self.address,
            revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef])),
        )
    }

    pub fn set_channel_open_context_hash(&mut self, hash: B256) -> Result<()> {
        self.channel_open_context_hash.t_write(hash)
    }

    pub fn storage_credits(&self, payer: Address) -> Result<u64> {
        self.channel_storage_credits[payer].read()
    }

    fn preserve_storage_credits(&mut self) -> Result<()> {
        if self.storage.spec().is_t7() {
            StorageCredits::new().preserve(self.address)?;
        }
        Ok(())
    }

    pub fn open(
        &mut self,
        msg_sender: Address,
        call: ITIP20ChannelReserve::openCall,
    ) -> Result<B256> {
        if call.payee.is_zero()
            || is_tip20_prefix(call.payee)
            || (call.payee.is_virtual() && (call.operator.is_zero() || call.operator.is_virtual()))
        {
            return Err(revert(ITIP20ChannelReserve::InvalidPayee {}));
        }
        let mut token = TIP20Token::from_address(call.token)?;
        if call.deposit.is_zero() {
            return Err(revert(ITIP20ChannelReserve::ZeroDeposit {}));
        }

        let context_hash = self.enclosing_channel_open_context_hash()?;
        let channel_id = self.compute_channel_id_inner(
            msg_sender,
            call.payee,
            call.operator,
            call.token,
            call.salt,
            call.authorizedSigner,
            context_hash,
        )?;
        if self.channel_states[channel_id].read()?.exists()
            || self.opened_this_tx[channel_id].t_read()?
        {
            return Err(revert(ITIP20ChannelReserve::ChannelAlreadyExists {}));
        }

        let payee = AddressRegistry::new().resolve_recipient(call.payee)?;
        if self.storage.spec().is_t12() {
            token.ensure_transfer_authorized(msg_sender, payee)?;
            token.ensure_receive_policy_authorized(msg_sender, payee)?;
            token.channel_reserve_transfer(
                msg_sender,
                Recipient::direct(self.address),
                U256::from(call.deposit),
                msg_sender,
            )?;
        } else {
            token.ensure_authorized_as(&[(payee, AuthRole::Recipient)])?;
            token.system_transfer_from(self.address, msg_sender, U256::from(call.deposit))?;
        }

        self.write_channel_state_spending_credit(
            msg_sender,
            channel_id,
            PackedChannelState {
                settled: U96::ZERO,
                deposit: call.deposit,
                close_requested_at: 0,
                uses_logical_receive_policy_sender: self.storage.spec().is_t12(),
            },
        )?;
        self.opened_this_tx[channel_id].t_write(true)?;
        self.emit_event(ITIP20ChannelReserve::ChannelOpened {
            channelId: channel_id,
            payer: msg_sender,
            payee: call.payee,
            operator: call.operator,
            token: call.token,
            authorizedSigner: call.authorizedSigner,
            salt: call.salt,
            expiringNonceHash: context_hash,
            deposit: call.deposit,
        })?;
        Ok(channel_id)
    }

    pub fn settle(
        &mut self,
        msg_sender: Address,
        call: ITIP20ChannelReserve::settleCall,
    ) -> Result<()> {
        let channel_id = self.channel_id(&call.descriptor)?;
        let mut state = self.load_existing_state(channel_id)?;
        Self::ensure_payee_or_operator(msg_sender, &call.descriptor)?;
        if call.cumulativeAmount > state.deposit {
            return Err(revert(ITIP20ChannelReserve::AmountExceedsDeposit {}));
        }
        if call.cumulativeAmount <= state.settled {
            return Err(revert(ITIP20ChannelReserve::AmountNotIncreasing {}));
        }
        self.validate_voucher(
            &call.descriptor,
            channel_id,
            call.cumulativeAmount,
            &call.signature,
        )?;

        let delta = call.cumulativeAmount.checked_sub(state.settled).unwrap();
        let mut token = TIP20Token::from_address(call.descriptor.token)?;
        if self.storage.spec().is_t12() {
            let payee = Recipient::resolve(call.descriptor.payee)?;
            token.ensure_transfer_authorized(call.descriptor.payer, payee.target)?;
            token.channel_reserve_transfer(
                self.address,
                payee,
                U256::from(delta),
                state.receive_policy_sender(call.descriptor.payer),
            )?;
            state.settled = call.cumulativeAmount;
            self.channel_states[channel_id].write(state)?;
        } else {
            token.ensure_authorized_as(&[(call.descriptor.payer, AuthRole::Sender)])?;
            // Pre-T12 writes the state before the transfer; the order is gas-visible.
            state.settled = call.cumulativeAmount;
            self.channel_states[channel_id].write(state)?;
            token.transfer(
                self.address,
                ITIP20::transferCall {
                    to: call.descriptor.payee,
                    amount: U256::from(delta),
                },
            )?;
        }
        self.emit_event(ITIP20ChannelReserve::Settled {
            channelId: channel_id,
            payer: call.descriptor.payer,
            payee: call.descriptor.payee,
            cumulativeAmount: call.cumulativeAmount,
            deltaPaid: delta,
            newSettled: call.cumulativeAmount,
        })
    }

    pub fn top_up(
        &mut self,
        msg_sender: Address,
        call: ITIP20ChannelReserve::topUpCall,
    ) -> Result<()> {
        let channel_id = self.channel_id(&call.descriptor)?;
        let mut state = self.load_existing_state(channel_id)?;
        if msg_sender != call.descriptor.payer {
            return Err(revert(ITIP20ChannelReserve::NotPayer {}));
        }
        let had_close_request = state.close_requested_at().is_some();
        if call.additionalDeposit.is_zero() && !had_close_request {
            return Ok(());
        }
        if !call.additionalDeposit.is_zero() {
            state.deposit = state
                .deposit
                .checked_add(call.additionalDeposit)
                .ok_or_else(|| revert(ITIP20ChannelReserve::DepositOverflow {}))?;
            if self.storage.spec().is_t12() {
                let mut token = TIP20Token::from_address(call.descriptor.token)?;
                let payee = AddressRegistry::new().resolve_recipient(call.descriptor.payee)?;
                token.ensure_transfer_authorized(msg_sender, payee)?;
                token.ensure_receive_policy_authorized(
                    state.receive_policy_sender(msg_sender),
                    payee,
                )?;
                token.channel_reserve_transfer(
                    msg_sender,
                    Recipient::direct(self.address),
                    U256::from(call.additionalDeposit),
                    msg_sender,
                )?;
            } else {
                let payee = AddressRegistry::new().resolve_recipient(call.descriptor.payee)?;
                let mut token = TIP20Token::from_address(call.descriptor.token)?;
                token.ensure_authorized_as(&[(payee, AuthRole::Recipient)])?;
                token.system_transfer_from(
                    self.address,
                    msg_sender,
                    U256::from(call.additionalDeposit),
                )?;
            }
        }
        if had_close_request {
            state.close_requested_at = 0;
        }
        self.channel_states[channel_id].write(state)?;
        if had_close_request {
            self.emit_event(ITIP20ChannelReserve::CloseRequestCancelled {
                channelId: channel_id,
                payer: call.descriptor.payer,
                payee: call.descriptor.payee,
            })?;
        }
        self.emit_event(ITIP20ChannelReserve::TopUp {
            channelId: channel_id,
            payer: call.descriptor.payer,
            payee: call.descriptor.payee,
            additionalDeposit: call.additionalDeposit,
            newDeposit: state.deposit,
        })
    }

    pub fn request_close(
        &mut self,
        msg_sender: Address,
        call: ITIP20ChannelReserve::requestCloseCall,
    ) -> Result<()> {
        let channel_id = self.channel_id(&call.descriptor)?;
        let mut state = self.load_existing_state(channel_id)?;
        if msg_sender != call.descriptor.payer {
            return Err(revert(ITIP20ChannelReserve::NotPayer {}));
        }
        if state.close_requested_at().is_some() {
            return Ok(());
        }
        state.close_requested_at = self.now_u32();
        self.channel_states[channel_id].write(state)?;
        self.emit_event(ITIP20ChannelReserve::CloseRequested {
            channelId: channel_id,
            payer: call.descriptor.payer,
            payee: call.descriptor.payee,
            closeGraceEnd: U256::from(self.now() + CLOSE_GRACE_PERIOD),
        })
    }

    pub fn close(
        &mut self,
        msg_sender: Address,
        call: ITIP20ChannelReserve::closeCall,
    ) -> Result<()> {
        let channel_id = self.channel_id(&call.descriptor)?;
        let state = self.load_existing_state(channel_id)?;
        Self::ensure_payee_or_operator(msg_sender, &call.descriptor)?;
        if call.captureAmount < state.settled || call.captureAmount > call.cumulativeAmount {
            return Err(revert(ITIP20ChannelReserve::CaptureAmountInvalid {}));
        }
        if call.captureAmount > state.deposit {
            return Err(revert(ITIP20ChannelReserve::AmountExceedsDeposit {}));
        }
        if call.captureAmount > state.settled {
            self.validate_voucher(
                &call.descriptor,
                channel_id,
                call.cumulativeAmount,
                &call.signature,
            )?;
        }
        let delta = call.captureAmount.checked_sub(state.settled).unwrap();
        let refund = state.deposit.checked_sub(call.captureAmount).unwrap();
        if self.storage.spec().is_t12() {
            let mut token = TIP20Token::from_address(call.descriptor.token)?;
            if !delta.is_zero() {
                let payee = Recipient::resolve(call.descriptor.payee)?;
                token.ensure_transfer_authorized(call.descriptor.payer, payee.target)?;
                token.channel_reserve_transfer(
                    self.address,
                    payee,
                    U256::from(delta),
                    state.receive_policy_sender(call.descriptor.payer),
                )?;
            }
            if !refund.is_zero() {
                token.channel_reserve_transfer(
                    self.address,
                    Recipient::resolve(call.descriptor.payer)?,
                    U256::from(refund),
                    self.address,
                )?;
            }
            // T12 deletes the channel only after both deliveries succeed, so a TIP-1028
            // rejection leaves it available for retry.
            self.delete_channel_state_and_credit_payer(channel_id, call.descriptor.payer)?;
        } else {
            self.delete_channel_state_and_credit_payer(channel_id, call.descriptor.payer)?;

            let mut token = TIP20Token::from_address(call.descriptor.token)?;
            if !delta.is_zero() {
                token.ensure_authorized_as(&[(call.descriptor.payer, AuthRole::Sender)])?;
                token.transfer(
                    self.address,
                    ITIP20::transferCall {
                        to: call.descriptor.payee,
                        amount: U256::from(delta),
                    },
                )?;
            }
            if !refund.is_zero() {
                token.transfer(
                    self.address,
                    ITIP20::transferCall {
                        to: call.descriptor.payer,
                        amount: U256::from(refund),
                    },
                )?;
            }
        }
        self.emit_event(ITIP20ChannelReserve::ChannelClosed {
            channelId: channel_id,
            payer: call.descriptor.payer,
            payee: call.descriptor.payee,
            settledToPayee: call.captureAmount,
            refundedToPayer: refund,
        })
    }

    pub fn withdraw(
        &mut self,
        msg_sender: Address,
        call: ITIP20ChannelReserve::withdrawCall,
    ) -> Result<()> {
        let channel_id = self.channel_id(&call.descriptor)?;
        let state = self.load_existing_state(channel_id)?;
        if msg_sender != call.descriptor.payer {
            return Err(revert(ITIP20ChannelReserve::NotPayer {}));
        }
        let close_ready = state
            .close_requested_at()
            .is_some_and(|at| self.now() >= u64::from(at) + CLOSE_GRACE_PERIOD);
        if !close_ready {
            return Err(revert(ITIP20ChannelReserve::CloseNotReady {}));
        }
        let refund = state.deposit.checked_sub(state.settled).unwrap();
        if self.storage.spec().is_t12() {
            if !refund.is_zero() {
                TIP20Token::from_address(call.descriptor.token)?.channel_reserve_transfer(
                    self.address,
                    Recipient::resolve(call.descriptor.payer)?,
                    U256::from(refund),
                    self.address,
                )?;
            }
            // T12 deletes the channel only after a nonzero refund succeeds.
            self.delete_channel_state_and_credit_payer(channel_id, call.descriptor.payer)?;
        } else {
            self.delete_channel_state_and_credit_payer(channel_id, call.descriptor.payer)?;
            if !refund.is_zero() {
                TIP20Token::from_address(call.descriptor.token)?.transfer(
                    self.address,
                    ITIP20::transferCall {
                        to: call.descriptor.payer,
                        amount: U256::from(refund),
                    },
                )?;
            }
        }
        self.emit_event(ITIP20ChannelReserve::ChannelClosed {
            channelId: channel_id,
            payer: call.descriptor.payer,
            payee: call.descriptor.payee,
            settledToPayee: state.settled,
            refundedToPayer: refund,
        })
    }

    pub fn get_channel(
        &self,
        call: ITIP20ChannelReserve::getChannelCall,
    ) -> Result<ITIP20ChannelReserve::Channel> {
        let channel_id = self.channel_id(&call.descriptor)?;
        Ok(ITIP20ChannelReserve::Channel {
            descriptor: call.descriptor,
            state: self.channel_states[channel_id].read()?.to_sol(),
        })
    }

    pub fn get_channel_state(
        &self,
        call: ITIP20ChannelReserve::getChannelStateCall,
    ) -> Result<ITIP20ChannelReserve::ChannelState> {
        Ok(self.channel_states[call.channelId].read()?.to_sol())
    }

    pub fn get_channel_states_batch(
        &self,
        call: ITIP20ChannelReserve::getChannelStatesBatchCall,
    ) -> Result<Vec<ITIP20ChannelReserve::ChannelState>> {
        call.channelIds
            .into_iter()
            .map(|id| {
                self.channel_states[id]
                    .read()
                    .map(PackedChannelState::to_sol)
            })
            .collect()
    }

    pub fn compute_channel_id(
        &self,
        call: ITIP20ChannelReserve::computeChannelIdCall,
    ) -> Result<B256> {
        self.compute_channel_id_inner(
            call.payer,
            call.payee,
            call.operator,
            call.token,
            call.salt,
            call.authorizedSigner,
            call.expiringNonceHash,
        )
    }

    pub fn get_voucher_digest(
        &self,
        call: ITIP20ChannelReserve::getVoucherDigestCall,
    ) -> Result<B256> {
        self.get_voucher_digest_inner(call.channelId, call.cumulativeAmount)
    }

    pub fn domain_separator(&self) -> Result<B256> {
        Ok(match self.storage.chain_id() {
            MAINNET_CHAIN_ID => *DOMAIN_SEPARATOR_MAINNET,
            MODERATO_CHAIN_ID => *DOMAIN_SEPARATOR_MODERATO,
            chain_id => domain_separator_inner(chain_id),
        })
    }

    fn delete_channel_state_and_credit_payer(
        &mut self,
        channel_id: B256,
        payer: Address,
    ) -> Result<()> {
        let (_, credits) = StorageCredits::new()
            .track_minted_credits(self.address, || self.channel_states[channel_id].delete())?;
        self.credit_channel_storage_slots(payer, credits)
    }

    fn credit_channel_storage_slots(&mut self, payer: Address, slots: u64) -> Result<()> {
        if slots == 0 {
            return Ok(());
        }

        let current = self.channel_storage_credits[payer].read()?;
        let updated = current.saturating_add(slots);
        if current == 0 {
            let (_, delta) = StorageCredits::new().with_budget(self.address, 1, || {
                self.channel_storage_credits[payer].write(updated)
            })?;
            if delta != -1 {
                return Err(TempoPrecompileError::Fatal(format!(
                    "channel storage credit bookkeeping spend mismatch: {delta}"
                )));
            }
            Ok(())
        } else {
            self.channel_storage_credits[payer].write(updated)
        }
    }

    fn write_channel_state_spending_credit(
        &mut self,
        payer: Address,
        channel_id: B256,
        state: PackedChannelState,
    ) -> Result<()> {
        if !self.storage.spec().is_t7() {
            return self.channel_states[channel_id].write(state);
        }

        let current = self.channel_storage_credits[payer].read()?;
        if current == 0 {
            return self.channel_states[channel_id].write(state);
        }

        self.channel_storage_credits[payer].delete()?;
        let (_, delta) = StorageCredits::new().with_budget(self.address, current, || {
            self.channel_states[channel_id].write(state)
        })?;
        let spent = delta.checked_neg().unwrap_or_default() as u64;
        if spent != 1 {
            return Err(TempoPrecompileError::Fatal(format!(
                "channel storage credit spend mismatch: {spent}"
            )));
        }
        self.credit_channel_storage_slots(payer, current.saturating_sub(spent))
    }

    fn now(&self) -> u64 {
        self.storage.timestamp().saturating_to::<u64>()
    }

    fn now_u32(&self) -> u32 {
        self.storage.timestamp().saturating_to::<u32>()
    }

    fn channel_id(&self, descriptor: &ITIP20ChannelReserve::ChannelDescriptor) -> Result<B256> {
        self.compute_channel_id_inner(
            descriptor.payer,
            descriptor.payee,
            descriptor.operator,
            descriptor.token,
            descriptor.salt,
            descriptor.authorizedSigner,
            descriptor.expiringNonceHash,
        )
    }

    fn ensure_payee_or_operator(
        sender: Address,
        descriptor: &ITIP20ChannelReserve::ChannelDescriptor,
    ) -> Result<()> {
        if sender != descriptor.payee
            && (descriptor.operator.is_zero() || sender != descriptor.operator)
        {
            return Err(revert(ITIP20ChannelReserve::NotPayeeOrOperator {}));
        }
        Ok(())
    }

    fn enclosing_channel_open_context_hash(&self) -> Result<B256> {
        let hash = self.channel_open_context_hash.t_read()?;
        if hash.is_zero() {
            return Err(revert(ITIP20ChannelReserve::ExpiringNonceHashNotSet {}));
        }
        Ok(hash)
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_channel_id_inner(
        &self,
        payer: Address,
        payee: Address,
        operator: Address,
        token: Address,
        salt: B256,
        authorized_signer: Address,
        context_hash: B256,
    ) -> Result<B256> {
        self.storage.keccak256(
            &(
                payer,
                payee,
                operator,
                token,
                salt,
                authorized_signer,
                context_hash,
                self.address,
                U256::from(self.storage.chain_id()),
            )
                .abi_encode(),
        )
    }

    fn load_existing_state(&self, channel_id: B256) -> Result<PackedChannelState> {
        let state = self.channel_states[channel_id].read()?;
        if !state.exists() {
            return Err(revert(ITIP20ChannelReserve::ChannelNotFound {}));
        }
        Ok(state)
    }

    fn expected_signer(&self, descriptor: &ITIP20ChannelReserve::ChannelDescriptor) -> Address {
        if descriptor.authorizedSigner.is_zero() {
            descriptor.payer
        } else {
            descriptor.authorizedSigner
        }
    }

    fn validate_voucher(
        &self,
        descriptor: &ITIP20ChannelReserve::ChannelDescriptor,
        channel_id: B256,
        cumulative_amount: U96,
        signature: &Bytes,
    ) -> Result<()> {
        let digest = self.get_voucher_digest_inner(channel_id, cumulative_amount)?;
        let signer = SignatureVerifier::new()
            .recover(digest, signature.clone())
            .map_err(|_| revert(ITIP20ChannelReserve::InvalidSignature {}))?;
        if signer != self.expected_signer(descriptor) {
            return Err(revert(ITIP20ChannelReserve::InvalidSignature {}));
        }
        Ok(())
    }

    fn get_voucher_digest_inner(&self, channel_id: B256, amount: U96) -> Result<B256> {
        let struct_hash = self
            .storage
            .keccak256(&(*VOUCHER_TYPEHASH, channel_id, amount).abi_encode())?;
        let domain_separator = self.domain_separator()?;
        let mut input = [0u8; 66];
        input[..2].copy_from_slice(&[0x19, 0x01]);
        input[2..34].copy_from_slice(domain_separator.as_slice());
        input[34..].copy_from_slice(struct_hash.as_slice());
        self.storage.keccak256(&input)
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }
}

impl ContractStorage for TIP20ChannelReserve {
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

/// Selectors gated by `#[schedule(since = ...)]` in official `tip20_channel_reserve/dispatch.rs`.
/// Checked before ABI decode, so they return `UnknownFunctionSelector` before activation.
const SCHEDULED_SELECTORS: &[([u8; 4], TempoHardfork)] = &[(
    ITIP20ChannelReserve::storageCreditsCall::SELECTOR,
    TempoHardfork::T7,
)];

impl Precompile for TIP20ChannelReserve {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if !self.storage.spec().is_t5() {
            let selector = calldata
                .get(..4)
                .and_then(|s| s.try_into().ok())
                .unwrap_or([0; 4]);
            return unknown_selector(selector, 0);
        }
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        let spec = self.storage.spec();
        dispatch_call(
            calldata,
            |selector| {
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::valid_selector(selector)
                    && SCHEDULED_SELECTORS
                        .iter()
                        .all(|&(gated, since)| gated != selector || spec >= since)
            },
            |data| {
                super::decode_precompile_call::<ITIP20ChannelReserve::ITIP20ChannelReserveCalls>(
                    data,
                    StorageCtx.spec(),
                )
            },
            |call| match call {
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::CLOSE_GRACE_PERIOD(_) => {
                    metadata::<ITIP20ChannelReserve::CLOSE_GRACE_PERIODCall>(|| {
                        Ok(CLOSE_GRACE_PERIOD)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::VOUCHER_TYPEHASH(_) => {
                    metadata::<ITIP20ChannelReserve::VOUCHER_TYPEHASHCall>(|| Ok(*VOUCHER_TYPEHASH))
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::open(call) => {
                    mutate(call, msg_sender, |sender, call| {
                        self.preserve_storage_credits()?;
                        self.open(sender, call)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::settle(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.preserve_storage_credits()?;
                        self.settle(sender, call)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::topUp(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.preserve_storage_credits()?;
                        self.top_up(sender, call)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::close(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.preserve_storage_credits()?;
                        self.close(sender, call)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::requestClose(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.preserve_storage_credits()?;
                        self.request_close(sender, call)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::withdraw(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.preserve_storage_credits()?;
                        self.withdraw(sender, call)
                    })
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::getChannel(call) => {
                    view(call, |call| self.get_channel(call))
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::getChannelState(call) => {
                    view(call, |call| self.get_channel_state(call))
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::getChannelStatesBatch(call) => {
                    view(call, |call| self.get_channel_states_batch(call))
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::computeChannelId(call) => {
                    view(call, |call| self.compute_channel_id(call))
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::getVoucherDigest(call) => {
                    view(call, |call| self.get_voucher_digest(call))
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::domainSeparator(call) => {
                    view(call, |_| self.domain_separator())
                }
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::storageCredits(call) => {
                    view(call, |call| self.storage_credits(call.payer))
                }
            },
        )
    }
}

fn domain_separator_inner(chain_id: u64) -> B256 {
    keccak256(
        (
            *EIP712_DOMAIN_TYPEHASH,
            *NAME_HASH,
            *VERSION_HASH,
            U256::from(chain_id),
            TIP20_CHANNEL_RESERVE_ADDRESS,
        )
            .abi_encode(),
    )
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::test_utils::TestStorageProvider;
    use crate::tempo::precompile::tip20::{IRolesAuth, ISSUER_ROLE};

    #[test]
    fn packed_channel_state_layout_matches_writer() {
        let state = PackedChannelState {
            settled: U96::from(0x11),
            deposit: U96::from(0x22),
            close_requested_at: 0x33445566,
            uses_logical_receive_policy_sender: false,
        };
        let mut word = super::super::storage_types::packing::PackedSlot(U256::ZERO);
        state.store(&mut word, U256::ZERO, LayoutCtx::FULL).unwrap();
        let bytes = word.0.to_be_bytes::<32>();
        assert_eq!(&bytes[..4], &[0; 4]);
        assert_eq!(&bytes[4..8], &0x33445566u32.to_be_bytes());
        assert_eq!(U96::from_be_slice(&bytes[8..20]), U96::from(0x22));
        assert_eq!(U96::from_be_slice(&bytes[20..]), U96::from(0x11));

        // TIP-1095 (T12): the flag is the next packed field, one byte after close_requested_at.
        let state = PackedChannelState {
            uses_logical_receive_policy_sender: true,
            ..state
        };
        let mut word = super::super::storage_types::packing::PackedSlot(U256::ZERO);
        state.store(&mut word, U256::ZERO, LayoutCtx::FULL).unwrap();
        let bytes = word.0.to_be_bytes::<32>();
        assert_eq!(&bytes[..4], &[0, 0, 0, 1]);
        assert_eq!(&bytes[4..8], &0x33445566u32.to_be_bytes());
        assert_eq!(
            PackedChannelState::load(&word, U256::ZERO, LayoutCtx::FULL).unwrap(),
            state
        );
    }

    #[test]
    fn channel_id_matches_explicit_abi_formula() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let call = ITIP20ChannelReserve::computeChannelIdCall {
            payer: address!("0x1111111111111111111111111111111111111111"),
            payee: address!("0x2222222222222222222222222222222222222222"),
            operator: Address::ZERO,
            token: super::super::PATH_USD_ADDRESS,
            salt: B256::repeat_byte(3),
            authorizedSigner: Address::ZERO,
            expiringNonceHash: B256::repeat_byte(4),
        };
        let expected = keccak256(
            &(
                call.payer,
                call.payee,
                call.operator,
                call.token,
                call.salt,
                call.authorizedSigner,
                call.expiringNonceHash,
                TIP20_CHANNEL_RESERVE_ADDRESS,
                U256::from(1),
            )
                .abi_encode(),
        );

        let actual = StorageCtx::enter(&mut provider, || {
            TIP20ChannelReserve::new().compute_channel_id(call)
        })
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn open_requires_transaction_context_hash() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let call = ITIP20ChannelReserve::openCall {
            payee: Address::repeat_byte(2),
            operator: Address::ZERO,
            token: super::super::PATH_USD_ADDRESS,
            deposit: U96::ONE,
            salt: B256::ZERO,
            authorizedSigner: Address::ZERO,
        };

        let result = StorageCtx::enter(&mut provider, || {
            TIP20ChannelReserve::new().open(Address::repeat_byte(1), call)
        });
        assert!(matches!(result, Err(TempoPrecompileError::Revert(_))));
    }

    #[test]
    fn open_validates_token_right_after_payee() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let call = ITIP20ChannelReserve::openCall {
            payee: Address::repeat_byte(2),
            operator: Address::ZERO,
            token: Address::repeat_byte(3),
            deposit: U96::ZERO,
            salt: B256::ZERO,
            authorizedSigner: Address::ZERO,
        };

        let result = StorageCtx::enter(&mut provider, || {
            TIP20ChannelReserve::new().open(Address::repeat_byte(1), call)
        });
        assert_eq!(
            result,
            Err(TempoPrecompileError::Revert(
                ITIP20::InvalidToken {}.abi_encode().into()
            ))
        );
    }

    #[test]
    fn activation_gate_is_t5() {
        let call = ITIP20ChannelReserve::CLOSE_GRACE_PERIODCall {};
        let mut provider = TestStorageProvider::new(TempoHardfork::T4);
        let output = StorageCtx::enter(&mut provider, || {
            TIP20ChannelReserve::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(output.reverted);
    }

    #[test]
    fn open_and_payee_close_refunds_deposit_and_blocks_same_tx_reopen() {
        let payer = Address::repeat_byte(0x11);
        let payee = Address::repeat_byte(0x22);
        let context_hash = B256::repeat_byte(0x33);
        let deposit = U96::from(100);
        let open_call = ITIP20ChannelReserve::openCall {
            payee,
            operator: Address::ZERO,
            token: super::super::PATH_USD_ADDRESS,
            deposit,
            salt: B256::repeat_byte(0x44),
            authorizedSigner: Address::ZERO,
        };
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);

        StorageCtx::enter(&mut provider, || {
            let mut token = TIP20Token::from_address_unchecked(super::super::PATH_USD_ADDRESS);
            token.initialize(
                Address::ZERO,
                "Path USD",
                "pathUSD",
                "USD",
                super::super::PATH_USD_ADDRESS,
                payer,
            )?;
            token.grant_role(
                payer,
                IRolesAuth::grantRoleCall {
                    role: *ISSUER_ROLE,
                    account: payer,
                },
            )?;
            token.mint(
                payer,
                ITIP20::mintCall {
                    to: payer,
                    amount: U256::from(deposit),
                },
            )?;

            let mut reserve = TIP20ChannelReserve::new();
            reserve.set_channel_open_context_hash(context_hash)?;
            let channel_id = reserve.open(payer, open_call.clone())?;
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall { account: payer })?,
                U256::ZERO,
            );
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall {
                    account: TIP20_CHANNEL_RESERVE_ADDRESS,
                })?,
                U256::from(deposit),
            );

            let descriptor = ITIP20ChannelReserve::ChannelDescriptor {
                payer,
                payee,
                operator: open_call.operator,
                token: open_call.token,
                salt: open_call.salt,
                authorizedSigner: open_call.authorizedSigner,
                expiringNonceHash: context_hash,
            };
            assert_eq!(
                reserve
                    .get_channel_state(ITIP20ChannelReserve::getChannelStateCall {
                        channelId: channel_id,
                    })?
                    .deposit,
                deposit,
            );
            reserve.close(
                payee,
                ITIP20ChannelReserve::closeCall {
                    descriptor,
                    cumulativeAmount: U96::ZERO,
                    captureAmount: U96::ZERO,
                    signature: Bytes::new(),
                },
            )?;
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall { account: payer })?,
                U256::from(deposit),
            );
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall {
                    account: TIP20_CHANNEL_RESERVE_ADDRESS,
                })?,
                U256::ZERO,
            );

            let reopened = reserve.open(payer, open_call);
            assert!(matches!(reopened, Err(TempoPrecompileError::Revert(_))));
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn t7_closed_channel_credit_is_reused_for_new_channel() {
        let payer = Address::repeat_byte(0x51);
        let payee = Address::repeat_byte(0x52);
        let first_context = B256::repeat_byte(0x53);
        let second_context = B256::repeat_byte(0x54);
        let deposit = U96::from(100);
        let open_call = ITIP20ChannelReserve::openCall {
            payee,
            operator: Address::ZERO,
            token: super::super::PATH_USD_ADDRESS,
            deposit,
            salt: B256::repeat_byte(0x55),
            authorizedSigner: Address::ZERO,
        };
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);

        StorageCtx::enter(&mut provider, || {
            let mut token = TIP20Token::from_address_unchecked(super::super::PATH_USD_ADDRESS);
            token.initialize(
                Address::ZERO,
                "Path USD",
                "pathUSD",
                "USD",
                super::super::PATH_USD_ADDRESS,
                payer,
            )?;
            token.grant_role(
                payer,
                IRolesAuth::grantRoleCall {
                    role: *ISSUER_ROLE,
                    account: payer,
                },
            )?;
            token.mint(
                payer,
                ITIP20::mintCall {
                    to: payer,
                    amount: U256::from(deposit),
                },
            )?;

            let mut reserve = TIP20ChannelReserve::new();
            reserve.set_channel_open_context_hash(first_context)?;
            let channel_id = reserve.open(payer, open_call.clone())?;
            reserve.close(
                payee,
                ITIP20ChannelReserve::closeCall {
                    descriptor: ITIP20ChannelReserve::ChannelDescriptor {
                        payer,
                        payee,
                        operator: open_call.operator,
                        token: open_call.token,
                        salt: open_call.salt,
                        authorizedSigner: open_call.authorizedSigner,
                        expiringNonceHash: first_context,
                    },
                    cumulativeAmount: U96::ZERO,
                    captureAmount: U96::ZERO,
                    signature: Bytes::new(),
                },
            )?;
            assert_eq!(reserve.storage_credits(payer)?, 1);

            reserve.set_channel_open_context_hash(second_context)?;
            let reopened = reserve.open(payer, open_call)?;
            assert_ne!(reopened, channel_id);
            assert_eq!(reserve.storage_credits(payer)?, 0);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn storage_credits_selector_rejects_malformed_calldata_before_t7() {
        let selector = ITIP20ChannelReserve::storageCreditsCall::SELECTOR;
        let calldata = [selector.as_slice(), &[0xff; 10]].concat();
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);
        let output = StorageCtx::enter(&mut provider, || {
            TIP20ChannelReserve::new().call(&calldata, Address::ZERO)
        })
        .unwrap();
        assert!(output.reverted);
        assert_eq!(
            output.bytes.as_ref(),
            super::super::UnknownFunctionSelector {
                selector: selector.into(),
            }
            .abi_encode()
        );
    }

    // TIP-1095 (T12) tests ported from tempo `tip20_channel_reserve/mod.rs` (upstream 402e2722).

    use crate::tempo::precompile::tip20::PAUSE_ROLE;
    use crate::tempo::precompile::tip403_registry::{
        ITIP403Registry, TIP403Registry, ALLOW_ALL_POLICY_ID,
    };
    use crate::tempo::precompile::RECEIVE_POLICY_GUARD_ADDRESS;

    fn policy_forbids() -> TempoPrecompileError {
        TempoPrecompileError::Revert(ITIP20::PolicyForbids {}.abi_encode().into())
    }

    fn path_usd() -> TIP20Token {
        TIP20Token::from_address_unchecked(super::super::PATH_USD_ADDRESS)
    }

    fn balance(token: &TIP20Token, account: Address) -> Result<U256> {
        token.balance_of(ITIP20::balanceOfCall { account })
    }

    /// Initializes pathUSD with `admin` as admin and issuer, minting `amount` to `admin`.
    fn setup_path_usd(admin: Address, amount: u128, roles: &[B256]) -> Result<TIP20Token> {
        let mut token = path_usd();
        token.initialize(
            Address::ZERO,
            "Path USD",
            "pathUSD",
            "USD",
            super::super::PATH_USD_ADDRESS,
            admin,
        )?;
        for &role in [*ISSUER_ROLE].iter().chain(roles) {
            token.grant_role(
                admin,
                IRolesAuth::grantRoleCall {
                    role,
                    account: admin,
                },
            )?;
        }
        token.mint(
            admin,
            ITIP20::mintCall {
                to: admin,
                amount: U256::from(amount),
            },
        )?;
        Ok(token)
    }

    /// Opens a pathUSD channel without operator or authorized signer.
    fn open_channel(
        reserve: &mut TIP20ChannelReserve,
        payer: Address,
        payee: Address,
        deposit: u128,
        salt: B256,
        context_hash: B256,
    ) -> Result<(B256, ITIP20ChannelReserve::ChannelDescriptor)> {
        reserve.set_channel_open_context_hash(context_hash)?;
        let channel_id = reserve.open(
            payer,
            ITIP20ChannelReserve::openCall {
                payee,
                operator: Address::ZERO,
                token: super::super::PATH_USD_ADDRESS,
                deposit: U96::from(deposit),
                salt,
                authorizedSigner: Address::ZERO,
            },
        )?;
        let descriptor = ITIP20ChannelReserve::ChannelDescriptor {
            payer,
            payee,
            operator: Address::ZERO,
            token: super::super::PATH_USD_ADDRESS,
            salt,
            authorizedSigner: Address::ZERO,
            expiringNonceHash: context_hash,
        };
        Ok((channel_id, descriptor))
    }

    fn sign_voucher(
        reserve: &TIP20ChannelReserve,
        signer: &k256::ecdsa::SigningKey,
        channel_id: B256,
        amount: U96,
    ) -> Result<Bytes> {
        let digest = reserve.get_voucher_digest(ITIP20ChannelReserve::getVoucherDigestCall {
            channelId: channel_id,
            cumulativeAmount: amount,
        })?;
        let signature: alloy::primitives::Signature = signer
            .sign_prehash_recoverable(digest.as_slice())
            .unwrap()
            .into();
        Ok(Bytes::copy_from_slice(&signature.as_bytes()))
    }

    fn channel_state(
        reserve: &TIP20ChannelReserve,
        channel_id: B256,
    ) -> Result<ITIP20ChannelReserve::ChannelState> {
        reserve.get_channel_state(ITIP20ChannelReserve::getChannelStateCall {
            channelId: channel_id,
        })
    }

    fn set_blacklisted(
        registry: &mut TIP403Registry,
        admin: Address,
        policy_id: u64,
        account: Address,
        restricted: bool,
    ) -> Result<()> {
        registry.modify_policy_blacklist(
            admin,
            ITIP403Registry::modifyPolicyBlacklistCall {
                policyId: policy_id,
                account,
                restricted,
            },
        )
    }

    fn install_recipient_whitelist_policy(
        token: &mut TIP20Token,
        admin: Address,
        recipients: &[Address],
    ) -> Result<()> {
        let mut registry = TIP403Registry::new();
        registry.initialize()?;
        let recipient_policy = registry.create_policy_with_accounts(
            admin,
            ITIP403Registry::createPolicyWithAccountsCall {
                admin,
                policyType: ITIP403Registry::PolicyType::WHITELIST,
                accounts: recipients.to_vec(),
            },
        )?;
        let compound_policy = registry.create_compound_policy(
            admin,
            ITIP403Registry::createCompoundPolicyCall {
                senderPolicyId: ALLOW_ALL_POLICY_ID,
                recipientPolicyId: recipient_policy,
                mintRecipientPolicyId: ALLOW_ALL_POLICY_ID,
            },
        )?;
        token.change_transfer_policy_id(
            admin,
            ITIP20::changeTransferPolicyIdCall {
                newPolicyId: compound_policy,
            },
        )
    }

    fn install_receive_sender_blacklist(
        receiver: Address,
        blocked_sender: Address,
    ) -> Result<(TIP403Registry, u64)> {
        let mut registry = TIP403Registry::new();
        registry.initialize()?;
        let sender_policy = registry.create_policy_with_accounts(
            receiver,
            ITIP403Registry::createPolicyWithAccountsCall {
                admin: receiver,
                policyType: ITIP403Registry::PolicyType::BLACKLIST,
                accounts: vec![blocked_sender],
            },
        )?;
        registry.set_receive_policy(
            receiver,
            ITIP403Registry::setReceivePolicyCall {
                senderPolicyId: sender_policy,
                tokenFilterId: ALLOW_ALL_POLICY_ID,
                recoveryAuthority: Address::ZERO,
            },
        )?;
        Ok((registry, sender_policy))
    }

    #[test]
    fn t12_recipient_restricted_token_supports_channel_capture_and_refund() -> Result<()> {
        let signer = k256::ecdsa::SigningKey::from_slice(&[0x11; 32]).unwrap();
        let payer = Address::from_public_key(signer.verifying_key());
        let payee = Address::repeat_byte(0x22);
        let stranger = Address::repeat_byte(0x33);
        let mut provider = TestStorageProvider::new(TempoHardfork::T12);

        StorageCtx::enter(&mut provider, || {
            let mut token = setup_path_usd(payer, 1_000, &[])?;
            install_recipient_whitelist_policy(&mut token, payer, &[payee])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;

            // The reserve is not an authorized recipient, but the logical payer -> payee path is.
            let (channel_id, descriptor) = open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            )?;
            reserve.top_up(
                payer,
                ITIP20ChannelReserve::topUpCall {
                    descriptor: descriptor.clone(),
                    additionalDeposit: U96::from(20),
                },
            )?;
            let signature = sign_voucher(&reserve, &signer, channel_id, U96::from(40))?;
            reserve.settle(
                payee,
                ITIP20ChannelReserve::settleCall {
                    descriptor: descriptor.clone(),
                    cumulativeAmount: U96::from(40),
                    signature,
                },
            )?;

            // Exercise a positive capture delta and a refund in the same close.
            let signature = sign_voucher(&reserve, &signer, channel_id, U96::from(60))?;
            reserve.close(
                payee,
                ITIP20ChannelReserve::closeCall {
                    descriptor,
                    cumulativeAmount: U96::from(60),
                    captureAmount: U96::from(60),
                    signature,
                },
            )?;

            assert_eq!(balance(&token, payer)?, U256::from(940));
            assert_eq!(balance(&token, payee)?, U256::from(60));
            assert_eq!(balance(&token, TIP20_CHANNEL_RESERVE_ADDRESS)?, U256::ZERO);

            // The channel path does not weaken ordinary transfer restrictions.
            let result = token.transfer(
                payer,
                ITIP20::transferCall {
                    to: stranger,
                    amount: U256::ONE,
                },
            );
            assert_eq!(result, Err(policy_forbids()));

            // A caller cannot use the reserve to route value to an unauthorized payee.
            let result = open_channel(
                &mut reserve,
                payer,
                stranger,
                1,
                B256::repeat_byte(3),
                B256::repeat_byte(4),
            );
            assert_eq!(result.unwrap_err(), policy_forbids());
            Ok(())
        })
    }

    #[test]
    fn t12_funding_checks_payee_receive_policy() -> Result<()> {
        let payer = Address::repeat_byte(0x11);
        let payee = Address::repeat_byte(0x22);
        let salt = B256::repeat_byte(1);
        let mut provider = TestStorageProvider::new(TempoHardfork::T12);

        StorageCtx::enter(&mut provider, || {
            let token = setup_path_usd(payer, 1_000, &[])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            let (mut registry, sender_policy) = install_receive_sender_blacklist(payee, payer)?;

            let result = open_channel(&mut reserve, payer, payee, 100, salt, B256::repeat_byte(2));
            assert_eq!(result.unwrap_err(), policy_forbids());
            assert_eq!(balance(&token, payer)?, U256::from(1_000));

            set_blacklisted(&mut registry, payee, sender_policy, payer, false)?;
            let (channel_id, descriptor) =
                open_channel(&mut reserve, payer, payee, 100, salt, B256::repeat_byte(3))?;

            set_blacklisted(&mut registry, payee, sender_policy, payer, true)?;
            let result = reserve.top_up(
                payer,
                ITIP20ChannelReserve::topUpCall {
                    descriptor,
                    additionalDeposit: U96::from(1),
                },
            );
            assert_eq!(result, Err(policy_forbids()));
            assert_eq!(channel_state(&reserve, channel_id)?.deposit, U96::from(100));
            Ok(())
        })
    }

    #[test]
    fn t12_grandfathers_pre_t12_receive_policy_sender() -> Result<()> {
        assert_eq!(<PackedChannelState as StorableType>::SLOTS, 1);

        let signer = k256::ecdsa::SigningKey::from_slice(&[0x11; 32]).unwrap();
        let payer = Address::from_public_key(signer.verifying_key());
        let payee = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T11);

        let (channel_id, descriptor) = StorageCtx::enter(&mut provider, || {
            setup_path_usd(payer, 1_000, &[])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            // This payee accepts the reserve but not the individual payer, matching the policy
            // under which pre-T12 channel captures were funded.
            install_receive_sender_blacklist(payee, payer)?;
            open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            )
        })?;

        provider.set_spec(TempoHardfork::T12);
        StorageCtx::enter(&mut provider, || {
            let token = path_usd();
            let mut reserve = TIP20ChannelReserve::new();

            // Post-activation top-ups and captures retain the reserve sender for this channel.
            reserve.top_up(
                payer,
                ITIP20ChannelReserve::topUpCall {
                    descriptor: descriptor.clone(),
                    additionalDeposit: U96::from(20),
                },
            )?;
            let signature = sign_voucher(&reserve, &signer, channel_id, U96::from(40))?;
            reserve.settle(
                payee,
                ITIP20ChannelReserve::settleCall {
                    descriptor: descriptor.clone(),
                    cumulativeAmount: U96::from(40),
                    signature,
                },
            )?;
            let signature = sign_voucher(&reserve, &signer, channel_id, U96::from(60))?;
            reserve.close(
                payee,
                ITIP20ChannelReserve::closeCall {
                    descriptor,
                    cumulativeAmount: U96::from(60),
                    captureAmount: U96::from(60),
                    signature,
                },
            )?;

            assert_eq!(balance(&token, payer)?, U256::from(940));
            assert_eq!(balance(&token, payee)?, U256::from(60));
            assert_eq!(balance(&token, TIP20_CHANNEL_RESERVE_ADDRESS)?, U256::ZERO);
            Ok(())
        })
    }

    #[test]
    fn t12_blocked_capture_reverts_and_remains_retryable() -> Result<()> {
        let signer = k256::ecdsa::SigningKey::from_slice(&[0x11; 32]).unwrap();
        let payer = Address::from_public_key(signer.verifying_key());
        let payee = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T12);

        StorageCtx::enter(&mut provider, || {
            let token = setup_path_usd(payer, 1_000, &[])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            let (channel_id, descriptor) = open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            )?;

            // Receive policies remain mutable after funding. The payee now rejects only the
            // logical payer, while the physical reserve sender remains authorized.
            let (mut registry, sender_policy) = install_receive_sender_blacklist(payee, payer)?;

            let cumulative = U96::from(40);
            let signature = sign_voucher(&reserve, &signer, channel_id, cumulative)?;
            let result = reserve.settle(
                payee,
                ITIP20ChannelReserve::settleCall {
                    descriptor: descriptor.clone(),
                    cumulativeAmount: cumulative,
                    signature: signature.clone(),
                },
            );
            assert_eq!(result, Err(policy_forbids()));
            assert_eq!(balance(&token, payer)?, U256::from(900));
            assert_eq!(balance(&token, payee)?, U256::ZERO);
            assert_eq!(
                balance(&token, TIP20_CHANNEL_RESERVE_ADDRESS)?,
                U256::from(100)
            );
            assert_eq!(balance(&token, RECEIVE_POLICY_GUARD_ADDRESS)?, U256::ZERO);
            assert_eq!(channel_state(&reserve, channel_id)?.settled, U96::ZERO);

            let result = reserve.close(
                payee,
                ITIP20ChannelReserve::closeCall {
                    descriptor: descriptor.clone(),
                    cumulativeAmount: cumulative,
                    captureAmount: cumulative,
                    signature: signature.clone(),
                },
            );
            assert_eq!(result, Err(policy_forbids()));
            let state = channel_state(&reserve, channel_id)?;
            assert_eq!(state.deposit, U96::from(100));
            assert_eq!(state.settled, U96::ZERO);

            // Once the payee accepts the logical payer, the same voucher can settle.
            set_blacklisted(&mut registry, payee, sender_policy, payer, false)?;
            reserve.settle(
                payee,
                ITIP20ChannelReserve::settleCall {
                    descriptor,
                    cumulativeAmount: cumulative,
                    signature,
                },
            )?;
            assert_eq!(balance(&token, payee)?, U256::from(40));
            assert_eq!(channel_state(&reserve, channel_id)?.settled, cumulative);
            Ok(())
        })
    }

    #[test]
    fn t12_recipient_restricted_token_supports_unilateral_withdraw() -> Result<()> {
        let payer = Address::repeat_byte(0x11);
        let payee = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T12);
        provider.set_timestamp(U256::from(1_000u64));

        let descriptor = StorageCtx::enter(&mut provider, || {
            let mut token = setup_path_usd(payer, 100, &[])?;
            install_recipient_whitelist_policy(&mut token, payer, &[payee])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            let (_, descriptor) = open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            )?;
            reserve.request_close(
                payer,
                ITIP20ChannelReserve::requestCloseCall {
                    descriptor: descriptor.clone(),
                },
            )?;
            Result::<_>::Ok(descriptor)
        })?;

        provider.set_timestamp(U256::from(1_000u64 + CLOSE_GRACE_PERIOD));
        StorageCtx::enter(&mut provider, || {
            let token = path_usd();
            TIP20ChannelReserve::new()
                .withdraw(payer, ITIP20ChannelReserve::withdrawCall { descriptor })?;
            assert_eq!(balance(&token, payer)?, U256::from(100));
            assert_eq!(balance(&token, TIP20_CHANNEL_RESERVE_ADDRESS)?, U256::ZERO);
            Ok(())
        })
    }

    #[test]
    fn t12_blocked_refund_reverts_and_remains_retryable() -> Result<()> {
        let payer = Address::repeat_byte(0x11);
        let payee = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T12);
        provider.set_timestamp(U256::from(1_000u64));

        let (channel_id, descriptor) = StorageCtx::enter(&mut provider, || {
            setup_path_usd(payer, 1_000, &[])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            let (channel_id, descriptor) = open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            )?;
            reserve.request_close(
                payer,
                ITIP20ChannelReserve::requestCloseCall {
                    descriptor: descriptor.clone(),
                },
            )?;
            Result::<_>::Ok((channel_id, descriptor))
        })?;

        provider.set_timestamp(U256::from(1_000u64 + CLOSE_GRACE_PERIOD));
        StorageCtx::enter(&mut provider, || {
            let token = path_usd();
            let mut reserve = TIP20ChannelReserve::new();

            // Originator recovery would make a guarded reserve-originated refund unclaimable.
            // Channel refunds reject the policy instead and leave channel state intact.
            let (mut registry, sender_policy) =
                install_receive_sender_blacklist(payer, TIP20_CHANNEL_RESERVE_ADDRESS)?;

            let result = reserve.close(
                payee,
                ITIP20ChannelReserve::closeCall {
                    descriptor: descriptor.clone(),
                    cumulativeAmount: U96::ZERO,
                    captureAmount: U96::ZERO,
                    signature: Bytes::new(),
                },
            );
            assert_eq!(result, Err(policy_forbids()));

            let result = reserve.withdraw(
                payer,
                ITIP20ChannelReserve::withdrawCall {
                    descriptor: descriptor.clone(),
                },
            );
            assert_eq!(result, Err(policy_forbids()));
            assert_eq!(channel_state(&reserve, channel_id)?.deposit, U96::from(100));
            assert_eq!(balance(&token, payer)?, U256::from(900));
            assert_eq!(balance(&token, RECEIVE_POLICY_GUARD_ADDRESS)?, U256::ZERO);

            set_blacklisted(
                &mut registry,
                payer,
                sender_policy,
                TIP20_CHANNEL_RESERVE_ADDRESS,
                false,
            )?;
            reserve.withdraw(payer, ITIP20ChannelReserve::withdrawCall { descriptor })?;
            assert_eq!(balance(&token, payer)?, U256::from(1_000));
            assert!(channel_state(&reserve, channel_id)?.deposit.is_zero());
            Ok(())
        })
    }

    #[test]
    fn pre_t12_recipient_restricted_token_cannot_fund_reserve() -> Result<()> {
        let payer = Address::repeat_byte(0x11);
        let payee = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T11);

        StorageCtx::enter(&mut provider, || {
            let mut token = setup_path_usd(payer, 100, &[])?;
            install_recipient_whitelist_policy(&mut token, payer, &[payee])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            let result = open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            );
            assert_eq!(result.unwrap_err(), policy_forbids());
            Ok(())
        })
    }

    #[test]
    fn pre_t12_settle_preserves_state_write_before_transfer_failure() -> Result<()> {
        let signer = k256::ecdsa::SigningKey::from_slice(&[0x11; 32]).unwrap();
        let payer = Address::from_public_key(signer.verifying_key());
        let payee = Address::repeat_byte(0x22);
        let mut provider = TestStorageProvider::new(TempoHardfork::T11);

        StorageCtx::enter(&mut provider, || {
            let mut token = setup_path_usd(payer, 100, &[*PAUSE_ROLE])?;
            let mut reserve = TIP20ChannelReserve::new();
            reserve.initialize()?;
            let (channel_id, descriptor) = open_channel(
                &mut reserve,
                payer,
                payee,
                100,
                B256::repeat_byte(1),
                B256::repeat_byte(2),
            )?;
            let cumulative = U96::from(40);
            let signature = sign_voucher(&reserve, &signer, channel_id, cumulative)?;

            token.pause(payer, ITIP20::pauseCall {})?;
            let result = reserve.settle(
                payee,
                ITIP20ChannelReserve::settleCall {
                    descriptor,
                    cumulativeAmount: cumulative,
                    signature,
                },
            );
            assert_eq!(
                result,
                Err(TempoPrecompileError::Revert(
                    ITIP20::ContractPaused {}.abi_encode().into()
                ))
            );

            // Direct unit calls do not apply EVM rollback, exposing the intermediate write and
            // pinning its position before the failing transfer. On-chain the whole call reverts.
            assert_eq!(channel_state(&reserve, channel_id)?.settled, cumulative);
            Ok(())
        })
    }
}
