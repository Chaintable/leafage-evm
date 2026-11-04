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
use zksync_vm2::precompiles::{LegacyPrecompiles, PrecompileMemoryReader, PrecompileOutput, Precompiles};

#[derive(Debug, Default)]
pub struct EraPrecompiles;

fn run_keccak_rounds(reader: PrecompileMemoryReader<'_>, round_count: usize) -> [u8; 32] {
    let mut state = [0u64; 25];
    let mut reader_in_words = reader.assume_offset_in_words();

    for _ in 0..round_count {
        let mut block = [0u8; 136];
        for b in &mut block {
            *b = reader_in_words.next().unwrap_or(0);
        }
        for (lane_idx, chunk) in block.chunks_exact(8).enumerate() {
            let lane = u64::from_le_bytes(chunk.try_into().unwrap());
            state[lane_idx] ^= lane;
        }
        tiny_keccak::keccakf(&mut state);
    }

    let mut out = [0u8; 32];
    for (i, lane) in state[0..4].iter().enumerate() {
        out[i * 8..(i + 1) * 8].copy_from_slice(&lane.to_le_bytes());
    }
    out
}

impl Precompiles for EraPrecompiles {
    fn call_precompile(
        &self,
        address_low: u16,
        memory: PrecompileMemoryReader<'_>,
        aux_input: u64,
    ) -> PrecompileOutput {
        if address_low == 0x8010 {
            let round_count = (aux_input as usize).max(1);
            let hash = run_keccak_rounds(memory, round_count);
            let u256_val = U256::from_big_endian(&hash);
            return PrecompileOutput::from(u256_val);
        }

        LegacyPrecompiles.call_precompile(address_low, memory, aux_input)
    }
}

pub struct EraStorageWorld<DB> {
    db: DB,
    program_cache: HashMap<U256, Program<(), Self>>,
    precompiles: EraPrecompiles,
}

