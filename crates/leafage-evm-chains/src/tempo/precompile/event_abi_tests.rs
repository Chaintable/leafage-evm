//! Golden tests for event declarations that previously differed from Tempo writer v1.13.2.

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolEvent;

use super::{
    fee_manager::{IFeeManager, ITIPFeeAMM},
    stablecoin_dex::IStablecoinDEX,
    tip20::{IRolesAuth, ITIP20},
    validator_config_v2::IValidatorConfigV2,
};

#[allow(clippy::too_many_arguments)]
mod canonical {
    alloy::sol! {
        interface CStablecoinDEX {
            event PairCreated(bytes32 indexed key, address indexed base, address indexed quote);
            event OrderPlaced(uint128 indexed orderId, address indexed maker, address indexed token, uint128 amount, bool isBid, int16 tick, bool isFlipOrder, int16 flipTick);
            event OrderFilled(uint128 indexed orderId, address indexed maker, address indexed taker, uint128 amountFilled, bool partialFill);
            event OrderCancelled(uint128 indexed orderId);
        }

        interface CRolesAuth {
            event RoleMembershipUpdated(bytes32 indexed role, address indexed account, address indexed sender, bool hasRole);
            event RoleAdminUpdated(bytes32 indexed role, bytes32 indexed newAdminRole, address indexed sender);
        }

        interface CTIP20 {
            event TransferWithMemo(address indexed from, address indexed to, uint256 amount, bytes32 indexed memo);
            event TransferPolicyUpdate(address indexed updater, uint64 indexed newPolicyId);
            event SupplyCapUpdate(address indexed updater, uint256 indexed newSupplyCap);
            event NextQuoteTokenSet(address indexed updater, address indexed nextQuoteToken);
            event QuoteTokenUpdate(address indexed updater, address indexed newQuoteToken);
            event RewardRecipientSet(address indexed holder, address indexed recipient);
        }

        interface CFeeManager {
            event ValidatorTokenSet(address indexed validator, address indexed token);
            event UserTokenSet(address indexed user, address indexed token);
            event FeesDistributed(address indexed validator, address indexed token, uint256 amount);
        }

        interface CFeeAMM {
            event Mint(address sender, address indexed to, address indexed userToken, address indexed validatorToken, uint256 amountValidatorToken, uint256 liquidity);
            event Burn(address indexed sender, address indexed userToken, address indexed validatorToken, uint256 amountUserToken, uint256 amountValidatorToken, uint256 liquidity, address to);
            event RebalanceSwap(address indexed userToken, address indexed validatorToken, address indexed swapper, uint256 amountIn, uint256 amountOut);
        }

        interface CValidatorConfigV2 {
            event ValidatorAdded(uint64 indexed index, address indexed validatorAddress, bytes32 publicKey, string ingress, string egress, address feeRecipient);
            event ValidatorDeactivated(uint64 indexed index, address indexed validatorAddress);
            event ValidatorRotated(uint64 indexed index, uint64 indexed deactivatedIndex, address indexed validatorAddress, bytes32 oldPublicKey, bytes32 newPublicKey, string ingress, string egress, address caller);
            event FeeRecipientUpdated(uint64 indexed index, address feeRecipient, address caller);
            event IpAddressesUpdated(uint64 indexed index, string ingress, string egress, address caller);
            event ValidatorOwnershipTransferred(uint64 indexed index, address indexed oldAddress, address indexed newAddress, address caller);
            event OwnershipTransferred(address indexed oldOwner, address indexed newOwner);
            event ValidatorMigrated(uint64 indexed index, address indexed validatorAddress, bytes32 publicKey);
            event NetworkIdentityRotationEpochSet(uint64 indexed previousEpoch, uint64 indexed nextEpoch);
            event SkippedValidatorMigration(uint64 indexed index, address indexed validatorAddress, bytes32 publicKey);
        }
    }
}

fn addr(value: u8) -> Address {
    Address::repeat_byte(value)
}

fn word(value: u8) -> B256 {
    B256::repeat_byte(value)
}

fn log_gas(log: &alloy::primitives::LogData) -> u64 {
    375 + 375 * log.topics().len() as u64 + 8 * log.data.len() as u64
}

macro_rules! assert_canonical_event {
    ($actual:ident, $expected:ident, $event:ident { $($field:ident: $value:expr),* $(,)? }) => {{
        let actual = $actual::$event {
            $($field: ($value).clone()),*
        }
        .encode_log_data();
        let expected = canonical::$expected::$event {
            $($field: $value),*
        }
        .encode_log_data();
        assert_eq!(actual, expected, "{}", stringify!($event));
        assert_eq!(log_gas(&actual), log_gas(&expected), "{} gas", stringify!($event));
    }};
}

#[test]
fn stablecoin_dex_events_match_writer() {
    assert_canonical_event!(
        IStablecoinDEX,
        CStablecoinDEX,
        PairCreated {
            key: word(1),
            base: addr(2),
            quote: addr(3),
        }
    );
    assert_canonical_event!(
        IStablecoinDEX,
        CStablecoinDEX,
        OrderPlaced {
            orderId: 1,
            maker: addr(2),
            token: addr(3),
            amount: 4,
            isBid: true,
            tick: -10,
            isFlipOrder: true,
            flipTick: 20,
        }
    );
    assert_canonical_event!(
        IStablecoinDEX,
        CStablecoinDEX,
        OrderFilled {
            orderId: 1,
            maker: addr(2),
            taker: addr(3),
            amountFilled: 4,
            partialFill: true,
        }
    );
    assert_canonical_event!(
        IStablecoinDEX,
        CStablecoinDEX,
        OrderCancelled { orderId: 1 }
    );
}

