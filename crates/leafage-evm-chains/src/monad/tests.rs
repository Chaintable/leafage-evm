//! End-to-end transaction tests for [`MonadEvm`] covering the Monad
//! deviations that are not visible at the unit level: MIP-8 page pricing,
//! MIP-3 memory pricing, zero refunds, pricing v1 precompile multipliers,
//! the staking precompile call path and the EIP-7702 CREATE guard.

use crate::monad::{
    MonadEvm, MonadHaltReason, MonadHardfork, RESERVE_BALANCE_CONTRACT_ADDRESS,
    STAKING_CONTRACT_ADDRESS,
};
use alloy::eips::eip2930::{AccessList, AccessListItem};
use alloy_evm::EvmEnv;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::result::{ExecResultAndState, HaltReason, OutOfGasError};
use revm::context::TxEnv;
use revm::database::{in_memory_db::CacheDB, EmptyDB};
use revm::inspector::NoOpInspector;
use revm::primitives::{address, Address, Bytes, TxKind, B256, U256};
use revm::state::{AccountInfo, Bytecode};
use revm::ExecuteEvm;

type ExecutionResult = revm::context::result::ExecutionResult<MonadHaltReason>;

const CALLER: Address = address!("00000000000000000000000000000000000000aa");
const CONTRACT: Address = address!("00000000000000000000000000000000000000c0");
const DELEGATE: Address = address!("00000000000000000000000000000000000000d0");
const RECIPIENT: Address = address!("00000000000000000000000000000000000000e0");
const GAS_LIMIT: u64 = 1_000_000;

fn mon(amount: u64) -> U256 {
    U256::from(amount) * U256::from(10u128.pow(18))
}

fn account(balance: U256, code: Bytecode) -> AccountInfo {
    let mut info = AccountInfo::from_bytecode(code);
    info.balance = balance;
    info
}

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

fn run_with_state(
    hardfork: MonadHardfork,
    db: CacheDB<EmptyDB>,
    tx: TxEnv,
) -> ExecResultAndState<ExecutionResult> {
    let mut cfg = CfgEnv::new_with_spec(hardfork);
    hardfork.apply_cfg(&mut cfg);
    cfg.chain_id = crate::monad::MONAD_MAINNET_CHAIN_ID;
    cfg.disable_nonce_check = true;
    let env = EvmEnv::new(cfg, BlockEnv::default());
    let mut evm = MonadEvm::new(env, db, NoOpInspector {});
    evm.transact(tx).expect("transaction executes")
}

fn run(hardfork: MonadHardfork, db: CacheDB<EmptyDB>, tx: TxEnv) -> ExecutionResult {
    run_with_state(hardfork, db, tx).result
}

fn halt_reason(result: &ExecutionResult) -> MonadHaltReason {
    match result {
        ExecutionResult::Halt { reason, .. } => reason.clone(),
        other => panic!("expected halt, got {other:?}"),
    }
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
            assert_eq!(
                reason,
                MonadHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::MemoryLimit))
            );
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
    assert_eq!(
        halt_reason(&run(MonadHardfork::MonadTen, db, call_tx(CONTRACT, &[]))),
        MonadHaltReason::Base(HaltReason::NotActivated)
    );

    // The same code executed directly may CREATE.
    let direct = run_code(
        MonadHardfork::MonadTen,
        &[0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0xf0, 0x00],
    );
    assert!(direct.is_success(), "{direct:?}");
}

#[test]
fn unused_gas_is_not_refunded() {
    // STOP with gas price 1: the sender pays the whole gas limit and the
    // beneficiary receives the priority fee on the whole gas limit.
    let mut tx = call_tx(CONTRACT, &[]);
    tx.gas_price = 1;
    let out = run_with_state(MonadHardfork::MonadTen, db_with_code(&[0x00]), tx);
    assert_eq!(success_gas(&out.result), 21_000);
    assert_eq!(
        out.state[&CALLER].info.balance,
        U256::from(10u128.pow(18) - GAS_LIMIT as u128)
    );
    assert_eq!(
        out.state[&Address::ZERO].info.balance,
        U256::from(GAS_LIMIT)
    );
}

