use super::{
    extend_arc_precompiles,
    helpers::{
        revert_message_to_bytes, ERR_CLEAR_EMPTY, ERR_DELEGATE_CALL_NOT_ALLOWED,
        NATIVE_FIAT_TOKEN_ADDRESS, PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
    },
    native_coin_authority::{INativeCoinAuthority, NATIVE_COIN_AUTHORITY_ADDRESS},
    native_coin_control::{
        compute_is_blocklisted_storage_slot, INativeCoinControl, NATIVE_COIN_CONTROL_ADDRESS,
    },
    pq::{IPQ, PQ_ADDRESS},
    system_accounting::{
        compute_gas_values_storage_slot, GasValues, ISystemAccounting, SYSTEM_ACCOUNTING_ADDRESS,
    },
};
use crate::arc::config::ARC_ZERO8_HARDFORK_TIMESTAMP_ACTIVATION_MAINNET;
use crate::arc::{
    native::{blocklist_storage_slot, eip7708_transfer_log},
    ArcChainConfig, ArcContext, ArcEvm, ArcEvmFactory, ArcForkActivation, ArcHardforkSchedule,
    ARC_MAINNET_CHAIN_ID,
};
use alloy::{
    hex,
    primitives::{address, keccak256, Address, Bytes, StorageKey, B256, U256},
    sol_types::SolCall,
};
use alloy_evm::{precompiles::PrecompilesMap, Database as AlloyDatabase, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::{
    bytecode::{opcode, Bytecode},
    context::{
        result::{ExecutionResult, HaltReason},
        ContextTr, JournalTr, TxEnv,
    },
    context_interface::block::BlobExcessGasAndPrice,
    database::InMemoryDB,
    database_interface::DBErrorMarker,
    handler::{EvmTr, FrameResult, ItemOrResult, PrecompileProvider, SYSTEM_ADDRESS},
    inspector::NoOpInspector,
    interpreter::{
        interpreter_action::{FrameInit, FrameInput},
        CallInput, CallInputs, CallValue, InstructionResult, SharedMemory,
    },
    precompile::{PrecompileSpecId, Precompiles},
    primitives::TxKind,
    state::AccountInfo,
    Database as RevmDatabase, ExecuteCommitEvm, ExecuteEvm, InspectEvm,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{convert::Infallible, error::Error, fmt};

const USER: Address = address!("1000000000000000000000000000000000000001");
const OTHER: Address = address!("2000000000000000000000000000000000000002");
const QUERY_CALLER: Address = address!("3000000000000000000000000000000000000003");
const P256_ADDRESS: Address = address!("0000000000000000000000000000000000000100");
const TOTAL_SUPPLY_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

fn evm_env_at_timestamp(timestamp: u64) -> EvmEnv<MainnetSpecId> {
    let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
    cfg.chain_id = ARC_MAINNET_CHAIN_ID;
    let block = BlockEnv {
        number: U256::from(1),
        timestamp: U256::from(timestamp),
        gas_limit: 30_000_000,
        prevrandao: Some(B256::ZERO),
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
            excess_blob_gas: 0,
            blob_gasprice: 1,
        }),
        ..Default::default()
    };
    EvmEnv::new(cfg, block)
}

fn evm_env() -> EvmEnv<MainnetSpecId> {
    evm_env_at_timestamp(1)
}

fn arc_evm<DB: AlloyDatabase>(db: DB) -> ArcEvm<DB, NoOpInspector> {
    ArcEvmFactory::new(ArcChainConfig::mainnet())
        .create(evm_env(), db, NoOpInspector {})
        .expect("valid Arc test environment")
}

fn arc_evm_with_zero8<DB: AlloyDatabase>(db: DB, active: bool) -> ArcEvm<DB, NoOpInspector> {
    let timestamp = if active {
        ARC_ZERO8_HARDFORK_TIMESTAMP_ACTIVATION_MAINNET
    } else {
        ARC_ZERO8_HARDFORK_TIMESTAMP_ACTIVATION_MAINNET - 1
    };
    ArcEvmFactory::new(ArcChainConfig::mainnet())
        .create(evm_env_at_timestamp(timestamp), db, NoOpInspector {})
        .expect("valid Arc test environment")
}

fn call_tx(caller: Address, target: Address, data: Bytes, gas_limit: u64, nonce: u64) -> TxEnv {
    TxEnv {
        caller,
        kind: TxKind::Call(target),
        data,
        gas_limit,
        nonce,
        chain_id: Some(ARC_MAINNET_CHAIN_ID),
        ..Default::default()
    }
}

fn direct_call<DB: AlloyDatabase>(
    evm: &mut ArcEvm<DB, NoOpInspector>,
    caller: Address,
    target: Address,
    data: Bytes,
    gas_limit: u64,
    value: U256,
) -> FrameResult {
    // `EthFrame::make_call_frame` expects CALL participants to have been loaded by
    // pre-execution/opcode handling. Direct frame tests reproduce that invariant.
    evm.ctx_mut()
        .journal_mut()
        .load_account(caller)
        .expect("load direct-call caller");
    evm.ctx_mut()
        .journal_mut()
        .load_account(target)
        .expect("load direct-call target");
    evm.ctx_mut()
        .journal_mut()
        .load_account(NATIVE_COIN_CONTROL_ADDRESS)
        .expect("load Arc blocklist account");

    let frame = FrameInit {
        frame_input: FrameInput::Call(Box::new(CallInputs {
            target_address: target,
            bytecode_address: target,
            caller,
            input: CallInput::Bytes(data),
            value: CallValue::Transfer(value),
            gas_limit,
            is_static: false,
            return_memory_offset: 0..0,
            known_bytecode: None,
            scheme: revm::interpreter::CallScheme::Call,
        })),
        memory: SharedMemory::default(),
        depth: 1,
    };

    match EvmTr::frame_init(evm, frame).expect("frame initialization must not return a DB error") {
        ItemOrResult::Result(result) => result,
        ItemOrResult::Item(_) => panic!("registered precompile must return an immediate result"),
    }
}

