use super::consensus::{CeloChainData, CeloHeader};
use super::*;
use revm::{
    context::TxEnv,
    database::InMemoryDB,
    primitives::{Address, Bytes, TxKind, U256},
    state::{AccountInfo, Bytecode},
    ExecuteEvm,
};
use serde::Deserialize;

struct NoChain;
impl CeloChainData for NoChain {
    fn header(&self, number: u64) -> Result<CeloHeader, String> {
        Err(format!("no header {number}"))
    }
}

#[derive(Deserialize)]
struct Vector {
    name: String,
    address: u8,
    input: String,
    expected: String,
    error: bool,
    gingerbread: bool,
    gas: Option<u64>,
}

#[test]
fn native_celo_precompile_vectors() {
    let vectors: Vec<Vector> =
        serde_json::from_str(include_str!("../../../testdata/celo_l1/precompiles.json")).unwrap();
    for vector in vectors {
        let input =
            revm::primitives::hex::decode(&vector.input[..vector.input.len() / 2 * 2]).unwrap();
        let fork = if vector.gingerbread {
            L1Fork::Gingerbread
        } else {
            L1Fork::Donut
        };
        let result = pure::run(vector.address, &input, fork);
        if vector.error {
            assert!(result.is_err(), "{}", vector.name);
        } else {
            assert_eq!(
                result.unwrap_or_else(|e| panic!("{}: {e:?}", vector.name)),
                Bytes::from(revm::primitives::hex::decode(&vector.expected).unwrap()),
                "{}",
                vector.name
            );
        }
        if let Some(gas) = vector.gas {
            assert_eq!(
                pure::required_gas(vector.address, &input),
                gas,
                "gas {}",
                vector.name
            );
        }
    }
}

#[test]
fn native_bls_public_key_encoding() {
    #[derive(Deserialize)]
    struct Key {
        public_key: String,
        uncompressed: String,
    }
    let keys: Vec<Key> =
        serde_json::from_str(include_str!("../../../testdata/celo_l1/public_keys.json")).unwrap();
    for key in keys {
        let raw = revm::primitives::hex::decode(key.uncompressed).unwrap();
        let mut expected = vec![0u8; 256];
        for (i, limb) in raw.chunks_exact(48).enumerate() {
            for (j, b) in limb.iter().enumerate() {
                expected[i * 64 + 63 - j] = *b;
            }
        }
        assert_eq!(
            crypto::public_key_uncompressed(
                &revm::primitives::hex::decode(key.public_key).unwrap()
            )
            .unwrap()
            .as_ref(),
            expected
        );
    }
}

