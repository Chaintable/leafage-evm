//! Differential test of the CAS20 port against go-bsc.
//!
//! `tests/data/bsc_cas20/golden.json` was produced by running the scenario in
//! `tests/data/bsc_cas20/cas20_golden_gen_test.go` through go-bsc v1.8.1's real state
//! transition (`core.ApplyMessage`, BSC mainnet rules with Jenner enabled). This test replays
//! the same transactions on `BscEvm` from the same pre-state and requires every transaction's
//! status, gas used, returndata and logs, and the final storage, to match exactly.

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy_evm::EvmEnv;
use leafage_evm_chains::bsc::{BscEvm, BscHardfork, BscTxEnv};
use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::database::{CacheDB, EmptyDB};
use revm::inspector::NoOpInspector;
use revm::primitives::TxKind;
use revm::state::{AccountInfo, Bytecode};
use revm::ExecuteCommitEvm;
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;

fn hex_u64(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

fn hex_u256(v: &Value) -> U256 {
    U256::from_str(v.as_str().unwrap()).unwrap()
}

fn bytes(v: &Value) -> Bytes {
    Bytes::from_str(v.as_str().unwrap()).unwrap()
}

fn address(v: &str) -> Address {
    Address::from_str(v).unwrap()
}

fn word(v: &str) -> B256 {
    B256::from_str(v).unwrap()
}

fn load_golden() -> Value {
    serde_json::from_str(include_str!("data/bsc_cas20/golden.json")).expect("golden json")
}

fn pre_state(golden: &Value) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    for (addr, acc) in golden["pre"].as_object().unwrap() {
        let addr = address(addr);
        let code = bytes(&acc["code"]);
        let mut info = AccountInfo {
            balance: hex_u256(&acc["balance"]),
            nonce: hex_u64(&acc["nonce"]),
            ..Default::default()
        };
        if !code.is_empty() {
            let bytecode = Bytecode::new_raw(code);
            info.code_hash = bytecode.hash_slow();
            info.code = Some(bytecode);
        }
        db.insert_account_info(addr, info);
        for (slot, value) in acc["storage"].as_object().unwrap() {
            db.insert_account_storage(
                addr,
                word(slot).into(),
                word(value.as_str().unwrap()).into(),
            )
            .unwrap();
        }
    }
    db
}

fn env(golden: &Value, spec: BscHardfork) -> EvmEnv<BscHardfork> {
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.chain_id = hex_u64(&golden["chainId"]);
    cfg.disable_nonce_check = true;
    cfg.disable_base_fee = true;
    let block = BlockEnv {
        number: U256::from(hex_u64(&golden["number"])),
        beneficiary: address(golden["coinbase"].as_str().unwrap()),
        timestamp: U256::from(hex_u64(&golden["time"])),
        gas_limit: hex_u64(&golden["gasLimit"]),
        basefee: 0,
        difficulty: U256::ZERO,
        prevrandao: Some(B256::ZERO),
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
            excess_blob_gas: 0,
            blob_gasprice: 1,
        }),
        slot_num: 0,
    };
    EvmEnv::new(cfg, block)
}

fn call(from: &str, to: Address, data: Bytes, milli_timestamp: Option<u64>) -> BscTxEnv {
    BscTxEnv {
        base: TxEnv {
            caller: address(from),
            kind: TxKind::Call(to),
            data,
            gas_limit: 1_000_000,
            gas_price: 0,
            chain_id: Some(56),
            ..Default::default()
        },
        is_system_transaction: false,
        milli_timestamp,
    }
}

const ALICE: &str = "0x00000000000000000000000000000000000a11ce";
const PROXY: &str = "0x0000000000000000000000000000000000900001";

/// The golden proxy's calldata: op | target | gas (8 bytes) | payload.
fn proxy_input(op: u8, target: Address, gas: u64, payload: &[u8]) -> Bytes {
    let mut out = vec![op];
    out.extend_from_slice(target.as_slice());
    out.extend_from_slice(&gas.to_be_bytes());
    out.extend_from_slice(payload);
    out.into()
}

fn cas20_tokens(golden: &Value) -> Vec<Address> {
    golden["pre"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, acc)| acc["code"] == "0xef" && !acc["storage"].as_object().unwrap().is_empty())
        .map(|(a, _)| address(a))
        .filter(leafage_evm_chains::bsc::cas20::is_cas20_token)
        .collect()
}