fn direct_delegatecall<DB: AlloyDatabase>(
    evm: &mut ArcEvm<DB, NoOpInspector>,
    caller: Address,
    target: Address,
    bytecode_address: Address,
    data: Bytes,
    gas_limit: u64,
) -> FrameResult {
    for address in [
        caller,
        target,
        bytecode_address,
        NATIVE_COIN_CONTROL_ADDRESS,
    ] {
        evm.ctx_mut()
            .journal_mut()
            .load_account(address)
            .expect("load delegatecall participant");
    }

    let frame = FrameInit {
        frame_input: FrameInput::Call(Box::new(CallInputs {
            target_address: target,
            bytecode_address,
            caller,
            input: CallInput::Bytes(data),
            value: CallValue::Apparent(U256::ZERO),
            gas_limit,
            is_static: false,
            return_memory_offset: 0..0,
            known_bytecode: None,
            scheme: revm::interpreter::CallScheme::DelegateCall,
        })),
        memory: SharedMemory::default(),
        depth: 1,
    };

    match EvmTr::frame_init(evm, frame).expect("frame initialization must not return a DB error") {
        ItemOrResult::Result(result) => result,
        ItemOrResult::Item(_) => panic!("registered precompile must return an immediate result"),
    }
}

fn call_output(result: &FrameResult) -> &[u8] {
    match result {
        FrameResult::Call(outcome) => outcome.result.output.as_ref(),
        FrameResult::Create(_) => panic!("expected CALL result"),
    }
}

fn call_instruction(result: &FrameResult) -> InstructionResult {
    match result {
        FrameResult::Call(outcome) => outcome.result.result,
        FrameResult::Create(_) => panic!("expected CALL result"),
    }
}

fn call_gas_spent(result: &FrameResult) -> u64 {
    match result {
        FrameResult::Call(outcome) => outcome.result.gas.spent(),
        FrameResult::Create(_) => panic!("expected CALL result"),
    }
}

fn decode_bool<C: SolCall<Return = bool>>(output: &[u8]) -> bool {
    C::abi_decode_returns(output).expect("valid boolean return")
}

fn account_info(balance: U256) -> AccountInfo {
    AccountInfo {
        balance,
        nonce: 1,
        ..Default::default()
    }
}

fn reverting_parent_code(calldata: &[u8]) -> Bytecode {
    assert!(calldata.len() <= u8::MAX as usize);
    let mut code = vec![
        opcode::PUSH1,
        calldata.len() as u8,
        opcode::PUSH1,
        0, // patched with the appended calldata offset
        opcode::PUSH1,
        0,
        opcode::CODECOPY,
        opcode::PUSH1,
        0, // output size
        opcode::PUSH1,
        0, // output offset
        opcode::PUSH1,
        calldata.len() as u8,
        opcode::PUSH1,
        0, // input offset
        opcode::PUSH1,
        0, // value
        opcode::PUSH20,
    ];
    code.extend_from_slice(NATIVE_COIN_AUTHORITY_ADDRESS.as_slice());
    code.extend_from_slice(&[
        opcode::GAS,
        opcode::CALL,
        opcode::PUSH1,
        0,
        opcode::MSTORE,
        opcode::PUSH1,
        32,
        opcode::PUSH1,
        0,
        opcode::REVERT,
    ]);
    code[3] = code.len() as u8;
    code.extend_from_slice(calldata);
    Bytecode::new_raw(code.into())
}

fn current_storage(
    evm: &mut ArcEvm<InMemoryDB, NoOpInspector>,
    address: Address,
    slot: U256,
) -> U256 {
    evm.ctx_mut()
        .journal_mut()
        .sload(address, slot)
        .expect("in-memory storage read")
        .data
}

fn current_balance(evm: &mut ArcEvm<InMemoryDB, NoOpInspector>, address: Address) -> U256 {
    evm.ctx_mut()
        .journal_mut()
        .load_account(address)
        .expect("in-memory account read")
        .info
        .balance
}

#[test]
fn provider_uses_cold_dynamic_lookup_and_keeps_standard_p256() {
    let flags = ArcChainConfig::mainnet().execution_spec_at(1, 1).arc_flags;
    let mut precompiles = PrecompilesMap::from_static(Precompiles::new(
        PrecompileSpecId::from_spec_id(MainnetSpecId::OSAKA),
    ));
    extend_arc_precompiles(&mut precompiles, flags);

    for address in [
        NATIVE_COIN_AUTHORITY_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
        SYSTEM_ACCOUNTING_ADDRESS,
        PQ_ADDRESS,
    ] {
        assert!(
            precompiles.get(&address).is_some(),
            "missing custom precompile {address}"
        );
        assert!(<PrecompilesMap as PrecompileProvider<
            ArcContext<InMemoryDB>,
        >>::contains(&precompiles, &address,));
    }
    assert!(precompiles.get(&P256_ADDRESS).is_some());

    let warm: Vec<_> =
        <PrecompilesMap as PrecompileProvider<ArcContext<InMemoryDB>>>::warm_addresses(
            &precompiles,
        )
        .collect();
    assert!(warm.contains(&P256_ADDRESS));
    assert!(!warm.contains(&NATIVE_COIN_AUTHORITY_ADDRESS));
    assert!(!warm.contains(&NATIVE_COIN_CONTROL_ADDRESS));
    assert!(!warm.contains(&SYSTEM_ACCOUNTING_ADDRESS));
    assert!(!warm.contains(&PQ_ADDRESS));

    let pre_zero6 = ArcHardforkSchedule::new(
        ArcForkActivation::Block(0),
        ArcForkActivation::Block(0),
        ArcForkActivation::Block(0),
        ArcForkActivation::Never,
        ArcForkActivation::Never,
        ArcForkActivation::Never,
    )
    .flags_at(1, 1);
    let mut gated = PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(
        MainnetSpecId::OSAKA,
    )));
    extend_arc_precompiles(&mut gated, pre_zero6);
    assert!(gated.get(&NATIVE_COIN_AUTHORITY_ADDRESS).is_some());
    assert!(gated.get(&PQ_ADDRESS).is_none());
}