#[test]
fn native_mainnet_headers_validators_and_seals() {
    use std::{collections::BTreeMap, io::Read};
    #[derive(Deserialize)]
    struct Header {
        number: u64,
        hash: String,
        rlp: String,
    }
    #[derive(Deserialize)]
    struct Call {
        name: String,
        block: u64,
        address: u8,
        input: String,
        expected: String,
        error: bool,
    }
    #[derive(Deserialize)]
    struct Fixture {
        headers: Vec<Header>,
        calls: Vec<Call>,
    }
    struct Chain(BTreeMap<u64, CeloHeader>);
    impl CeloChainData for Chain {
        fn header(&self, number: u64) -> Result<CeloHeader, String> {
            self.0
                .get(&number)
                .cloned()
                .ok_or_else(|| format!("missing fixture {number}"))
        }
    }
    let mut data = Vec::new();
    flate2::read::GzDecoder::new(
        &include_bytes!("../../../testdata/celo_l1/consensus.json.gz")[..],
    )
    .read_to_end(&mut data)
    .unwrap();
    let fixture: Fixture = serde_json::from_slice(&data).unwrap();
    let mut chain = Chain(BTreeMap::new());
    for h in fixture.headers {
        let raw = revm::primitives::hex::decode(h.rlp).unwrap();
        let header = CeloHeader::decode(&raw).unwrap();
        assert_eq!(header.encode().as_ref(), raw, "RLP {}", h.number);
        assert_eq!(header.hash().to_string(), h.hash, "hash {}", h.number);
        if h.number < GINGERBREAD_BLOCK {
            let fields: Vec<Bytes> = alloy_rlp::decode_exact(&raw).unwrap();
            let archive = alloy::consensus::Header {
                parent_hash: revm::primitives::B256::from_slice(&fields[0]),
                beneficiary: Address::from_slice(&fields[1]),
                state_root: revm::primitives::B256::from_slice(&fields[2]),
                transactions_root: revm::primitives::B256::from_slice(&fields[3]),
                receipts_root: revm::primitives::B256::from_slice(&fields[4]),
                logs_bloom: alloy::primitives::Bloom::from_slice(&fields[5]),
                number: h.number,
                gas_used: U256::from_be_slice(&fields[7]).to(),
                timestamp: U256::from_be_slice(&fields[8]).to(),
                extra_data: fields[9].clone(),
                // None of these compatibility fields belongs to the legacy
                // Celo consensus header, even when the archive fills them.
                ommers_hash: alloy::consensus::constants::EMPTY_OMMER_ROOT_HASH,
                gas_limit: 32_000_000,
                base_fee_per_gas: Some(777),
                ..Default::default()
            };
            let normalized = CeloHeader::from_header(&archive);
            assert_eq!(
                normalized.encode(),
                header.encode(),
                "archive RLP {}",
                h.number
            );
            assert_eq!(
                normalized.hash(),
                header.hash(),
                "archive hash {}",
                h.number
            );
        }
        chain.0.insert(h.number, header);
    }
    for c in fixture.calls {
        let input = revm::primitives::hex::decode(c.input).unwrap();
        let result = if c.address == 0xf6 || c.address == 0xf7 {
            pure::run(c.address, &input, L1Fork::Gingerbread)
        } else {
            consensus::run(c.address, &input, c.block, true, &chain)
        };
        if c.error {
            assert!(
                matches!(result, Err(precompile::Failure::Invalid)),
                "{}: {result:?}",
                c.name
            );
        } else {
            assert_eq!(
                result
                    .unwrap_or_else(|e| panic!("{}: {e:?}", c.name))
                    .as_ref(),
                revm::primitives::hex::decode(c.expected).unwrap(),
                "{}",
                c.name
            );
        }
    }
}

#[test]
fn fork_address_sets_and_migration_boundary() {
    assert_eq!(L1Fork::at(DONUT_BLOCK - 1).unwrap(), L1Fork::Istanbul);
    assert_eq!(L1Fork::at(DONUT_BLOCK).unwrap(), L1Fork::Donut);
    assert_eq!(L1Fork::at(ESPRESSO_BLOCK).unwrap(), L1Fork::Espresso);
    assert_eq!(L1Fork::at(GINGERBREAD_BLOCK).unwrap(), L1Fork::Gingerbread);
    assert!(L1Fork::at(super::super::L2_MIGRATION_BLOCK).is_err());
    for id in 0xe2..=0xf3 {
        assert!(!L1Fork::Istanbul.precompile_active(id));
        assert!(L1Fork::Donut.precompile_active(id));
        assert!(!L1Fork::Gingerbread.precompile_active(id));
    }
    assert!(L1Fork::Gingerbread.precompile_active(0xe1));
    assert!(L1Fork::Gingerbread.precompile_active(0xfd));
}