/// PUSH1 0 x4 PUSH8 <value> PUSH20 <to> GAS CALL POP STOP
fn send_value_code(to: Address, value: u64) -> Vec<u8> {
    let mut code = vec![0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x67];
    code.extend_from_slice(&value.to_be_bytes());
    code.push(0x73);
    code.extend_from_slice(to.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x00]);
    code
}

fn five_mon_wei() -> u64 {
    5_000_000_000_000_000_000
}

/// `EthCallFixture::eth_call_reserve_balance`: a delegated EOA with 7 MON
/// receives 3 MON and forwards 5 MON, ending below `min(10 MON, 7 MON)`.
fn delegated_recipient_db(forwarded: u64) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(
        CALLER,
        AccountInfo {
            balance: mon(100),
            ..Default::default()
        },
    );
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from(send_value_code(
            RECIPIENT, forwarded,
        )))),
    );
    db.insert_account_info(CONTRACT, account(mon(7), Bytecode::new_eip7702(DELEGATE)));
    db
}

#[test]
fn reserve_balance_violation_reverts_transaction() {
    let mut tx = call_tx(CONTRACT, &[]);
    tx.value = mon(3);
    let out = run_with_state(
        MonadHardfork::MonadTen,
        delegated_recipient_db(five_mon_wei()),
        tx,
    );
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
    assert_eq!(out.result.gas_used(), GAS_LIMIT);
    // the message was rejected: no value moved
    assert_eq!(out.state[&CONTRACT].info.balance, mon(7));
    assert!(out
        .state
        .get(&RECIPIENT)
        .is_none_or(|a| a.info.balance.is_zero()));

    // Forwarding 2 MON leaves 8 MON >= 7 MON: fine.
    let mut tx = call_tx(CONTRACT, &[]);
    tx.value = mon(3);
    let out = run_with_state(
        MonadHardfork::MonadTen,
        delegated_recipient_db(2_000_000_000_000_000_000),
        tx,
    );
    assert!(out.result.is_success(), "{:?}", out.result);
    assert_eq!(out.state[&CONTRACT].info.balance, mon(8));
    assert_eq!(out.state[&RECIPIENT].info.balance, mon(2));
}

/// `execute_create_message`: the sender nonce is bumped before the message
/// frame is pushed, so a reserve balance violation of a top level CREATE
/// rejects the frame but keeps the nonce increment.
#[test]
fn reserve_violation_of_top_level_create_keeps_sender_nonce() {
    let mut db = CacheDB::new(EmptyDB::default());
    let mut caller = account(mon(20), Bytecode::new_eip7702(DELEGATE));
    caller.nonce = 0;
    db.insert_account_info(CALLER, caller);
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(&[0x00]))),
    );
    let tx = TxEnv {
        caller: CALLER,
        gas_limit: GAS_LIMIT,
        kind: TxKind::Create,
        data: Bytes::from_static(&[0x00]),
        value: mon(15),
        chain_id: Some(crate::monad::MONAD_MAINNET_CHAIN_ID),
        ..Default::default()
    };
    let out = run_with_state(MonadHardfork::MonadTen, db, tx);
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
    assert_eq!(out.result.gas_used(), GAS_LIMIT);
    let caller = &out.state[&CALLER].info;
    assert_eq!(caller.nonce, 1, "nonce consumed by the rejected CREATE");
    assert_eq!(caller.balance, mon(20), "value transfer reverted");
    let created = CALLER.create(0);
    assert!(out
        .state
        .get(&created)
        .is_none_or(|a| a.info.balance.is_zero() && a.info.is_empty_code_hash()));
}