#[test]
fn native_coin_control_commit_is_visible_and_any_nonzero_status_is_blocked() {
    let mut evm = arc_evm(InMemoryDB::default());
    let blocklist = INativeCoinControl::blocklistCall { account: USER }
        .abi_encode()
        .into();
    let result = evm
        .transact_commit(call_tx(
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_CONTROL_ADDRESS,
            blocklist,
            100_000,
            0,
        ))
        .expect("blocklist transaction");
    assert!(result.is_success());
    assert_eq!(result.logs().len(), 1);
    assert_eq!(result.logs()[0].address, NATIVE_COIN_CONTROL_ADDRESS);

    let canonical_slot = blocklist_storage_slot(USER);
    assert_eq!(
        compute_is_blocklisted_storage_slot(USER),
        StorageKey::from(canonical_slot.to_be_bytes::<32>())
    );
    assert_eq!(
        evm.ctx_mut()
            .db_mut()
            .storage(NATIVE_COIN_CONTROL_ADDRESS, canonical_slot)
            .expect("committed blocklist status"),
        U256::ONE
    );

    let query: Bytes = INativeCoinControl::isBlocklistedCall { account: USER }
        .abi_encode()
        .into();
    let result = evm
        .transact_commit(call_tx(
            QUERY_CALLER,
            NATIVE_COIN_CONTROL_ADDRESS,
            query.clone(),
            100_000,
            0,
        ))
        .expect("blocklist query");
    assert!(decode_bool::<INativeCoinControl::isBlocklistedCall>(
        result.output().expect("query output")
    ));

    evm.ctx_mut()
        .db_mut()
        .insert_account_storage(NATIVE_COIN_CONTROL_ADDRESS, canonical_slot, U256::from(2))
        .expect("overwrite blocklist status");
    let result = evm
        .transact_commit(call_tx(
            QUERY_CALLER,
            NATIVE_COIN_CONTROL_ADDRESS,
            query,
            100_000,
            1,
        ))
        .expect("noncanonical blocklist query");
    assert!(decode_bool::<INativeCoinControl::isBlocklistedCall>(
        result.output().expect("query output")
    ));
}

#[test]
fn native_coin_control_zero6_auth_precedes_floor_and_late_oog_rolls_back_write() {
    let call: Bytes = INativeCoinControl::blocklistCall { account: USER }
        .abi_encode()
        .into();
    let slot = blocklist_storage_slot(USER);

    let mut unauthorized = arc_evm(InMemoryDB::default());
    let rejected = direct_call(
        &mut unauthorized,
        OTHER,
        NATIVE_COIN_CONTROL_ADDRESS,
        call.clone(),
        200,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&rejected), InstructionResult::Revert);
    assert_eq!(call_gas_spent(&rejected), 200);
    assert_eq!(
        current_storage(&mut unauthorized, NATIVE_COIN_CONTROL_ADDRESS, slot),
        U256::ZERO
    );

    let mut below_floor = arc_evm(InMemoryDB::default());
    let oog = direct_call(
        &mut below_floor,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
        call.clone(),
        4_024,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&oog), InstructionResult::PrecompileOOG);
    assert_eq!(
        current_storage(&mut below_floor, NATIVE_COIN_CONTROL_ADDRESS, slot),
        U256::ZERO
    );

    let mut success_evm = arc_evm(InMemoryDB::default());
    let success = direct_call(
        &mut success_evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
        call.clone(),
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&success), InstructionResult::Return);
    assert_eq!(call_gas_spent(&success), 23_225);

    let repeated = direct_call(
        &mut success_evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
        call.clone(),
        4_025,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&repeated), InstructionResult::Return);
    assert_eq!(call_gas_spent(&repeated), 1_225);
    assert_eq!(
        current_storage(&mut success_evm, NATIVE_COIN_CONTROL_ADDRESS, slot),
        U256::ONE
    );

    let unblocked = direct_call(
        &mut success_evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
        INativeCoinControl::unBlocklistCall { account: USER }
            .abi_encode()
            .into(),
        4_025,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&unblocked), InstructionResult::Return);
    assert_eq!(call_gas_spent(&unblocked), 1_225);
    assert_eq!(
        current_storage(&mut success_evm, NATIVE_COIN_CONTROL_ADDRESS, slot),
        U256::ZERO
    );
    assert_eq!(success_evm.ctx().journaled_state.logs.len(), 3);

    let mut late_oog = arc_evm(InMemoryDB::default());
    let failed = direct_call(
        &mut late_oog,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
        call,
        call_gas_spent(&success) - 1,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&failed), InstructionResult::PrecompileOOG);
    assert_eq!(
        current_storage(&mut late_oog, NATIVE_COIN_CONTROL_ADDRESS, slot),
        U256::ZERO
    );
    assert!(late_oog.ctx().journaled_state.logs.is_empty());
}

