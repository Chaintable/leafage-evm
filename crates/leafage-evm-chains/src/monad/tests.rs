//! End-to-end transaction tests for [`MonadEvm`] covering the Monad
//! deviations that are not visible at the unit level: MIP-8 page pricing,
//! MIP-3 memory pricing, zero refunds, pricing v1 precompile multipliers,
//! the staking precompile call path and the EIP-7702 CREATE guard.

use crate::monad::{MonadEvm, MonadHardfork, STAKING_CONTRACT_ADDRESS};
use alloy::eips::eip2930::{AccessList, AccessListItem};
use alloy_evm::EvmEnv;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::result::{ExecutionResult, HaltReason, OutOfGasError};
use revm::context::TxEnv;
use revm::database::{in_memory_db::CacheDB, EmptyDB};
use revm::inspector::NoOpInspector;
use revm::primitives::{address, Address, Bytes, TxKind, B256, U256};
use revm::state::{AccountInfo, Bytecode};
use revm::ExecuteEvm;

const CALLER: Address = address!("00000000000000000000000000000000000000aa");
const CONTRACT: Address = address!("00000000000000000000000000000000000000c0");
const DELEGATE: Address = address!("00000000000000000000000000000000000000d0");
const GAS_LIMIT: u64 = 1_000_000;

fn db_with_code(code: &[u8]) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(
        CALLER,
        AccountInfo {
            balance: U256::from(10u128.pow(18)),
            ..Default::default()
        },
    );
    db.insert_account_info(
        CONTRACT,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::copy_from_slice(code))),
    );
    db
}

fn call_tx(to: Address, data: &[u8]) -> TxEnv {
    TxEnv {
        caller: CALLER,
        gas_limit: GAS_LIMIT,
        kind: TxKind::Call(to),
        data: Bytes::copy_from_slice(data),
        chain_id: Some(crate::monad::MONAD_MAINNET_CHAIN_ID),
        ..Default::default()
    }
}

fn run(hardfork: MonadHardfork, db: CacheDB<EmptyDB>, tx: TxEnv) -> ExecutionResult {
    let mut cfg = CfgEnv::new_with_spec(hardfork);
    hardfork.apply_cfg(&mut cfg);
    cfg.chain_id = crate::monad::MONAD_MAINNET_CHAIN_ID;
    cfg.disable_nonce_check = true;
    let env = EvmEnv::new(cfg, BlockEnv::default());
    let mut evm = MonadEvm::new(env, db, NoOpInspector {});
    evm.transact(tx).expect("transaction executes").result
}

fn run_code(hardfork: MonadHardfork, code: &[u8]) -> ExecutionResult {
    run(hardfork, db_with_code(code), call_tx(CONTRACT, &[]))
}

fn success_gas(result: &ExecutionResult) -> u64 {
    assert!(result.is_success(), "expected success, got {result:?}");
    result.gas_used()
}

// PUSH1 5 SLOAD POP PUSH1 6 SLOAD POP STOP
const TWO_SLOADS_SAME_PAGE: &[u8] = &[0x60, 0x05, 0x54, 0x50, 0x60, 0x06, 0x54, 0x50, 0x00];

#[test]
fn mip8_sload_charges_cold_once_per_page() {
    // MONAD_TEN: 3 + (100 + 8000) + 2 + 3 + 100 + 2
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadTen, TWO_SLOADS_SAME_PAGE)),
        21_000 + 8_210
    );
    // MONAD_NINE: per slot cold cost, 3 + 8100 + 2 + 3 + 8100 + 2
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadNine, TWO_SLOADS_SAME_PAGE)),
        21_000 + 16_210
    );
}

#[test]
fn mip8_sload_pages_are_distinct() {
    // slot 5 and slot 128 (page 1): both cold
    let code = &[0x60, 0x05, 0x54, 0x50, 0x60, 0x80, 0x54, 0x50, 0x00];
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadTen, code)),
        21_000 + 16_210
    );
}

#[test]
fn mip8_sstore_prices_page_write_and_growth() {
    // PUSH1 1 PUSH1 5 SSTORE STOP
    // 3 + 3 + (100 + 8000 cold page + 2800 first write + 17000 growth)
    let first = &[0x60, 0x01, 0x60, 0x05, 0x55, 0x00];
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadTen, first)),
        21_000 + 27_906
    );

    // Second slot in the same page grows the page again: 3 + 3 + 100 + 17000
    let two = &[
        0x60, 0x01, 0x60, 0x05, 0x55, 0x60, 0x01, 0x60, 0x06, 0x55, 0x00,
    ];
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadTen, two)),
        21_000 + 27_906 + 17_106
    );

    // Set then clear the same slot: the clear undoes the growth
    // (current 1 -> 0, peak stays 1): 3 + 3 + 100
    let set_clear = &[
        0x60, 0x01, 0x60, 0x05, 0x55, 0x60, 0x00, 0x60, 0x05, 0x55, 0x00,
    ];
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadTen, set_clear)),
        21_000 + 27_906 + 106
    );
}

