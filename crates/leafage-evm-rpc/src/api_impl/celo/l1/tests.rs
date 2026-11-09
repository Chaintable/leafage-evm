use super::*;
use leafage_evm_chains::celo::{
    l1::{DONUT_BLOCK, ESPRESSO_BLOCK, GINGERBREAD_BLOCK},
    CELO_CHAIN_ID,
};
use leafage_evm_types::{AccountInfo, Address, Bytecode, CfgEnv, U256};
use revm::database::InMemoryDB;
use serde_json::{json, Value};

struct NoHeaders;
impl BlockIndex for NoHeaders {
    type Error = std::io::Error;
    fn get_block_by_id(&self, _: BlockId) -> Result<Option<BlockInfo>, Self::Error> {
        Ok(None)
    }
}

fn setup(height: u64) -> (CeloL1ApiImpl<NoHeaders>, BlockInfo, BlockEnv, InMemoryDB) {
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::LONDON);
    cfg.chain_id = CELO_CHAIN_ID;
    cfg.disable_base_fee = true;
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    let api = ApiImpl::new(
        NoHeaders,
        cfg,
        Some(CeloL1Config),
        None,
        None,
        None,
        true,
        false,
        String::new(),
        100,
        None,
        None,
        None,
    );
    let mut info = BlockInfo::default();
    info.inner.header.inner.number = height;
    info.inner.header.inner.base_fee_per_gas = (height >= GINGERBREAD_BLOCK).then_some(100);
    let block = BlockEnv {
        number: U256::from(height),
        gas_limit: 30_000_000,
        basefee: if height >= GINGERBREAD_BLOCK { 100 } else { 0 },
        ..Default::default()
    };
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        Address::repeat_byte(0x11),
        AccountInfo {
            balance: U256::from(1_000_000_000u64),
            ..Default::default()
        },
    );
    // GAS; MSTORE(0); RETURN(0,32), exposes intrinsic cost independently.
    db.insert_account_info(
        Address::repeat_byte(0x22),
        AccountInfo::default().with_code(Bytecode::new_raw(
            vec![0x5a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3].into(),
        )),
    );
    (api, info, block, db)
}

fn request(fields: Value) -> CallRequest {
    let mut value = json!({"from":Address::repeat_byte(0x11), "to":Address::repeat_byte(0x22), "gas":"0x186a0"});
    value
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    serde_json::from_value(value).unwrap()
}

fn execute(
    height: u64,
    fields: Value,
    estimation: bool,
) -> Result<ExecutionResult<HaltReason>, String> {
    let (api, info, block, db) = setup(height);
    let mut tx = api
        .create_txn_env(&info, &block, request(fields), &db, CELO_CHAIN_ID)
        .map_err(|e| e.to_string())?;
    if estimation {
        use crate::api_impl::core::TxSetter;
        tx.set_gas_estimation();
        api.transact_for_estimation(&block, &db, tx)
            .map_err(|e| e.to_string())
    } else {
        api.transact(&block, &db, tx).map_err(|e| e.to_string())
    }
}

#[test]
fn alternative_currency_charges_intrinsic_before_whitelist() {
    for height in [ESPRESSO_BLOCK, GINGERBREAD_BLOCK] {
        let error = execute(
            height,
            json!({"gas":"0xc350","feeCurrency":Address::repeat_byte(0xcc)}),
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("71000") && error.contains("50000"),
            "{error}"
        );
        assert!(execute(height, json!({"gas":"0xc350"}), false)
            .unwrap()
            .is_success());
    }
}

#[test]
fn unlisted_currency_is_rejected_in_call_and_estimate() {
    for estimation in [false, true] {
        let error = execute(
            ESPRESSO_BLOCK,
            json!({"feeCurrency":Address::repeat_byte(0xcc)}),
            estimation,
        )
        .unwrap_err();
        assert!(error.contains("whitelist"), "{error}");
    }
}

#[test]
fn gingerbread_rejects_gateway_fields_but_accepts_explicit_zero_fee() {
    for fields in [
        json!({"gatewayFee":"0x1"}),
        json!({"gatewayFeeRecipient":Address::ZERO}),
    ] {
        let error = execute(GINGERBREAD_BLOCK, fields.clone(), false).unwrap_err();
        assert!(error.contains("gateway"), "{error}");
        assert!(execute(GINGERBREAD_BLOCK - 1, fields, false)
            .unwrap()
            .is_success());
    }
    assert!(
        execute(GINGERBREAD_BLOCK, json!({"gatewayFee":"0x0"}), false)
            .unwrap()
            .is_success()
    );
}

#[test]
fn historical_call_access_list_is_charged_without_typed_tx_validation() {
    let result = execute(DONUT_BLOCK-1, json!({"accessList":[{"address":Address::repeat_byte(0x22),"storageKeys":[format!("0x{:064x}",0)]}]}), false).unwrap();
    assert_eq!(
        U256::from_be_slice(result.output().unwrap()),
        U256::from(74698)
    );
}