#[test]
fn zero8_stateful_precompile_delegatecall_rejections_charge_200_gas() {
    let cases: [(&str, Address, Address, Bytes); 3] = [
        (
            "native coin authority",
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            INativeCoinAuthority::mintCall {
                to: USER,
                amount: U256::ONE,
            }
            .abi_encode()
            .into(),
        ),
        (
            "native coin control",
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_CONTROL_ADDRESS,
            INativeCoinControl::blocklistCall { account: USER }
                .abi_encode()
                .into(),
        ),
        (
            "system accounting",
            SYSTEM_ADDRESS,
            SYSTEM_ACCOUNTING_ADDRESS,
            ISystemAccounting::storeGasValuesCall {
                blockNumber: 1,
                gasValues: GasValues {
                    gasUsed: 1,
                    gasUsedSmoothed: 2,
                    nextBaseFee: 3,
                },
            }
            .abi_encode()
            .into(),
        ),
    ];

    for (name, caller, precompile, calldata) in cases {
        let mut below_penalty = arc_evm_with_zero8(InMemoryDB::default(), true);
        let result = direct_delegatecall(
            &mut below_penalty,
            caller,
            OTHER,
            precompile,
            calldata.clone(),
            PRECOMPILE_EARLY_REVERT_GAS_PENALTY - 1,
        );
        assert_eq!(
            call_instruction(&result),
            InstructionResult::PrecompileOOG,
            "{name}: 199 gas must halt as OOG"
        );
        let mut exact_penalty = arc_evm_with_zero8(InMemoryDB::default(), true);
        let result = direct_delegatecall(
            &mut exact_penalty,
            caller,
            OTHER,
            precompile,
            calldata.clone(),
            PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
        );
        assert_eq!(
            call_instruction(&result),
            InstructionResult::Revert,
            "{name}: 200 gas must reach the delegatecall revert"
        );
        assert_eq!(call_gas_spent(&result), PRECOMPILE_EARLY_REVERT_GAS_PENALTY);
        assert_eq!(
            call_output(&result),
            revert_message_to_bytes(ERR_DELEGATE_CALL_NOT_ALLOWED).as_ref()
        );

        let mut pre_zero8 = arc_evm_with_zero8(InMemoryDB::default(), false);
        let result =
            direct_delegatecall(&mut pre_zero8, caller, OTHER, precompile, calldata, 100_000);
        assert_eq!(call_instruction(&result), InstructionResult::Revert);
        assert_eq!(
            call_gas_spent(&result),
            0,
            "{name}: pre-Zero8 delegatecall rejection must remain free"
        );
    }
}

#[test]
fn native_coin_control_zero8_orders_auth_delegatecall_then_success_floor() {
    let cases: [(&str, Bytes, &str); 2] = [
        (
            "blocklist",
            INativeCoinControl::blocklistCall { account: USER }
                .abi_encode()
                .into(),
            "Not enabled for blocklisting",
        ),
        (
            "unblocklist",
            INativeCoinControl::unBlocklistCall { account: USER }
                .abi_encode()
                .into(),
            "Not enabled for unblocklisting",
        ),
    ];

    for (name, calldata, auth_error) in cases {
        let mut unauthorized = arc_evm_with_zero8(InMemoryDB::default(), true);
        let result = direct_delegatecall(
            &mut unauthorized,
            OTHER,
            USER,
            NATIVE_COIN_CONTROL_ADDRESS,
            calldata.clone(),
            PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
        );
        assert_eq!(call_instruction(&result), InstructionResult::Revert);
        assert_eq!(
            call_output(&result),
            revert_message_to_bytes(auth_error).as_ref(),
            "{name}: authorization must run before delegatecall validation"
        );

        let mut authorized_delegate = arc_evm_with_zero8(InMemoryDB::default(), true);
        let result = direct_delegatecall(
            &mut authorized_delegate,
            NATIVE_FIAT_TOKEN_ADDRESS,
            USER,
            NATIVE_COIN_CONTROL_ADDRESS,
            calldata.clone(),
            PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
        );
        assert_eq!(call_instruction(&result), InstructionResult::Revert);
        assert_eq!(
            call_output(&result),
            revert_message_to_bytes(ERR_DELEGATE_CALL_NOT_ALLOWED).as_ref(),
            "{name}: delegatecall validation must run before the success gas floor"
        );

        let mut authorized_direct = arc_evm_with_zero8(InMemoryDB::default(), true);
        let result = direct_call(
            &mut authorized_direct,
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_CONTROL_ADDRESS,
            calldata.clone(),
            PRECOMPILE_EARLY_REVERT_GAS_PENALTY,
            U256::ZERO,
        );
        assert_eq!(
            call_instruction(&result),
            InstructionResult::PrecompileOOG,
            "{name}: a direct call must still enforce the success gas floor"
        );

        let mut pre_zero8 = arc_evm_with_zero8(InMemoryDB::default(), false);
        let result = direct_delegatecall(
            &mut pre_zero8,
            NATIVE_FIAT_TOKEN_ADDRESS,
            USER,
            NATIVE_COIN_CONTROL_ADDRESS,
            calldata,
            1_000,
        );
        assert_eq!(
            call_instruction(&result),
            InstructionResult::PrecompileOOG,
            "{name}: pre-Zero8 must retain success-floor-before-delegatecall ordering"
        );
    }
}

#[test]
fn native_coin_authority_zero8_permits_draining_an_account_to_empty() {
    let make_db = || {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            USER,
            AccountInfo {
                balance: U256::from(10),
                nonce: 0,
                ..Default::default()
            },
        );
        db.insert_account_storage(
            NATIVE_COIN_AUTHORITY_ADDRESS,
            TOTAL_SUPPLY_SLOT,
            U256::from(10),
        )
        .expect("insert total supply");
        db
    };
    let burn: Bytes = INativeCoinAuthority::burnCall {
        from: USER,
        amount: U256::from(10),
    }
    .abi_encode()
    .into();

    let mut pre_zero8 = arc_evm_with_zero8(make_db(), false);
    let result = direct_call(
        &mut pre_zero8,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        burn.clone(),
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&result), InstructionResult::Revert);
    assert_eq!(
        call_output(&result),
        revert_message_to_bytes(ERR_CLEAR_EMPTY).as_ref()
    );
    assert_eq!(current_balance(&mut pre_zero8, USER), U256::from(10));
    assert_eq!(
        current_storage(
            &mut pre_zero8,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            TOTAL_SUPPLY_SLOT,
        ),
        U256::from(10)
    );

    let mut zero8 = arc_evm_with_zero8(make_db(), true);
    let result = direct_call(
        &mut zero8,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        burn,
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&result), InstructionResult::Return);
    assert_eq!(current_balance(&mut zero8, USER), U256::ZERO);
    assert_eq!(
        current_storage(&mut zero8, NATIVE_COIN_AUTHORITY_ADDRESS, TOTAL_SUPPLY_SLOT,),
        U256::ZERO
    );
}

