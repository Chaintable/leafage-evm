use alloy::{
    primitives::{address, Address, Bytes, B256, U256},
    sol,
    sol_types::SolCall,
};
use alloy_evm::EvmEnv;
use leafage_evm_chains::arc::{ArcChainConfig, ArcEvmFactory};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::{
    bytecode::Bytecode,
    context::{JournalTr, TxEnv},
    database::InMemoryDB,
    database_interface::DBErrorMarker,
    inspector::NoOpInspector,
    primitives::TxKind,
    state::AccountInfo,
    Database, ExecuteEvm, InspectEvm,
};

const ACCOUNTING: Address = address!("1800000000000000000000000000000000000002");
const WRAPPER: Address = address!("000000000000000000000000000000000000ca11");
const CALLER: Address = address!("000000000000000000000000000000000000ca12");
sol! { function getGasValues(uint64 blockNumber) external; }

#[derive(Debug)]
struct ReadFailure;
impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("injected accounting storage read failure")
    }
}
impl std::error::Error for ReadFailure {}
impl DBErrorMarker for ReadFailure {}
#[derive(Debug)]
struct FaultDb {
    inner: InMemoryDB,
    fail: bool,
    failures: usize,
}
impl Database for FaultDb {
    type Error = ReadFailure;
    fn basic(&mut self, a: Address) -> Result<Option<AccountInfo>, ReadFailure> {
        Ok(self.inner.basic(a).unwrap())
    }
    fn code_by_hash(&mut self, h: B256) -> Result<Bytecode, ReadFailure> {
        Ok(self.inner.code_by_hash(h).unwrap())
    }
    fn block_hash(&mut self, n: u64) -> Result<B256, ReadFailure> {
        Ok(self.inner.block_hash(n).unwrap())
    }
    fn storage(&mut self, a: Address, k: U256) -> Result<U256, ReadFailure> {
        if self.fail && a == ACCOUNTING {
            self.failures += 1;
            return Err(ReadFailure);
        }
        Ok(self.inner.storage(a, k).unwrap())
    }
}

#[test]
fn precompile_db_failure_must_abort_outer_transaction() {
    // Forward calldata, then return the CALL success bit. No mock precompile is installed.
    // Write storage and emit a log before the call, so a fatal child error must
    // also discard changes already made by the outer contract.
    let mut code = vec![
        0x60, 0x2a, 0x5f, 0x55, 0x5f, 0x5f, 0xa0, 0x36, 0x5f, 0x5f, 0x37, 0x5f, 0x5f, 0x36, 0x5f,
        0x5f, 0x73,
    ];
    code.extend_from_slice(ACCOUNTING.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
    for (fail, inspect) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            CALLER,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        db.insert_account_info(
            WRAPPER,
            AccountInfo {
                nonce: 1,
                code: Some(Bytecode::new_raw(Bytes::from(code.clone()))),
                ..Default::default()
            },
        );
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = 5042;
        let block = BlockEnv {
            number: U256::ONE,
            timestamp: U256::from(1789052400u64),
            gas_limit: 30_000_000,
            prevrandao: Some(B256::ZERO),
            ..Default::default()
        };
        let mut evm = ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(
                EvmEnv::new(cfg, block),
                FaultDb {
                    inner: db,
                    fail,
                    failures: 0,
                },
                NoOpInspector {},
            )
            .unwrap();
        let tx = TxEnv {
            caller: CALLER,
            kind: TxKind::Call(WRAPPER),
            data: getGasValuesCall { blockNumber: 1 }.abi_encode().into(),
            gas_limit: 200_000,
            chain_id: Some(5042),
            ..Default::default()
        };
        let result = if inspect {
            evm.inspect(tx, NoOpInspector {})
        } else {
            evm.transact(tx)
        };
        if fail {
            assert_eq!(evm.ctx().journaled_state.db().failures, 1);
            assert!(
                matches!(result, Err(revm::context::result::EVMError::Custom(ref message)) if message.contains("ReadFailure"))
            );
            assert!(evm.ctx().journaled_state.logs.is_empty());
            // inspect() discards the failed transaction but retains read caches;
            // transact() additionally finalizes. Neither may retain mutations.
            for account in evm.finalize().values() {
                assert!(!account.is_touched());
                assert!(account.storage.values().all(|slot| !slot.is_changed()));
            }
            assert!(evm.ctx().journaled_state.state.is_empty());
        } else {
            let result = result.unwrap();
            assert!(result.result.is_success());
            assert_eq!(result.result.logs().len(), 1);
            assert_eq!(
                result.state[&WRAPPER].storage[&U256::ZERO].present_value(),
                U256::from(42)
            );
            assert_eq!(
                U256::from_be_slice(result.result.output().unwrap()),
                U256::ONE
            );
        }
    }
}