#[test]
fn historical_fee_fields_follow_native_message_conversion() {
    let result = execute(
        DONUT_BLOCK,
        json!({"maxFeePerGas":"0x1","maxPriorityFeePerGas":"0x1"}),
        false,
    )
    .unwrap();
    assert!(result.is_success());
    let error = execute(GINGERBREAD_BLOCK, json!({"gasPrice":"0x1"}), false).unwrap_err();
    assert!(error.contains("base fee"), "{error}");
    // Native estimation zeroes all gas prices and skips fee debit/credit.
    assert!(execute(GINGERBREAD_BLOCK, json!({"gasPrice":"0x1"}), true)
        .unwrap()
        .is_success());
}

#[test]
fn archive_base_fee_does_not_activate_dynamic_call_fees_early() {
    for height in [DONUT_BLOCK, ESPRESSO_BLOCK] {
        let (api, mut info, mut block, mut db) = setup(height);
        // The archive may store a synthesized gas-price minimum in this field.
        info.inner.header.inner.base_fee_per_gas = Some(100);
        block.basefee = 100;
        db.insert_account_info(
            Address::repeat_byte(0x22),
            AccountInfo::default().with_code(Bytecode::new_raw(
                vec![0x3a, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3].into(),
            )),
        );
        let tx = api
            .create_txn_env(
                &info,
                &block,
                request(json!({"maxFeePerGas":"0x1","maxPriorityFeePerGas":"0x1"})),
                &db,
                CELO_CHAIN_ID,
            )
            .unwrap();
        let result = api.transact(&block, &db, tx).unwrap();
        assert_eq!(U256::from_be_slice(result.output().unwrap()), U256::ZERO);
    }
}

#[test]
fn invalid_celo_fields_are_not_silently_ignored() {
    assert!(execute(GINGERBREAD_BLOCK, json!({"feeCurrency":"garbage"}), false).is_err());
    assert!(execute(
        GINGERBREAD_BLOCK,
        json!({"maxFeeInFeeCurrency":"0x0"}),
        false
    )
    .is_err());
    assert!(execute(DONUT_BLOCK - 1, json!({"ethCompatible":true}), false).is_err());
}

#[test]
fn fee_credit_rejects_broken_registry_but_allows_missing_registry() {
    use alloy::primitives::address;
    for (code, allowed) in [
        (vec![0xfe], false),
        (vec![0x60, 0, 0x60, 0, 0xfd], true),
        (return_data(&[0; 31]), false),
        (return_data(&[0; 33]), false),
        (vec![0], true),
    ] {
        let (api, info, block, mut db) = setup(GINGERBREAD_BLOCK);
        db.insert_account_info(
            address!("000000000000000000000000000000000000ce10"),
            AccountInfo::default().with_code(Bytecode::new_raw(code.clone().into())),
        );
        let tx = api
            .create_txn_env(&info, &block, request(json!({})), &db, CELO_CHAIN_ID)
            .unwrap();
        assert_eq!(
            api.transact(&block, &db, tx).is_ok(),
            allowed,
            "registry code: {code:?}"
        );
    }
}

#[test]
fn gingerbread_keeps_ethereum_header_fields() {
    for height in [GINGERBREAD_BLOCK - 1, GINGERBREAD_BLOCK] {
        let (api, info, _, _) = setup(height);
        let mut value = serde_json::to_value(&info).unwrap();
        api.format_block_response(&info, &mut value);
        for key in ["nonce", "mixHash", "sha3Uncles", "uncles"] {
            assert_eq!(
                value.get(key).is_some(),
                height >= GINGERBREAD_BLOCK,
                "{key}"
            );
        }
    }
}

fn return_data(data: &[u8]) -> Vec<u8> {
    let length = (data.len() as u16).to_be_bytes();
    let mut code = vec![
        0x61, length[0], length[1], 0x61, 0, 15, 0x60, 0, 0x39, 0x61, length[0], length[1], 0x60,
        0, 0xf3,
    ];
    code.extend_from_slice(data);
    code
}

