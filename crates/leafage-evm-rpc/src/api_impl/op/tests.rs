//! Exercise the production OP transaction builder and both execution paths.
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

fn cfg(spec: OpSpecId, code: Option<usize>, init: Option<usize>) -> CfgEnv<OpSpecId> {
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg.disable_base_fee = true;
    cfg.disable_block_gas_limit = true;
    // Metis code-deposit boundary needs ~492M gas. Test-only, not an RPC default.
    cfg.tx_gas_limit_cap = Some(1_000_000_000);
    cfg.limit_contract_code_size = code;
    cfg.limit_contract_initcode_size = init;
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
                tempo: None,
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
    for (limit, init) in [(24576, 49152), (262144, 524288), (2457600, usize::MAX)] {
        for inspect in [false, true] {
            for size in [limit, limit + 1] {
                let config = cfg(OpSpecId::JOVIAN, Some(limit), Some(init));
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
    for limit in [49152, 524288] {
        for inspect in [false, true] {
            let config = cfg(OpSpecId::JOVIAN, None, Some(limit));
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
        let metis = cfg(OpSpecId::OSAKA, Some(2457600), Some(usize::MAX));
        for contract in [None, Some(factory(false)), Some(factory(true))] {
            assert!(execute(metis.clone(), vec![0; 4915201], contract, inspect)
                .unwrap()
                .is_success());
        }
        for spec in [OpSpecId::BEDROCK, OpSpecId::REGOLITH] {
            let early = cfg(spec, None, Some(0));
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
        let rise = execute(
            cfg(OpSpecId::JOVIAN, Some(262144), Some(524288)),
            vec![],
            Some(code.clone()),
            inspect,
        )
        .unwrap();
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
        let osaka = execute(
            cfg(OpSpecId::OSAKA, None, None),
            vec![],
            Some(code),
            inspect,
        )
        .unwrap();
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
        for spec in [OpSpecId::ECOTONE, OpSpecId::FJORD] {
            let api: ApiImpl<(), OpSpecId, NoneEvmCustomConfig> = ApiImpl::new(
                (),
                cfg(spec, None, None),
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
                tempo: None,
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