impl<DB: DatabaseRef> EraStorageWorld<DB> {
    pub fn new(db: DB) -> Self {
        Self {
            db,
            program_cache: HashMap::new(),
            precompiles: EraPrecompiles::default(),
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

    fn precompiles(&self) -> &impl Precompiles {
        &self.precompiles
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

    #[test]
    fn test_execute_balance_of() {
        use alloy::primitives::{address, b256, hex, U256 as AlloyU256};
        let mut db = CacheDB::new(EmptyDB::default());
        let to = address!("ed0994232328b470d44a88485b430b8ba965d434");
        let caller = Address::repeat_byte(0x11);
        let account_code_storage = address!("0000000000000000000000000000000000008002");
        let keccak_addr = address!("0000000000000000000000000000000000008010");

        let token_code_hex = include_str!("fixtures/token_code.hex");
        let token_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(token_code_hex.trim()).unwrap()));
        let token_code_hash = b256!("01000495e4cde8595b0bac9434ca209bc0c81288f99c68b83e0037c2ee8f6328");

        db.insert_account_info(to, AccountInfo {
            code_hash: token_code_hash,
            code: Some(token_bytecode.clone()),
            ..Default::default()
        });

        // 0x8010 keccak bytecode
        let keccak_code_hex = "0002000000000002000200000000000200010000000103550000006001100270000000280010019d00000080010000390000004004000039000000000014043500000000030004160000000102200190000000550000c13d000000000230004c0000005f0000c13d00000000020004120000002a022001970000000003000410000000000232004b0000005f0000c13d000000010500036700000000030000310000001f0630018f0000000004040433000000882730011a0000000507300270000000000870004c000000230000613d00000000080000190000000509800210000000000a940019000000000995034f000000000909043b00000000009a04350000000108800039000000000978004b0000001b0000413d000000000860004c000000320000613d0000000507700210000000000575034f00000000077400190000000306600210000000000807043300000000086801cf000000000868022f000000000505043b0000010006600089000000000565022f00000000056501cf000000000585019f0000000000570435000000800440008c000000620000c13d000000000323004900000088043000390000002c030000410000000001100031000000870220008c0000003e0000613d0000002d0200004100000000002104350000002e030000410000007f014000390000000000310435000000881240011a000100000002001d00000028212000c9000200000004001d009a00880000040f0000000102000029000000c00220021000000002030000290000001b033002100000002f033000410000003003300197000000000223019f00000031022001c7000000000301001900000000010200190000000002030019009a007f0000040f009a00790000040f000000200200003900000000010000190000000003000019009a006a0000040f000000000130004c0000005f0000c13d0000002002000039000001000100003900000000002104390000012002000039000000000002043900000040020000390000002903000041009a006a0000040f00000000010000190000000002000019009a00720000040f0000002b01000041000000000010043500000001010000390000000402000039000000000012043500000024020000390000000001000019009a00720000040f0000002804000041000000280510009c000000000104801900000040011002100000006002200210000000000121019f00000000013100190000009b0001042e0000002803000041000000280410009c000000000103801900000040011002100000006002200210000000000121019f0000009c00010430000000000110004c0000007c0000613d000000000001042d00000000010000190000000002000019009a00720000040f00000028022001970000000003000414000000000323004b000000850000413d0000000001210420000000000001042d00000000010000190000000002000019009a00720000040f000000320210009c0000008b0000813d000000000001042d00000040010000390000000001010433000000440210003900000033030000410000000000320435000000240210003900000008030000390000000000320435000000340200004100000000002104350000000402100039000000200300003900000000003204350000006402000039009a00720000040f0000009a000004320000009b0001042e0000009c0001043000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffff0000000200000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffffffffffffffffffffffffffffffffffff4e487b710000000000000000000000000000000000000000000000000000000081000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f8000000000000000000000000000000000000000000000000000000ffffffff00000000000000000000000000000000000000000000000100000000000000000000000400000000000000000000000000000000000000000000000000000001000000004f766572666c6f7700000000000000000000000000000000000000000000000008c379a000000000000000000000000000000000000000000000000000000000";
        let keccak_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(keccak_code_hex).unwrap()));
        let keccak_code_hash = b256!("01000035eef477bbc1a894f3191ae73812f5e391e42e8372fd8ea60fa2c38a8a");

        db.insert_account_info(keccak_addr, AccountInfo {
            code_hash: keccak_code_hash,
            code: Some(keccak_bytecode.clone()),
            ..Default::default()
        });

        // In 0x8002 AccountCodeStorage:
        // storage[to] = token_code_hash
        // storage[keccak_addr] = keccak_code_hash
        let to_u256 = AlloyU256::from_be_bytes({
            let mut b = [0u8; 32];
            b[12..32].copy_from_slice(to.as_slice());
            b
        });
        let keccak_u256 = AlloyU256::from_be_bytes({
            let mut b = [0u8; 32];
            b[12..32].copy_from_slice(keccak_addr.as_slice());
            b
        });
        db.insert_account_storage(account_code_storage, to_u256, AlloyU256::from_be_bytes(token_code_hash.0)).unwrap();
        db.insert_account_storage(account_code_storage, keccak_u256, AlloyU256::from_be_bytes(keccak_code_hash.0)).unwrap();

        // Also token storage slot:
        let balance_slot: AlloyU256 = "0x93d4cbcfa5a98fb5bd536ff17e260613d273c79c8ccda09d93dac45cff472bb0".parse().unwrap();
        db.insert_account_storage(to, balance_slot, AlloyU256::from(0x26ac)).unwrap();

        let calldata = hex::decode("70a08231000000000000000000000000e5b06bfd663c94005b8b159cd320fd7976549f9b").unwrap();
        let res = execute_era_call(&db, caller, Some(to), &calldata, 100_000).unwrap();
        let mut expected = [0u8; 32];
        expected[30..32].copy_from_slice(&0x26acu16.to_be_bytes());
        match res {
            ExecutionResult::Success { output: Output::Call(out), .. } => {
                assert_eq!(out.as_ref(), &expected);
            }
            other => panic!("expected successful call, got: {:?}", other),
        }
    }
}
