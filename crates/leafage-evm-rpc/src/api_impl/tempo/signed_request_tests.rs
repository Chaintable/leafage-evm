use super::*;
use alloy::primitives::{address, bytes, Address, Signature, B256, U256};
use leafage_evm_chains::tempo::{fee_payer as fp, precompile::storage_types::StorageKey};
use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};
use serde_json::{json, Value};

const CALLER: Address = address!("1111111111111111111111111111111111111111");
const DELEGATE: Address = address!("1111111111111111111111111111111111111112");
const AUTHORITY: Address = address!("6008858e353d5cdf21b8ae9b6bf248630db38445");
const TOKEN: Address = address!("20c0000000000000000000000000000000000000");

fn block() -> BlockEnv {
    BlockEnv {
        timestamp: U256::from(1_788_743_086u64),
        gas_limit: 100_000_000,
        ..Default::default()
    }
}

fn request() -> Value {
    json!({
        "from":CALLER,"to":TOKEN,"gas":"0xf4240","chainId":"0x1079","nonce":"0x0",
        "maxFeePerGas":"0x0","maxPriorityFeePerGas":"0x0","feeToken":TOKEN,
        "data":"0x70a082310000000000000000000000001111111111111111111111111111111111111111"
    })
}

fn signature() -> Value {
    json!({"r":"0x1","s":"0x1","yParity":"0x0"})
}

fn authorization() -> Value {
    let mut signature = signature();
    signature["type"] = json!("secp256k1");
    json!({"chainId":"0x0","address":DELEGATE,"nonce":"0x0","signature":signature})
}

fn tx(request: Value, db: &InMemoryDB) -> TempoTxEnv {
    tests::review_api()
        .create_txn_env(
            &Default::default(),
            &block(),
            serde_json::from_value(request).unwrap(),
            db,
            4217,
        )
        .unwrap()
}

#[test]
fn review_sponsor_warms_its_own_balance_and_controls_allowance() {
    let api = tests::review_api();
    let mut db = InMemoryDB::default();
    let marker = Bytecode::new_legacy(bytes!("ef"));
    db.insert_account_info(
        TOKEN,
        AccountInfo::new(U256::ZERO, 1, marker.hash_slow(), marker),
    );
    db.insert_account_storage(TOKEN, CALLER.mapping_slot(U256::from(9)), U256::from(2500))
        .unwrap();
    let mut json = request();
    let plain = api.transact(&block(), &db, tx(json.clone(), &db)).unwrap();
    assert_eq!(plain.gas().used(), 271_644);
    json["feePayerSignature"] = signature();
    // A signed sponsor takes precedence over the nonstandard legacy override.
    json["feePayer"] = json!(CALLER);
    let sponsored = tx(json.clone(), &db);
    let payer = sponsored.tempo_fields.as_ref().unwrap().fee_payer.unwrap();
    assert_ne!(payer, CALLER);
    let result = api.transact(&block(), &db, sponsored).unwrap();
    assert_eq!(result.gas().used(), 273_644);
    assert_eq!(result.output(), plain.output());

    json["maxFeePerGas"] = json!("0xe8d4a51000"); // 1e12
    let request: CallRequest = serde_json::from_value(json).unwrap();
    let sponsored = api
        .create_txn_env(&Default::default(), &block(), request.clone(), &db, 4217)
        .unwrap();
    let payer = sponsored.tempo_fields.as_ref().unwrap().fee_payer.unwrap();
    db.insert_account_storage(
        TOKEN,
        payer.mapping_slot(U256::from(9)),
        U256::from(123_456),
    )
    .unwrap();
    assert_eq!(
        api.gas_allowance(&request, &sponsored, &db, &block())
            .unwrap(),
        123_456
    );
}

