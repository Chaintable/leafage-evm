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
pub struct EraPrecompiles {
    pub expected_caller: Option<H160>,
}

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

        if address_low == 0x01 {
            if let Some(caller) = self.expected_caller {
                let mut addr_bytes = [0u8; 32];
                addr_bytes[12..32].copy_from_slice(caller.as_bytes());
                return PrecompileOutput::from([U256::one(), U256::from_big_endian(&addr_bytes)]);
            }
        }

        LegacyPrecompiles.call_precompile(address_low, memory, aux_input)
    }
}

pub struct EraStorageWorld<DB> {
    db: DB,
    program_cache: HashMap<U256, Program<(), Self>>,
    precompiles: EraPrecompiles,
    expected_caller: Option<H160>,
}

impl<DB: DatabaseRef> EraStorageWorld<DB> {
    pub fn new(db: DB) -> Self {
        Self {
            db,
            program_cache: HashMap::new(),
            precompiles: EraPrecompiles::default(),
            expected_caller: None,
        }
    }

    pub fn set_expected_caller(&mut self, caller: H160) {
        self.expected_caller = Some(caller);
        self.precompiles.expected_caller = Some(caller);
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
        use super::bootloader::constants::L2_BASE_TOKEN_ADDRESS;
        let address = Address::from_slice(contract.as_bytes());
        let mut key_bytes = [0u8; 32];
        key.to_big_endian(&mut key_bytes);
        let index = AlloyU256::from_be_bytes(key_bytes);

        let val = match self.db.storage_ref(address, index) {
            Ok(val) => {
                let out_bytes = val.to_be_bytes::<32>();
                U256::from_big_endian(&out_bytes)
            }
            Err(_) => U256::zero(),
        };

        if contract == L2_BASE_TOKEN_ADDRESS {
            if let Some(caller) = self.expected_caller {
                if key == super::bootloader::constants::eth_balance_storage_key(caller) {
                    return val.saturating_add(U256::from(10u128.pow(25)));
                }
            }
        }

        val
    }

    fn cost_of_writing_storage(&mut self, slot: StorageSlot, new_value: U256) -> u32 {
        super::bootloader::pubdata::cost_of_writing_storage(
            slot.value,
            slot.is_write_initial,
            new_value,
        )
    }