#[test]
fn mip8_sstore_existing_value_no_growth() {
    // slot 5 = 1 -> write 2: 3 + 3 + (100 + 8000 + 2800), no growth
    let mut db = db_with_code(&[0x60, 0x02, 0x60, 0x05, 0x55, 0x00]);
    db.insert_account_storage(CONTRACT, U256::from(5), U256::from(1))
        .unwrap();
    assert_eq!(
        success_gas(&run(MonadHardfork::MonadTen, db, call_tx(CONTRACT, &[]))),
        21_000 + 10_906
    );
}

#[test]
fn refunds_are_zero() {
    // slot 5 = 1 -> 0 under MONAD_NINE (pricing v1, no MIP-8):
    // PUSH1 0 PUSH1 5 SSTORE STOP: 3 + 3 + (8100 cold + 2900 reset); no
    // clearing refund of 4800.
    let mut db = db_with_code(&[0x60, 0x00, 0x60, 0x05, 0x55, 0x00]);
    db.insert_account_storage(CONTRACT, U256::from(5), U256::from(1))
        .unwrap();
    assert_eq!(
        success_gas(&run(MonadHardfork::MonadNine, db, call_tx(CONTRACT, &[]))),
        21_000 + 11_006
    );
}

#[test]
fn mip8_access_list_warms_pages() {
    // PUSH1 5 SLOAD POP STOP with slot 6 (same page) in the access list.
    let code = &[0x60, 0x05, 0x54, 0x50, 0x00];
    let mut tx = call_tx(CONTRACT, &[]);
    tx.tx_type = 1;
    tx.access_list = AccessList(vec![AccessListItem {
        address: CONTRACT,
        storage_keys: vec![B256::from(U256::from(6))],
    }]);
    // 21000 + 2400 (address) + 1900 (key) + 3 + 100 + 2
    assert_eq!(
        success_gas(&run(
            MonadHardfork::MonadTen,
            db_with_code(code),
            tx.clone()
        )),
        25_300 + 105
    );
    // Before MIP-8 the access list warms slot 6 only; slot 5 is still cold.
    assert_eq!(
        success_gas(&run(MonadHardfork::MonadNine, db_with_code(code), tx)),
        25_300 + 8_105
    );
}

/// Outer frame calls itself with one byte of calldata; the inner frame
/// touches slot 5 and either returns or reverts, then the outer frame
/// reads slot 5. A reverted inner frame must not leave the page warm.
fn self_call_code(inner_exit: u8) -> Vec<u8> {
    let mut code = vec![
        0x36, 0x60, 0x17, 0x57, // CALLDATASIZE PUSH1 inner JUMPI
        0x60, 0x00, 0x60, 0x00, 0x60, 0x01, 0x60, 0x00, 0x60, 0x00, // ret/args
        0x30, 0x5a, 0xf1, 0x50, // ADDRESS GAS CALL POP
        0x60, 0x05, 0x54, 0x50, 0x00, // PUSH1 5 SLOAD POP STOP
        0x5b, 0x60, 0x05, 0x54, 0x50, // JUMPDEST PUSH1 5 SLOAD POP
        0x60, 0x00, 0x60, 0x00, inner_exit, // PUSH1 0 PUSH1 0 RETURN/REVERT
    ];
    assert_eq!(code[0x17], 0x5b);
    code.shrink_to_fit();
    code
}

#[test]
fn mip8_page_access_reverts_with_frame() {
    let returned = success_gas(&run_code(MonadHardfork::MonadTen, &self_call_code(0xf3)));
    let reverted = success_gas(&run_code(MonadHardfork::MonadTen, &self_call_code(0xfd)));
    assert_eq!(reverted - returned, 8_000);
}

#[test]
fn mip3_memory_expansion_is_linear() {
    // PUSH1 0 PUSH2 0x1000 MSTORE STOP: 129 words
    let code = &[0x60, 0x00, 0x61, 0x10, 0x00, 0x52, 0x00];
    // MIP-3: 129 >> 1 = 64
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadTen, code)),
        21_000 + 3 + 3 + 3 + 64
    );
    // Ethereum: 3 * 129 + 129^2 / 512 = 419
    assert_eq!(
        success_gas(&run_code(MonadHardfork::MonadEight, code)),
        21_000 + 3 + 3 + 3 + 419
    );
}