#[test]
fn review_sponsor_hash_commits_to_original_complete_request() {
    let mut json = request();
    json["nonce"] = json!("0x7");
    json["maxFeePerGas"] = json!("0x64");
    json["maxPriorityFeePerGas"] = json!("0x2");
    json["nonceKey"] = json!("0x8");
    json["validAfter"] = json!("0x1");
    json["validBefore"] = json!("0xffffffff");
    json["calls"] = json!([{"to":DELEGATE,"value":"0x0","input":"0x1234"}]);
    json["accessList"] = json!([{"address":DELEGATE,"storageKeys":[B256::ZERO]}]);
    json["aaAuthorizationList"] = json!([authorization()]);
    let mut key_signature = signature();
    key_signature["type"] = json!("secp256k1");
    json["keyAuthorization"] = json!({
        "chainId":"0x1079","keyType":"p256","keyId":DELEGATE,
        "expiry":"0xffffffff","limits":[],"signature":key_signature
    });
    json["feePayerSignature"] = signature();
    let original: CallRequest = serde_json::from_value(json.clone()).unwrap();
    let signed_auth: fp::TempoSignedAuthorization =
        serde_json::from_value(authorization()).unwrap();
    let key_authorization = fp::SignedKeyAuthorization {
        authorization: fp::KeyAuthorization {
            chain_id: 4217,
            key_type: fp::SignatureType::P256,
            key_id: DELEGATE,
            expiry: Some(0xffffffff),
            limits: Some(vec![]),
            allowed_calls: None,
            witness: None,
            is_admin: false,
            account: None,
        },
        signature: fp::PrimitiveSignature::Secp256k1(serde_json::from_value(signature()).unwrap()),
    };
    let expected = fp::recover_fee_payer(
        &serde_json::from_value(signature()).unwrap(),
        4217,
        2,
        100,
        1_000_000,
        &[
            fp::Call {
                to: Some(DELEGATE),
                value: U256::ZERO,
                input: bytes!("1234"),
            },
            fp::Call {
                to: Some(TOKEN),
                value: U256::ZERO,
                input: original.input.clone().into_input().unwrap(),
            },
        ],
        &original.access_list.clone().unwrap(),
        U256::from(8),
        7,
        Some(0xffffffff),
        Some(1),
        Some(TOKEN),
        CALLER,
        &[signed_auth],
        Some(&key_authorization),
    )
    .unwrap();
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        CALLER,
        AccountInfo {
            nonce: 99,
            ..Default::default()
        },
    );
    let resolved = tx(json.clone(), &db)
        .tempo_fields
        .unwrap()
        .fee_payer
        .unwrap();
    assert_eq!(resolved, expected);
    for field in [
        "calls",
        "accessList",
        "aaAuthorizationList",
        "keyAuthorization",
        "to",
    ] {
        let mut modified = json.clone();
        modified.as_object_mut().unwrap().remove(field);
        assert_ne!(
            tx(modified, &db).tempo_fields.unwrap().fee_payer.unwrap(),
            expected,
            "{field}"
        );
    }
    json.as_object_mut().unwrap().remove("nonceKey");
    let normalized = tx(json.clone(), &db);
    assert_eq!(normalized.base.nonce, 99);
    assert_eq!(
        normalized.tempo_fields.unwrap().fee_payer,
        tx(json, &InMemoryDB::default())
            .tempo_fields
            .unwrap()
            .fee_payer,
        "state-derived simulation nonce must not change the signed payer"
    );
}

#[test]
fn review_self_sponsorship_gate_and_incomplete_requests() {
    let signer = k256::ecdsa::SigningKey::from_slice(&[1u8; 32]).unwrap();
    let caller = Address::from_public_key(signer.verifying_key());
    let hash = fp::fee_payer_signature_hash(
        4217,
        0,
        0,
        1_000_000,
        &[fp::Call {
            to: Some(DELEGATE),
            value: U256::ZERO,
            input: Default::default(),
        }],
        &Default::default(),
        U256::ZERO,
        0,
        None,
        None,
        None,
        caller,
        &[],
        None,
    );
    let signature: Signature = signer
        .sign_prehash_recoverable(hash.as_slice())
        .unwrap()
        .into();
    let json = json!({
        "from":caller,"to":DELEGATE,"gas":"0xf4240","nonce":"0x0",
        "maxFeePerGas":"0x0","maxPriorityFeePerGas":"0x0","feePayerSignature":signature
    });
    let request: CallRequest = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(
        resolve_fee_payer(&request, &[], TempoHardfork::T1C).unwrap(),
        Some(caller)
    );
    assert!(resolve_fee_payer(&request, &[], TempoHardfork::T2)
        .unwrap_err()
        .message()
        .contains("cannot resolve to sender"));
    for field in ["nonce", "gas", "maxFeePerGas", "maxPriorityFeePerGas", "to"] {
        let mut incomplete = json.clone();
        incomplete.as_object_mut().unwrap().remove(field);
        let request = serde_json::from_value(incomplete).unwrap();
        assert!(
            resolve_fee_payer(&request, &[], TempoHardfork::T1C).is_err(),
            "{field}"
        );
    }
    let mut bad = request;
    bad.tempo.as_mut().unwrap().fee_payer_signature =
        Some(Signature::new(U256::ZERO, U256::ZERO, false));
    assert!(resolve_fee_payer(&bad, &[], TempoHardfork::T1C).is_err());
}

