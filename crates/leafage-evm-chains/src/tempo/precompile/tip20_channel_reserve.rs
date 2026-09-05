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
use super::tip20::{is_tip20_prefix, TIP20Token, ITIP20};
use super::tip403_registry::AuthRole;
use super::{
    dispatch_call, input_cost, metadata, mutate, mutate_void, unknown_selector, view, Precompile,
    TIP20_CHANNEL_RESERVE_ADDRESS,
};
use crate::tempo::address::TempoAddressExt;

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

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ITIP20ChannelReserve {
        struct ChannelDescriptor {
            address payer;
            address payee;
            address operator;
            address token;
            bytes32 salt;
            address authorizedSigner;
            bytes32 expiringNonceHash;
        }

        struct ChannelState {
            uint96 settled;
            uint96 deposit;
            uint32 closeRequestedAt;
        }

        struct Channel {
            ChannelDescriptor descriptor;
            ChannelState state;
        }

        function CLOSE_GRACE_PERIOD() external view returns (uint64);
        function VOUCHER_TYPEHASH() external view returns (bytes32);
        function open(address payee, address operator, address token, uint96 deposit, bytes32 salt, address authorizedSigner) external returns (bytes32 channelId);
        function settle(ChannelDescriptor descriptor, uint96 cumulativeAmount, bytes signature) external;
        function topUp(ChannelDescriptor descriptor, uint96 additionalDeposit) external;
        function close(ChannelDescriptor descriptor, uint96 cumulativeAmount, uint96 captureAmount, bytes signature) external;
        function requestClose(ChannelDescriptor descriptor) external;
        function withdraw(ChannelDescriptor descriptor) external;
        function getChannel(ChannelDescriptor descriptor) external view returns (Channel);
        function getChannelState(bytes32 channelId) external view returns (ChannelState);
        function getChannelStatesBatch(bytes32[] channelIds) external view returns (ChannelState[]);
        function computeChannelId(address payer, address payee, address operator, address token, bytes32 salt, address authorizedSigner, bytes32 expiringNonceHash) external view returns (bytes32);
        function getVoucherDigest(bytes32 channelId, uint96 cumulativeAmount) external view returns (bytes32);
        function domainSeparator() external view returns (bytes32);
        function storageCredits(address payer) external view returns (uint64 credits);

        event ChannelOpened(bytes32 indexed channelId, address indexed payer, address indexed payee, address operator, address token, address authorizedSigner, bytes32 salt, bytes32 expiringNonceHash, uint96 deposit);
        event Settled(bytes32 indexed channelId, address indexed payer, address indexed payee, uint96 cumulativeAmount, uint96 deltaPaid, uint96 newSettled);
        event TopUp(bytes32 indexed channelId, address indexed payer, address indexed payee, uint96 additionalDeposit, uint96 newDeposit);
        event CloseRequested(bytes32 indexed channelId, address indexed payer, address indexed payee, uint256 closeGraceEnd);
        event ChannelClosed(bytes32 indexed channelId, address indexed payer, address indexed payee, uint96 settledToPayee, uint96 refundedToPayer);
        event CloseRequestCancelled(bytes32 indexed channelId, address indexed payer, address indexed payee);

        error ChannelAlreadyExists();
        error ChannelNotFound();
        error NotPayer();
        error NotPayeeOrOperator();
        error InvalidPayee();
        error ZeroDeposit();
        error ExpiringNonceHashNotSet();
        error InvalidSignature();
        error AmountExceedsDeposit();
        error AmountNotIncreasing();
        error CaptureAmountInvalid();
        error CloseNotReady();
        error DepositOverflow();
    }
}

#[inline]
fn revert(error: impl SolError) -> TempoPrecompileError {
    TempoPrecompileError::Revert(error.abi_encode().into())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PackedChannelState {
    settled: U96,
    deposit: U96,
    close_requested_at: u32,
}

impl PackedChannelState {
    fn exists(self) -> bool {
        !self.deposit.is_zero()
    }

    fn close_requested_at(self) -> Option<u32> {
        (self.close_requested_at != 0).then_some(self.close_requested_at)
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
    const LAYOUT: Layout = Layout::Bytes(28);
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
            close_requested_at: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            deposit: U96::from_be_slice(&bytes[8..20]),
            settled: U96::from_be_slice(&bytes[20..32]),
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        let mut bytes = [0u8; 32];
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
        let mut token = TIP20Token::from_address(call.token)?;
        token.ensure_authorized_as(&[(payee, AuthRole::Recipient)])?;
        token.system_transfer_from(self.address, msg_sender, U256::from(call.deposit))?;

        self.write_channel_state_spending_credit(
            msg_sender,
            channel_id,
            PackedChannelState {
                settled: U96::ZERO,
                deposit: call.deposit,
                close_requested_at: 0,
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
        token.ensure_authorized_as(&[(call.descriptor.payer, AuthRole::Sender)])?;
        state.settled = call.cumulativeAmount;
        self.channel_states[channel_id].write(state)?;
        token.transfer(
            self.address,
            ITIP20::transferCall {
                to: call.descriptor.payee,
                amount: U256::from(delta),
            },
        )?;
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
            let payee = AddressRegistry::new().resolve_recipient(call.descriptor.payee)?;
            let mut token = TIP20Token::from_address(call.descriptor.token)?;
            token.ensure_authorized_as(&[(payee, AuthRole::Recipient)])?;
            token.system_transfer_from(
                self.address,
                msg_sender,
                U256::from(call.additionalDeposit),
            )?;
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

        dispatch_call(
            calldata,
            ITIP20ChannelReserve::ITIP20ChannelReserveCalls::valid_selector,
            |data| {
                ITIP20ChannelReserve::ITIP20ChannelReserveCalls::abi_decode_with_config(
                    data,
                    super::abi_decoder_config(),
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
                    if !self.storage.spec().is_t7() {
                        return unknown_selector(
                            ITIP20ChannelReserve::storageCreditsCall::SELECTOR,
                            self.storage.gas_used(),
                        );
                    }
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
        };
        let mut word = super::super::storage_types::packing::PackedSlot(U256::ZERO);
        state.store(&mut word, U256::ZERO, LayoutCtx::FULL).unwrap();
        let bytes = word.0.to_be_bytes::<32>();
        assert_eq!(&bytes[..4], &[0; 4]);
        assert_eq!(&bytes[4..8], &0x33445566u32.to_be_bytes());
        assert_eq!(U96::from_be_slice(&bytes[8..20]), U96::from(0x22));
        assert_eq!(U96::from_be_slice(&bytes[20..]), U96::from(0x11));
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
}