/// `execute_call_message`: `revert_transaction` runs before `post_call`
/// rejects a reverted frame, so a violation caused by a successful inner
/// call is reported even when the top level message reverts afterwards.
#[test]
fn reserve_violation_is_detected_before_top_level_revert() {
    const REVERTER: Address = address!("00000000000000000000000000000000000000f0");
    // CALL CONTRACT (delegated EOA forwarding 5 of its 7 MON), then REVERT
    let mut code = send_value_code(CONTRACT, 0);
    code.pop(); // STOP
    code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0xfd]);
    let mut db = delegated_recipient_db(five_mon_wei());
    db.insert_account_info(
        REVERTER,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from(code))),
    );
    let out = run_with_state(MonadHardfork::MonadTen, db, call_tx(REVERTER, &[]));
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
    assert_eq!(out.result.gas_used(), GAS_LIMIT);
    assert_eq!(out.state[&CONTRACT].info.balance, mon(7));
}

/// The node checks the reserve after `deploy_contract_code`: from MONAD_EIGHT
/// (recent code hash) a pre-funded address that becomes a contract in the
/// transaction is not an EOA any more, before that the original (empty) code
/// hash makes it one.
#[test]
fn created_contract_is_not_subject_to_reserve_from_monad_eight() {
    let created = CALLER.create(0);
    // init code: send the 5 MON pre-fund to RECIPIENT, return one byte of code
    let mut init = send_value_code(RECIPIENT, five_mon_wei());
    init.pop(); // STOP
    init.extend_from_slice(&[0x60, 0x01, 0x60, 0x00, 0xf3]);
    let mut db = db_with_code(&[]);
    db.insert_account_info(
        created,
        AccountInfo {
            balance: mon(5),
            ..Default::default()
        },
    );
    let tx = TxEnv {
        caller: CALLER,
        gas_limit: GAS_LIMIT,
        kind: TxKind::Create,
        data: Bytes::from(init),
        chain_id: Some(crate::monad::MONAD_MAINNET_CHAIN_ID),
        ..Default::default()
    };

    let out = run_with_state(MonadHardfork::MonadTen, db.clone(), tx.clone());
    assert!(out.result.is_success(), "{:?}", out.result);
    assert_eq!(out.state[&RECIPIENT].info.balance, mon(5));
    assert_eq!(out.state[&created].info.balance, U256::ZERO);

    let out = run_with_state(MonadHardfork::MonadSeven, db, tx);
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
}

/// The inspected execution path (`inspect_run_exec_loop`,
/// `inspect_frame_run`) applies the rule too, for a running frame and for a
/// top level message that finishes during its init.
#[test]
fn reserve_violation_is_reported_under_inspection() {
    use revm::InspectEvm;

    let inspect = |db: CacheDB<EmptyDB>, tx: TxEnv| {
        let hardfork = MonadHardfork::MonadTen;
        let mut cfg = CfgEnv::new_with_spec(hardfork);
        hardfork.apply_cfg(&mut cfg);
        cfg.chain_id = crate::monad::MONAD_MAINNET_CHAIN_ID;
        cfg.disable_nonce_check = true;
        let mut evm = MonadEvm::new(EvmEnv::new(cfg, BlockEnv::default()), db, NoOpInspector {});
        evm.inspect_tx(tx).expect("transaction executes")
    };

    // running frame: delegated EOA forwards 5 of its 7 MON
    let mut tx = call_tx(CONTRACT, &[]);
    tx.value = mon(3);
    let out = inspect(delegated_recipient_db(five_mon_wei()), tx);
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
    assert_eq!(out.state[&CONTRACT].info.balance, mon(7));

    // no frame: delegated sender transfers 95 of its 100 MON to an EOA
    let mut tx = call_tx(RECIPIENT, &[]);
    tx.value = mon(95);
    let mut db = db_with_code(&[]);
    db.insert_account_info(CALLER, account(mon(100), Bytecode::new_eip7702(DELEGATE)));
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(&[0x00]))),
    );
    let out = inspect(db, tx);
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
    assert_eq!(out.result.gas_used(), GAS_LIMIT);
    assert!(out
        .state
        .get(&RECIPIENT)
        .is_none_or(|a| a.info.balance.is_zero()));
}