#[test]
fn celo_block_opcodes_follow_the_native_activation_schedule() {
    for height in [1, DONUT_BLOCK, ESPRESSO_BLOCK, GINGERBREAD_BLOCK] {
        for opcode in [0x44, 0x45, 0x48] {
            let destination = Address::repeat_byte(0x22);
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                destination,
                AccountInfo::default().with_code(Bytecode::new_raw(
                    vec![opcode, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3].into(),
                )),
            );
            let mut evm = evm::create_celo_l1_evm(
                BlockEnv {
                    number: U256::from(height),
                    gas_limit: 1_000_000,
                    basefee: 7,
                    ..Default::default()
                },
                config(),
                db,
                revm::inspector::NoOpInspector,
                &NoChain,
            )
            .unwrap();
            let result = evm
                .transact(TxEnv {
                    chain_id: Some(super::super::CELO_CHAIN_ID),
                    kind: TxKind::Call(destination),
                    gas_limit: 100_000,
                    ..Default::default()
                })
                .unwrap();
            let active = opcode != 0x44 && height >= GINGERBREAD_BLOCK;
            assert_eq!(
                result.result.is_success(),
                active,
                "height={height}, opcode={opcode:x}"
            );
            if active {
                assert_eq!(
                    result.result.output().unwrap(),
                    &pure::word(if opcode == 0x45 { 1_000_000 } else { 7 })
                );
            }
        }
    }
}

fn token_db(revert: bool) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        Address::repeat_byte(0x11),
        AccountInfo {
            balance: U256::from(10),
            ..Default::default()
        },
    );
    let mut registry = vec![0x73];
    registry.extend(super::super::CELO_TOKEN.as_slice());
    registry.extend([0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3]);
    db.insert_account_info(
        Address::from_word(U256::from(0xce10).into()),
        AccountInfo::default().with_code(Bytecode::new_raw(registry.into())),
    );
    let mut code = vec![
        0x36, 0x60, 0, 0x60, 0, 0x37, 0x60, 0, 0x60, 0, 0x36, 0x60, 0, 0x60, 0, 0x60, 0xfd, 0x5a,
        0xf1, 0x50,
    ];
    if revert {
        code.extend([0x60, 0, 0x60, 0, 0xfd]);
    } else {
        code.extend([0x3d, 0x60, 0, 0x60, 0, 0x3e, 0x3d, 0x60, 0, 0xf3]);
    }
    db.insert_account_info(
        super::super::CELO_TOKEN,
        AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
    );
    db
}

fn config() -> CfgEnv<MainnetSpecId> {
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::LONDON);
    cfg.chain_id = super::super::CELO_CHAIN_ID;
    cfg.disable_base_fee = true;
    cfg.disable_eip3607 = true;
    cfg
}

#[test]
fn token_transfer_uses_live_registry_returns_input_and_rolls_back() {
    let sender = Address::repeat_byte(0x11);
    let to = Address::repeat_byte(0x22);
    let mut input = vec![0; 96];
    input[12..32].copy_from_slice(sender.as_slice());
    input[44..64].copy_from_slice(to.as_slice());
    input[95] = 3;
    for revert in [false, true] {
        let mut evm = evm::create_celo_l1_evm(
            BlockEnv {
                number: U256::from(31_056_499),
                ..Default::default()
            },
            config(),
            token_db(revert),
            revm::inspector::NoOpInspector,
            &NoChain,
        )
        .unwrap();
        let tx = TxEnv {
            caller: sender,
            chain_id: Some(super::super::CELO_CHAIN_ID),
            kind: TxKind::Call(super::super::CELO_TOKEN),
            gas_limit: 500_000,
            data: input.clone().into(),
            ..Default::default()
        };
        let result = evm.transact(tx).unwrap();
        assert_eq!(result.result.is_success(), !revert);
        if !revert {
            assert_eq!(result.result.output().unwrap().as_ref(), input);
        }
        assert_eq!(
            result.state.get(&sender).unwrap().info.balance,
            U256::from(if revert { 10 } else { 7 })
        );
        assert_eq!(
            result
                .state
                .get(&to)
                .map(|a| a.info.balance)
                .unwrap_or_default(),
            U256::from(if revert { 0 } else { 3 })
        );
    }
}

