//! Exercise the production OP config parser, transaction builder and both execution paths.
use super::build_op_custom_config;
use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::{core::EvmExecutor, ApiImpl};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use leafage_evm_types::{
    AccountInfo, Address, BlockInfo, Bytecode, Bytes, CallRequest, CfgEnv, OpSpecId, U256,
};
use op_revm::{OpHaltReason, OpTransactionError};
use revm::context::result::{ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::BlockEnv;
use revm::database::{CacheDB, EmptyDB};
use revm_inspectors::tracing::TracingInspectorConfig;

const RISE: &str = r#"{"op_spec_id":"Jovian","limit_contract_code_size":262144,"limit_contract_initcode_size":524288}"#;
const METIS: &str =
    r#"{"limit_contract_code_size":2457600,"limit_contract_initcode_size":"unlimited"}"#;

fn cfg(json: &str) -> CfgEnv<OpSpecId> {
    let mut cfg = build_op_custom_config(Some(json)).unwrap();
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_base_fee = true;
    cfg.disable_block_gas_limit = true;
    // Metis code-deposit boundary needs ~492M gas. Test-only, not an RPC default.
    cfg.tx_gas_limit_cap = Some(1_000_000_000);
    cfg
}

fn execute(
    cfg: CfgEnv<OpSpecId>,
    data: Vec<u8>,
    contract: Option<Vec<u8>>,
    inspect: bool,
) -> Result<
    ExecutionResult<OpHaltReason>,
    revm::context::result::EVMError<std::convert::Infallible, OpTransactionError>,
> {
    let api: ApiImpl<(), OpSpecId, NoneEvmCustomConfig> = ApiImpl::new(
        (),
        cfg,
        None,
        None,
        None,
        None,
        false,
        false,
        String::new(),
        0,
        None,
        None,
        None,
    );
    let mut db = CacheDB::new(EmptyDB::default());
    let mut req = TransactionRequest::default()
        .from(Address::repeat_byte(0x11))
        .gas_limit(1_000_000_000)
        .input(TransactionInput::new(Bytes::from(data)));
    if let Some(code) = contract {
        let address = Address::repeat_byte(0x22);
        db.insert_account_info(
            address,
            AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
        );
        req = req.to(address);
    }
    let block = BlockEnv::default();
    let tx = api
        .create_txn_env(
            &BlockInfo::default(),
            &block,
            CallRequest {
                inner: req,
                other: Default::default(),
            },
            &db,
            1,
        )
        .unwrap();
    if inspect {
        api.inspect_tx_commit(
            &block,
            &mut db,
            TracingInspectorConfig::default_parity(),
            |_| (),
            tx,
        )
        .map(|(result, _)| result)
    } else {
        api.transact(&block, &db, tx)
    }
}

fn runtime(size: usize) -> Vec<u8> {
    // PUSH3 size; PUSH1 0; RETURN: return zero-filled runtime without a large initcode.
    vec![
        0x62,
        (size >> 16) as u8,
        (size >> 8) as u8,
        size as u8,
        0x60,
        0,
        0xf3,
    ]
}

fn factory(create2: bool) -> Vec<u8> {
    // Copy calldata to memory, CREATE/CREATE2 it, return the address (zero on child failure).
    let mut code = vec![0x36, 0x60, 0, 0x60, 0, 0x37];
    if create2 {
        code.extend([0x60, 0]);
    } // salt
    code.extend([
        0x36,
        0x60,
        0,
        0x60,
        0,
        if create2 { 0xf5 } else { 0xf0 },
        0x60,
        0,
        0x52,
        0x60,
        32,
        0x60,
        0,
        0xf3,
    ]);
    code
}

#[test]
fn code_limits_cover_top_level_and_internal_creation() {
    for (limit, json) in [
        (24576, r#"{"op_spec_id":"Jovian"}"#),
        (262144, RISE),
        (2457600, METIS),
    ] {
        for inspect in [false, true] {
            for size in [limit, limit + 1] {
                let config = cfg(json);
                let result = execute(config.clone(), runtime(size), None, inspect).unwrap();
                if size == limit {
                    assert!(result.is_success(), "{result:?}");
                    assert_eq!(result.output().unwrap().len(), size);
                } else {
                    assert!(
                        matches!(
                            result,
                            ExecutionResult::Halt {
                                reason: OpHaltReason::Base(HaltReason::CreateContractSizeLimit),
                                ..
                            }
                        ),
                        "{result:?}"
                    );
                }
                for create2 in [false, true] {
                    let result = execute(
                        config.clone(),
                        runtime(size),
                        Some(factory(create2)),
                        inspect,
                    )
                    .unwrap();
                    assert!(result.is_success(), "{result:?}");
                    assert_eq!(
                        U256::from_be_slice(result.output().unwrap()) != U256::ZERO,
                        size == limit
                    );
                }
            }
        }
    }
}

#[test]
fn initcode_limits_cover_top_level_and_internal_creation() {
    for (limit, json) in [
        (49152, r#"{"op_spec_id":"Jovian"}"#),
        (
            524288,
            r#"{"op_spec_id":"Jovian","limit_contract_initcode_size":524288}"#,
        ),
        // Code-only override: initcode follows revm's 2 * code size.
        (
            524288,
            r#"{"op_spec_id":"Jovian","limit_contract_code_size":262144}"#,
        ),
    ] {
        for inspect in [false, true] {
            let config = cfg(json);
            assert!(execute(config.clone(), vec![0; limit], None, inspect)
                .unwrap()
                .is_success());
            let err = execute(config.clone(), vec![0; limit + 1], None, inspect).unwrap_err();
            assert!(
                matches!(
                    err,
                    revm::context::result::EVMError::Transaction(OpTransactionError::Base(
                        InvalidTransaction::CreateInitCodeSizeLimit
                    ))
                ),
                "{err:?}"
            );
            for create2 in [false, true] {
                let good = execute(
                    config.clone(),
                    vec![0; limit],
                    Some(factory(create2)),
                    inspect,
                )
                .unwrap();
                assert!(good.is_success(), "{good:?}");
                assert_ne!(U256::from_be_slice(good.output().unwrap()), U256::ZERO);
                let bad = execute(
                    config.clone(),
                    vec![0; limit + 1],
                    Some(factory(create2)),
                    inspect,
                )
                .unwrap();
                assert!(
                    matches!(
                        bad,
                        ExecutionResult::Halt {
                            reason: OpHaltReason::Base(HaltReason::CreateInitCodeSizeLimit),
                            ..
                        }
                    ),
                    "{bad:?}"
                );
            }
        }
    }
}

#[test]
fn unlimited_initcode_and_early_forks_do_not_add_a_length_limit() {
    for inspect in [false, true] {
        let metis = cfg(METIS);
        for contract in [None, Some(factory(false)), Some(factory(true))] {
            assert!(execute(metis.clone(), vec![0; 4915201], contract, inspect)
                .unwrap()
                .is_success());
        }
        for spec in ["Bedrock", "Regolith"] {
            let early = cfg(&format!(
                r#"{{"op_spec_id":"{spec}","limit_contract_initcode_size":0}}"#
            ));
            assert!(execute(early, vec![0; 49153], None, inspect)
                .unwrap()
                .is_success());
        }
    }
}

#[test]
fn clz_follows_spec_with_rise_size_overrides() {
    for inspect in [false, true] {
        let code = vec![0x60, 1, 0x1e, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3];
        let rise = execute(cfg(RISE), vec![], Some(code.clone()), inspect).unwrap();
        assert!(
            matches!(
                rise,
                ExecutionResult::Halt {
                    reason: OpHaltReason::Base(HaltReason::NotActivated),
                    ..
                }
            ),
            "{rise:?}"
        );
        let osaka = execute(cfg("{}"), vec![], Some(code), inspect).unwrap();
        assert_eq!(
            U256::from_be_slice(osaka.output().unwrap()),
            U256::from(255)
        );
    }
}

#[test]
fn p256_precompile_follows_op_fork_in_normal_and_trace_execution() {
    // revm-precompile secp256r1 ok_1 (daimo-eth/p256-verifier test vectors).
    let input: Bytes = "4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4da73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff37618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e".parse().unwrap();
    for inspect in [false, true] {
        let mut results = Vec::new();
        for spec in ["Ecotone", "Fjord"] {
            let api: ApiImpl<(), OpSpecId, NoneEvmCustomConfig> = ApiImpl::new(
                (),
                cfg(&format!(r#"{{"op_spec_id":"{spec}"}}"#)),
                None,
                None,
                None,
                None,
                false,
                false,
                String::new(),
                0,
                None,
                None,
                None,
            );
            let mut db = CacheDB::new(EmptyDB::default());
            let req = CallRequest {
                inner: TransactionRequest::default()
                    .to(Address::from_word(U256::from(256).into()))
                    .from(Address::repeat_byte(0x11))
                    .gas_limit(100000)
                    .input(TransactionInput::new(input.clone())),
                other: Default::default(),
            };
            let block = BlockEnv::default();
            let tx = api
                .create_txn_env(&BlockInfo::default(), &block, req, &db, 1)
                .unwrap();
            let result = if inspect {
                api.inspect_tx_commit(
                    &block,
                    &mut db,
                    TracingInspectorConfig::default_parity(),
                    |_| (),
                    tx,
                )
                .unwrap()
                .0
            } else {
                api.transact(&block, &db, tx).unwrap()
            };
            assert!(result.is_success(), "{result:?}");
            results.push(result);
        }
        assert!(results[0].output().unwrap().is_empty());
        assert_eq!(
            U256::from_be_slice(results[1].output().unwrap()),
            U256::from(1)
        );
        assert_eq!(results[1].gas_used() - results[0].gas_used(), 3450);
    }
}

/// On OP Mainnet a call runs the fork of its block through both execution paths:
/// PUSH0 halts on the last pre-Canyon block and runs from the Canyon block on.
#[test]
fn op_mainnet_calls_run_the_block_fork() {
    let mut cfg = cfg("{}");
    cfg.chain_id = 10;
    let api: ApiImpl<(), OpSpecId, NoneEvmCustomConfig> = ApiImpl::new(
        (),
        cfg,
        None,
        None,
        None,
        None,
        false,
        false,
        String::new(),
        0,
        None,
        None,
        None,
    );
    let target = Address::repeat_byte(0x22);
    let mut db = CacheDB::new(EmptyDB::default());
    // PUSH0, PUSH0, MSTORE, PUSH1 32, PUSH0, RETURN
    db.insert_account_info(
        target,
        AccountInfo::default().with_code(Bytecode::new_raw(
            vec![0x5f, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3].into(),
        )),
    );
    let at = |number: u64, timestamp: u64| BlockEnv {
        number: U256::from(number),
        timestamp: U256::from(timestamp),
        ..Default::default()
    };
    for inspect in [false, true] {
        for (block, canyon) in [
            (at(114_696_811, 1_704_992_399), false),
            (at(114_696_812, 1_704_992_401), true),
        ] {
            let req = CallRequest {
                inner: TransactionRequest::default()
                    .to(target)
                    .from(Address::repeat_byte(0x11))
                    .gas_limit(100_000),
                ..Default::default()
            };
            let tx = api
                .create_txn_env(&BlockInfo::default(), &block, req, &db, 10)
                .unwrap();
            let result = if inspect {
                api.inspect_tx_commit(
                    &block,
                    &mut db,
                    TracingInspectorConfig::default_parity(),
                    |_| (),
                    tx,
                )
                .unwrap()
                .0
            } else {
                api.transact(&block, &db, tx).unwrap()
            };
            if canyon {
                assert!(result.is_success(), "{result:?}");
            } else {
                assert!(
                    matches!(
                        result,
                        ExecutionResult::Halt {
                            reason: OpHaltReason::Base(HaltReason::NotActivated),
                            ..
                        }
                    ),
                    "{result:?}"
                );
            }
        }
    }
}

/// Other chains with a built-in schedule follow it too: on Soneium (1868) and Unichain
/// (130) CLZ halts on the last pre-Karst block and runs on the Karst block. A chain
/// without a schedule keeps the configured spec (Karst) at the same height.
#[test]
fn scheduled_op_stack_chains_run_the_block_fork() {
    let api_for = |chain_id: u64| -> ApiImpl<(), OpSpecId, NoneEvmCustomConfig> {
        let mut cfg = cfg("{}");
        cfg.chain_id = chain_id;
        ApiImpl::new(
            (),
            cfg,
            None,
            None,
            None,
            None,
            false,
            false,
            String::new(),
            0,
            None,
            None,
            None,
        )
    };
    let target = Address::repeat_byte(0x22);
    let mut db = CacheDB::new(EmptyDB::default());
    // PUSH1 1, CLZ, PUSH1 0, MSTORE, PUSH1 32, PUSH1 0, RETURN
    db.insert_account_info(
        target,
        AccountInfo::default().with_code(Bytecode::new_raw(
            vec![0x60, 1, 0x1e, 0x60, 0, 0x52, 0x60, 0x20, 0x60, 0, 0xf3].into(),
        )),
    );
    let at = |number: u64, timestamp: u64| BlockEnv {
        number: U256::from(number),
        timestamp: U256::from(timestamp),
        ..Default::default()
    };
    let pre_karst = at(25_195_824, 1_783_526_399);
    let karst = at(25_195_825, 1_783_526_401);
    let uni_pre_karst = at(52_778_041, 1_783_526_400);
    let uni_karst = at(52_778_042, 1_783_526_401);
    for inspect in [false, true] {
        for (chain_id, block, runs) in [
            (1868, &pre_karst, false),
            (1868, &karst, true),
            (130, &uni_pre_karst, false),
            (130, &uni_karst, true),
            (480, &pre_karst, true),
        ] {
            let api = api_for(chain_id);
            let req = CallRequest {
                inner: TransactionRequest::default()
                    .to(target)
                    .from(Address::repeat_byte(0x11))
                    .gas_limit(100_000),
                ..Default::default()
            };
            let tx = api
                .create_txn_env(&BlockInfo::default(), block, req, &db, chain_id)
                .unwrap();
            let result = if inspect {
                api.inspect_tx_commit(
                    block,
                    &mut db,
                    TracingInspectorConfig::default_parity(),
                    |_| (),
                    tx,
                )
                .unwrap()
                .0
            } else {
                api.transact(block, &db, tx).unwrap()
            };
            if runs {
                assert_eq!(
                    U256::from_be_slice(result.output().unwrap()),
                    U256::from(255),
                    "chain {chain_id}: {result:?}"
                );
            } else {
                assert!(
                    matches!(
                        result,
                        ExecutionResult::Halt {
                            reason: OpHaltReason::Base(HaltReason::NotActivated),
                            ..
                        }
                    ),
                    "chain {chain_id}: {result:?}"
                );
            }
        }
    }
}

mod pre_bedrock {
    //! OP Mainnet pre-Bedrock calls follow l2geth (OVM 2.0). Expected values were
    //! measured against the legacy l2geth archive (op-pre-l2geth) on 2026-11-05.
    use super::*;
    use leafage_evm_chains::op::legacy::{
        not_whitelisted_output, OVM_DEPLOYER_WHITELIST, OVM_FEE_WALLET,
    };

    const ISTANBUL_BLOCK: u64 = 1_000_000;
    const BERLIN_BLOCK: u64 = 3_950_000;
    const CALLER: Address = Address::repeat_byte(0x11);
    const TARGET: Address = Address::repeat_byte(0x22);
    // MSTORE(0, top); RETURN(0, 32)
    const RET: [u8; 8] = [0x60, 0, 0x52, 0x60, 0x20, 0x60, 0, 0xf3];

    fn api() -> ApiImpl<(), OpSpecId, NoneEvmCustomConfig> {
        let mut cfg = cfg("{}");
        cfg.chain_id = 10;
        ApiImpl::new(
            (),
            cfg,
            None,
            None,
            None,
            None,
            false,
            false,
            String::new(),
            0,
            None,
            None,
            None,
        )
    }

    /// A pre-Bedrock header as leafage stores it: difficulty 2, zero miner.
    fn block(number: u64) -> BlockEnv {
        BlockEnv {
            number: U256::from(number),
            timestamp: U256::from(1_639_128_595u64),
            difficulty: U256::from(2),
            gas_limit: 15_000_000,
            ..Default::default()
        }
    }

    fn with_code(code: &[u8]) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            TARGET,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::copy_from_slice(code))),
        );
        db
    }

    fn whitelist_owned(db: &mut CacheDB<EmptyDB>) {
        db.insert_account_info(OVM_DEPLOYER_WHITELIST, AccountInfo::default());
        db.insert_account_storage(OVM_DEPLOYER_WHITELIST, U256::ZERO, U256::from(0xabc))
            .unwrap();
    }

    fn whitelist(db: &mut CacheDB<EmptyDB>, deployer: Address) {
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(deployer.as_slice());
        preimage[63] = 1;
        let key: U256 = revm::primitives::keccak256(preimage).into();
        db.insert_account_storage(OVM_DEPLOYER_WHITELIST, key, U256::ONE)
            .unwrap();
    }

    fn run(
        db: &mut CacheDB<EmptyDB>,
        number: u64,
        to: Option<Address>,
        data: Vec<u8>,
        inspect: bool,
    ) -> ExecutionResult<OpHaltReason> {
        let api = api();
        let block = block(number);
        let mut req = TransactionRequest::default()
            .from(CALLER)
            .gas_limit(1_000_000)
            .input(TransactionInput::new(Bytes::from(data)));
        if let Some(to) = to {
            req = req.to(to);
        }
        let req = CallRequest {
            inner: req,
            ..Default::default()
        };
        let tx = api
            .create_txn_env(&BlockInfo::default(), &block, req, &*db, 10)
            .unwrap();
        if inspect {
            api.inspect_tx_commit(
                &block,
                db,
                TracingInspectorConfig::default_parity(),
                |_| (),
                tx,
            )
            .unwrap()
            .0
        } else {
            api.transact(&block, &*db, tx).unwrap()
        }
    }

    fn word(result: &ExecutionResult<OpHaltReason>) -> U256 {
        assert!(result.is_success(), "{result:?}");
        U256::from_be_slice(result.output().unwrap())
    }

    fn probe(code: &[u8], number: u64, inspect: bool) -> ExecutionResult<OpHaltReason> {
        let mut code = code.to_vec();
        code.extend(RET);
        run(&mut with_code(&code), number, Some(TARGET), vec![], inspect)
    }

    #[test]
    fn block_context_is_the_ovm_one() {
        for inspect in [false, true] {
            for number in [ISTANBUL_BLOCK, BERLIN_BLOCK] {
                // COINBASE is the OVM fee wallet, DIFFICULTY is zero.
                assert_eq!(
                    word(&probe(&[0x41], number, inspect)),
                    U256::from_be_slice(OVM_FEE_WALLET.as_slice())
                );
                assert_eq!(word(&probe(&[0x44], number, inspect)), U256::ZERO);
                // No London: BASEFEE is invalid.
                assert!(
                    matches!(
                        probe(&[0x48], number, inspect),
                        ExecutionResult::Halt {
                            reason: OpHaltReason::Base(HaltReason::NotActivated),
                            ..
                        }
                    ),
                    "BASEFEE at {number}"
                );
            }
        }
        // Bedrock and later keep the OP path: COINBASE is the block's beneficiary.
        assert_eq!(word(&probe(&[0x41], 105_235_063, false)), U256::ZERO);
    }

    #[test]
    fn istanbul_blocks_pay_cold_costs_on_every_access() {
        // GAS; PUSH1 0; SLOAD; POP; PUSH1 0; SLOAD; POP; GAS; SWAP1; SUB
        let sload_twice = [
            0x5a, 0x60, 0, 0x54, 0x50, 0x60, 0, 0x54, 0x50, 0x5a, 0x90, 0x03,
        ];
        // GAS; ADDRESS; BALANCE; POP; GAS; SWAP1; SUB
        let balance_self = [0x5a, 0x30, 0x31, 0x50, 0x5a, 0x90, 0x03];
        for inspect in [false, true] {
            assert_eq!(
                word(&probe(&sload_twice, ISTANBUL_BLOCK, inspect)),
                U256::from(4212)
            );
            assert_eq!(
                word(&probe(&sload_twice, BERLIN_BLOCK, inspect)),
                U256::from(2212)
            );
            assert_eq!(
                word(&probe(&balance_self, ISTANBUL_BLOCK, inspect)),
                U256::from(2606)
            );
            assert_eq!(
                word(&probe(&balance_self, BERLIN_BLOCK, inspect)),
                U256::from(106)
            );
        }
    }

    #[test]
    fn refunds_follow_l2geth() {
        // Clear three set slots: PUSH1 0; PUSH1 i; SSTORE (x3); STOP.
        let mut code = Vec::new();
        for slot in 0..3u8 {
            code.extend([0x60, 0, 0x60, slot, 0x55]);
        }
        code.push(0x00);
        for (number, gas_used) in [
            // EIP-2200 clears refund 15000 each, capped at half of 36018.
            (ISTANBUL_BLOCK, 18_009),
            // l2geth Berlin: EIP-3529 clears refund 4800 each, Berlin half cap.
            (BERLIN_BLOCK, 21_618),
        ] {
            for inspect in [false, true] {
                let mut db = with_code(&code);
                for slot in 0..3u64 {
                    db.insert_account_storage(TARGET, U256::from(slot), U256::ONE)
                        .unwrap();
                }
                let result = run(&mut db, number, Some(TARGET), vec![], inspect);
                assert!(result.is_success(), "{result:?}");
                assert_eq!(result.tx_gas_used(), gas_used, "block {number}");
            }
        }
    }

    #[test]
    fn no_london_code_rules() {
        // Deploy runtime code 0xEF (EIP-3541 is London).
        let init = vec![0x60, 0xef, 0x60, 0, 0x53, 0x60, 1, 0x60, 0, 0xf3];
        let result = run(
            &mut CacheDB::new(EmptyDB::default()),
            BERLIN_BLOCK,
            None,
            init,
            false,
        );
        assert!(result.is_success(), "{result:?}");
        assert_eq!(result.output().unwrap().as_ref(), &[0xef]);
    }

    #[test]
    fn top_level_deployments_follow_the_whitelist() {
        let init = vec![0x60, 1, 0x60, 0, 0x53, 0x60, 1, 0x60, 0, 0xf3];
        for inspect in [false, true] {
            let mut db = CacheDB::new(EmptyDB::default());
            whitelist_owned(&mut db);
            let rejected = run(&mut db, ISTANBUL_BLOCK, None, init.clone(), inspect);
            match &rejected {
                ExecutionResult::Revert { output, .. } => {
                    assert_eq!(output, &not_whitelisted_output(CALLER))
                }
                other => panic!("expected a revert, got {other:?}"),
            }
            // Only the intrinsic gas is spent: 53000 + 8 non-zero * 16 + 2 zero * 4.
            assert_eq!(rejected.tx_gas_used(), 53_136);

            whitelist(&mut db, CALLER);
            let allowed = run(&mut db, ISTANBUL_BLOCK, None, init.clone(), inspect);
            assert!(allowed.is_success(), "{allowed:?}");
        }
        // Berlin blocks no longer check it.
        let mut db = CacheDB::new(EmptyDB::default());
        whitelist_owned(&mut db);
        assert!(run(&mut db, BERLIN_BLOCK, None, init, false).is_success());
    }

    #[test]
    fn create_opcode_follows_the_whitelist() {
        // CREATE(0, 0, 0); MSTORE(0, address); MSTORE(32, RETURNDATASIZE); RETURN(0, 64)
        let code = [
            0x60, 0, 0x60, 0, 0x60, 0, 0xf0, 0x60, 0, 0x52, 0x3d, 0x60, 0x20, 0x52, 0x60, 0x40,
            0x60, 0, 0xf3,
        ];
        for inspect in [false, true] {
            let mut db = with_code(&code);
            whitelist_owned(&mut db);
            let rejected = run(&mut db, ISTANBUL_BLOCK, Some(TARGET), vec![], inspect);
            assert!(rejected.is_success(), "{rejected:?}");
            let out = rejected.output().unwrap();
            assert_eq!(U256::from_be_slice(&out[..32]), U256::ZERO);
            assert_eq!(
                U256::from_be_slice(&out[32..]),
                U256::from(not_whitelisted_output(TARGET).len())
            );

            let mut db = with_code(&code);
            whitelist_owned(&mut db);
            whitelist(&mut db, TARGET);
            let allowed = run(&mut db, ISTANBUL_BLOCK, Some(TARGET), vec![], inspect);
            let out = allowed.output().unwrap();
            assert_ne!(U256::from_be_slice(&out[..32]), U256::ZERO);
            assert_eq!(U256::from_be_slice(&out[32..]), U256::ZERO);
        }
    }
}
