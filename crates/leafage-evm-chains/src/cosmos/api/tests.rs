use super::*;
use revm::context::result::{EVMError, ExecutionResult};
use revm::database::{CacheDB, EmptyDB};
use revm::inspector::{InspectEvm, NoOpInspector};
use revm::primitives::TxKind;
use revm::state::{AccountInfo, Bytecode};
use revm::ExecuteEvm;
use std::convert::Infallible;

fn execute(
    config: &str,
    target: Address,
    opcode: Option<u8>,
    inspect: bool,
) -> Result<ExecutionResult, EVMError<Infallible>> {
    let mut db = CacheDB::new(EmptyDB::default());
    let to = if let Some(opcode) = opcode {
        // CALL has a value argument; STATICCALL and DELEGATECALL do not.
        let mut code = [0x60, 0].repeat(if opcode == 0xf1 { 5 } else { 4 });
        code.push(0x73); // PUSH20 target
        code.extend_from_slice(target.as_slice());
        // GAS; call; POP; STOP: even a caller ignoring failure must not hide the error.
        code.extend_from_slice(&[0x5a, opcode, 0x50, 0]);
        let contract = Address::repeat_byte(0x77);
        db.insert_account_info(
            contract,
            AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
        );
        contract
    } else {
        target
    };
    let mut evm = CosmosEvm::new(
        EvmEnv::new(
            CfgEnv::new_with_spec(CosmosHardfork::default()),
            BlockEnv::default(),
        ),
        serde_json::from_str(config).unwrap(),
        db,
        NoOpInspector {},
        inspect,
    );
    let tx = TxEnv {
        caller: Address::repeat_byte(0x11),
        kind: TxKind::Call(to),
        gas_limit: 100_000,
        ..Default::default()
    };
    if inspect {
        evm.inspect_one_tx(tx)
    } else {
        evm.transact_one(tx)
    }
}

#[test]
fn unsupported_precompiles_abort_direct_and_nested_calls() {
    let config = r#"{
        "native_token":"0x000000000000000000000000000000000000beef",
        "unsupported_precompiles":[
            "0x000000000000000000000000000000000000aabb",
            "0x000000000000000000000000000000000000ccdd"
        ]
    }"#;
    for suffix in ["0800", "beef", "aabb", "ccdd", "eeee"] {
        let target: Address = format!("0x{}{}", "0".repeat(36), suffix).parse().unwrap();
        for opcode in [None, Some(0xf1), Some(0xfa), Some(0xf4)] {
            for inspect in [false, true] {
                let result = execute(config, target, opcode, inspect);
                if suffix == "eeee" {
                    assert!(result.unwrap().is_success());
                } else {
                    assert!(
                        matches!(&result, Err(EVMError::Custom(msg)) if msg == &format!("unsupported precompile address: {target}")),
                        "target={target}, opcode={opcode:?}, inspect={inspect}: {result:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn omitted_or_empty_list_preserves_legacy_behavior() {
    let target: Address = "0x000000000000000000000000000000000000beef"
        .parse()
        .unwrap();
    for config in ["{}", r#"{"unsupported_precompiles":[]}"#] {
        assert!(execute(config, target, None, false).unwrap().is_success());
    }
    let legacy = r#"{"native_token":"0x000000000000000000000000000000000000beef"}"#;
    assert!(matches!(
        execute(legacy, target, None, false),
        Err(EVMError::Custom(_))
    ));
}