#[test]
fn cas20_matches_go_bsc() {
    let golden = load_golden();
    let mut db = pre_state(&golden);
    let evm_env = env(&golden, BscHardfork::Jenner);
    let milli_timestamp = hex_u64(&golden["milliTimestamp"]);

    let mut failures = Vec::new();
    for (i, tx) in golden["txs"].as_array().unwrap().iter().enumerate() {
        let name = tx["name"].as_str().unwrap();
        let tx_env = BscTxEnv {
            base: TxEnv {
                caller: address(tx["from"].as_str().unwrap()),
                kind: TxKind::Call(address(tx["to"].as_str().unwrap())),
                data: bytes(&tx["input"]),
                gas_limit: hex_u64(&tx["gas"]),
                value: hex_u256(&tx["value"]),
                gas_price: 0,
                chain_id: Some(evm_env.cfg_env.chain_id),
                ..Default::default()
            },
            is_system_transaction: false,
            milli_timestamp: Some(milli_timestamp),
        };
        let mut evm = BscEvm::new(evm_env.clone(), &mut db, NoOpInspector, false);
        let result = match evm.transact_commit(tx_env) {
            Ok(result) => result,
            Err(err) => {
                failures.push(format!("#{i} {name}: transaction error {err:?}"));
                continue;
            }
        };

        let (status, output, logs) = match &result {
            ExecutionResult::Success { output, logs, .. } => {
                ("success", output.data().clone(), logs.clone())
            }
            ExecutionResult::Revert { output, .. } => ("revert", output.clone(), Vec::new()),
            ExecutionResult::Halt { .. } => ("halt", Bytes::new(), Vec::new()),
        };
        let mut diffs = Vec::new();
        if status != tx["status"].as_str().unwrap() {
            diffs.push(format!("status {status} != {}", tx["status"]));
        }
        let gas_used = result.tx_gas_used();
        let want_gas = hex_u64(&tx["gasUsed"]);
        if gas_used != want_gas {
            diffs.push(format!(
                "gasUsed {gas_used} != {want_gas} ({:+})",
                gas_used as i64 - want_gas as i64
            ));
        }
        if output != bytes(&tx["output"]) {
            diffs.push(format!("output {output} != {}", tx["output"]));
        }
        let want_logs = tx["logs"].as_array().unwrap();
        let logs_match = logs.len() == want_logs.len()
            && logs.iter().zip(want_logs).all(|(got, want)| {
                got.address == address(want["address"].as_str().unwrap())
                    && got.data.topics()
                        == want["topics"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|t| word(t.as_str().unwrap()))
                            .collect::<Vec<_>>()
                    && got.data.data == bytes(&want["data"])
            });
        if !logs_match {
            diffs.push(format!("logs {logs:?} != {want_logs:?}"));
        }
        if !diffs.is_empty() {
            failures.push(format!("#{i} {name}: {}", diffs.join("; ")));
        }
    }

    // Final storage: every slot go-bsc recorded must hold go-bsc's value, and the port must
    // not have written anything go-bsc did not.
    let post: BTreeMap<Address, BTreeMap<B256, B256>> = golden["post"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(a, slots)| {
            (
                address(a),
                slots
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (word(k), word(v.as_str().unwrap())))
                    .collect(),
            )
        })
        .collect();
    for (addr, slots) in &post {
        for (slot, want) in slots {
            let got: B256 = db
                .cache
                .accounts
                .get(addr)
                .and_then(|a| a.storage.get(&U256::from_be_bytes(slot.0)))
                .copied()
                .unwrap_or_default()
                .into();
            if got != *want {
                failures.push(format!("post {addr} slot {slot}: {got} != {want}"));
            }
        }
    }
    for (addr, account) in &db.cache.accounts {
        for (slot, value) in &account.storage {
            let slot = B256::from(*slot);
            if !value.is_zero() && !post.get(addr).is_some_and(|s| s.contains_key(&slot)) {
                failures.push(format!("post {addr} slot {slot}: unexpected write {value}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The factory and both registries are not executed locally: a frame entering one fails with
/// the message `leafage-evm-rpc` maps to `-39008`, at the top level and nested alike.
#[test]
fn singletons_are_forwarded() {
    let golden = load_golden();
    let singletons = [
        leafage_evm_chains::bsc::cas20::FACTORY,
        leafage_evm_chains::bsc::cas20::ACTIVATION_REGISTRY,
        leafage_evm_chains::bsc::cas20::POLICY_REGISTRY,
    ];
    for target in singletons {
        for tx in [
            call(ALICE, target, Bytes::new(), None),
            call(
                ALICE,
                address(PROXY),
                proxy_input(1, target, 100_000, &[]),
                None,
            ),
        ] {
            let mut db = pre_state(&golden);
            let mut evm = BscEvm::new(
                env(&golden, BscHardfork::Jenner),
                &mut db,
                NoOpInspector,
                false,
            );
            let err = evm.transact_commit(tx).expect_err("forwarded");
            let msg = format!("{err}");
            assert!(
                msg.contains(&format!("unsupported precompile address: {target}")),
                "{target}: {msg}"
            );
        }
    }
}

/// Before Jenner a CAS20 address is an ordinary account: its 0xEF marker is an invalid
/// opcode, and 0x70 is an empty account.
#[test]
fn nothing_routes_before_jenner() {
    let golden = load_golden();
    let token = cas20_tokens(&golden)[0];
    let mut db = pre_state(&golden);
    let mut evm = BscEvm::new(
        env(&golden, BscHardfork::Pasteur),
        &mut db,
        NoOpInspector,
        false,
    );
    let res = evm
        .transact_commit(call(
            ALICE,
            token,
            Bytes::from_static(&[0x18, 0x16, 0x0d, 0xdd]),
            None,
        ))
        .unwrap();
    assert!(matches!(res, ExecutionResult::Halt { .. }), "{res:?}");

    let milli = Address::with_last_byte(0x70);
    let res = evm
        .transact_commit(call(
            ALICE,
            address(PROXY),
            proxy_input(1, milli, 100_000, &[]),
            None,
        ))
        .unwrap();
    // ok = 1 with no returndata: a call to an empty account.
    assert_eq!(res.output().unwrap().len(), 32, "{res:?}");
}

/// Without a millisecond timestamp in the transaction, 0x70 answers `time * 1000`.
#[test]
fn milli_timestamp_falls_back_to_seconds() {
    let golden = load_golden();
    let mut db = pre_state(&golden);
    let mut evm = BscEvm::new(
        env(&golden, BscHardfork::Jenner),
        &mut db,
        NoOpInspector,
        false,
    );
    let milli = Address::with_last_byte(0x70);
    let res = evm
        .transact_commit(call(
            ALICE,
            address(PROXY),
            proxy_input(1, milli, 100_000, &[]),
            None,
        ))
        .unwrap();
    let out = res.output().unwrap();
    assert_eq!(out[31], 1, "call succeeded");
    assert_eq!(
        U256::from_be_slice(&out[32..64]),
        U256::from(hex_u64(&golden["time"]) * 1000)
    );
}
