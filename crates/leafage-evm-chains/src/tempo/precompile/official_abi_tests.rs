//! Fixed wire-schema snapshot captured from Leafage 4d16f6b before replacing local definitions.
//! Parameter/internal type names are not wire data; tuple shapes and indexed flags are. Mutability is reviewed as ABI metadata.
use super::test_utils::TestStorageProvider;
use super::tip20::{IRolesAuth, TIP20Token, DEFAULT_ADMIN_ROLE, ITIP20};
use super::tip20_factory::{compute_tip20_address, ITIP20Factory, TIP20Factory};
use super::validator_config::{IValidatorConfig, ValidatorConfig};
use super::{Precompile, StorageCtx, UnknownFunctionSelector, PATH_USD_ADDRESS};
use crate::tempo::hardfork::TempoHardfork;
use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::sol_types::{SolCall, SolError, SolEvent, SolValue};
use serde_json::Value;

fn raw_call(signature: &str, arguments: Vec<u8>) -> Vec<u8> {
    let mut data = keccak256(signature)[..4].to_vec();
    data.extend(arguments);
    data
}

#[test]
fn official_enum_discriminants_match_frozen_storage_and_wire_values() {
    assert_eq!(
        u8::from(super::account_keychain::IAccountKeychain::SignatureType::Secp256k1),
        0
    );
    assert_eq!(
        u8::from(super::account_keychain::IAccountKeychain::SignatureType::P256),
        1
    );
    assert_eq!(
        u8::from(super::account_keychain::IAccountKeychain::SignatureType::WebAuthn),
        2
    );
    assert_eq!(
        u8::from(super::tip403_registry::ITIP403Registry::PolicyType::WHITELIST),
        0
    );
    assert_eq!(
        u8::from(super::tip403_registry::ITIP403Registry::PolicyType::BLACKLIST),
        1
    );
    assert_eq!(
        u8::from(super::tip403_registry::ITIP403Registry::PolicyType::COMPOUND),
        2
    );
    assert_eq!(
        u8::from(super::tip403_registry::ITIP403Registry::BlockedReason::NONE),
        0
    );
    assert_eq!(
        u8::from(super::tip403_registry::ITIP403Registry::BlockedReason::TOKEN_FILTER),
        1
    );
    assert_eq!(
        u8::from(super::tip403_registry::ITIP403Registry::BlockedReason::RECEIVE_POLICY),
        2
    );
    assert_eq!(
        u8::from(super::receive_policy_guard::IReceivePolicyGuard::InboundKind::TRANSFER),
        0
    );
    assert_eq!(
        u8::from(super::receive_policy_guard::IReceivePolicyGuard::InboundKind::MINT),
        1
    );
    assert_eq!(u8::from(super::zone_factory::IZonePortal::Role::None), 0);
    assert_eq!(
        u8::from(super::zone_factory::IZonePortal::Role::Sequencer),
        1
    );
    assert_eq!(u8::from(super::zone_factory::IZonePortal::Role::Account), 2);
    assert_eq!(
        u8::from(super::zone_factory::IZonePortal::Role::CallbackGateway),
        3
    );
    assert_eq!(
        u8::from(super::zone_factory::IZonePortal::Role::PauseGuardian),
        4
    );
    assert_eq!(
        u8::from(super::zone_factory::IZonePortal::Capability::PausePortal),
        0
    );
    assert_eq!(
        u8::from(super::zone_factory::IZonePortal::Capability::AccessPolicy),
        1
    );
}