#[test]
fn native_coin_authority_commit_updates_slot_two_balance_and_system_logs() {
    let mut db = InMemoryDB::default();
    db.insert_account_info(USER, account_info(U256::ZERO));
    let mut evm = arc_evm(db);

    let mint: Bytes = INativeCoinAuthority::mintCall {
        to: USER,
        amount: U256::from(100),
    }
    .abi_encode()
    .into();
    let minted = evm
        .transact_commit(call_tx(
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            mint,
            120_000,
            0,
        ))
        .expect("mint transaction");
    assert!(minted.is_success());
    assert_eq!(
        minted.logs(),
        &[eip7708_transfer_log(Address::ZERO, USER, U256::from(100))]
    );

    let burn = INativeCoinAuthority::burnCall {
        from: USER,
        amount: U256::from(40),
    }
    .abi_encode()
    .into();
    let burned = evm
        .transact_commit(call_tx(
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            burn,
            120_000,
            1,
        ))
        .expect("burn transaction");
    assert!(burned.is_success());
    assert_eq!(
        burned.logs(),
        &[eip7708_transfer_log(USER, Address::ZERO, U256::from(40))]
    );

    assert_eq!(
        evm.ctx_mut()
            .db_mut()
            .storage(NATIVE_COIN_AUTHORITY_ADDRESS, TOTAL_SUPPLY_SLOT)
            .expect("committed total supply"),
        U256::from(60)
    );
    assert_eq!(
        evm.ctx_mut()
            .db_mut()
            .basic(USER)
            .expect("committed user")
            .expect("user account")
            .balance,
        U256::from(60)
    );

    let supply = evm
        .transact_commit(call_tx(
            QUERY_CALLER,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            INativeCoinAuthority::totalSupplyCall {}.abi_encode().into(),
            100_000,
            0,
        ))
        .expect("total supply query");
    assert_eq!(
        INativeCoinAuthority::totalSupplyCall::abi_decode_returns(
            supply.output().expect("supply output")
        )
        .expect("uint256 output"),
        U256::from(60)
    );
}

#[test]
fn zero_and_self_transfers_succeed_without_logs_and_self_transfer_runs_balance_path() {
    let mut db = InMemoryDB::default();
    db.insert_account_info(USER, account_info(U256::from(100)));
    let mut evm = arc_evm(db);

    let zero = direct_call(
        &mut evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        INativeCoinAuthority::transferCall {
            from: USER,
            to: OTHER,
            amount: U256::ZERO,
        }
        .abi_encode()
        .into(),
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&zero), InstructionResult::Return);
    assert!(decode_bool::<INativeCoinAuthority::transferCall>(
        call_output(&zero)
    ));
    assert!(evm.ctx().journaled_state.logs.is_empty());

    let self_transfer = direct_call(
        &mut evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        INativeCoinAuthority::transferCall {
            from: USER,
            to: USER,
            amount: U256::from(10),
        }
        .abi_encode()
        .into(),
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&self_transfer), InstructionResult::Return);
    assert!(decode_bool::<INativeCoinAuthority::transferCall>(
        call_output(&self_transfer)
    ));
    assert_eq!(current_balance(&mut evm, USER), U256::from(100));
    assert!(evm.ctx().journaled_state.logs.is_empty());
    assert!(
        call_gas_spent(&self_transfer) >= 2 * 2_900,
        "self transfer must not skip both balance writes"
    );
}

#[test]
fn native_coin_authority_empty_account_surcharge_is_exactly_25000() {
    let mint: Bytes = INativeCoinAuthority::mintCall {
        to: USER,
        amount: U256::ONE,
    }
    .abi_encode()
    .into();

    let mut nonempty_db = InMemoryDB::default();
    nonempty_db.insert_account_info(USER, account_info(U256::ZERO));
    let mut nonempty = arc_evm(nonempty_db);
    let nonempty_result = direct_call(
        &mut nonempty,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        mint.clone(),
        100_000,
        U256::ZERO,
    );
    assert_eq!(
        call_instruction(&nonempty_result),
        InstructionResult::Return
    );

    let mut empty = arc_evm(InMemoryDB::default());
    let empty_result = direct_call(
        &mut empty,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        mint,
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&empty_result), InstructionResult::Return);
    assert_eq!(
        call_gas_spent(&empty_result) - call_gas_spent(&nonempty_result),
        25_000
    );
}

#[test]
fn native_coin_authority_zero6_rejects_noncanonical_address_padding_before_state_reads() {
    let mut data = INativeCoinAuthority::mintCall {
        to: USER,
        amount: U256::ONE,
    }
    .abi_encode();
    data[4] = 1;

    let mut evm = arc_evm(InMemoryDB::default());
    let result = direct_call(
        &mut evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        data.into(),
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&result), InstructionResult::Revert);
    assert_eq!(call_gas_spent(&result), 200);
    assert_eq!(
        current_storage(&mut evm, NATIVE_COIN_AUTHORITY_ADDRESS, TOTAL_SUPPLY_SLOT,),
        U256::ZERO
    );
    assert_eq!(current_balance(&mut evm, USER), U256::ZERO);
    assert!(evm.ctx().journaled_state.logs.is_empty());
}