#[test]
fn review_signed_authorizations_skip_invalid_recovery_nonce_chain_and_code() {
    let api = tests::review_api();
    for case in [
        "valid",
        "invalid-signature",
        "wrong-chain",
        "wrong-nonce",
        "has-code",
        "keychain",
    ] {
        let mut auth = authorization();
        // Signature must win over forged legacy hints.
        auth["authority"] = json!(AUTHORITY);
        auth["sigType"] = json!("webAuthn");
        auth["isKeychain"] = json!(true);
        let mut db = InMemoryDB::default();
        let code = Bytecode::new_legacy(bytes!("602a60005260206000f3"));
        db.insert_account_info(
            DELEGATE,
            AccountInfo::new(U256::ZERO, 1, code.hash_slow(), code),
        );
        match case {
            "invalid-signature" => auth["signature"]["r"] = json!("0x0"),
            "wrong-chain" => auth["chainId"] = json!("0x1"),
            "wrong-nonce" => db.insert_account_info(
                AUTHORITY,
                AccountInfo {
                    nonce: 1,
                    ..Default::default()
                },
            ),
            "has-code" => {
                let code = Bytecode::new_legacy(bytes!("00"));
                db.insert_account_info(
                    AUTHORITY,
                    AccountInfo::new(U256::ZERO, 0, code.hash_slow(), code),
                );
            }
            "keychain" => {
                auth["signature"] =
                    json!({"userAddress":AUTHORITY,"version":"v2","signature":auth["signature"]})
            }
            _ => {}
        }
        let target = if case == "wrong-chain" {
            parse_tempo_authorization(&serde_json::from_value(auth.clone()).unwrap())
                .unwrap()
                .authority
                .unwrap()
        } else {
            AUTHORITY
        };
        let tx = tx(
            json!({"from":CALLER,"to":target,"gas":"0x1e8480","aaAuthorizationList":[auth]}),
            &db,
        );
        let result = api.transact(&block(), &db, tx).unwrap();
        assert!(result.is_success(), "{case}");
        if case == "valid" {
            assert_eq!(
                result.output().unwrap().as_ref(),
                U256::from(42).to_be_bytes::<32>()
            );
        } else {
            assert!(result.output().unwrap().is_empty(), "{case}");
        }
    }
    for missing in ["chainId", "address", "nonce"] {
        let mut auth = authorization();
        auth.as_object_mut().unwrap().remove(missing);
        let value = serde_json::from_value(auth).unwrap();
        assert!(parse_tempo_authorization(&value).is_err(), "{missing}");
    }

    // A nonempty AA list must not fall back to the standard 7702 list even
    // when none of its signatures recover an authority.
    let mut auth = authorization();
    auth["signature"]["r"] = json!("0x0");
    let mut db = InMemoryDB::default();
    let code = Bytecode::new_legacy(bytes!("602a60005260206000f3"));
    db.insert_account_info(
        DELEGATE,
        AccountInfo::new(U256::ZERO, 1, code.hash_slow(), code),
    );
    let result = api.transact(&block(), &db, tx(json!({
        "from":CALLER,"to":AUTHORITY,"gas":"0x1e8480","aaAuthorizationList":[auth],
        "authorizationList":[{"chainId":"0x0","address":DELEGATE,"nonce":"0x0","r":"0x1","s":"0x1","yParity":"0x0"}]
    }), &db)).unwrap();
    assert!(result.output().unwrap().is_empty());
}

#[test]
fn review_real_authorization_signature_gas_and_keychain_versions() {
    let api = tests::review_api();
    let mut p256 = json!({
        "type":"p256","r":B256::ZERO,"s":B256::ZERO,
        "pubKeyX":B256::ZERO,"pubKeyY":B256::ZERO,"preHash":false
    });
    let mut webauthn = p256.clone();
    webauthn["type"] = json!("webAuthn");
    webauthn["webauthnData"] = json!("0x0001"); // 4 + 16 gas, not a mocked length
    p256.as_object_mut().unwrap().remove("webauthnData");
    let mut baseline = None;
    for (signature, extra) in [
        (authorization()["signature"].clone(), 0),
        (p256.clone(), 5000),
        (webauthn, 5020),
        (
            json!({"userAddress":CALLER,"version":"v2","signature":p256.clone()}),
            8000,
        ),
    ] {
        let mut auth = authorization();
        auth["signature"] = signature;
        let db = InMemoryDB::default();
        let result = api
            .transact(
                &block(),
                &db,
                tx(
                    json!({
                        "from":CALLER,"to":DELEGATE,"gas":"0x1e8480","aaAuthorizationList":[auth]
                    }),
                    &db,
                ),
            )
            .unwrap();
        let base = *baseline.get_or_insert(result.gas().used());
        assert_eq!(result.gas().used(), base + extra);
    }
    for (version, timestamp, accepted) in [
        ("v1", 1_773_327_599, true),
        ("v2", 1_773_327_599, false),
        ("v1", 1_773_327_600, false),
        ("v2", 1_773_327_600, true),
    ] {
        let mut auth = authorization();
        auth["signature"] = json!({"userAddress":CALLER,"version":version,"signature":p256});
        let mut block = block();
        block.timestamp = U256::from(timestamp as u64);
        let db = InMemoryDB::default();
        let request = serde_json::from_value(
            json!({"from":CALLER,"to":DELEGATE,"gas":"0x1e8480","aaAuthorizationList":[auth]}),
        )
        .unwrap();
        let tx = api
            .create_txn_env(&Default::default(), &block, request, &db, 4217)
            .unwrap();
        assert_eq!(
            api.transact(&block, &db, tx).is_ok(),
            accepted,
            "{version}/{timestamp}"
        );
    }
}