#[test]
fn reserve_balance_rule_is_inactive_before_monad_four() {
    let mut tx = call_tx(CONTRACT, &[]);
    tx.value = mon(3);
    // revm resolves the delegation regardless of the spec; without the
    // reserve rule the forwarded value simply leaves the account.
    let out = run_with_state(
        MonadHardfork::MonadThree,
        delegated_recipient_db(five_mon_wei()),
        tx,
    );
    assert!(out.result.is_success(), "{:?}", out.result);
    assert_eq!(out.state[&CONTRACT].info.balance, mon(5));
}

#[test]
fn sender_may_dip_into_reserve_unless_delegated() {
    // Plain EOA sender: 100 MON -> 5 MON is allowed.
    let mut tx = call_tx(RECIPIENT, &[]);
    tx.value = mon(95);
    let mut db = db_with_code(&[]);
    db.insert_account_info(
        CALLER,
        AccountInfo {
            balance: mon(100),
            ..Default::default()
        },
    );
    let out = run_with_state(MonadHardfork::MonadTen, db.clone(), tx.clone());
    assert!(out.result.is_success(), "{:?}", out.result);
    assert_eq!(out.state[&RECIPIENT].info.balance, mon(95));

    // Delegated sender: cannot dip below min(10 MON, 100 MON).
    db.insert_account_info(CALLER, account(mon(100), Bytecode::new_eip7702(DELEGATE)));
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(&[0x00]))),
    );
    let out = run_with_state(MonadHardfork::MonadTen, db, tx);
    assert_eq!(
        halt_reason(&out.result),
        MonadHaltReason::ReserveBalanceViolation
    );
    assert!(out
        .state
        .get(&RECIPIENT)
        .is_none_or(|a| a.info.balance.is_zero()));
}

#[test]
fn dipped_into_reserve_reports_transient_violation() {
    // DELEGATE (run by the delegated EOA CONTRACT holding 7 MON):
    //   1. send 5 MON to RECIPIENT (a contract) -> below reserve
    //   2. CALL 0x1001 dippedIntoReserve(), answer at mem[0x20..0x40]
    //   3. CALL RECIPIENT with 1 byte of calldata: it sends its balance back
    //   4. RETURN mem[0x20..0x40]
    let mut delegate = vec![0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x67];
    delegate.extend_from_slice(&five_mon_wei().to_be_bytes());
    delegate.push(0x73);
    delegate.extend_from_slice(RECIPIENT.as_slice());
    delegate.extend_from_slice(&[0x5a, 0xf1, 0x50]);
    // PUSH4 selector PUSH1 0xe0 SHL PUSH1 0 MSTORE
    delegate.extend_from_slice(&[
        0x63, 0x3a, 0x61, 0x58, 0x4e, 0x60, 0xe0, 0x1b, 0x60, 0x00, 0x52,
    ]);
    // retSize 0x20, retOffset 0x20, argsSize 4, argsOffset 0, value 0, 0x1001, GAS, CALL, POP
    delegate.extend_from_slice(&[
        0x60, 0x20, 0x60, 0x20, 0x60, 0x04, 0x60, 0x00, 0x60, 0x00, 0x61, 0x10, 0x01, 0x5a, 0xf1,
        0x50,
    ]);
    // retSize 0, retOffset 0, argsSize 1, argsOffset 0, value 0, RECIPIENT, GAS, CALL, POP
    delegate.extend_from_slice(&[
        0x60, 0x00, 0x60, 0x00, 0x60, 0x01, 0x60, 0x00, 0x60, 0x00, 0x73,
    ]);
    delegate.extend_from_slice(RECIPIENT.as_slice());
    delegate.extend_from_slice(&[0x5a, 0xf1, 0x50]);
    // PUSH1 0x20 PUSH1 0x20 RETURN
    delegate.extend_from_slice(&[0x60, 0x20, 0x60, 0x20, 0xf3]);

    // RECIPIENT: with calldata, send the whole balance back to CALLER
    // through SELFDESTRUCT (a CALL would re-enter the delegated code).
    // CALLDATASIZE ISZERO PUSH1 end JUMPI CALLER SELFDESTRUCT end: JUMPDEST STOP
    let recipient = vec![0x36, 0x15, 0x60, 0x07, 0x57, 0x33, 0xff, 0x5b, 0x00];
    assert_eq!(recipient[0x07], 0x5b);

    let mut db = db_with_code(&[]);
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from(delegate))),
    );
    db.insert_account_info(
        RECIPIENT,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from(recipient))),
    );
    db.insert_account_info(CONTRACT, account(mon(7), Bytecode::new_eip7702(DELEGATE)));

    let out = run_with_state(MonadHardfork::MonadTen, db, call_tx(CONTRACT, &[]));
    assert!(out.result.is_success(), "{:?}", out.result);
    assert_eq!(U256::from_be_slice(out.result.output().unwrap()), U256::ONE);
    assert_eq!(out.state[&CONTRACT].info.balance, mon(7));

    // Direct call from an EOA that never dips: false.
    let result = run(
        MonadHardfork::MonadTen,
        db_with_code(&[]),
        call_tx(
            RESERVE_BALANCE_CONTRACT_ADDRESS,
            &0x3a61584eu32.to_be_bytes(),
        ),
    );
    assert_eq!(success_gas(&result), 21_064 + 100);
    assert_eq!(U256::from_be_slice(result.output().unwrap()), U256::ZERO);
}