#[test]
fn native_coin_authority_late_oog_reverts_state_value_and_both_logs() {
    let mint: Bytes = INativeCoinAuthority::mintCall {
        to: USER,
        amount: U256::from(10),
    }
    .abi_encode()
    .into();

    let mut success_db = InMemoryDB::default();
    success_db.insert_account_info(USER, account_info(U256::from(5)));
    success_db.insert_account_info(NATIVE_FIAT_TOKEN_ADDRESS, account_info(U256::from(100)));
    let mut success_evm = arc_evm(success_db);
    let success = direct_call(
        &mut success_evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        mint.clone(),
        100_000,
        U256::from(7),
    );
    assert_eq!(call_instruction(&success), InstructionResult::Return);
    let exact_mint_gas = call_gas_spent(&success);
    assert_eq!(exact_mint_gas, 31_456);
    assert_eq!(
        success_evm.ctx().journaled_state.logs,
        vec![
            eip7708_transfer_log(
                NATIVE_FIAT_TOKEN_ADDRESS,
                NATIVE_COIN_AUTHORITY_ADDRESS,
                U256::from(7),
            ),
            eip7708_transfer_log(Address::ZERO, USER, U256::from(10)),
        ]
    );

    let mut failed_db = InMemoryDB::default();
    failed_db.insert_account_info(USER, account_info(U256::from(5)));
    failed_db.insert_account_info(NATIVE_FIAT_TOKEN_ADDRESS, account_info(U256::from(100)));
    let mut failed_evm = arc_evm(failed_db);
    let failed = direct_call(
        &mut failed_evm,
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        mint,
        exact_mint_gas - 1,
        U256::from(7),
    );
    assert_eq!(call_instruction(&failed), InstructionResult::PrecompileOOG);
    assert_eq!(
        current_storage(
            &mut failed_evm,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            TOTAL_SUPPLY_SLOT,
        ),
        U256::ZERO
    );
    assert_eq!(current_balance(&mut failed_evm, USER), U256::from(5));
    assert_eq!(
        current_balance(&mut failed_evm, NATIVE_FIAT_TOKEN_ADDRESS),
        U256::from(100)
    );
    assert_eq!(
        current_balance(&mut failed_evm, NATIVE_COIN_AUTHORITY_ADDRESS),
        U256::ZERO
    );
    assert!(failed_evm.ctx().journaled_state.logs.is_empty());
}

#[test]
fn parent_revert_rolls_back_successful_native_coin_authority_child() {
    let mint = INativeCoinAuthority::mintCall {
        to: USER,
        amount: U256::from(10),
    }
    .abi_encode();
    let code = reverting_parent_code(&mint);
    let mut db = InMemoryDB::default();
    db.insert_account_info(USER, account_info(U256::from(5)));
    db.insert_account_info(
        NATIVE_FIAT_TOKEN_ADDRESS,
        AccountInfo {
            nonce: 1,
            code_hash: keccak256(code.original_bytes()),
            code: Some(code),
            ..Default::default()
        },
    );
    let mut evm = arc_evm(db);
    let result = evm
        .transact(call_tx(
            OTHER,
            NATIVE_FIAT_TOKEN_ADDRESS,
            Bytes::new(),
            200_000,
            0,
        ))
        .expect("parent transaction executes");

    let ExecutionResult::Revert { output, .. } = &result.result else {
        panic!("parent transaction must revert");
    };
    assert_eq!(
        output.as_ref(),
        U256::from(1).to_be_bytes::<32>(),
        "the NCA child CALL must succeed before its state is rolled back"
    );
    assert!(result.result.logs().is_empty());
    assert!(
        result
            .state
            .get(&NATIVE_COIN_AUTHORITY_ADDRESS)
            .and_then(|account| account.storage.get(&TOTAL_SUPPLY_SLOT))
            .is_none_or(|slot| slot.present_value().is_zero()),
        "parent REVERT must roll back the child totalSupply write"
    );
    assert_eq!(
        result
            .state
            .get(&USER)
            .map_or(U256::from(5), |account| account.info.balance),
        U256::from(5),
        "parent REVERT must roll back the child balance increment"
    );
}

#[test]
fn total_supply_accepts_zero6_trailing_bytes() {
    let mut data = INativeCoinAuthority::totalSupplyCall {}.abi_encode();
    data.extend_from_slice(&[0u8; 32]);
    let mut evm = arc_evm(InMemoryDB::default());
    let result = direct_call(
        &mut evm,
        QUERY_CALLER,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        data.into(),
        10_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&result), InstructionResult::Return);
    assert_eq!(call_gas_spent(&result), 2_100);
    assert_eq!(
        INativeCoinAuthority::totalSupplyCall::abi_decode_returns(call_output(&result))
            .expect("supply output"),
        U256::ZERO
    );
}

#[test]
fn system_accounting_commit_uses_ring_64_and_exact_packing() {
    let values = GasValues {
        gasUsed: 0x0102_0304_0506_0708,
        gasUsedSmoothed: 0x1112_1314_1516_1718,
        nextBaseFee: 0x2122_2324_2526_2728,
    };
    let mut evm = arc_evm(InMemoryDB::default());
    let stored = evm
        .transact_commit(call_tx(
            SYSTEM_ADDRESS,
            SYSTEM_ACCOUNTING_ADDRESS,
            ISystemAccounting::storeGasValuesCall {
                blockNumber: 5,
                gasValues: values.clone(),
            }
            .abi_encode()
            .into(),
            100_000,
            0,
        ))
        .expect("system accounting store");
    assert!(stored.is_success());
    assert!(stored.logs().is_empty());
    assert_eq!(
        compute_gas_values_storage_slot(5),
        compute_gas_values_storage_slot(69)
    );

    let raw = evm
        .ctx_mut()
        .db_mut()
        .storage(
            SYSTEM_ACCOUNTING_ADDRESS,
            compute_gas_values_storage_slot(5).into(),
        )
        .expect("stored packed values")
        .to_be_bytes::<32>();
    assert_eq!(&raw[0..8], &[0u8; 8]);
    assert_eq!(&raw[8..16], &values.nextBaseFee.to_be_bytes());
    assert_eq!(&raw[16..24], &values.gasUsedSmoothed.to_be_bytes());
    assert_eq!(&raw[24..32], &values.gasUsed.to_be_bytes());

    let queried = evm
        .transact_commit(call_tx(
            QUERY_CALLER,
            SYSTEM_ACCOUNTING_ADDRESS,
            ISystemAccounting::getGasValuesCall { blockNumber: 69 }
                .abi_encode()
                .into(),
            100_000,
            0,
        ))
        .expect("system accounting query");
    let decoded = ISystemAccounting::getGasValuesCall::abi_decode_returns(
        queried.output().expect("gas values output"),
    )
    .expect("gas values decode");
    assert_eq!(decoded.gasUsed, values.gasUsed);
    assert_eq!(decoded.gasUsedSmoothed, values.gasUsedSmoothed);
    assert_eq!(decoded.nextBaseFee, values.nextBaseFee);
}