#[test]
fn tip20_events_match_writer() {
    assert_canonical_event!(
        IRolesAuth,
        CRolesAuth,
        RoleMembershipUpdated {
            role: word(1),
            account: addr(2),
            sender: addr(3),
            hasRole: true,
        }
    );
    assert_canonical_event!(
        IRolesAuth,
        CRolesAuth,
        RoleAdminUpdated {
            role: word(1),
            newAdminRole: word(2),
            sender: addr(3),
        }
    );
    assert_canonical_event!(
        ITIP20,
        CTIP20,
        TransferWithMemo {
            from: addr(1),
            to: addr(2),
            amount: U256::from(3),
            memo: word(4),
        }
    );
    assert_canonical_event!(
        ITIP20,
        CTIP20,
        TransferPolicyUpdate {
            updater: addr(1),
            newPolicyId: 2,
        }
    );
    assert_canonical_event!(
        ITIP20,
        CTIP20,
        SupplyCapUpdate {
            updater: addr(1),
            newSupplyCap: U256::from(2),
        }
    );
    assert_canonical_event!(
        ITIP20,
        CTIP20,
        NextQuoteTokenSet {
            updater: addr(1),
            nextQuoteToken: addr(2),
        }
    );
    assert_canonical_event!(
        ITIP20,
        CTIP20,
        QuoteTokenUpdate {
            updater: addr(1),
            newQuoteToken: addr(2),
        }
    );
    assert_canonical_event!(
        ITIP20,
        CTIP20,
        RewardRecipientSet {
            holder: addr(1),
            recipient: addr(2),
        }
    );
}

#[test]
fn fee_manager_events_match_writer() {
    assert_canonical_event!(
        IFeeManager,
        CFeeManager,
        ValidatorTokenSet {
            validator: addr(1),
            token: addr(2),
        }
    );
    assert_canonical_event!(
        IFeeManager,
        CFeeManager,
        UserTokenSet {
            user: addr(1),
            token: addr(2),
        }
    );
    assert_canonical_event!(
        IFeeManager,
        CFeeManager,
        FeesDistributed {
            validator: addr(1),
            token: addr(2),
            amount: U256::from(3),
        }
    );
    assert_canonical_event!(
        ITIPFeeAMM,
        CFeeAMM,
        Mint {
            sender: addr(1),
            to: addr(2),
            userToken: addr(3),
            validatorToken: addr(4),
            amountValidatorToken: U256::from(5),
            liquidity: U256::from(6),
        }
    );
    assert_canonical_event!(
        ITIPFeeAMM,
        CFeeAMM,
        Burn {
            sender: addr(1),
            userToken: addr(2),
            validatorToken: addr(3),
            amountUserToken: U256::from(4),
            amountValidatorToken: U256::from(5),
            liquidity: U256::from(6),
            to: addr(7),
        }
    );
    assert_canonical_event!(
        ITIPFeeAMM,
        CFeeAMM,
        RebalanceSwap {
            userToken: addr(1),
            validatorToken: addr(2),
            swapper: addr(3),
            amountIn: U256::from(4),
            amountOut: U256::from(5),
        }
    );
}

#[test]
fn validator_config_v2_events_match_writer() {
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        ValidatorAdded {
            index: 1,
            validatorAddress: addr(2),
            publicKey: word(3),
            ingress: "ingress-4".to_owned(),
            egress: "egress-5".to_owned(),
            feeRecipient: addr(6),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        ValidatorDeactivated {
            index: 1,
            validatorAddress: addr(2),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        ValidatorRotated {
            index: 1,
            deactivatedIndex: 2,
            validatorAddress: addr(3),
            oldPublicKey: word(4),
            newPublicKey: word(5),
            ingress: "ingress-6".to_owned(),
            egress: "egress-7".to_owned(),
            caller: addr(8),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        FeeRecipientUpdated {
            index: 1,
            feeRecipient: addr(2),
            caller: addr(3),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        IpAddressesUpdated {
            index: 1,
            ingress: "ingress-2".to_owned(),
            egress: "egress-3".to_owned(),
            caller: addr(4),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        ValidatorOwnershipTransferred {
            index: 1,
            oldAddress: addr(2),
            newAddress: addr(3),
            caller: addr(4),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        OwnershipTransferred {
            oldOwner: addr(1),
            newOwner: addr(2),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        ValidatorMigrated {
            index: 1,
            validatorAddress: addr(2),
            publicKey: word(3),
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        NetworkIdentityRotationEpochSet {
            previousEpoch: 1,
            nextEpoch: 2,
        }
    );
    assert_canonical_event!(
        IValidatorConfigV2,
        CValidatorConfigV2,
        SkippedValidatorMigration {
            index: 1,
            validatorAddress: addr(2),
            publicKey: word(3),
        }
    );
}