#[test]
fn official_addresses_match_frozen_writer_storage_keys() {
    assert_eq!(
        super::TIP_FEE_MANAGER_ADDRESS,
        alloy::primitives::address!("0xfeec000000000000000000000000000000000000")
    );
    assert_eq!(
        super::PATH_USD_ADDRESS,
        alloy::primitives::address!("0x20C0000000000000000000000000000000000000")
    );
    assert_eq!(
        super::TIP403_REGISTRY_ADDRESS,
        alloy::primitives::address!("0x403C000000000000000000000000000000000000")
    );
    assert_eq!(
        super::TIP20_FACTORY_ADDRESS,
        alloy::primitives::address!("0x20FC000000000000000000000000000000000000")
    );
    assert_eq!(
        super::STABLECOIN_DEX_ADDRESS,
        alloy::primitives::address!("0xdec0000000000000000000000000000000000000")
    );
    assert_eq!(
        super::TIP20_CHANNEL_RESERVE_ADDRESS,
        alloy::primitives::address!("0x4D50500000000000000000000000000000000000")
    );
    assert_eq!(
        super::NONCE_PRECOMPILE_ADDRESS,
        alloy::primitives::address!("0x4E4F4E4345000000000000000000000000000000")
    );
    assert_eq!(
        super::VALIDATOR_CONFIG_ADDRESS,
        alloy::primitives::address!("0xCCCCCCCC00000000000000000000000000000000")
    );
    assert_eq!(
        super::ACCOUNT_KEYCHAIN_ADDRESS,
        alloy::primitives::address!("0xAAAAAAAA00000000000000000000000000000000")
    );
    assert_eq!(
        super::VALIDATOR_CONFIG_V2_ADDRESS,
        alloy::primitives::address!("0xCCCCCCCC00000000000000000000000000000001")
    );
    assert_eq!(
        super::SIGNATURE_VERIFIER_ADDRESS,
        alloy::primitives::address!("0x5165300000000000000000000000000000000000")
    );
    assert_eq!(
        super::ADDRESS_REGISTRY_ADDRESS,
        alloy::primitives::address!("0xFDC0000000000000000000000000000000000000")
    );
    assert_eq!(
        super::RECEIVE_POLICY_GUARD_ADDRESS,
        alloy::primitives::address!("0xB10C000000000000000000000000000000000000")
    );
    assert_eq!(
        super::STORAGE_CREDITS_ADDRESS,
        alloy::primitives::address!("0x1060000000000000000000000000000000000000")
    );
    assert_eq!(
        super::CURRENT_COMMITTEE_ADDRESS,
        alloy::primitives::address!("0xC077E00000000000000000000000000000000000")
    );
    assert_eq!(
        super::ZONE_FACTORY_ADDRESS,
        alloy::primitives::address!("0x5AF2000000000000000000000000000000000000")
    );
    assert_eq!(
        super::INITIAL_ZONE_FACTORY_OWNER,
        alloy::primitives::address!("0xaF571FD4B3AD43a5807A5E58bFb25ea1aB327A14")
    );
    assert_eq!(
        super::ZONE_PORTAL_IMPL_ADDRESS,
        alloy::primitives::address!("0x5AD1000000000000000000000000000000000000")
    );
    assert_eq!(
        super::ZONE_VERIFIER_ADDRESS,
        alloy::primitives::address!("0x5A56000000000000000000000000000000000000")
    );
    assert_eq!(
        super::ZONE_MESSENGER_ADDRESS,
        alloy::primitives::address!("0x5A4D000000000000000000000000000000000000")
    );
    assert_eq!(super::DEFAULT_FEE_TOKEN, super::PATH_USD_ADDRESS);
}

#[test]
fn official_has_role_order_is_used_by_dispatch() {
    let admin = Address::repeat_byte(0x11);
    for fork in [TempoHardfork::Genesis, TempoHardfork::T11] {
        let mut provider = TestStorageProvider::new(fork);
        StorageCtx::enter(&mut provider, || {
            let mut token = TIP20Token::from_address(PATH_USD_ADDRESS).unwrap();
            token
                .initialize(admin, "USD", "USD", "USD", Address::ZERO, admin)
                .unwrap();
            for (account, expected) in [(admin, true), (Address::repeat_byte(0x22), false)] {
                let data = raw_call(
                    "hasRole(address,bytes32)",
                    (account, DEFAULT_ADMIN_ROLE).abi_encode(),
                );
                assert_eq!(
                    data,
                    IRolesAuth::hasRoleCall {
                        account,
                        role: DEFAULT_ADMIN_ROLE
                    }
                    .abi_encode()
                );
                let output = token.call(&data, admin).unwrap();
                assert!(!output.reverted);
                assert_eq!(output.bytes.as_ref(), expected.abi_encode());
            }
            let legacy = raw_call(
                "hasRole(bytes32,address)",
                (DEFAULT_ADMIN_ROLE, admin).abi_encode(),
            );
            let output = token.call(&legacy, admin).unwrap();
            assert!(output.reverted);
            assert_eq!(&output.bytes[..4], &UnknownFunctionSelector::SELECTOR);
        });
    }
}