#[test]
fn delegation_to_staking_precompile_is_rejected() {
    // DELEGATE account is an EOA delegating to 0x1000.
    let mut db = db_with_code(&[]);
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_eip7702(STAKING_CONTRACT_ADDRESS)),
    );
    // top level: EVMC_REJECTED, all gas consumed
    let result = run(MonadHardfork::MonadTen, db.clone(), call_tx(DELEGATE, &[]));
    assert_eq!(
        halt_reason(&result),
        MonadHaltReason::Base(HaltReason::PrecompileError)
    );
    assert_eq!(result.gas_used(), GAS_LIMIT);

    // nested: CONTRACT calls DELEGATE and returns the CALL success flag
    let mut code = vec![
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73,
    ];
    code.extend_from_slice(DELEGATE.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    db.insert_account_info(
        CONTRACT,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from(code.clone()))),
    );
    let result = run(MonadHardfork::MonadTen, db, call_tx(CONTRACT, &[]));
    let gas_used = success_gas(&result);
    assert_eq!(result.output().unwrap().as_ref(), &[0u8; 32]);
    assert!(gas_used > GAS_LIMIT / 64 * 62, "gas used {gas_used}");

    // Delegation to an Ethereum precompile runs as empty code (EIP-7702).
    let mut db = db_with_code(&code);
    db.insert_account_info(
        DELEGATE,
        AccountInfo::from_bytecode(Bytecode::new_eip7702(address!(
            "0000000000000000000000000000000000000001"
        ))),
    );
    let result = run(MonadHardfork::MonadTen, db, call_tx(CONTRACT, &[]));
    assert!(success_gas(&result) < 40_000);
    assert_eq!(U256::from_be_slice(result.output().unwrap()), U256::ONE);
}

#[test]
fn initcode_limits_before_monad_four() {
    // CREATE with 48 KiB + 1 of zeroed initcode:
    // PUSH3 0xc001 PUSH1 0 PUSH1 0 CREATE STOP
    let code = &[0x62, 0x00, 0xc0, 0x01, 0x60, 0x00, 0x60, 0x00, 0xf0, 0x00];
    assert_eq!(
        halt_reason(&run_code(MonadHardfork::MonadThree, code)),
        MonadHaltReason::Base(HaltReason::CreateInitCodeSizeLimit)
    );
    assert!(run_code(MonadHardfork::MonadFour, code).is_success());

    // A top level deployment may use up to 2 * max code size even before
    // MONAD_FOUR.
    let mut tx = call_tx(CONTRACT, &[]);
    tx.kind = TxKind::Create;
    tx.data = Bytes::from(vec![0u8; 60 * 1024]);
    let result = run(MonadHardfork::MonadThree, db_with_code(&[]), tx);
    assert!(result.is_success(), "{result:?}");
}
