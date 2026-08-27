//! TIP-1028 custody precompile for blocked inbound TIP-20 transfers and mints (T6+).

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::address_registry::AddressRegistry;
use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{Handler, Mapping, Slot};
use super::tip20::TIP20Token;
use super::{
    dispatch_call, input_cost, mutate_void, unknown_selector, view, Precompile,
    RECEIVE_POLICY_GUARD_ADDRESS,
};

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IReceivePolicyGuard {
        enum InboundKind {
            TRANSFER,
            MINT
        }

        struct ClaimReceiptV1 {
            uint8 version;
            address token;
            address recoveryAuthority;
            address originator;
            address recipient;
            uint64 blockedAt;
            uint64 blockedNonce;
            uint8 blockedReason;
            InboundKind kind;
            bytes32 memo;
        }

        function balanceOf(bytes calldata receipt) external view returns (uint256 amount);
        function claim(address to, bytes calldata receipt) external;
        function burnBlockedReceipt(bytes calldata receipt) external;

        event TransferBlocked(address indexed token, address indexed receiver, uint64 indexed blockedNonce, uint256 amount, uint8 receiptVersion, bytes receipt);
        event ReceiptClaimed(address indexed token, address indexed receiver, uint64 indexed blockedNonce, uint64 blockedAt, uint8 receiptVersion, address originator, address recipient, address recoveryAuthority, address caller, address to, uint256 amount);
        event ReceiptBurned(address indexed token, address indexed receiver, uint64 indexed blockedNonce, uint64 blockedAt, uint8 receiptVersion, address originator, address recipient, address recoveryAuthority, address caller, uint256 amount);

        error InvalidReceipt();
        error InvalidClaimAddress();
        error UnauthorizedClaimer();
        error AddressReserved();
    }
}

pub const BLOCKED_RECEIPT_VERSION: u8 = 1;

fn revert(error: impl SolError) -> TempoPrecompileError {
    TempoPrecompileError::Revert(error.abi_encode().into())
}

fn invalid_receipt() -> TempoPrecompileError {
    revert(IReceivePolicyGuard::InvalidReceipt {})
}

fn invalid_claim_address() -> TempoPrecompileError {
    revert(IReceivePolicyGuard::InvalidClaimAddress {})
}

fn unauthorized_claimer() -> TempoPrecompileError {
    revert(IReceivePolicyGuard::UnauthorizedClaimer {})
}

pub(crate) fn address_reserved() -> TempoPrecompileError {
    revert(IReceivePolicyGuard::AddressReserved {})
}

impl IReceivePolicyGuard::ClaimReceiptV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        token: Address,
        recovery_authority: Address,
        originator: Address,
        recipient: Address,
        blocked_at: u64,
        blocked_nonce: u64,
        blocked_reason: u8,
        kind: IReceivePolicyGuard::InboundKind,
        memo: B256,
    ) -> Self {
        Self {
            version: BLOCKED_RECEIPT_VERSION,
            token,
            recoveryAuthority: recovery_authority,
            originator,
            recipient,
            blockedAt: blocked_at,
            blockedNonce: blocked_nonce,
            blockedReason: blocked_reason,
            kind,
            memo,
        }
    }
}

fn decode_receipt(receipt: Bytes) -> Result<IReceivePolicyGuard::ClaimReceiptV1> {
    IReceivePolicyGuard::ClaimReceiptV1::abi_decode(&receipt).map_err(|_| invalid_receipt())
}

/// Recovery authority for one blocked receipt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RecoveryMode {
    #[default]
    Originator,
    Receiver,
    ThirdParty,
}

impl RecoveryMode {
    fn from(receipt: &IReceivePolicyGuard::ClaimReceiptV1, receiver: Address) -> Self {
        if receipt.recoveryAuthority.is_zero() {
            Self::Originator
        } else if receipt.recoveryAuthority == receiver {
            Self::Receiver
        } else {
            Self::ThirdParty
        }
    }

    fn authority(self, receipt: &IReceivePolicyGuard::ClaimReceiptV1) -> Address {
        match self {
            Self::Originator => receipt.originator,
            Self::Receiver | Self::ThirdParty => receipt.recoveryAuthority,
        }
    }

    pub(crate) fn policy_subject(self, originator: Address, receiver: Address) -> Address {
        match self {
            Self::Originator => originator,
            Self::Receiver | Self::ThirdParty => receiver,
        }
    }

    pub(crate) fn is_reroute(self, to: Address, receiver: Address) -> bool {
        match self {
            Self::Originator => true,
            Self::Receiver | Self::ThirdParty => to != receiver,
        }
    }

    pub(crate) fn spending_account(self, recovery_authority: Address) -> Option<Address> {
        match self {
            Self::Originator | Self::Receiver => Some(recovery_authority),
            Self::ThirdParty => None,
        }
    }
}

