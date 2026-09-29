//! End-to-end transaction tests for [`RskEvm`]: the native contract guard on
//! internal calls and the block env RSK contracts can observe.

use crate::rsk::{RskEvm, RskHardfork};
use alloy_evm::EvmEnv;
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::context::result::{EVMError, ExecutionResult};
use revm::context::TxEnv;
use revm::database::{in_memory_db::CacheDB, EmptyDB};
use revm::inspector::NoOpInspector;
use revm::primitives::{address, Address, Bytes, TxKind, U256};
use revm::state::{AccountInfo, Bytecode};
use revm::ExecuteEvm;
use std::convert::Infallible;

const CALLER: Address = address!("00000000000000000000000000000000000000aa");
const CONTRACT: Address = address!("00000000000000000000000000000000000000c0");
const BRIDGE: Address = address!("0000000000000000000000000000000001000006");
/// RSK mainnet-like minimum gas price, reported by the writer as the base fee.
const MINIMUM_GAS_PRICE: u64 = 60_000_000;

/// `CALL(gas, target, 0, 0, 0, 0, 0); POP; STOP`
fn call_code(target: Address) -> Vec<u8> {
    let mut code = vec![
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73,
    ];
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x00]);
    code
}

fn transact(code: &[u8]) -> Result<ExecutionResult, EVMError<Infallible>> {
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(CALLER, AccountInfo::default());
    db.insert_account_info(
        CONTRACT,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::copy_from_slice(code))),
    );

    // same cfg the standalone binary builds for `--evm-type=rsk`
    let mut cfg = CfgEnv::new_with_spec(RskHardfork::from(MainnetSpecId::CANCUN));
    cfg.disable_balance_check = true;
    cfg.disable_base_fee = true;
    cfg.disable_nonce_check = true;
    let block = BlockEnv {
        basefee: MINIMUM_GAS_PRICE,
        ..Default::default()
    };

    let tx = TxEnv {
        caller: CALLER,
        gas_limit: 1_000_000,
        kind: TxKind::Call(CONTRACT),
        chain_id: Some(cfg.chain_id),
        ..Default::default()
    };

    let mut evm = RskEvm::new(EvmEnv::new(cfg, block), db, NoOpInspector {});
    evm.transact(tx).map(|res| res.result)
}

/// A regular contract calling the Bridge must fail the whole call with the
/// unsupported precompile error (so nodex-proxy forwards it), instead of
/// treating the Bridge as an empty account and succeeding.
#[test]
fn internal_call_to_native_contract_is_forwarded() {
    let err = transact(&call_code(BRIDGE)).expect_err("the call must not be executed locally");
    match err {
        EVMError::Custom(msg) => assert_eq!(
            msg,
            format!("unsupported precompile address: {BRIDGE}"),
            "wire-format must match api_impl.rs parser"
        ),
        other => panic!("expected EVMError::Custom, got {other:?}"),
    }
}

/// Precompiles RSK shares with Ethereum (here identity, 0x04) run locally.
#[test]
fn internal_call_to_shared_precompile_runs_locally() {
    let result = transact(&call_code(Address::with_last_byte(4))).expect("executes");
    assert!(result.is_success(), "unexpected result: {result:?}");
}

/// `BASEFEE` returns the block minimum gas price on RSK (RSKIP412). The writer
/// reports it as `baseFeePerGas`, so it has to reach the opcode even though the
/// base fee validation is disabled and the call has a zero gas price.
#[test]
fn basefee_opcode_returns_minimum_gas_price() {
    // BASEFEE; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN
    let result =
        transact(&[0x48, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]).expect("executes");
    assert!(result.is_success(), "unexpected result: {result:?}");
    let output = result.output().expect("has output");
    assert_eq!(U256::from_be_slice(output), U256::from(MINIMUM_GAS_PRICE));
}