#[test]
fn missing_consensus_header_is_not_an_evm_success() {
    let mut evm = evm::create_celo_l1_evm(
        BlockEnv {
            number: U256::from(31_056_499),
            ..Default::default()
        },
        config(),
        token_db(false),
        revm::inspector::NoOpInspector,
        &NoChain,
    )
    .unwrap();
    let tx = TxEnv {
        caller: Address::repeat_byte(0x11),
        chain_id: Some(super::super::CELO_CHAIN_ID),
        kind: TxKind::Call(Address::with_last_byte(0xf9)),
        gas_limit: 100_000,
        data: pure::word(10),
        ..Default::default()
    };
    assert!(evm
        .transact(tx)
        .unwrap_err()
        .to_string()
        .contains("no header"));
}

#[test]
fn tobin_tax_applies_before_call_and_create_and_reverts_with_them() {
    let sender = Address::repeat_byte(0x11);
    let recipient = Address::repeat_byte(0x22);
    let reserve = Address::repeat_byte(0x33);
    for height in [10_000_000, 20_000_000, 30_000_000] {
        for create in [false, true] {
            for revert in [false, true] {
                let mut db = InMemoryDB::default();
                db.insert_account_info(
                    sender,
                    AccountInfo {
                        balance: U256::from(100),
                        ..Default::default()
                    },
                );
                let mut registry = vec![0x73];
                registry.extend_from_slice(reserve.as_slice());
                registry.extend([0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3]);
                db.insert_account_info(
                    Address::from_word(U256::from(0xce10).into()),
                    AccountInfo::default().with_code(Bytecode::new_raw(registry.into())),
                );
                // Count reserve calls in slot 0, and return a 1/20 tax rate.
                let reserve_code = vec![
                    0x60, 0, 0x54, 0x60, 1, 0x01, 0x60, 0, 0x55, 0x60, 1, 0x60, 0, 0x52, 0x60, 20,
                    0x60, 32, 0x52, 0x60, 64, 0x60, 0, 0xf3,
                ];
                db.insert_account_info(
                    reserve,
                    AccountInfo::default().with_code(Bytecode::new_raw(reserve_code.into())),
                );
                let code = if revert {
                    vec![0x60, 0, 0x60, 0, 0xfd]
                } else {
                    vec![0x00]
                };
                if !create {
                    db.insert_account_info(
                        recipient,
                        AccountInfo::default().with_code(Bytecode::new_raw(code.clone().into())),
                    );
                }
                let mut evm = evm::create_celo_l1_evm(
                    BlockEnv {
                        number: U256::from(height),
                        ..Default::default()
                    },
                    config(),
                    db,
                    revm::inspector::NoOpInspector,
                    &NoChain,
                )
                .unwrap();
                let tx = TxEnv {
                    caller: sender,
                    chain_id: Some(super::super::CELO_CHAIN_ID),
                    kind: if create {
                        TxKind::Create
                    } else {
                        TxKind::Call(recipient)
                    },
                    gas_limit: 500_000,
                    value: U256::from(20),
                    data: if create { code.into() } else { Bytes::new() },
                    ..Default::default()
                };
                let result = evm.transact(tx).unwrap();
                assert_eq!(
                    result.result.is_success(),
                    !revert,
                    "height={height}, create={create}, revert={revert}"
                );
                let balance = |addr| {
                    result
                        .state
                        .get(&addr)
                        .map(|a| a.info.balance)
                        .unwrap_or_default()
                };
                let taxed = height < GINGERBREAD_BLOCK && !revert;
                assert_eq!(balance(sender), U256::from(if revert { 100 } else { 80 }));
                assert_eq!(balance(reserve), U256::from(u8::from(taxed)));
                let destination = if create { sender.create(0) } else { recipient };
                assert_eq!(
                    balance(destination),
                    U256::from(if revert {
                        0
                    } else if taxed {
                        19
                    } else {
                        20
                    })
                );
                let counter = result
                    .state
                    .get(&reserve)
                    .and_then(|a| a.storage.get(&U256::ZERO))
                    .map(|s| s.present_value())
                    .unwrap_or_default();
                assert_eq!(counter, U256::from(u8::from(taxed)));
            }
        }
    }
}