#[test]
fn official_validator_index_is_uint64_and_activates_at_t1() {
    let owner = Address::repeat_byte(0x11);
    let validator = Address::repeat_byte(0x22);
    for fork in [
        TempoHardfork::Genesis,
        TempoHardfork::T1,
        TempoHardfork::T11,
    ] {
        let mut provider = TestStorageProvider::new(fork);
        StorageCtx::enter(&mut provider, || {
            let mut config = ValidatorConfig::new();
            config.initialize(owner).unwrap();
            config
                .add_validator(
                    owner,
                    IValidatorConfig::addValidatorCall {
                        newValidatorAddress: validator,
                        publicKey: B256::repeat_byte(1),
                        active: true,
                        inboundAddress: "127.0.0.1:8080".into(),
                        outboundAddress: "127.0.0.1:8081".into(),
                    },
                )
                .unwrap();
            let data = raw_call(
                "changeValidatorStatusByIndex(uint64,bool)",
                (0u64, false).abi_encode(),
            );
            assert_eq!(
                data,
                IValidatorConfig::changeValidatorStatusByIndexCall {
                    index: 0,
                    active: false
                }
                .abi_encode()
            );
            let output = config.call(&data, owner).unwrap();
            assert_eq!(output.reverted, !fork.is_t1());
            assert_eq!(config.validators(validator).unwrap().active, !fork.is_t1());
            if !fork.is_t1() {
                assert_eq!(&output.bytes[..4], &UnknownFunctionSelector::SELECTOR);
                // Gate before decoding malformed calldata.
                assert_eq!(config.call(&data[..4], owner).unwrap().bytes, output.bytes);
            } else {
                let max = raw_call(
                    "changeValidatorStatusByIndex(uint64,bool)",
                    (u64::MAX, false).abi_encode(),
                );
                let output = config.call(&max, owner).unwrap();
                assert!(output.reverted);
                assert_eq!(
                    &output.bytes[..4],
                    &IValidatorConfig::ValidatorNotFound::SELECTOR
                );
                let output = config.call(&data, validator).unwrap();
                assert!(output.reverted);
                assert_eq!(
                    &output.bytes[..4],
                    &IValidatorConfig::Unauthorized::SELECTOR
                );
            }
            let legacy = raw_call(
                "changeValidatorStatusByIndex(uint256,bool)",
                (U256::ZERO, false).abi_encode(),
            );
            assert_eq!(
                &config.call(&legacy, owner).unwrap().bytes[..4],
                &UnknownFunctionSelector::SELECTOR
            );
        });
    }
}

fn factory_call(salt: u8, logo: &str) -> ITIP20Factory::createToken_1Call {
    ITIP20Factory::createToken_1Call {
        name: "Test".into(),
        symbol: "TST".into(),
        currency: "USD".into(),
        quoteToken: PATH_USD_ADDRESS,
        admin: Address::repeat_byte(0x22),
        salt: B256::repeat_byte(salt),
        logoURI: logo.into(),
    }
}

fn setup_quote(provider: &mut TestStorageProvider, creator: Address) {
    StorageCtx::enter(provider, || {
        TIP20Token::from_address(PATH_USD_ADDRESS)
            .unwrap()
            .initialize(creator, "USD", "USD", "USD", Address::ZERO, creator)
            .unwrap();
    });
}

