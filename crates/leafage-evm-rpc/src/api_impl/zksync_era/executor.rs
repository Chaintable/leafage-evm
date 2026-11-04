use alloy::primitives::{Address, Bytes, B256, U256 as AlloyU256};
use primitive_types::{H160, U256};
use revm::context::result::{
    EVMError, ExecutionResult, HaltReason, InvalidTransaction, Output, ResultGas, SuccessReason,
};
use revm::DatabaseRef;
use std::collections::HashMap;
use zksync_vm2::{
    ExecutionEnd, Program, Settings, StorageInterface, StorageSlot, VirtualMachine, World,
};

pub struct EraStorageWorld<DB> {
    db: DB,
    program_cache: HashMap<U256, Program<(), Self>>,
}

impl<DB: DatabaseRef> EraStorageWorld<DB> {
    pub fn new(db: DB) -> Self {
        Self {
            db,
            program_cache: HashMap::new(),
        }
    }
}

impl<DB: DatabaseRef> StorageInterface for EraStorageWorld<DB> {
    fn read_storage(&mut self, contract: H160, key: U256) -> StorageSlot {
        StorageSlot {
            value: self.read_storage_value(contract, key),
            is_write_initial: false,
        }
    }

    fn read_storage_value(&mut self, contract: H160, key: U256) -> U256 {
        let address = Address::from_slice(contract.as_bytes());
        let mut key_bytes = [0u8; 32];
        key.to_big_endian(&mut key_bytes);
        let index = AlloyU256::from_be_bytes(key_bytes);

        match self.db.storage_ref(address, index) {
            Ok(val) => {
                let out_bytes = val.to_be_bytes::<32>();
                U256::from_big_endian(&out_bytes)
            }
            Err(_) => U256::zero(),
        }
    }

    fn cost_of_writing_storage(&mut self, _slot: StorageSlot, _new_value: U256) -> u32 {
        0
    }

    fn is_free_storage_slot(&self, _contract: &H160, _key: &U256) -> bool {
        false
    }
}

impl<DB: DatabaseRef> World<()> for EraStorageWorld<DB> {
    fn decommit(&mut self, hash: U256) -> Program<(), Self> {
        if let Some(prog) = self.program_cache.get(&hash) {
            return prog.clone();
        }
        let code = self.decommit_code(hash);
        let prog = Program::new(&code, false);
        self.program_cache.insert(hash, prog.clone());
        prog
    }

    fn decommit_code(&mut self, hash: U256) -> Vec<u8> {
        let mut hash_bytes = [0u8; 32];
        hash.to_big_endian(&mut hash_bytes);
        let code_hash = B256::from(hash_bytes);

        match self.db.code_by_hash_ref(code_hash) {
            Ok(code) => code.original_bytes().to_vec(),
            Err(_) => vec![],
        }
    }
}

pub fn execute_era_call<DB: DatabaseRef>(
    db: DB,
    caller: Address,
    to: Option<Address>,
    data: &[u8],
    gas_limit: u64,
) -> Result<ExecutionResult<HaltReason>, EVMError<DB::Error, InvalidTransaction>> {
    let Some(to_addr) = to else {
        return Ok(ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::default(),
            logs: vec![],
            output: Output::Call(Bytes::new()),
        });
    };

    let account_info = db.basic_ref(to_addr).map_err(EVMError::Database)?;
    let Some(account) = account_info else {
        return Ok(ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::default(),
            logs: vec![],
            output: Output::Call(Bytes::new()),
        });
    };

    let bytecode = match account.code {
        Some(ref code) => code.original_bytes().to_vec(),
        None if !account.code_hash.is_zero() => {
            match db.code_by_hash_ref(account.code_hash).map_err(EVMError::Database)? {
                code => code.original_bytes().to_vec(),
            }
        }
        None => vec![],
    };

    if bytecode.is_empty() {
        return Ok(ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::default(),
            logs: vec![],
            output: Output::Call(Bytes::new()),
        });
    }

    let caller_h160 = H160::from_slice(caller.as_slice());
    let to_h160 = H160::from_slice(to_addr.as_slice());
    let gas_u32 = gas_limit.min(u32::MAX as u64) as u32;

    let program = Program::new(&bytecode, false);
    let mut world = EraStorageWorld::new(db);
    let settings = Settings {
        default_aa_code_hash: [0; 32],
        evm_interpreter_code_hash: [0; 32],
        hook_address: 0,
    };

    let mut vm = VirtualMachine::new(to_h160, program, caller_h160, data, gas_u32, settings);
    let mut tracer = ();
    let end = vm.run(&mut world, &mut tracer);

    match end {
        ExecutionEnd::ProgramFinished(out) => Ok(ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::default().with_total_gas_spent(21_000),
            logs: vec![],
            output: Output::Call(Bytes::from(out)),
        }),
        ExecutionEnd::Reverted(out) => Ok(ExecutionResult::Revert {
            gas: ResultGas::default().with_total_gas_spent(21_000),
            logs: vec![],
            output: Bytes::from(out),
        }),
        ExecutionEnd::Panicked => Ok(ExecutionResult::Halt {
            reason: HaltReason::OpcodeNotFound,
            gas: ResultGas::default().with_total_gas_spent(gas_limit),
            logs: vec![],
        }),
        ExecutionEnd::SuspendedOnHook(_) | ExecutionEnd::StoppedByTracer => {
            Ok(ExecutionResult::Halt {
                reason: HaltReason::PrecompileError,
                gas: ResultGas::default().with_total_gas_spent(gas_limit),
                logs: vec![],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256, bytes};
    use revm::database::EmptyDB;
    use revm::database::CacheDB;
    use leafage_evm_types::{AccountInfo, Bytecode};

    #[test]
    fn test_execute_era_call_eoa() {
        let db = EmptyDB::default();
        let caller = address!("1111111111111111111111111111111111111111");
        let to = address!("2222222222222222222222222222222222222222");

        let res = execute_era_call(&db, caller, Some(to), &[], 100_000).unwrap();
        match res {
            ExecutionResult::Success { output, .. } => {
                assert_eq!(output.into_data(), bytes!(""));
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn test_execute_era_call_none_to() {
        let db = EmptyDB::default();
        let caller = address!("1111111111111111111111111111111111111111");

        let res = execute_era_call(&db, caller, None, &[], 100_000).unwrap();
        match res {
            ExecutionResult::Success { output, .. } => {
                assert_eq!(output.into_data(), bytes!(""));
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn test_execute_era_call_with_bytecode() {
        let mut db = CacheDB::new(EmptyDB::default());
        let to = address!("2222222222222222222222222222222222222222");
        let caller = address!("1111111111111111111111111111111111111111");

        // Use the call_far test bytecode from vm2 (32 bytes)
        let raw_bytecode = bytes!("7e1800000021c428171600000500926d702b2ad95a12017f926d000000f4c4cf");
        let code_hash = b256!("0100000100000000000000000000000000000000000000000000000000000001");

        let bytecode = Bytecode::new_raw(raw_bytecode);
        let account = AccountInfo {
            code_hash,
            code: Some(bytecode),
            ..Default::default()
        };
        db.insert_account_info(to, account);

        let res = execute_era_call(&db, caller, Some(to), &[], 100_000).unwrap();
        // Since call_far tries to call an unpopulated address, it panics/halts in vm2
        assert!(matches!(res, ExecutionResult::Halt { .. }));
    }
}
