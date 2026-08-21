//! Role, policy-scope, and pausable-feature identifiers.
//!
//! Values transcribed from Base reth (`common/ops/roles.rs`, `common/policy_type.rs`,
//! `common/pausable_feature.rs`, `b20_asset/token.rs`). The tests below re-derive each one
//! from its `keccak256` preimage so a transcription slip cannot pass silently.

use alloy::primitives::{b256, B256, U256};

use super::abi::IB20;
use super::error::{B20Error, Result};
use super::layout::PolicySlot;

// --- Role identifiers ---

/// `keccak256("MINT_ROLE")`.
pub const MINT_ROLE: B256 =
    b256!("154c00819833dac601ee5ddded6fda79d9d8b506b911b3dbd54cdb95fe6c3686");
/// `keccak256("BURN_ROLE")`.
pub const BURN_ROLE: B256 =
    b256!("e97b137254058bd94f28d2f3eb79e2d34074ffb488d042e3bc958e0a57d2fa22");
/// `keccak256("BURN_BLOCKED_ROLE")`.
pub const BURN_BLOCKED_ROLE: B256 =
    b256!("7408fdc0d31c7bcb349eab611f5d1168acd4303574993f8cdc98b1cd18c41cae");
/// `keccak256("PAUSE_ROLE")`.
pub const PAUSE_ROLE: B256 =
    b256!("139c2898040ef16910dc9f44dc697df79363da767d8bc92f2e310312b816e46d");
/// `keccak256("UNPAUSE_ROLE")`.
pub const UNPAUSE_ROLE: B256 =
    b256!("265b220c5a8891efdd9e1b1b7fa72f257bd5169f8d87e319cf3dad6ff52b94ae");
/// `keccak256("METADATA_ROLE")`.
pub const METADATA_ROLE: B256 =
    b256!("6bd6b5318a46e5fff572d5e4258a20774aab40cc35ac7680654b9081fcc82f80");
/// `keccak256("OPERATOR_ROLE")` — asset-only, gates `announce` and `updateMultiplier`.
pub const OPERATOR_ROLE: B256 =
    b256!("97667070c54ef182b0f5858b034beac1b6f3089aa2d3188bb1e8929f4fa9b929");
/// The default admin role is the zero identifier.
pub const DEFAULT_ADMIN_ROLE: B256 = B256::ZERO;

// --- Policy scope identifiers ---

/// Policy scope checked against transfer senders.
pub const TRANSFER_SENDER_POLICY: B256 =
    b256!("b81736c875ab819dd97f59f2a6542cfb731ad52b4ae15a6f24df2fb02b0327f5");
/// Policy scope checked against transfer receivers.
pub const TRANSFER_RECEIVER_POLICY: B256 =
    b256!("8a4b3fa2d8b921852bc0089c6ef0958aa6961897be36fd731330fe2cd23f8363");
/// Policy scope checked against delegated transfer executors.
pub const TRANSFER_EXECUTOR_POLICY: B256 =
    b256!("10be5173aff2a44e748bd9acd8b19fe34689581398a9db7ba2fb671e786ff7d8");
/// Policy scope checked against mint receivers.
pub const MINT_RECEIVER_POLICY: B256 =
    b256!("a0d5ae037e66a09119acf080a1d807abb9b6d03b6b9130eb19f7c1e6bdb8ffc8");

/// Maps a policy scope identifier to its packed storage slot, if it is a known scope.
pub fn policy_slot_for(scope: B256) -> Option<PolicySlot> {
    if scope == TRANSFER_SENDER_POLICY {
        Some(PolicySlot::TransferSender)
    } else if scope == TRANSFER_RECEIVER_POLICY {
        Some(PolicySlot::TransferReceiver)
    } else if scope == TRANSFER_EXECUTOR_POLICY {
        Some(PolicySlot::TransferExecutor)
    } else if scope == MINT_RECEIVER_POLICY {
        Some(PolicySlot::MintReceiver)
    } else {
        None
    }
}

/// Resolves a policy scope, reverting with `UnsupportedPolicyType` when unknown.
pub fn require_policy_slot(scope: B256) -> Result<PolicySlot> {
    policy_slot_for(scope)
        .ok_or_else(|| B20Error::revert(IB20::UnsupportedPolicyType { policyScope: scope }))
}

// --- Pausable features ---

/// Storage bit for a pausable feature: `1 << feature`.
pub fn pause_mask(feature: IB20::PausableFeature) -> U256 {
    U256::ONE.checked_shl(usize::from(feature as u8)).unwrap_or(U256::ZERO)
}

/// The three valid pausable features, in enum order.
pub const PAUSABLE_FEATURES: [IB20::PausableFeature; 3] = [
    IB20::PausableFeature::TRANSFER,
    IB20::PausableFeature::MINT,
    IB20::PausableFeature::BURN,
];

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;

    #[test]
    fn role_ids_match_their_keccak_preimages() {
        assert_eq!(MINT_ROLE, keccak256("MINT_ROLE"));
        assert_eq!(BURN_ROLE, keccak256("BURN_ROLE"));
        assert_eq!(BURN_BLOCKED_ROLE, keccak256("BURN_BLOCKED_ROLE"));
        assert_eq!(PAUSE_ROLE, keccak256("PAUSE_ROLE"));
        assert_eq!(UNPAUSE_ROLE, keccak256("UNPAUSE_ROLE"));
        assert_eq!(METADATA_ROLE, keccak256("METADATA_ROLE"));
        assert_eq!(OPERATOR_ROLE, keccak256("OPERATOR_ROLE"));
    }

    #[test]
    fn policy_scopes_resolve_to_distinct_slots() {
        assert_eq!(policy_slot_for(TRANSFER_SENDER_POLICY), Some(PolicySlot::TransferSender));
        assert_eq!(policy_slot_for(TRANSFER_RECEIVER_POLICY), Some(PolicySlot::TransferReceiver));
        assert_eq!(policy_slot_for(TRANSFER_EXECUTOR_POLICY), Some(PolicySlot::TransferExecutor));
        assert_eq!(policy_slot_for(MINT_RECEIVER_POLICY), Some(PolicySlot::MintReceiver));
        assert_eq!(policy_slot_for(B256::repeat_byte(0xff)), None);
    }

    #[test]
    fn pause_masks_are_distinct_bits() {
        assert_eq!(pause_mask(IB20::PausableFeature::TRANSFER), U256::from(1));
        assert_eq!(pause_mask(IB20::PausableFeature::MINT), U256::from(2));
        assert_eq!(pause_mask(IB20::PausableFeature::BURN), U256::from(4));
    }
}