#[test]
fn system_accounting_enforces_caller_and_sstore_gas_before_mutation() {
    let call: Bytes = ISystemAccounting::storeGasValuesCall {
        blockNumber: 7,
        gasValues: GasValues {
            gasUsed: 1,
            gasUsedSmoothed: 2,
            nextBaseFee: 3,
        },
    }
    .abi_encode()
    .into();
    let slot: U256 = compute_gas_values_storage_slot(7).into();

    let mut exact = arc_evm(InMemoryDB::default());
    let success = direct_call(
        &mut exact,
        SYSTEM_ADDRESS,
        SYSTEM_ACCOUNTING_ADDRESS,
        call.clone(),
        22_100,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&success), InstructionResult::Return);
    assert_eq!(call_gas_spent(&success), 22_100);
    assert_ne!(
        current_storage(&mut exact, SYSTEM_ACCOUNTING_ADDRESS, slot),
        U256::ZERO
    );

    for gas_limit in [22_099, 2_300] {
        let mut evm = arc_evm(InMemoryDB::default());
        let failed = direct_call(
            &mut evm,
            SYSTEM_ADDRESS,
            SYSTEM_ACCOUNTING_ADDRESS,
            call.clone(),
            gas_limit,
            U256::ZERO,
        );
        assert_eq!(call_instruction(&failed), InstructionResult::PrecompileOOG);
        assert_eq!(
            current_storage(&mut evm, SYSTEM_ACCOUNTING_ADDRESS, slot),
            U256::ZERO
        );
        assert!(evm.ctx().journaled_state.logs.is_empty());
    }

    let mut unauthorized = arc_evm(InMemoryDB::default());
    let failed = direct_call(
        &mut unauthorized,
        USER,
        SYSTEM_ACCOUNTING_ADDRESS,
        call,
        100_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&failed), InstructionResult::Revert);
    assert_eq!(call_gas_spent(&failed), 200);
    assert_eq!(
        current_storage(&mut unauthorized, SYSTEM_ACCOUNTING_ADDRESS, slot),
        U256::ZERO
    );
}

#[derive(Debug)]
struct InjectedStorageError;

impl fmt::Display for InjectedStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("injected native coin control storage error")
    }
}

impl Error for InjectedStorageError {}
impl DBErrorMarker for InjectedStorageError {}

#[derive(Clone, Debug)]
struct FailingBlocklistDb {
    inner: InMemoryDB,
    failing_slot: U256,
    ncc_reads: Vec<U256>,
}

fn infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

impl RevmDatabase for FailingBlocklistDb {
    type Error = InjectedStorageError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(infallible(self.inner.basic(address)))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(infallible(self.inner.code_by_hash(code_hash)))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if address == NATIVE_COIN_CONTROL_ADDRESS {
            self.ncc_reads.push(index);
            if index == self.failing_slot {
                return Err(InjectedStorageError);
            }
        }
        Ok(infallible(self.inner.storage(address, index)))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(infallible(self.inner.block_hash(number)))
    }
}

#[derive(Debug)]
struct InjectedRecipientError;

impl fmt::Display for InjectedRecipientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("injected recipient account load error")
    }
}

impl Error for InjectedRecipientError {}
impl DBErrorMarker for InjectedRecipientError {}

#[derive(Clone, Debug)]
struct FailingRecipientDb {
    inner: InMemoryDB,
    failed_recipient_reads: usize,
}

impl RevmDatabase for FailingRecipientDb {
    type Error = InjectedRecipientError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if address == USER {
            self.failed_recipient_reads += 1;
            return Err(InjectedRecipientError);
        }
        Ok(infallible(self.inner.basic(address)))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(infallible(self.inner.code_by_hash(code_hash)))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(infallible(self.inner.storage(address, index)))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(infallible(self.inner.block_hash(number)))
    }
}

#[test]
fn blocklist_db_error_becomes_precompile_halt_and_leaks_no_custom_state_or_logs() {
    let mut inner = InMemoryDB::default();
    inner.insert_account_info(USER, account_info(U256::from(5)));
    let db = FailingBlocklistDb {
        inner,
        failing_slot: blocklist_storage_slot(USER),
        ncc_reads: Vec::new(),
    };
    let mut evm = arc_evm(db);
    let mint = INativeCoinAuthority::mintCall {
        to: USER,
        amount: U256::from(10),
    }
    .abi_encode()
    .into();
    let result = evm
        .transact(call_tx(
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            mint,
            100_000,
            0,
        ))
        .expect("DB failure inside precompile is an EVM halt");

    assert!(matches!(
        result.result,
        ExecutionResult::Halt {
            reason: HaltReason::PrecompileError,
            ..
        }
    ));
    assert!(result.result.logs().is_empty());
    assert_eq!(
        evm.ctx().journaled_state.db().ncc_reads,
        [
            blocklist_storage_slot(NATIVE_FIAT_TOKEN_ADDRESS),
            blocklist_storage_slot(USER),
        ]
    );
    let nca = result.state.get(&NATIVE_COIN_AUTHORITY_ADDRESS);
    assert!(
        nca.and_then(|account| account.storage.get(&TOTAL_SUPPLY_SLOT))
            .is_none_or(|slot| slot.present_value().is_zero()),
        "total supply write must not escape the failed precompile"
    );
    assert_eq!(
        result
            .state
            .get(&USER)
            .map_or(U256::from(5), |account| account.info.balance),
        U256::from(5)
    );
}