#[test]
fn metered_system_storage_warms_its_owner_and_reverts_with_the_frame() {
    use revm::context_interface::JournalTr;
    let target = Address::repeat_byte(0x77);
    for height in [10_000_000, 20_000_000, 30_000_000] {
        for unmetered in [false, true] {
            for revert in [false, true] {
                let mut db = InMemoryDB::default();
                let mut code = vec![0x60, 0, 0x54, 0x50]; // SLOAD; POP
                code.extend(if revert {
                    vec![0x60, 0, 0x60, 0, 0xfd]
                } else {
                    vec![0]
                });
                db.insert_account_info(
                    target,
                    AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
                );
                let mut evm = evm::create_celo_l1_evm(
                    BlockEnv {
                        number: U256::from(height),
                        ..Default::default()
                    },
                    config(),
                    db,
                    revm::inspector::NoOpInspector,
                    &NoChain,
                )
                .unwrap();
                let fork = L1Fork::at(height).unwrap();
                evm.inner.ctx.journaled_state.set_spec_id(fork.spec());
                let result = evm::system_call(
                    &mut evm.inner.ctx,
                    fork,
                    &NoChain,
                    Address::ZERO,
                    target,
                    Bytes::new(),
                    100_000,
                    true,
                    unmetered,
                );
                assert_eq!(result.is_ok(), !revert);
                let cold = evm
                    .inner
                    .ctx
                    .journaled_state
                    .load_account(target)
                    .unwrap()
                    .is_cold;
                assert_eq!(
                    cold,
                    unmetered || revert || fork < L1Fork::Espresso,
                    "height={height}, unmetered={unmetered}, revert={revert}"
                );
            }
        }
    }
}

#[test]
fn native_opcode_gas_at_fork_boundaries() {
    let mut cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../testdata/celo_l1/opcode_gas.json")).unwrap();
    cases.extend(
        serde_json::from_str::<Vec<serde_json::Value>>(include_str!(
            "../../../testdata/celo_l1/system_storage_gas.json"
        ))
        .unwrap(),
    );
    for case in cases {
        let params = case["params"].as_array().unwrap();
        let request = &params[0];
        let number =
            u64::from_str_radix(params[1].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
        let mut db = InMemoryDB::default();
        for (address, account) in params[2].as_object().unwrap() {
            let address = address.parse::<Address>().unwrap();
            let code = revm::primitives::hex::decode(account["code"].as_str().unwrap()).unwrap();
            let balance = account
                .get("balance")
                .map(|v| {
                    U256::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
                })
                .unwrap_or_default();
            db.insert_account_info(
                address,
                AccountInfo {
                    balance,
                    ..Default::default()
                }
                .with_code(Bytecode::new_raw(code.into())),
            );
            if let Some(storage) = account.get("state") {
                for (key, value) in storage.as_object().unwrap() {
                    db.insert_account_storage(
                        address,
                        U256::from_str_radix(key.trim_start_matches("0x"), 16).unwrap(),
                        U256::from_str_radix(value.as_str().unwrap().trim_start_matches("0x"), 16)
                            .unwrap(),
                    )
                    .unwrap();
                }
            }
        }
        let mut evm = evm::create_celo_l1_evm(
            BlockEnv {
                number: U256::from(number),
                ..Default::default()
            },
            config(),
            db,
            revm::inspector::NoOpInspector,
            &NoChain,
        )
        .unwrap();
        let result = evm
            .transact(TxEnv {
                caller: request["from"].as_str().unwrap().parse().unwrap(),
                chain_id: Some(super::super::CELO_CHAIN_ID),
                kind: TxKind::Call(request["to"].as_str().unwrap().parse().unwrap()),
                gas_limit: 1_000_000,
                ..Default::default()
            })
            .unwrap();
        let expected =
            revm::primitives::hex::decode(case["expected"]["result"].as_str().unwrap()).unwrap();
        assert!(
            result.result.is_success(),
            "{}: {:?}",
            case["name"],
            result.result
        );
        assert_eq!(
            result.result.output().unwrap().as_ref(),
            expected,
            "{}",
            case["name"]
        );
    }
}