#[test]
fn official_factory_logo_overload_preserves_gate_state_and_events() {
    let creator = Address::repeat_byte(0x11);
    for fork in [TempoHardfork::T4, TempoHardfork::T5, TempoHardfork::T11] {
        for logo in ["", "https://example.com/logo.svg"] {
            let mut provider = TestStorageProvider::new(fork);
            setup_quote(&mut provider, creator);
            let call = factory_call(3, logo);
            let token_address = compute_tip20_address(creator, call.salt).0;
            let data = raw_call(
                "createToken(string,string,string,address,address,bytes32,string)",
                (
                    call.name.clone(),
                    call.symbol.clone(),
                    call.currency.clone(),
                    call.quoteToken,
                    call.admin,
                    call.salt,
                    call.logoURI.clone(),
                )
                    .abi_encode_params(),
            );
            assert_eq!(data, call.abi_encode());
            let output = StorageCtx::enter(&mut provider, || {
                TIP20Factory::new().call(&data, creator).unwrap()
            });
            if !fork.is_t5() {
                assert!(output.reverted);
                assert_eq!(&output.bytes[..4], &UnknownFunctionSelector::SELECTOR);
                let malformed = StorageCtx::enter(&mut provider, || {
                    TIP20Factory::new().call(&data[..4], creator).unwrap()
                });
                assert_eq!(malformed.bytes, output.bytes);
                assert!(provider.account(token_address).is_none());
                continue;
            }
            assert!(!output.reverted, "{fork:?}: {output:?}");
            assert_eq!(Address::abi_decode(&output.bytes).unwrap(), token_address);
            StorageCtx::enter(&mut provider, || {
                let token = TIP20Token::from_address(token_address).unwrap();
                assert_eq!(token.logo_uri().unwrap(), logo);
                assert!(token
                    .has_role(IRolesAuth::hasRoleCall {
                        account: call.admin,
                        role: DEFAULT_ADMIN_ROLE
                    })
                    .unwrap());
                assert!(!token
                    .has_role(IRolesAuth::hasRoleCall {
                        account: creator,
                        role: DEFAULT_ADMIN_ROLE
                    })
                    .unwrap());
            });
            let events: Vec<_> = provider
                .events(token_address)
                .iter()
                .filter(|event| event.topics()[0] == ITIP20::LogoURIUpdated::SIGNATURE_HASH)
                .collect();
            assert_eq!(events.len(), usize::from(!logo.is_empty()));
            if !logo.is_empty() {
                assert_eq!(
                    *events[0],
                    ITIP20::LogoURIUpdated {
                        updater: creator,
                        newLogoURI: logo.into()
                    }
                    .encode_log_data()
                );
            }
            let duplicate = StorageCtx::enter(&mut provider, || {
                TIP20Factory::new().call(&data, creator).unwrap()
            });
            assert!(duplicate.reverted);
            assert_eq!(
                &duplicate.bytes[..4],
                &ITIP20Factory::TokenAlreadyExists::SELECTOR
            );
        }
    }
    assert_eq!(
        ITIP20Factory::createTokenCall::SELECTOR,
        [0x68, 0x13, 0x04, 0x45]
    );
}

#[test]
fn official_factory_rejects_bad_logo_before_creating_state_and_static_calls() {
    let creator = Address::repeat_byte(0x11);
    for fork in [TempoHardfork::T5, TempoHardfork::T11] {
        for (logo, selector) in [
            (
                "javascript:alert(1)".to_owned(),
                ITIP20::InvalidLogoURI::SELECTOR,
            ),
            (
                format!("https:{}", "x".repeat(251)),
                ITIP20::LogoURITooLong::SELECTOR,
            ),
        ] {
            let mut provider = TestStorageProvider::new(fork);
            setup_quote(&mut provider, creator);
            let call = factory_call(4, &logo);
            let address = compute_tip20_address(creator, call.salt).0;
            let before = provider.storage_len();
            let output = StorageCtx::enter(&mut provider, || {
                TIP20Factory::new()
                    .call(&call.abi_encode(), creator)
                    .unwrap()
            });
            assert!(output.reverted);
            assert_eq!(&output.bytes[..4], &selector);
            assert!(provider.account(address).is_none());
            assert!(provider.events(address).is_empty());
            assert!(provider.events(super::TIP20_FACTORY_ADDRESS).is_empty());
            assert_eq!(provider.storage_len(), before);
        }
        let mut provider = TestStorageProvider::new(fork);
        setup_quote(&mut provider, creator);
        provider.set_static(true);
        let call = factory_call(5, "https://example.com");
        let address = compute_tip20_address(creator, call.salt).0;
        let output = StorageCtx::enter(&mut provider, || {
            TIP20Factory::new()
                .call(&call.abi_encode(), creator)
                .unwrap()
        });
        assert!(output.reverted);
        assert_eq!(&output.bytes[..4], &super::StaticCallNotAllowed::SELECTOR);
        assert!(provider.account(address).is_none());
    }
}

fn normalized_abi(mut abi: Value) -> Vec<String> {
    fn strip_param_names(value: &mut Value) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(strip_param_names),
            Value::Object(fields) => {
                fields.remove("name");
                fields.remove("internalType");
                fields.values_mut().for_each(strip_param_names);
            }
            _ => {}
        }
    }
    let items = abi.as_array_mut().unwrap();
    for item in items.iter_mut() {
        for field in ["inputs", "outputs"] {
            if let Some(params) = item.get_mut(field) {
                strip_param_names(params);
            }
        }
        item.sort_all_objects();
    }
    let mut items: Vec<_> = items
        .iter()
        .map(|item| serde_json::to_string(item).unwrap())
        .collect();
    items.sort();
    items
}