#[test]
fn mip3_memory_limit_halts() {
    // PUSH1 0 PUSH4 0x00800000 MSTORE STOP: 8 MiB + 32 bytes
    let code = &[0x60, 0x00, 0x63, 0x00, 0x80, 0x00, 0x00, 0x52, 0x00];
    match run_code(MonadHardfork::MonadNine, code) {
        ExecutionResult::Halt { reason, gas, .. } => {
            assert_eq!(reason, HaltReason::OutOfGas(OutOfGasError::MemoryLimit));
            assert_eq!(gas.used(), GAS_LIMIT);
        }
        other => panic!("expected halt, got {other:?}"),
    }
    // Exactly 8 MiB is allowed: PUSH1 0 PUSH4 0x007fffe0 MSTORE STOP
    let code = &[0x60, 0x00, 0x63, 0x00, 0x7f, 0xff, 0xe0, 0x52, 0x00];
    assert!(run_code(MonadHardfork::MonadNine, code).is_success());
}

#[test]
fn pricing_v1_scales_ecrecover() {
    // PUSH1 0 x4 PUSH1 1 GAS STATICCALL POP STOP
    let code = &[
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x01, 0x5a, 0xfa, 0x50, 0x00,
    ];
    let six = success_gas(&run_code(MonadHardfork::MonadSix, code));
    let seven = success_gas(&run_code(MonadHardfork::MonadSeven, code));
    assert_eq!(seven - six, 3_000);
}

fn u64_slot(value: u64) -> U256 {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&value.to_be_bytes());
    U256::from_be_bytes(bytes)
}

#[test]
fn staking_get_epoch_from_eoa() {
    let mut db = db_with_code(&[]);
    db.insert_account_storage(STAKING_CONTRACT_ADDRESS, U256::from(1), u64_slot(42))
        .unwrap();
    let mut in_boundary = [0u8; 32];
    in_boundary[0] = 1;
    db.insert_account_storage(
        STAKING_CONTRACT_ADDRESS,
        U256::from(2),
        U256::from_be_bytes(in_boundary),
    )
    .unwrap();

    let result = run(
        MonadHardfork::MonadTen,
        db,
        call_tx(STAKING_CONTRACT_ADDRESS, &0x757991a8u32.to_be_bytes()),
    );
    // 21000 + 4 * 16 calldata + 200
    assert_eq!(success_gas(&result), 21_264);
    let output = result.output().unwrap();
    assert_eq!(output.len(), 64);
    assert_eq!(U256::from_be_slice(&output[..32]), U256::from(42));
    assert_eq!(U256::from_be_slice(&output[32..]), U256::ONE);
}

#[test]
fn staking_fallback_reverts_with_all_gas() {
    let result = run(
        MonadHardfork::MonadTen,
        db_with_code(&[]),
        call_tx(STAKING_CONTRACT_ADDRESS, &[0xde, 0xad, 0xbe, 0xef]),
    );
    match result {
        ExecutionResult::Revert { gas, output, .. } => {
            assert_eq!(gas.used(), GAS_LIMIT);
            assert_eq!(output.as_ref(), b"method not supported");
        }
        other => panic!("expected revert, got {other:?}"),
    }
}

#[test]
fn staking_is_plain_account_before_monad_four() {
    // Calling 0x1000 before MONAD_FOUR is a call to an empty account.
    let result = run(
        MonadHardfork::MonadThree,
        db_with_code(&[]),
        call_tx(STAKING_CONTRACT_ADDRESS, &[0xde, 0xad, 0xbe, 0xef]),
    );
    assert_eq!(success_gas(&result), 21_064);
}

#[test]
fn staking_rejects_staticcall() {
    // PUSH1 0 x4 PUSH2 0x1000 GAS STATICCALL PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
    let code = &[
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x61, 0x10, 0x00, 0x5a, 0xfa, 0x60, 0x00,
        0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ];
    let result = run_code(MonadHardfork::MonadTen, code);
    let gas_used = success_gas(&result);
    assert_eq!(result.output().unwrap().as_ref(), &[0u8; 32]);
    // The callee consumed everything it was given (all but 1/64).
    assert!(gas_used > GAS_LIMIT / 64 * 62, "gas used {gas_used}");
}

#[test]
fn create_inside_delegated_account_is_blocked() {
    // DELEGATE: PUSH1 0 PUSH1 0 PUSH1 0 CREATE STOP
    let mut db = db_with_code(&[]);
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(&[
            0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0xf0, 0x00,
        ]))),
    );
    db.insert_account_info(
        CONTRACT,
        AccountInfo::from_bytecode(Bytecode::new_eip7702(DELEGATE)),
    );
    match run(MonadHardfork::MonadTen, db, call_tx(CONTRACT, &[])) {
        ExecutionResult::Halt { reason, .. } => assert_eq!(reason, HaltReason::NotActivated),
        other => panic!("expected halt, got {other:?}"),
    }

    // The same code executed directly may CREATE.
    let direct = run_code(
        MonadHardfork::MonadTen,
        &[0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0xf0, 0x00],
    );
    assert!(direct.is_success(), "{direct:?}");
}