#[test]
fn recipient_db_error_after_total_supply_write_rolls_back_partial_mutation() {
    let mut inner = InMemoryDB::default();
    inner.insert_account_info(USER, account_info(U256::from(5)));
    let db = FailingRecipientDb {
        inner,
        failed_recipient_reads: 0,
    };
    let mut evm = arc_evm(db);
    let result = evm
        .transact(call_tx(
            NATIVE_FIAT_TOKEN_ADDRESS,
            NATIVE_COIN_AUTHORITY_ADDRESS,
            INativeCoinAuthority::mintCall {
                to: USER,
                amount: U256::from(10),
            }
            .abi_encode()
            .into(),
            100_000,
            0,
        ))
        .expect("recipient DB failure inside precompile is an EVM halt");

    assert!(matches!(
        result.result,
        ExecutionResult::Halt {
            reason: HaltReason::PrecompileError,
            ..
        }
    ));
    assert!(result.result.logs().is_empty());
    assert_eq!(evm.ctx().journaled_state.db().failed_recipient_reads, 1);
    assert!(
        result
            .state
            .get(&NATIVE_COIN_AUTHORITY_ADDRESS)
            .and_then(|account| account.storage.get(&TOTAL_SUPPLY_SLOT))
            .is_none_or(|slot| slot.present_value().is_zero()),
        "totalSupply was written before recipient load and must be rolled back"
    );
    assert_eq!(
        infallible(evm.ctx_mut().db_mut().inner.basic(USER))
            .expect("fixture recipient")
            .balance,
        U256::from(5)
    );
}

#[test]
fn transact_and_inspect_return_exactly_the_same_result_and_state() {
    let mut db = InMemoryDB::default();
    db.insert_account_info(USER, account_info(U256::from(5)));
    let tx = call_tx(
        NATIVE_FIAT_TOKEN_ADDRESS,
        NATIVE_COIN_AUTHORITY_ADDRESS,
        INativeCoinAuthority::mintCall {
            to: USER,
            amount: U256::from(10),
        }
        .abi_encode()
        .into(),
        100_000,
        0,
    );

    let mut normal = arc_evm(db.clone());
    let normal_result = normal.transact(tx.clone()).expect("normal transaction");
    let mut inspected = arc_evm(db);
    let inspect_result = inspected
        .inspect(tx, NoOpInspector {})
        .expect("inspected transaction");
    assert_eq!(normal_result, inspect_result);
}

#[derive(Debug, Deserialize)]
struct PqFixture {
    slh_dsa_sha2_128s: Vec<PqVector>,
}

#[derive(Debug, Deserialize)]
struct PqVector {
    verifying_key: String,
    message: String,
    signature: String,
    is_valid: bool,
}

fn decode_hex(value: &str) -> Vec<u8> {
    hex::decode(value.strip_prefix("0x").expect("0x-prefixed fixture value"))
        .expect("valid fixture hex")
}

fn pq_call(vector: &PqVector) -> Bytes {
    IPQ::verifySlhDsaSha2128sCall {
        vk: decode_hex(&vector.verifying_key).into(),
        message: decode_hex(&vector.message).into(),
        sig: decode_hex(&vector.signature).into(),
    }
    .abi_encode()
    .into()
}

#[test]
fn pq_matches_arc_v073_checked_in_static_vectors_and_exact_gas() {
    // Source: arc-node v0.7.3 commit 79b6fddf18345732007bb94b4af3add4c2efd12d,
    // tests/helpers/pq_test_vectors.json. Keep this static: runtime sign+verify is only self-consistency.
    const FIXTURE: &str = include_str!("pq_test_vectors.json");
    assert_eq!(
        hex::encode(Sha256::digest(FIXTURE.as_bytes())),
        "3ad19c1064dc7030f777305a015c5ada899e116f7f09b2f4b59effb3aeb2c012"
    );
    let fixture: PqFixture = serde_json::from_str(FIXTURE).expect("official PQ fixture JSON");
    assert_eq!(fixture.slh_dsa_sha2_128s.len(), 3);

    for vector in &fixture.slh_dsa_sha2_128s {
        let message_len = decode_hex(&vector.message).len() as u64;
        let expected_gas = 230_000 + message_len.div_ceil(32) * 6;
        let mut evm = arc_evm(InMemoryDB::default());
        let result = direct_call(
            &mut evm,
            USER,
            PQ_ADDRESS,
            pq_call(vector),
            expected_gas,
            U256::ZERO,
        );
        assert_eq!(call_instruction(&result), InstructionResult::Return);
        assert_eq!(call_gas_spent(&result), expected_gas);
        assert_eq!(
            decode_bool::<IPQ::verifySlhDsaSha2128sCall>(call_output(&result)),
            vector.is_valid
        );
    }
}

#[test]
fn pq_charges_before_length_validation_and_classifies_malformed_and_oog() {
    let fixture: PqFixture = serde_json::from_str(include_str!("pq_test_vectors.json"))
        .expect("official PQ fixture JSON");
    let vector = &fixture.slh_dsa_sha2_128s[0];
    let vk = decode_hex(&vector.verifying_key);
    let message = decode_hex(&vector.message);
    let sig = decode_hex(&vector.signature);
    let exact_gas = 230_000 + (message.len() as u64).div_ceil(32) * 6;

    for (bad_vk, bad_sig) in [
        (vk[..31].to_vec(), sig.clone()),
        (vk.clone(), sig[..7855].to_vec()),
    ] {
        let data = IPQ::verifySlhDsaSha2128sCall {
            vk: bad_vk.into(),
            message: message.clone().into(),
            sig: bad_sig.into(),
        }
        .abi_encode()
        .into();
        let mut evm = arc_evm(InMemoryDB::default());
        let result = direct_call(&mut evm, USER, PQ_ADDRESS, data, exact_gas, U256::ZERO);
        assert_eq!(call_instruction(&result), InstructionResult::Revert);
        assert_eq!(call_gas_spent(&result), exact_gas);
    }

    let mut evm = arc_evm(InMemoryDB::default());
    let malformed = direct_call(
        &mut evm,
        USER,
        PQ_ADDRESS,
        IPQ::verifySlhDsaSha2128sCall::SELECTOR.into(),
        1_000_000,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&malformed), InstructionResult::Revert);
    assert_eq!(call_gas_spent(&malformed), 200);

    let mut evm = arc_evm(InMemoryDB::default());
    let oog = direct_call(
        &mut evm,
        USER,
        PQ_ADDRESS,
        pq_call(vector),
        exact_gas - 1,
        U256::ZERO,
    );
    assert_eq!(call_instruction(&oog), InstructionResult::PrecompileOOG);
    assert!(evm.ctx().journaled_state.logs.is_empty());
}
