//! Role, policy-scope, and pausable-feature identifiers.
//!
//! Values transcribed from Base reth (`common/ops/roles.rs`, `common/policy_type.rs`,
//! `common/pausable_feature.rs`, `b20_asset/token.rs`). The tests below re-derive each one
//! from its `keccak256` preimage so a transcription slip cannot pass silently.

use alloy::primitives::{b256, B256, U256};

use super::abi::IB20;
use super::error::{B20Error, Result};
use super::layout::PolicySlot;
use super::version::B20Version;

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
/// `keccak256("SEIZE_ROLE")` — Cobalt, gates `seizeWithMemo`.
pub const SEIZE_ROLE: B256 =
    b256!("3469b8b0d89e9604f8510ed143f74a8336d22955d4f83e23bf53d9414e27f432");
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

/// Policy scope whose members are *exempt* from seizure (Cobalt). An account is seizable only
/// when this policy does not authorize it, so the unset always-allow default keeps seize closed.
pub const SEIZE_EXEMPT_POLICY: B256 =
    b256!("edb5da348cfb67af08746d3afd1be81034b50d5c8576f31aff688f39dfd540ed");
/// Policy scope checked against seize destinations (Cobalt).
pub const SEIZE_RECEIVER_POLICY: B256 =
    b256!("bf15b19caf5c77422c038bc25f26b8b815c3a14f6d04c6616076b81bcfe07b3d");

/// Maps a policy scope identifier to its packed storage slot, if it is a scope `version`'s
/// surface accepts. The seize scopes only exist from Cobalt; on Beryl they are unknown.
pub fn policy_slot_for(scope: B256, version: B20Version) -> Option<PolicySlot> {
    if version.is_cobalt() {
        if scope == SEIZE_EXEMPT_POLICY {
            return Some(PolicySlot::SeizeExempt);
        }
        if scope == SEIZE_RECEIVER_POLICY {
            return Some(PolicySlot::SeizeReceiver);
        }
    }
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
pub fn require_policy_slot(scope: B256, version: B20Version) -> Result<PolicySlot> {
    policy_slot_for(scope, version)
        .ok_or_else(|| B20Error::revert(IB20::UnsupportedPolicyType { policyScope: scope }))
}

// --- Pausable features ---

/// Storage bit for a pausable feature: `1 << feature`.
pub fn pause_mask(feature: IB20::PausableFeature) -> U256 {
    U256::ONE.checked_shl(usize::from(feature as u8)).unwrap_or(U256::ZERO)
}

/// The pausable features `version` recognizes, in enum order. Cobalt appends `SEIZE`.
pub fn pausable_features(version: B20Version) -> &'static [IB20::PausableFeature] {
    const V1: [IB20::PausableFeature; 3] = [
        IB20::PausableFeature::TRANSFER,
        IB20::PausableFeature::MINT,
        IB20::PausableFeature::BURN,
    ];
    const V2: [IB20::PausableFeature; 4] = [
        IB20::PausableFeature::TRANSFER,
        IB20::PausableFeature::MINT,
        IB20::PausableFeature::BURN,
        IB20::PausableFeature::SEIZE,
    ];
    match version {
        B20Version::V1 => &V1,
        B20Version::V2 => &V2,
    }
}

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
        assert_eq!(SEIZE_ROLE, keccak256("SEIZE_ROLE"));
    }

    /// The seize-from scope was renamed from `SEIZE_HOLDER_POLICY` before Cobalt shipped
    /// (Base #4845); the old preimage must not resolve.
    #[test]
    fn seize_scopes_match_their_keccak_preimages() {
        assert_eq!(SEIZE_EXEMPT_POLICY, keccak256("SEIZE_EXEMPT_POLICY"));
        assert_eq!(SEIZE_RECEIVER_POLICY, keccak256("SEIZE_RECEIVER_POLICY"));
        assert_eq!(policy_slot_for(keccak256("SEIZE_HOLDER_POLICY"), B20Version::V2), None);
    }

    #[test]
    fn seize_scopes_exist_only_from_cobalt() {
        for scope in [SEIZE_EXEMPT_POLICY, SEIZE_RECEIVER_POLICY] {
            assert_eq!(policy_slot_for(scope, B20Version::V1), None);
            assert!(policy_slot_for(scope, B20Version::V2).is_some());
        }
    }

    #[test]
    fn policy_scopes_resolve_to_distinct_slots() {
        assert_eq!(policy_slot_for(TRANSFER_SENDER_POLICY, B20Version::V1), Some(PolicySlot::TransferSender));
        assert_eq!(policy_slot_for(TRANSFER_RECEIVER_POLICY, B20Version::V1), Some(PolicySlot::TransferReceiver));
        assert_eq!(policy_slot_for(TRANSFER_EXECUTOR_POLICY, B20Version::V1), Some(PolicySlot::TransferExecutor));
        assert_eq!(policy_slot_for(MINT_RECEIVER_POLICY, B20Version::V1), Some(PolicySlot::MintReceiver));
        assert_eq!(policy_slot_for(B256::repeat_byte(0xff), B20Version::V2), None);
    }

    #[test]
    fn pause_masks_are_distinct_bits() {
        assert_eq!(pause_mask(IB20::PausableFeature::TRANSFER), U256::from(1));
        assert_eq!(pause_mask(IB20::PausableFeature::MINT), U256::from(2));
        assert_eq!(pause_mask(IB20::PausableFeature::BURN), U256::from(4));
        assert_eq!(pause_mask(IB20::PausableFeature::SEIZE), U256::from(8));
    }
}