    fn is_free_storage_slot(&self, contract: &H160, key: &U256) -> bool {
        use super::bootloader::constants::{
            bootloader_balance_key, L2_BASE_TOKEN_ADDRESS, SYSTEM_CONTEXT_ADDRESS,
        };
        if contract == &SYSTEM_CONTEXT_ADDRESS {
            return true;
        }
        if contract == &L2_BASE_TOKEN_ADDRESS {
            return key == &bootloader_balance_key();
        }
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
        use super::bootloader::artifacts::{
            default_aa_hash_u256, fee_estimate_hash_u256, DEFAULT_AA_BYTECODE,
            FEE_ESTIMATE_BYTECODE,
        };
        if hash == fee_estimate_hash_u256() {
            return FEE_ESTIMATE_BYTECODE.to_vec();
        }
        if hash == default_aa_hash_u256() {
            return DEFAULT_AA_BYTECODE.to_vec();
        }

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
        default_aa_code_hash: super::bootloader::artifacts::DEFAULT_AA_CODE_HASH,
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

pub fn estimate_era_gas<DB: DatabaseRef>(
    db: &DB,
    caller: Address,
    to: Option<Address>,
    data: &[u8],
    value: AlloyU256,
    block_number: u64,
    block_timestamp: u64,
    block_hash: Option<alloy::primitives::B256>,
    base_fee: u64,
    gas_per_pubdata_byte: u64,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<u64, String> {
    use super::bootloader::{
        constants::{
            BOOTLOADER_ADDRESS, DEFAULT_GAS_PER_PUBDATA, DEFAULT_L2_BASE_FEE, EIP_712_TX_TYPE,
            MAX_GAS_PER_PUBDATA_BYTE, MAX_L2_TX_GAS_LIMIT,
        },
        BootloaderEnv, GasEstimator, TransactionData, TxExecutionMode,
    };

    let caller_h160 = H160::from_slice(caller.as_slice());
    let to_h160 = to.map(|a| H160::from_slice(a.as_slice()));
    let val_bytes = value.to_be_bytes::<32>();
    let value_u256 = U256::from_big_endian(&val_bytes);

    let gas_per_pubdata_byte = if gas_per_pubdata_byte == 0 {
        DEFAULT_GAS_PER_PUBDATA
    } else {
        gas_per_pubdata_byte
    };

    let base_fee = if base_fee == 0 {
        DEFAULT_L2_BASE_FEE
    } else {
        base_fee
    };

    let fair_l2_gas_price = base_fee;
    let fair_pubdata_price = gas_per_pubdata_byte * fair_l2_gas_price;

    let (sim_block_number, sim_block_timestamp, prev_block_hash) = if let Some(hash) = block_hash {
        let h256 = primitive_types::H256::from_slice(hash.as_slice());
        if block_number == 0 {
            (1, 1, h256)
        } else {
            (block_number + 1, block_timestamp + 1, h256)
        }
    } else if block_number <= 1 {
        // Legacy L2 block 0 hash: keccak256(uint32(0))
        (
            1,
            1,
            primitive_types::H256([
                0xe8, 0xe7, 0x76, 0x26, 0x58, 0x6f, 0x73, 0xb9, 0x55, 0x36, 0x4c, 0x7b, 0x4b,
                0xbf, 0x0b, 0xb7, 0xf7, 0x68, 0x5e, 0xbd, 0x40, 0xe8, 0x52, 0xb1, 0x64, 0x63,
                0x3a, 0x4a, 0xcb, 0xd3, 0x24, 0x4c,
            ]),
        )
    } else {
        (
            block_number + 1,
            block_timestamp + 1,
            primitive_types::H256::zero(),
        )
    };

    let env = BootloaderEnv {
        block_number: sim_block_number,
        block_timestamp: sim_block_timestamp,
        prev_block_hash,
        fee_account: BOOTLOADER_ADDRESS,
        fair_pubdata_price,
        fair_l2_gas_price,
        base_fee,
        execution_mode: TxExecutionMode::EstimateFee,
    };

    let signature = {
        let mut sig = vec![0u8; 65];
        sig[64] = 27;
        sig
    };

    let nonce = match db.basic_ref(caller) {
        Ok(Some(account)) => U256::from(account.nonce),
        _ => U256::zero(),
    };

    let tx = TransactionData {
        tx_type: EIP_712_TX_TYPE,
        from: caller_h160,
        to: to_h160,
        gas_limit: U256::from(MAX_L2_TX_GAS_LIMIT),
        pubdata_price_limit: U256::from(MAX_GAS_PER_PUBDATA_BYTE),
        max_fee_per_gas: U256::from(base_fee),
        max_priority_fee_per_gas: U256::zero(),
        paymaster: H160::zero(),
        nonce,
        value: value_u256,
        reserved: [U256::zero(); 4],
        data: data.to_vec(),
        signature,
        factory_deps: Vec::new(),
        paymaster_input: Vec::new(),
        reserved_dynamic: Vec::new(),
    };

    let acceptable_overestimation = 1000;
    let estimator = GasEstimator::new(
        db,
        tx,
        env,
        gas_per_pubdata_byte,
        acceptable_overestimation,
        cancel_token,
    );

    estimator.estimate()
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

        // 0x8002 AccountCodeStorage
        let acs_code_hex = "00020000000000020000008003000039000000400030043f0000000100200190000000240000c13d00000060021002700000006502200197000000040020008c000000f70000413d000000000301043b000000e003300270000000670030009c0000002c0000a13d000000680030009c0000003f0000213d0000006b0030009c0000006f0000613d0000006c0030009c000000f70000c13d0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b000000710010009c000000f70000213d000000000101041a0000007401100197000000750010009c00000000010000390000000101006039000000800010043f0000007f01000041000001920001042e0000000001000416000000000001004b000000f70000c13d0000002001000039000001000010044300000120000004430000006601000041000001920001042e0000006d0030009c000000590000613d0000006e0030009c000000970000613d0000006f0030009c000000f70000c13d0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b000000710010009c000000f70000213d000000000101041a000000800010043f0000007f01000041000001920001042e000000690030009c000000830000613d0000006a0030009c000000f70000c13d0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d00000070020000410000000401100370000000000101043b00000000040004150000007103100197000001000030008c000000b50000813d000000000100041500000000011400490000000001000002000000400100043d0000000000210435000000650010009c000000650100804100000040011002100000007c011001c7000001920001042e0000000003000416000000000003004b000000f70000c13d000000440020008c000000f70000413d0000000402100370000000000202043b000000710020009c000000f70000213d0000002401100370000000000101043b0000000003000411000080060030008c000000b00000c13d0000007200100198000000800000613d0000008001000041000000000010043f0000000101000039000000040010043f000000770100004100000193000104300000000003000416000000000003004b000000f70000c13d000000440020008c000000f70000413d0000000402100370000000000202043b000000710020009c000000f70000213d0000002401100370000000000101043b0000000003000411000080060030008c000000b00000c13d0000007203100197000000730030009c000000fe0000c13d000000000012041b0000000001000019000001920001042e0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b000000710010009c000000f70000213d0000000002000411000080060020008c000000f90000c13d000000000201041a0000007203200197000000730030009c000000fe0000c13d0000007e02200197000000000021041b0000000001000019000001920001042e0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b0000007101100197000001000010008c0000014c0000413d000000000201041a000000000002004b0000016d0000613d0000007201200197000000730010009c00000000010000190000016e0000613d0000007403200197000000e0012002700000ffff0110018f000000750030009c000000050110c210000000800010043f0000007f01000041000001920001042e0000007d01000041000000000010043f000000040030043f00000077010000410000019300010430000000000103041a000000000001004b000001030000c13d000100000004001d000000400200043d000200000002001d0000007a01000041000000000012043500000004012000390000000000310435000000650020009c0000006501000041000000000102401900000040011002100000000002000414000000650020009c0000006502008041000000c002200210000000000112019f00000077011001c700008003020000390191018c0000040f000000020b00002900000060031002700000006503300197000000200030008c000000200400003900000000040340190000001f0640018f000000200740019000000000057b0019000000db0000613d000000000801034f00000000090b0019000000008a08043c0000000009a90436000000000059004b000000d70000c13d000000000006004b000000e80000613d000000000771034f0000000306600210000000000805043300000000086801cf000000000868022f000000000707043b0000010006600089000000000767022f00000000066701cf000000000686019f000000000065043500000001002001900000014f0000613d0000001f01400039000000600210018f0000000001b20019000000000021004b00000000020000390000000102004039000000790010009c000001710000213d0000000100200190000001710000c13d000000400010043f000000200030008c000001850000813d000000000100001900000193000104300000007d01000041000000000010043f000000040020043f000000770100004100000193000104300000008001000041000000000010043f000000040000043f000000770100004100000193000104300000007202100197000000730020009c00000070020000410000004f0000613d0000007402100197000000750020009c000001770000c13d000100000004001d000000400300043d000200000003001d0000007602000041000000000023043500000004023000390000000000120435000000650030009c0000006501000041000000000103401900000040011002100000000002000414000000650020009c0000006502008041000000c002200210000000000112019f00000077011001c700008015020000390191018c0000040f000000020b00002900000060031002700000006503300197000000200030008c000000200400003900000000040340190000001f0640018f000000200740019000000000057b00190000012d0000613d000000000801034f00000000090b0019000000008a08043c0000000009a90436000000000059004b000001290000c13d000000000006004b0000013a0000613d000000000771034f0000000306600210000000000805043300000000086801cf000000000868022f000000000707043b0000010006600089000000000767022f00000000066701cf000000000686019f00000000006504350000000100200190000001790000613d0000001f01400039000000600210018f0000000001b20019000000000021004b00000000020000390000000102004039000000790010009c000001710000213d0000000100200190000001710000c13d000000400010043f000000200030008c000000f70000413d00000000020b043300000001040000290000004f0000013d000000800000043f0000007f01000041000001920001042e0000001f0530018f0000007806300198000000400200043d00000000046200190000015a0000613d000000000701034f0000000008020019000000007907043c0000000008980436000000000048004b000001560000c13d000000000005004b000001670000613d000000000161034f0000000305500210000000000604043300000000065601cf000000000656022f000000000101043b0000010005500089000000000151022f00000000015101cf000000000161019f00000000001404350000006001300210000000650020009c00000065020080410000004002200210000000000112019f00000193000104300000000001000019000000800010043f0000007f01000041000001920001042e0000007b01000041000000000010043f0000004101000039000000040010043f0000007701000041000001930001043000000000020100190000004f0000013d0000001f0530018f0000007806300198000000400200043d00000000046200190000015a0000613d000000000701034f0000000008020019000000007907043c0000000008980436000000000048004b000001800000c13d0000015a0000013d00000000010b0433000000000001004b000000000200001900000001040000290000004f0000613d00000070020000410000004f0000013d0000018f002104230000000102000039000000000001042d0000000002000019000000000001042d0000019100000432000001920001042e000001930001043000000000000000000000000000000000000000000000000000000000ffffffff0000000200000000000000000000000000000040000001000000000000000000000000000000000000000000000000000000000000000000000000004f1e1bdf00000000000000000000000000000000000000000000000000000000c2e4ff9600000000000000000000000000000000000000000000000000000000c2e4ff9700000000000000000000000000000000000000000000000000000000e03fe177000000000000000000000000000000000000000000000000000000004f1e1be0000000000000000000000000000000000000000000000000000000008c040477000000000000000000000000000000000000000000000000000000000d4651aa000000000000000000000000000000000000000000000000000000001806aa18000000000000000000000000000000000000000000000000000000004de2e468c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470000000000000000000000000ffffffffffffffffffffffffffffffffffffffff00ff0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000ff0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000005f8f27b000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002400000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffe0000000000000000000000000000000000000000000000000ffffffffffffffff5aa9b6b5000000000000000000000000000000000000000000000000000000004e487b710000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000008e4a23d600000000000000000000000000000000000000000000000000000000ff00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00000000000000000000000000000000000000200000008000000000000000006a84bc39000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a26469706673582212205b5c00c4c23e306051f0c953453dafd64655826b43ffac26bb3ca0d6399c04f064736f6c6378247a6b736f6c633a312e352e31373b736f6c633a302e382e32383b6c6c766d3a312e302e320055";
        let acs_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(acs_code_hex).unwrap()));
        let acs_code_hash = b256!("0100008539fa56176e2bafd2dcf957f7cc55b1fd3b400aee6d67ca485205f711");
        db.insert_account_info(account_code_storage, AccountInfo {
            code_hash: acs_code_hash,
            code: Some(acs_bytecode.clone()),
            ..Default::default()
        });
        db.insert_account_storage(account_code_storage, AlloyU256::from(0x8002), AlloyU256::from_be_bytes(acs_code_hash.0)).unwrap();

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

        // 0x800b SystemContext code hash and bytecode
        let system_context_addr = address!("000000000000000000000000000000000000800b");
        let sys_context_hex = include_str!("fixtures/system_context_code.hex");
        let sys_context_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(sys_context_hex.trim()).unwrap()));
        let system_context_code_hash = b256!("010001b3f2c3a6bdd5ad00ae29a7cbbb32dca3c31fb608b5cd52f8f3056a3847");

        db.insert_account_info(system_context_addr, AccountInfo {
            code_hash: system_context_code_hash,
            code: Some(sys_context_bytecode),
            ..Default::default()
        });

        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800b),
            AlloyU256::from_be_bytes(system_context_code_hash.0),
        ).unwrap();

        // 0x800c BootloaderUtilities
        let bootloader_utilities_addr = address!("000000000000000000000000000000000000800c");
        let bootloader_utilities_hex = include_str!("fixtures/bootloader_utilities_code.hex");
        let bootloader_utilities_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(bootloader_utilities_hex.trim()).unwrap()));
        let bootloader_utilities_code_hash = b256!("0100091fbfe99162c28d2e7103c2a4b97c89c20eff761eb870c3e402f01f20de");
        db.insert_account_info(bootloader_utilities_addr, AccountInfo {
            code_hash: bootloader_utilities_code_hash,
            code: Some(bootloader_utilities_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800c),
            AlloyU256::from_be_bytes(bootloader_utilities_code_hash.0),
        ).unwrap();

        // 0x8003 NonceHolder
        let nonce_holder_addr = address!("0000000000000000000000000000000000008003");
        let nonce_holder_hex = include_str!("fixtures/nonce_holder_code.hex");
        let nonce_holder_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(nonce_holder_hex.trim()).unwrap()));
        let nonce_holder_code_hash = b256!("010000cbdf6b2fd65d1468945260131106db1026fb0ff306cac4199246d423be");
        db.insert_account_info(nonce_holder_addr, AccountInfo {
            code_hash: nonce_holder_code_hash,
            code: Some(nonce_holder_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8003),
            AlloyU256::from_be_bytes(nonce_holder_code_hash.0),
        ).unwrap();

        // 0x800a L2BaseToken
        let base_token_addr = address!("000000000000000000000000000000000000800a");
        let base_token_hex = include_str!("fixtures/base_token_code.hex");
        let base_token_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(base_token_hex.trim()).unwrap()));
        let base_token_code_hash = b256!("0100013f4d07387c7946c03086c4257a2c43e37d8d4482f7a0de00c6a3865644");
        db.insert_account_info(base_token_addr, AccountInfo {
            code_hash: base_token_code_hash,
            code: Some(base_token_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800a),
            AlloyU256::from_be_bytes(base_token_code_hash.0),
        ).unwrap();

        // 0x8006 ContractDeployer
        let deployer_addr = address!("0000000000000000000000000000000000008006");
        let deployer_hex = include_str!("fixtures/contract_deployer_code.hex");
        let deployer_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(deployer_hex.trim()).unwrap()));
        let deployer_code_hash = b256!("01000685f357db07648774aecf3fbe0c68ecd6af84c9afd0f33b03d66fccb919");
        db.insert_account_info(deployer_addr, AccountInfo {
            code_hash: deployer_code_hash,
            code: Some(deployer_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8006),
            AlloyU256::from_be_bytes(deployer_code_hash.0),
        ).unwrap();

        // 0x0001 EcRecover
        let ecrecover_addr = address!("0000000000000000000000000000000000000001");
        let ecrecover_hex = include_str!("fixtures/ecrecover_code.hex");
        let ecrecover_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(ecrecover_hex.trim()).unwrap()));
        let ecrecover_code_hash = b256!("01000015d738b26d046c5b3e15d7cd728a474b00c3dcd6df569f71cf07ef9bba");
        db.insert_account_info(ecrecover_addr, AccountInfo {
            code_hash: ecrecover_code_hash,
            code: Some(ecrecover_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x01),
            AlloyU256::from_be_bytes(ecrecover_code_hash.0),
        ).unwrap();

        // 0x8009 MsgValueSimulator
        let msg_val_sim_addr = address!("0000000000000000000000000000000000008009");
        let msg_val_sim_hex = include_str!("fixtures/msg_value_simulator_code.hex");
        let msg_val_sim_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(msg_val_sim_hex.trim()).unwrap()));
        let msg_val_sim_code_hash = b256!("01000059dd0f8a5da7e3989b679f91367ef9b35828b9bd4ab08755d60ef79e60");
        db.insert_account_info(msg_val_sim_addr, AccountInfo {
            code_hash: msg_val_sim_code_hash,
            code: Some(msg_val_sim_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8009),
            AlloyU256::from_be_bytes(msg_val_sim_code_hash.0),
        ).unwrap();

        // 0x8001 Bootloader Formal Address (EmptyContract)
        let bootloader_addr = address!("0000000000000000000000000000000000008001");
        let empty_contract_hex = include_str!("fixtures/empty_contract_code.hex");
        let empty_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(empty_contract_hex.trim()).unwrap()));
        let empty_code_hash = b256!("0100000946e24db427acd87d0e943193a7988205458bd15460cdb4a3f5e934e3");
        db.insert_account_info(bootloader_addr, AccountInfo {
            code_hash: empty_code_hash,
            code: Some(empty_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8001),
            AlloyU256::from_be_bytes(empty_code_hash.0),
        ).unwrap();

        // 0x800d EventWriter
        let event_writer_addr = address!("000000000000000000000000000000000000800d");
        let event_writer_hex = include_str!("fixtures/event_writer_code.hex");
        let event_writer_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(event_writer_hex.trim()).unwrap()));
        let event_writer_code_hash = b256!("0100001715cb85470e8cf806b3450d613950b645b7ffbf4a798d49bce75bb5fb");
        db.insert_account_info(event_writer_addr, AccountInfo {
            code_hash: event_writer_code_hash,
            code: Some(event_writer_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800d),
            AlloyU256::from_be_bytes(event_writer_code_hash.0),
        ).unwrap();

        // 0x8004 KnownCodesStorage
        let known_codes_addr = address!("0000000000000000000000000000000000008004");
        let known_codes_hex = include_str!("fixtures/known_codes_storage_code.hex");
        let known_codes_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(known_codes_hex.trim()).unwrap()));
        let known_codes_code_hash = b256!("010000c9735069835771c378602b198cf3e5169463321ce6793d24d7f5e75939");
        db.insert_account_info(known_codes_addr, AccountInfo {
            code_hash: known_codes_code_hash,
            code: Some(known_codes_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8004),
            AlloyU256::from_be_bytes(known_codes_code_hash.0),
        ).unwrap();

        // Give caller balance in L2BaseToken and AccountInfo
        let mut bal_key_bytes = [0u8; 64];
        bal_key_bytes[12..32].copy_from_slice(caller.as_slice());
        use tiny_keccak::{Hasher, Keccak};
        let mut hasher = Keccak::v256();
        let mut caller_bal_slot = [0u8; 32];
        hasher.update(&bal_key_bytes);
        hasher.finalize(&mut caller_bal_slot);
        let caller_balance_slot = AlloyU256::from_be_bytes(caller_bal_slot);
        db.insert_account_storage(base_token_addr, caller_balance_slot, AlloyU256::from(10u128.pow(25))).unwrap();

        db.insert_account_info(caller, AccountInfo {
            balance: AlloyU256::from(10u128.pow(25)),
            ..Default::default()
        });

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

    #[test]
    fn test_estimate_era_gas_balance_of() {
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

        // 0x8002 AccountCodeStorage
        let acs_code_hex = "00020000000000020000008003000039000000400030043f0000000100200190000000240000c13d00000060021002700000006502200197000000040020008c000000f70000413d000000000301043b000000e003300270000000670030009c0000002c0000a13d000000680030009c0000003f0000213d0000006b0030009c0000006f0000613d0000006c0030009c000000f70000c13d0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b000000710010009c000000f70000213d000000000101041a0000007401100197000000750010009c00000000010000390000000101006039000000800010043f0000007f01000041000001920001042e0000000001000416000000000001004b000000f70000c13d0000002001000039000001000010044300000120000004430000006601000041000001920001042e0000006d0030009c000000590000613d0000006e0030009c000000970000613d0000006f0030009c000000f70000c13d0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b000000710010009c000000f70000213d000000000101041a000000800010043f0000007f01000041000001920001042e000000690030009c000000830000613d0000006a0030009c000000f70000c13d0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d00000070020000410000000401100370000000000101043b00000000040004150000007103100197000001000030008c000000b50000813d000000000100041500000000011400490000000001000002000000400100043d0000000000210435000000650010009c000000650100804100000040011002100000007c011001c7000001920001042e0000000003000416000000000003004b000000f70000c13d000000440020008c000000f70000413d0000000402100370000000000202043b000000710020009c000000f70000213d0000002401100370000000000101043b0000000003000411000080060030008c000000b00000c13d0000007200100198000000800000613d0000008001000041000000000010043f0000000101000039000000040010043f000000770100004100000193000104300000000003000416000000000003004b000000f70000c13d000000440020008c000000f70000413d0000000402100370000000000202043b000000710020009c000000f70000213d0000002401100370000000000101043b0000000003000411000080060030008c000000b00000c13d0000007203100197000000730030009c000000fe0000c13d000000000012041b0000000001000019000001920001042e0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b000000710010009c000000f70000213d0000000002000411000080060020008c000000f90000c13d000000000201041a0000007203200197000000730030009c000000fe0000c13d0000007e02200197000000000021041b0000000001000019000001920001042e0000000003000416000000000003004b000000f70000c13d000000240020008c000000f70000413d0000000401100370000000000101043b0000007101100197000001000010008c0000014c0000413d000000000201041a000000000002004b0000016d0000613d0000007201200197000000730010009c00000000010000190000016e0000613d0000007403200197000000e0012002700000ffff0110018f000000750030009c000000050110c210000000800010043f0000007f01000041000001920001042e0000007d01000041000000000010043f000000040030043f00000077010000410000019300010430000000000103041a000000000001004b000001030000c13d000100000004001d000000400200043d000200000002001d0000007a01000041000000000012043500000004012000390000000000310435000000650020009c0000006501000041000000000102401900000040011002100000000002000414000000650020009c0000006502008041000000c002200210000000000112019f00000077011001c700008003020000390191018c0000040f000000020b00002900000060031002700000006503300197000000200030008c000000200400003900000000040340190000001f0640018f000000200740019000000000057b0019000000db0000613d000000000801034f00000000090b0019000000008a08043c0000000009a90436000000000059004b000000d70000c13d000000000006004b000000e80000613d000000000771034f0000000306600210000000000805043300000000086801cf000000000868022f000000000707043b0000010006600089000000000767022f00000000066701cf000000000686019f000000000065043500000001002001900000014f0000613d0000001f01400039000000600210018f0000000001b20019000000000021004b00000000020000390000000102004039000000790010009c000001710000213d0000000100200190000001710000c13d000000400010043f000000200030008c000001850000813d000000000100001900000193000104300000007d01000041000000000010043f000000040020043f000000770100004100000193000104300000008001000041000000000010043f000000040000043f000000770100004100000193000104300000007202100197000000730020009c00000070020000410000004f0000613d0000007402100197000000750020009c000001770000c13d000100000004001d000000400300043d000200000003001d0000007602000041000000000023043500000004023000390000000000120435000000650030009c0000006501000041000000000103401900000040011002100000000002000414000000650020009c0000006502008041000000c002200210000000000112019f00000077011001c700008015020000390191018c0000040f000000020b00002900000060031002700000006503300197000000200030008c000000200400003900000000040340190000001f0640018f000000200740019000000000057b00190000012d0000613d000000000801034f00000000090b0019000000008a08043c0000000009a90436000000000059004b000001290000c13d000000000006004b0000013a0000613d000000000771034f0000000306600210000000000805043300000000086801cf000000000868022f000000000707043b0000010006600089000000000767022f00000000066701cf000000000686019f00000000006504350000000100200190000001790000613d0000001f01400039000000600210018f0000000001b20019000000000021004b00000000020000390000000102004039000000790010009c000001710000213d0000000100200190000001710000c13d000000400010043f000000200030008c000000f70000413d00000000020b043300000001040000290000004f0000013d000000800000043f0000007f01000041000001920001042e0000001f0530018f0000007806300198000000400200043d00000000046200190000015a0000613d000000000701034f0000000008020019000000007907043c0000000008980436000000000048004b000001560000c13d000000000005004b000001670000613d000000000161034f0000000305500210000000000604043300000000065601cf000000000656022f000000000101043b0000010005500089000000000151022f00000000015101cf000000000161019f00000000001404350000006001300210000000650020009c00000065020080410000004002200210000000000112019f00000193000104300000000001000019000000800010043f0000007f01000041000001920001042e0000007b01000041000000000010043f0000004101000039000000040010043f0000007701000041000001930001043000000000020100190000004f0000013d0000001f0530018f0000007806300198000000400200043d00000000046200190000015a0000613d000000000701034f0000000008020019000000007907043c0000000008980436000000000048004b000001800000c13d0000015a0000013d00000000010b0433000000000001004b000000000200001900000001040000290000004f0000613d00000070020000410000004f0000013d0000018f002104230000000102000039000000000001042d0000000002000019000000000001042d0000019100000432000001920001042e000001930001043000000000000000000000000000000000000000000000000000000000ffffffff0000000200000000000000000000000000000040000001000000000000000000000000000000000000000000000000000000000000000000000000004f1e1bdf00000000000000000000000000000000000000000000000000000000c2e4ff9600000000000000000000000000000000000000000000000000000000c2e4ff9700000000000000000000000000000000000000000000000000000000e03fe177000000000000000000000000000000000000000000000000000000004f1e1be0000000000000000000000000000000000000000000000000000000008c040477000000000000000000000000000000000000000000000000000000000d4651aa000000000000000000000000000000000000000000000000000000001806aa18000000000000000000000000000000000000000000000000000000004de2e468c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470000000000000000000000000ffffffffffffffffffffffffffffffffffffffff00ff0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000ff0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000005f8f27b000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002400000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffe0000000000000000000000000000000000000000000000000ffffffffffffffff5aa9b6b5000000000000000000000000000000000000000000000000000000004e487b710000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000008e4a23d600000000000000000000000000000000000000000000000000000000ff00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00000000000000000000000000000000000000200000008000000000000000006a84bc39000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a26469706673582212205b5c00c4c23e306051f0c953453dafd64655826b43ffac26bb3ca0d6399c04f064736f6c6378247a6b736f6c633a312e352e31373b736f6c633a302e382e32383b6c6c766d3a312e302e320055";
        let acs_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(acs_code_hex).unwrap()));
        let acs_code_hash = b256!("0100008539fa56176e2bafd2dcf957f7cc55b1fd3b400aee6d67ca485205f711");
        db.insert_account_info(account_code_storage, AccountInfo {
            code_hash: acs_code_hash,
            code: Some(acs_bytecode.clone()),
            ..Default::default()
        });
        db.insert_account_storage(account_code_storage, AlloyU256::from(0x8002), AlloyU256::from_be_bytes(acs_code_hash.0)).unwrap();

        let keccak_code_hex = "0002000000000002000200000000000200010000000103550000006001100270000000280010019d00000080010000390000004004000039000000000014043500000000030004160000000102200190000000550000c13d000000000230004c0000005f0000c13d00000000020004120000002a022001970000000003000410000000000232004b0000005f0000c13d000000010500036700000000030000310000001f0630018f0000000004040433000000882730011a0000000507300270000000000870004c000000230000613d00000000080000190000000509800210000000000a940019000000000995034f000000000909043b00000000009a04350000000108800039000000000978004b0000001b0000413d000000000860004c000000320000613d0000000507700210000000000575034f00000000077400190000000306600210000000000807043300000000086801cf000000000868022f000000000505043b0000010006600089000000000565022f00000000056501cf000000000585019f0000000000570435000000800440008c000000620000c13d000000000323004900000088043000390000002c030000410000000001100031000000870220008c0000003e0000613d0000002d0200004100000000002104350000002e030000410000007f014000390000000000310435000000881240011a000100000002001d00000028212000c9000200000004001d009a00880000040f0000000102000029000000c00220021000000002030000290000001b033002100000002f033000410000003003300197000000000223019f00000031022001c7000000000301001900000000010200190000000002030019009a007f0000040f009a00790000040f000000200200003900000000010000190000000003000019009a006a0000040f000000000130004c0000005f0000c13d0000002002000039000001000100003900000000002104390000012002000039000000000002043900000040020000390000002903000041009a006a0000040f00000000010000190000000002000019009a00720000040f0000002b01000041000000000010043500000001010000390000000402000039000000000012043500000024020000390000000001000019009a00720000040f0000002804000041000000280510009c000000000104801900000040011002100000006002200210000000000121019f00000000013100190000009b0001042e0000002803000041000000280410009c000000000103801900000040011002100000006002200210000000000121019f0000009c00010430000000000110004c0000007c0000613d000000000001042d00000000010000190000000002000019009a00720000040f00000028022001970000000003000414000000000323004b000000850000413d0000000001210420000000000001042d00000000010000190000000002000019009a00720000040f000000320210009c0000008b0000813d000000000001042d00000040010000390000000001010433000000440210003900000033030000410000000000320435000000240210003900000008030000390000000000320435000000340200004100000000002104350000000402100039000000200300003900000000003204350000006402000039009a00720000040f0000009a000004320000009b0001042e0000009c0001043000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffff0000000200000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffffffffffffffffffffffffffffffffffff4e487b710000000000000000000000000000000000000000000000000000000081000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f8000000000000000000000000000000000000000000000000000000ffffffff00000000000000000000000000000000000000000000000100000000000000000000000400000000000000000000000000000000000000000000000000000001000000004f766572666c6f7700000000000000000000000000000000000000000000000008c379a000000000000000000000000000000000000000000000000000000000";
        let keccak_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(keccak_code_hex).unwrap()));
        let keccak_code_hash = b256!("01000035eef477bbc1a894f3191ae73812f5e391e42e8372fd8ea60fa2c38a8a");

        db.insert_account_info(keccak_addr, AccountInfo {
            code_hash: keccak_code_hash,
            code: Some(keccak_bytecode.clone()),
            ..Default::default()
        });

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

        // 0x800b SystemContext code hash and bytecode
        let system_context_addr = address!("000000000000000000000000000000000000800b");
        let sys_context_hex = include_str!("fixtures/system_context_code.hex");
        let sys_context_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(sys_context_hex.trim()).unwrap()));
        let system_context_code_hash = b256!("010001b3f2c3a6bdd5ad00ae29a7cbbb32dca3c31fb608b5cd52f8f3056a3847");

        db.insert_account_info(system_context_addr, AccountInfo {
            code_hash: system_context_code_hash,
            code: Some(sys_context_bytecode),
            ..Default::default()
        });

        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800b),
            AlloyU256::from_be_bytes(system_context_code_hash.0),
        ).unwrap();

        // 0x800c BootloaderUtilities
        let bootloader_utilities_addr = address!("000000000000000000000000000000000000800c");
        let bootloader_utilities_hex = include_str!("fixtures/bootloader_utilities_code.hex");
        let bootloader_utilities_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(bootloader_utilities_hex.trim()).unwrap()));
        let bootloader_utilities_code_hash = b256!("0100091fbfe99162c28d2e7103c2a4b97c89c20eff761eb870c3e402f01f20de");
        db.insert_account_info(bootloader_utilities_addr, AccountInfo {
            code_hash: bootloader_utilities_code_hash,
            code: Some(bootloader_utilities_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800c),
            AlloyU256::from_be_bytes(bootloader_utilities_code_hash.0),
        ).unwrap();

        // 0x8003 NonceHolder
        let nonce_holder_addr = address!("0000000000000000000000000000000000008003");
        let nonce_holder_hex = include_str!("fixtures/nonce_holder_code.hex");
        let nonce_holder_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(nonce_holder_hex.trim()).unwrap()));
        let nonce_holder_code_hash = b256!("010000cbdf6b2fd65d1468945260131106db1026fb0ff306cac4199246d423be");
        db.insert_account_info(nonce_holder_addr, AccountInfo {
            code_hash: nonce_holder_code_hash,
            code: Some(nonce_holder_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8003),
            AlloyU256::from_be_bytes(nonce_holder_code_hash.0),
        ).unwrap();

        // 0x800a L2BaseToken
        let base_token_addr = address!("000000000000000000000000000000000000800a");
        let base_token_hex = include_str!("fixtures/base_token_code.hex");
        let base_token_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(base_token_hex.trim()).unwrap()));
        let base_token_code_hash = b256!("0100013f4d07387c7946c03086c4257a2c43e37d8d4482f7a0de00c6a3865644");
        db.insert_account_info(base_token_addr, AccountInfo {
            code_hash: base_token_code_hash,
            code: Some(base_token_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800a),
            AlloyU256::from_be_bytes(base_token_code_hash.0),
        ).unwrap();

        // 0x8006 ContractDeployer
        let deployer_addr = address!("0000000000000000000000000000000000008006");
        let deployer_hex = include_str!("fixtures/contract_deployer_code.hex");
        let deployer_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(deployer_hex.trim()).unwrap()));
        let deployer_code_hash = b256!("01000685f357db07648774aecf3fbe0c68ecd6af84c9afd0f33b03d66fccb919");
        db.insert_account_info(deployer_addr, AccountInfo {
            code_hash: deployer_code_hash,
            code: Some(deployer_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8006),
            AlloyU256::from_be_bytes(deployer_code_hash.0),
        ).unwrap();

        // 0x0001 EcRecover
        let ecrecover_addr = address!("0000000000000000000000000000000000000001");
        let ecrecover_hex = include_str!("fixtures/ecrecover_code.hex");
        let ecrecover_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(ecrecover_hex.trim()).unwrap()));
        let ecrecover_code_hash = b256!("01000015d738b26d046c5b3e15d7cd728a474b00c3dcd6df569f71cf07ef9bba");
        db.insert_account_info(ecrecover_addr, AccountInfo {
            code_hash: ecrecover_code_hash,
            code: Some(ecrecover_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x01),
            AlloyU256::from_be_bytes(ecrecover_code_hash.0),
        ).unwrap();

        // 0x8009 MsgValueSimulator
        let msg_val_sim_addr = address!("0000000000000000000000000000000000008009");
        let msg_val_sim_hex = include_str!("fixtures/msg_value_simulator_code.hex");
        let msg_val_sim_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(msg_val_sim_hex.trim()).unwrap()));
        let msg_val_sim_code_hash = b256!("01000059dd0f8a5da7e3989b679f91367ef9b35828b9bd4ab08755d60ef79e60");
        db.insert_account_info(msg_val_sim_addr, AccountInfo {
            code_hash: msg_val_sim_code_hash,
            code: Some(msg_val_sim_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8009),
            AlloyU256::from_be_bytes(msg_val_sim_code_hash.0),
        ).unwrap();

        // 0x8001 Bootloader Formal Address (EmptyContract)
        let bootloader_addr = address!("0000000000000000000000000000000000008001");
        let empty_contract_hex = include_str!("fixtures/empty_contract_code.hex");
        let empty_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(empty_contract_hex.trim()).unwrap()));
        let empty_code_hash = b256!("0100000946e24db427acd87d0e943193a7988205458bd15460cdb4a3f5e934e3");
        db.insert_account_info(bootloader_addr, AccountInfo {
            code_hash: empty_code_hash,
            code: Some(empty_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8001),
            AlloyU256::from_be_bytes(empty_code_hash.0),
        ).unwrap();

        // 0x800d EventWriter
        let event_writer_addr = address!("000000000000000000000000000000000000800d");
        let event_writer_hex = include_str!("fixtures/event_writer_code.hex");
        let event_writer_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(event_writer_hex.trim()).unwrap()));
        let event_writer_code_hash = b256!("0100001715cb85470e8cf806b3450d613950b645b7ffbf4a798d49bce75bb5fb");
        db.insert_account_info(event_writer_addr, AccountInfo {
            code_hash: event_writer_code_hash,
            code: Some(event_writer_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x800d),
            AlloyU256::from_be_bytes(event_writer_code_hash.0),
        ).unwrap();

        // 0x8004 KnownCodesStorage
        let known_codes_addr = address!("0000000000000000000000000000000000008004");
        let known_codes_hex = include_str!("fixtures/known_codes_storage_code.hex");
        let known_codes_bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(hex::decode(known_codes_hex.trim()).unwrap()));
        let known_codes_code_hash = b256!("010000c9735069835771c378602b198cf3e5169463321ce6793d24d7f5e75939");
        db.insert_account_info(known_codes_addr, AccountInfo {
            code_hash: known_codes_code_hash,
            code: Some(known_codes_bytecode),
            ..Default::default()
        });
        db.insert_account_storage(
            account_code_storage,
            AlloyU256::from(0x8004),
            AlloyU256::from_be_bytes(known_codes_code_hash.0),
        ).unwrap();

        // Give caller balance in L2BaseToken and AccountInfo
        let mut bal_key_bytes = [0u8; 64];
        bal_key_bytes[12..32].copy_from_slice(caller.as_slice());
        use tiny_keccak::{Hasher, Keccak};
        let mut hasher = Keccak::v256();
        let mut caller_bal_slot = [0u8; 32];
        hasher.update(&bal_key_bytes);
        hasher.finalize(&mut caller_bal_slot);
        let caller_balance_slot = AlloyU256::from_be_bytes(caller_bal_slot);
        db.insert_account_storage(base_token_addr, caller_balance_slot, AlloyU256::from(10u128.pow(25))).unwrap();

        db.insert_account_info(caller, AccountInfo {
            balance: AlloyU256::from(10u128.pow(25)),
            ..Default::default()
        });

        let balance_slot: AlloyU256 = "0x93d4cbcfa5a98fb5bd536ff17e260613d273c79c8ccda09d93dac45cff472bb0".parse().unwrap();
        db.insert_account_storage(to, balance_slot, AlloyU256::from(0x26ac)).unwrap();

        let calldata = hex::decode("70a08231000000000000000000000000e5b06bfd663c94005b8b159cd320fd7976549f9b").unwrap();
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let gas_estimate = estimate_era_gas(
            &db,
            caller,
            Some(to),
            &calldata,
            AlloyU256::ZERO,
            1,
            1,
            None,
            250_000_000,
            800,
            cancel_token,
        ).unwrap();

        assert!(gas_estimate > 0);
        assert!(gas_estimate < crate::api_impl::zksync_era::bootloader::constants::MAX_L2_TX_GAS_LIMIT);
    }
}