#[test]
fn adopted_abis_match_frozen_schema_with_only_reviewed_differences() {
    let baseline: Value = serde_json::from_str(include_str!("fixtures/abi-4d16f6b.json")).unwrap();
    let actual = [
        (
            "IAccountKeychain",
            serde_json::to_value(super::account_keychain::IAccountKeychain::abi::contract())
                .unwrap(),
        ),
        (
            "IAddressRegistry",
            serde_json::to_value(super::address_registry::IAddressRegistry::abi::contract())
                .unwrap(),
        ),
        (
            "ICurrentCommittee",
            serde_json::to_value(super::current_committee::ICurrentCommittee::abi::contract())
                .unwrap(),
        ),
        (
            "IFeeManager",
            serde_json::to_value(super::fee_manager::IFeeManager::abi::contract()).unwrap(),
        ),
        (
            "ITIPFeeAMM",
            serde_json::to_value(super::fee_manager::ITIPFeeAMM::abi::contract()).unwrap(),
        ),
        (
            "INonce",
            serde_json::to_value(super::nonce::INonce::abi::contract()).unwrap(),
        ),
        (
            "IReceivePolicyGuard",
            serde_json::to_value(super::receive_policy_guard::IReceivePolicyGuard::abi::contract())
                .unwrap(),
        ),
        (
            "ISignatureVerifier",
            serde_json::to_value(super::signature_verifier::ISignatureVerifier::abi::contract())
                .unwrap(),
        ),
        (
            "IStablecoinDEX",
            serde_json::to_value(super::stablecoin_dex::IStablecoinDEX::abi::contract()).unwrap(),
        ),
        (
            "IStorageCredits",
            serde_json::to_value(super::storage_credits::IStorageCredits::abi::contract()).unwrap(),
        ),
        (
            "ITIP20",
            serde_json::to_value(super::tip20::ITIP20::abi::contract()).unwrap(),
        ),
        (
            "IRolesAuth",
            serde_json::to_value(super::tip20::IRolesAuth::abi::contract()).unwrap(),
        ),
        (
            "ITIP20ChannelReserve",
            serde_json::to_value(
                super::tip20_channel_reserve::ITIP20ChannelReserve::abi::contract(),
            )
            .unwrap(),
        ),
        (
            "ITIP20Factory",
            serde_json::to_value(super::tip20_factory::ITIP20Factory::abi::contract()).unwrap(),
        ),
        (
            "ITIP403Registry",
            serde_json::to_value(super::tip403_registry::ITIP403Registry::abi::contract()).unwrap(),
        ),
        (
            "IValidatorConfig",
            serde_json::to_value(super::validator_config::IValidatorConfig::abi::contract())
                .unwrap(),
        ),
        (
            "IValidatorConfigV2",
            serde_json::to_value(super::validator_config_v2::IValidatorConfigV2::abi::contract())
                .unwrap(),
        ),
        (
            "IZoneFactory",
            serde_json::to_value(super::zone_factory::IZoneFactory::abi::contract()).unwrap(),
        ),
        (
            "IZonePortal",
            serde_json::to_value(super::zone_factory::IZonePortal::abi::contract()).unwrap(),
        ),
    ];
    // Independently frozen, reviewed differences against v1.14.0; not regenerated at test time.
    let delta: Value =
        serde_json::from_str(include_str!("fixtures/abi-v1.14-reviewed-delta.json")).unwrap();
    for (name, abi) in actual {
        let actual = normalized_abi(abi);
        let expected = normalized_abi(baseline[name].clone());
        let added: Vec<_> = actual
            .iter()
            .filter(|item| !expected.contains(item))
            .cloned()
            .collect();
        let removed: Vec<_> = expected
            .iter()
            .filter(|item| !actual.contains(item))
            .cloned()
            .collect();
        let empty = serde_json::json!([]);
        assert_eq!(
            added,
            normalized_abi(delta[name].get("added").unwrap_or(&empty).clone()),
            "{name} additions"
        );
        assert_eq!(
            removed,
            normalized_abi(delta[name].get("removed").unwrap_or(&empty).clone()),
            "{name} removals"
        );
    }
}