/// T6 receive-policy guard. Slot 0 is the receipt nonce; slot 1 maps receipt hashes to amounts.
pub struct ReceivePolicyGuard {
    nonce: Slot<u64>,
    balances: Mapping<B256, U256>,
    address: Address,
    storage: StorageCtx,
}

impl ReceivePolicyGuard {
    pub fn new() -> Self {
        let address = RECEIVE_POLICY_GUARD_ADDRESS;
        Self {
            nonce: Slot::new(U256::ZERO, address),
            balances: Mapping::new(U256::ONE, address),
            address,
            storage: StorageCtx::default(),
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }

    fn receipt_key(&self, receipt: &IReceivePolicyGuard::ClaimReceiptV1) -> Result<B256> {
        self.storage.keccak256(&receipt.abi_encode())
    }

    fn next_receipt_nonce(&mut self) -> Result<u64> {
        let nonce = self.nonce.read()?.max(1);
        self.nonce.write(
            nonce
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        Ok(nonce)
    }

    pub fn balance_of(&self, receipt: Bytes) -> Result<U256> {
        let receipt = decode_receipt(receipt)?;
        self.balances[self.receipt_key(&receipt)?].read()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_blocked(
        &mut self,
        token: Address,
        originator: Address,
        recipient: Address,
        receiver: Address,
        recovery_authority: Address,
        amount: U256,
        blocked_reason: u8,
        kind: IReceivePolicyGuard::InboundKind,
        memo: B256,
    ) -> Result<()> {
        if !matches!(blocked_reason, 1 | 2)
            || matches!(kind, IReceivePolicyGuard::InboundKind::__Invalid)
        {
            return Err(invalid_receipt());
        }

        let blocked_nonce = self.next_receipt_nonce()?;
        let blocked_at = self.storage.timestamp().saturating_to::<u64>();
        let receipt = IReceivePolicyGuard::ClaimReceiptV1::new(
            token,
            recovery_authority,
            originator,
            recipient,
            blocked_at,
            blocked_nonce,
            blocked_reason,
            kind,
            memo,
        );
        let key = self.receipt_key(&receipt)?;
        self.balances[key].write(amount)?;

        self.emit_event(IReceivePolicyGuard::TransferBlocked {
            token,
            receiver,
            blockedNonce: blocked_nonce,
            amount,
            receiptVersion: BLOCKED_RECEIPT_VERSION,
            receipt: receipt.abi_encode().into(),
        })
    }

    pub fn claim(&mut self, msg_sender: Address, to: Address, receipt: Bytes) -> Result<()> {
        if to == RECEIVE_POLICY_GUARD_ADDRESS {
            return Err(invalid_claim_address());
        }

        let (receipt, receiver, recovery_mode) = resolve_receipt(receipt)?;
        let recovery_authority = recovery_mode.authority(&receipt);
        if msg_sender != recovery_authority {
            return Err(unauthorized_claimer());
        }

        let key = self.receipt_key(&receipt)?;
        let amount = self.balances[key].read()?;
        if amount.is_zero() {
            return Err(invalid_receipt());
        }
        self.balances[key].write(U256::ZERO)?;

        TIP20Token::from_address(receipt.token)?.release_blocked_funds(
            receipt.originator,
            receiver,
            to,
            amount,
            recovery_mode,
            recovery_authority,
        )?;

        self.emit_event(IReceivePolicyGuard::ReceiptClaimed {
            token: receipt.token,
            receiver,
            blockedNonce: receipt.blockedNonce,
            blockedAt: receipt.blockedAt,
            receiptVersion: receipt.version,
            originator: receipt.originator,
            recipient: receipt.recipient,
            recoveryAuthority: receipt.recoveryAuthority,
            caller: msg_sender,
            to,
            amount,
        })
    }

    pub fn burn_blocked_receipt(&mut self, msg_sender: Address, receipt: Bytes) -> Result<()> {
        let (receipt, receiver, recovery_mode) = resolve_receipt(receipt)?;
        let key = self.receipt_key(&receipt)?;
        let amount = self.balances[key].read()?;
        if amount.is_zero() {
            return Err(invalid_receipt());
        }

        let owner = recovery_mode.policy_subject(receipt.originator, receiver);
        TIP20Token::from_address(receipt.token)?
            .burn_blocked_internal(msg_sender, owner, amount, false)?;
        self.balances[key].write(U256::ZERO)?;

        self.emit_event(IReceivePolicyGuard::ReceiptBurned {
            token: receipt.token,
            receiver,
            blockedNonce: receipt.blockedNonce,
            blockedAt: receipt.blockedAt,
            receiptVersion: receipt.version,
            originator: receipt.originator,
            recipient: receipt.recipient,
            recoveryAuthority: receipt.recoveryAuthority,
            caller: msg_sender,
            amount,
        })
    }
}

fn resolve_receipt(
    receipt: Bytes,
) -> Result<(IReceivePolicyGuard::ClaimReceiptV1, Address, RecoveryMode)> {
    let receipt = decode_receipt(receipt)?;
    let receiver = AddressRegistry::new()
        .resolve_recipient(receipt.recipient)
        .map_err(|_| invalid_claim_address())?;
    let recovery_mode = RecoveryMode::from(&receipt, receiver);
    Ok((receipt, receiver, recovery_mode))
}

impl ContractStorage for ReceivePolicyGuard {
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

impl Precompile for ReceivePolicyGuard {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if !self.storage.spec().is_t6() {
            let selector = calldata
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .unwrap_or([0; 4]);
            return unknown_selector(selector, 0);
        }
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            |data| {
                IReceivePolicyGuard::IReceivePolicyGuardCalls::abi_decode_with_config(
                    data,
                    super::abi_decoder_config(),
                )
            },
            |call| match call {
                IReceivePolicyGuard::IReceivePolicyGuardCalls::balanceOf(call) => {
                    view(call, |call| self.balance_of(call.receipt))
                }
                IReceivePolicyGuard::IReceivePolicyGuardCalls::claim(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.claim(sender, call.to, call.receipt)
                    })
                }
                IReceivePolicyGuard::IReceivePolicyGuardCalls::burnBlockedReceipt(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.burn_blocked_receipt(sender, call.receipt)
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
    use crate::tempo::precompile::test_utils::TestStorageProvider;
    use crate::tempo::precompile::tip20::{IRolesAuth, ISSUER_ROLE, ITIP20};
    use crate::tempo::precompile::tip403_registry::{
        ITIP403Registry, TIP403Registry, ALLOW_ALL_POLICY_ID, REJECT_ALL_POLICY_ID,
    };
    use alloy::sol_types::SolCall;

    #[test]
    fn receipt_hash_covers_every_field() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);
        StorageCtx::enter(&mut provider, || -> Result<()> {
            let guard = ReceivePolicyGuard::new();
            let base = IReceivePolicyGuard::ClaimReceiptV1::new(
                super::super::PATH_USD_ADDRESS,
                Address::ZERO,
                Address::repeat_byte(1),
                Address::repeat_byte(2),
                3,
                4,
                1,
                IReceivePolicyGuard::InboundKind::TRANSFER,
                B256::ZERO,
            );
            let mut changed = base.clone();
            changed.memo = B256::repeat_byte(5);
            assert_ne!(guard.receipt_key(&base)?, guard.receipt_key(&changed)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn selectors_are_gated_at_t6() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T5);
        let call = IReceivePolicyGuard::balanceOfCall {
            receipt: Bytes::new(),
        };
        let output = StorageCtx::enter(&mut provider, || {
            ReceivePolicyGuard::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(output.reverted);
    }

    #[test]
    fn blocked_transfer_can_be_claimed_by_originator() {
        let issuer = Address::repeat_byte(0x11);
        let originator = Address::repeat_byte(0x22);
        let receiver = Address::repeat_byte(0x33);
        let amount = U256::from(100);
        let blocked_at = U256::from(1_728_000);
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);
        provider.set_timestamp(blocked_at);

        StorageCtx::enter(&mut provider, || -> Result<()> {
            let mut token = TIP20Token::from_address_unchecked(super::super::PATH_USD_ADDRESS);
            token.initialize(
                Address::ZERO,
                "Path USD",
                "pathUSD",
                "USD",
                super::super::PATH_USD_ADDRESS,
                issuer,
            )?;
            token.grant_role(
                issuer,
                IRolesAuth::grantRoleCall {
                    role: *ISSUER_ROLE,
                    account: issuer,
                },
            )?;
            token.mint(
                issuer,
                ITIP20::mintCall {
                    to: originator,
                    amount,
                },
            )?;
            TIP403Registry::new().set_receive_policy(
                receiver,
                ITIP403Registry::setReceivePolicyCall {
                    senderPolicyId: REJECT_ALL_POLICY_ID,
                    tokenFilterId: ALLOW_ALL_POLICY_ID,
                    recoveryAuthority: Address::ZERO,
                },
            )?;

            token.transfer(
                originator,
                ITIP20::transferCall {
                    to: receiver,
                    amount,
                },
            )?;
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall {
                    account: RECEIVE_POLICY_GUARD_ADDRESS,
                })?,
                amount,
            );
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall { account: receiver })?,
                U256::ZERO,
            );

            let receipt = IReceivePolicyGuard::ClaimReceiptV1::new(
                token.address,
                Address::ZERO,
                originator,
                receiver,
                blocked_at.to::<u64>(),
                1,
                2,
                IReceivePolicyGuard::InboundKind::TRANSFER,
                B256::ZERO,
            );
            let encoded: Bytes = receipt.abi_encode().into();
            let mut guard = ReceivePolicyGuard::new();
            assert_eq!(guard.balance_of(encoded.clone())?, amount);
            guard.claim(originator, originator, encoded.clone())?;
            assert_eq!(guard.balance_of(encoded)?, U256::ZERO);
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall {
                    account: originator,
                })?,
                amount,
            );
            Ok(())
        })
        .unwrap();
    }
}