fn fee_contracts(db: &mut InMemoryDB) {
    use alloy::primitives::{address, keccak256};
    let currency = Address::repeat_byte(0xcc);
    let whitelist = Address::repeat_byte(0xdd);
    let parameters = Address::repeat_byte(0xee);
    let mut registry = Vec::new();
    let mut jumps = Vec::new();
    for (name, address) in [
        ("FeeCurrencyWhitelist", whitelist),
        ("BlockchainParameters", parameters),
    ] {
        registry.extend([0x60, 4, 0x35, 0x7f]);
        registry.extend_from_slice(keccak256(name).as_slice());
        registry.extend([0x14, 0x61]);
        let at = registry.len();
        registry.extend([0, 0, 0x57]);
        jumps.push((at, address));
    }
    registry.extend([0x60, 0, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3]);
    for (at, address) in jumps {
        let target = (registry.len() as u16).to_be_bytes();
        registry[at..at + 2].copy_from_slice(&target);
        registry.extend([0x5b, 0x73]);
        registry.extend_from_slice(address.as_slice());
        registry.extend([0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3]);
    }
    let mut allow = Vec::new();
    for word in [
        U256::from(32),
        U256::from(1),
        U256::from_be_slice(currency.as_slice()),
    ] {
        allow.extend(word.to_be_bytes::<32>());
    }
    let mut token = Vec::new();
    let mut jumps = Vec::new();
    for selector in [0x70a08231u32, 0x58cf9672, 0x6a30b253] {
        token.extend([0x60, 0, 0x35, 0x60, 224, 0x1c, 0x63]);
        token.extend(selector.to_be_bytes());
        token.extend([0x14, 0x61]);
        let at = token.len();
        token.extend([0, 0, 0x57]);
        jumps.push(at);
    }
    token.push(0);
    for (at, body) in jumps.into_iter().zip([
        vec![0x5b, 0x60, 0, 0x54, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3],
        vec![0x5b, 0x60, 36, 0x35, 0x60, 0, 0x54, 0x03, 0x60, 0, 0x55, 0],
        vec![0x5b, 0x60, 132, 0x35, 0x60, 0, 0x54, 0x01, 0x60, 0, 0x55, 0],
    ]) {
        let target = (token.len() as u16).to_be_bytes();
        token[at..at + 2].copy_from_slice(&target);
        token.extend(body);
    }
    for (address, code) in [
        (
            address!("000000000000000000000000000000000000ce10"),
            registry,
        ),
        (whitelist, return_data(&allow)),
        (
            parameters,
            return_data(&U256::from(70_000).to_be_bytes::<32>()),
        ),
        (currency, token),
    ] {
        db.insert_account_info(
            address,
            AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
        );
    }
    db.insert_account_storage(currency, U256::ZERO, U256::from(1_000_000_000u64))
        .unwrap();
}

#[test]
fn currency_intrinsic_comes_from_historical_configuration() {
    let (api, info, block, mut db) = setup(ESPRESSO_BLOCK);
    fee_contracts(&mut db);
    let tx = api
        .create_txn_env(
            &info,
            &block,
            request(json!({"gas":"0x13880","feeCurrency":Address::repeat_byte(0xcc)})),
            &db,
            CELO_CHAIN_ID,
        )
        .unwrap();
    let error = api.transact(&block, &db, tx).unwrap_err().to_string();
    assert!(
        error.contains("91000") && error.contains("80000"),
        "{error}"
    );
}

#[test]
fn missing_exchange_rate_uses_native_base_fee() {
    let (api, info, block, mut db) = setup(GINGERBREAD_BLOCK);
    fee_contracts(&mut db);
    let tx = api
        .create_txn_env(
            &info,
            &block,
            request(json!({"feeCurrency":Address::repeat_byte(0xcc),"gasPrice":"0x1"})),
            &db,
            CELO_CHAIN_ID,
        )
        .unwrap();
    let error = api.transact(&block, &db, tx).unwrap_err().to_string();
    assert!(error.contains("base fee"), "{error}");
}

#[test]
fn currency_debit_is_visible_to_call_but_estimation_skips_debit_and_credit() {
    use crate::api_impl::core::TxSetter;
    for height in [DONUT_BLOCK - 1, ESPRESSO_BLOCK, GINGERBREAD_BLOCK] {
        let (api, info, block, mut db) = setup(height);
        fee_contracts(&mut db);
        let price = if height >= GINGERBREAD_BLOCK {
            // Keep a nonzero tip when the missing FeeHandler refunds base fees.
            101u64
        } else {
            1
        };
        let after_debit = 1_000_000_000u64 - 100_000 * price;
        let request = request(
            json!({"to":Address::repeat_byte(0xcc),"feeCurrency":Address::repeat_byte(0xcc),"gasPrice":format!("0x{price:x}"),
            "data":"0x70a082310000000000000000000000001111111111111111111111111111111111111111"}),
        );
        let tx = api
            .create_txn_env(&info, &block, request, &db, CELO_CHAIN_ID)
            .unwrap();
        let ordinary = api.transact(&block, &db, tx.clone()).unwrap();
        assert!(ordinary.is_success(), "{ordinary:?}");
        assert_eq!(
            U256::from_be_slice(ordinary.output().unwrap()),
            U256::from(after_debit)
        );
        let mut estimate = tx.clone();
        estimate.set_gas_estimation();
        let estimate = api.transact_for_estimation(&block, &db, estimate).unwrap();
        assert_eq!(
            U256::from_be_slice(estimate.output().unwrap()),
            U256::from(1_000_000_000u64)
        );
        let (inspected, _) = api
            .inspect_tx_commit(
                &block,
                &mut db,
                TracingInspectorConfig::default_parity(),
                |_| (),
                tx,
            )
            .unwrap();
        assert_eq!(ordinary, inspected);
        let balance = db
            .storage_ref(Address::repeat_byte(0xcc), U256::ZERO)
            .unwrap();
        assert!(
            balance > U256::from(after_debit) && balance < U256::from(1_000_000_000u64),
            "{balance}"
        );
    }
}
