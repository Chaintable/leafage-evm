//! The RSK gas schedule end to end. Expected values are derived by hand from
//! rskj `GasCost` / `VM`; each test says where Ethereum would differ.

use crate::rsk::{RskEvm, RskHardfork};
use alloy_evm::EvmEnv;
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::context::result::{ExecutionResult, HaltReason, OutOfGasError, ResultAndState};
use revm::context::TxEnv;
use revm::database::{in_memory_db::CacheDB, EmptyDB};
use revm::inspector::NoOpInspector;
use revm::primitives::{address, Address, Bytes, TxKind, B256, U256};
use revm::state::{AccountInfo, Bytecode};
use revm::ExecuteEvm;

const CALLER: Address = address!("00000000000000000000000000000000000000aa");
const CONTRACT: Address = address!("00000000000000000000000000000000000000c0");
const OTHER: Address = address!("00000000000000000000000000000000000000c1");
const NOBODY: Address = address!("00000000000000000000000000000000000000dd");
const TX_GAS: u64 = 21_000;
const GAS_LIMIT: u64 = 1_000_000;

struct Env {
    db: CacheDB<EmptyDB>,
    block: BlockEnv,
}

impl Env {
    fn new(code: &[u8]) -> Self {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(CALLER, AccountInfo::default());
        db.insert_account_info(
            CONTRACT,
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::copy_from_slice(code)))
                .with_balance(U256::from(1_000)),
        );
        Self {
            db,
            block: BlockEnv::default(),
        }
    }

    fn with_code(mut self, address: Address, code: &[u8]) -> Self {
        self.db.insert_account_info(
            address,
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::copy_from_slice(code))),
        );
        self
    }

    /// In the state, but empty: `Repository.isExist` is true.
    fn with_empty_account(mut self, address: Address) -> Self {
        self.db.insert_account_info(address, AccountInfo::default());
        self
    }

    fn with_storage(mut self, slot: u64, value: u64) -> Self {
        self.db
            .insert_account_storage(CONTRACT, U256::from(slot), U256::from(value))
            .unwrap();
        self
    }

    fn call(self, to: Address, data: Vec<u8>) -> ResultAndState {
        self.call_with(to, data, GAS_LIMIT)
    }

    fn call_with(self, to: Address, data: Vec<u8>, gas_limit: u64) -> ResultAndState {
        // same cfg the standalone binary builds for `--evm-type=rsk`
        let mut cfg = CfgEnv::new_with_spec(RskHardfork::from(MainnetSpecId::CANCUN));
        cfg.disable_balance_check = true;
        cfg.disable_base_fee = true;
        cfg.disable_nonce_check = true;
        let tx = TxEnv {
            caller: CALLER,
            gas_limit,
            kind: TxKind::Call(to),
            data: data.into(),
            chain_id: Some(cfg.chain_id),
            ..Default::default()
        };
        let mut evm = RskEvm::new(EvmEnv::new(cfg, self.block), self.db, NoOpInspector {});
        evm.transact(tx).expect("executes")
    }

    fn run(self) -> ExecutionResult {
        self.call(CONTRACT, vec![]).result
    }
}

fn gas_used(result: &ExecutionResult) -> u64 {
    assert!(result.is_success(), "unexpected result: {result:?}");
    result.gas_used()
}

/// `CALL(GAS, target, value, 0, 0, 0, 0); POP; STOP` — 5 PUSH1 + PUSH20 + GAS
/// + POP = 22 gas around the call itself.
fn call_code(target: Address, value: u8) -> Vec<u8> {
    let mut code = vec![
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, value, 0x73,
    ];
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x00]);
    code
}
const CALL_OVERHEAD: u64 = 22;

/// No EIP-2929: a first access costs the same as any other.
/// Ethereum (Cancun): SLOAD 2100, BALANCE / EXTCODESIZE / EXTCODEHASH 2600.
#[test]
fn state_reads_have_flat_eip150_prices() {
    // PUSH1 0; <op>; POP; STOP
    for (opcode, cost) in [(0x54, 200), (0x31, 400), (0x3b, 700), (0x3f, 400)] {
        let result = Env::new(&[0x60, 0x00, opcode, 0x50, 0x00]).run();
        assert_eq!(
            gas_used(&result),
            TX_GAS + 3 + cost + 2,
            "opcode {opcode:#x}"
        );
    }
    // twice in a row: PUSH1 0; SLOAD; POP; PUSH1 0; SLOAD; POP; STOP
    let result = Env::new(&[0x60, 0x00, 0x54, 0x50, 0x60, 0x00, 0x54, 0x50, 0x00]).run();
    assert_eq!(gas_used(&result), TX_GAS + 2 * (3 + 200 + 2));
}

/// `PUSH1 value; PUSH1 0; SSTORE; STOP`
fn sstore_code(value: u8) -> [u8; 6] {
    [0x60, value, 0x60, 0x00, 0x55, 0x00]
}

/// `VM.doSSTORE`. Ethereum: 22 100 / 5 000 / 5 000 with a 4 800 refund.
#[test]
fn sstore_is_the_three_case_rule() {
    let set = Env::new(&sstore_code(1)).run();
    assert_eq!(gas_used(&set), TX_GAS + 6 + 20_000);

    let reset = Env::new(&sstore_code(2)).with_storage(0, 1).run();
    assert_eq!(gas_used(&reset), TX_GAS + 6 + 5_000);

    let same = Env::new(&sstore_code(1)).with_storage(0, 1).run();
    assert_eq!(gas_used(&same), TX_GAS + 6 + 5_000);

    // 15 000 refund, capped at half of the 26 006 used
    let clear = Env::new(&sstore_code(0)).with_storage(0, 1).run();
    assert_eq!(gas_used(&clear), (TX_GAS + 6 + 5_000) / 2);
}

/// No net gas metering: setting and clearing a slot in one transaction pays
/// both writes in full (Ethereum: 22 100 + 100, 19 900 refunded).
#[test]
fn sstore_has_no_net_metering() {
    let mut code = sstore_code(1)[..5].to_vec();
    code.extend_from_slice(&sstore_code(0));
    let result = Env::new(&code).run();
    assert_eq!(gas_used(&result), TX_GAS + 12 + 20_000 + 5_000 - 15_000);
}

/// No EIP-2200 sentry: SSTORE with less than 2 300 gas left is a plain out of
/// gas, and is fine as long as the write itself is covered.
#[test]
fn sstore_has_no_stipend_sentry() {
    let exact = TX_GAS + 6 + 5_000;
    let result = Env::new(&sstore_code(2))
        .with_storage(0, 1)
        .call_with(CONTRACT, vec![], exact)
        .result;
    assert_eq!(gas_used(&result), exact);
}

/// `VM.computeCallGas`: 25 000 for a `CALL` to an address that is not in the
/// state, even without value (Ethereum since EIP-161: only with value; and
/// 2 600 for the cold access).
#[test]
fn call_pays_for_accounts_missing_from_the_state() {
    let missing = Env::new(&call_code(NOBODY, 0)).run();
    assert_eq!(gas_used(&missing), TX_GAS + CALL_OVERHEAD + 700 + 25_000);

    let empty = Env::new(&call_code(NOBODY, 0))
        .with_empty_account(NOBODY)
        .run();
    assert_eq!(gas_used(&empty), TX_GAS + CALL_OVERHEAD + 700);
}

/// The 2 300 stipend is charged to the caller like any other forwarded gas, so
/// a plain transfer nets 9 000 (Ethereum: 9 000 - 2 300 handed back = 6 700).
#[test]
fn value_transfer_stipend_is_not_free() {
    let result = Env::new(&call_code(OTHER, 1))
        .with_empty_account(OTHER)
        .run();
    assert_eq!(gas_used(&result), TX_GAS + CALL_OVERHEAD + 700 + 9_000);
}

/// No 63/64 rule: a callee that burns everything leaves the caller with
/// nothing, so the caller cannot even run its next opcode. On Ethereum the
/// caller keeps 1/64 and finishes.
#[test]
fn call_forwards_all_the_remaining_gas() {
    let result = Env::new(&call_code(OTHER, 0))
        .with_code(OTHER, &[0xfe]) // INVALID
        .run();
    assert!(
        matches!(
            result,
            ExecutionResult::Halt {
                reason: HaltReason::OutOfGas(OutOfGasError::Basic),
                ..
            }
        ),
        "unexpected result: {result:?}"
    );
    assert_eq!(result.gas_used(), GAS_LIMIT);
}

/// `VM.doSUICIDE`: 5 000 + 25 000 for a beneficiary that is not in the state
/// even though no balance moves, 24 000 refunded, and the contract is really
/// gone (no EIP-6780). Ethereum: 5 000 + 2 600, no refund, code kept.
#[test]
fn selfdestruct_keeps_the_pre_london_rules() {
    let mut code = vec![0x73];
    code.extend_from_slice(NOBODY.as_slice());
    code.push(0xff);
    let mut env = Env::new(&code);
    env.db.insert_account_info(
        CONTRACT,
        AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::copy_from_slice(&code))),
    );
    let ResultAndState { result, state } = env.call(CONTRACT, vec![]);
    assert_eq!(gas_used(&result), TX_GAS + 3 + 5_000 + 25_000 - 24_000);
    assert!(state[&CONTRACT].is_selfdestructed());
}

/// RSK answers `DIFFICULTY` with the block difficulty, not PREVRANDAO.
#[test]
fn difficulty_opcode_returns_the_block_difficulty() {
    // DIFFICULTY; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN
    let mut env = Env::new(&[0x44, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    env.block.difficulty = U256::from(0x1234_5678_u64);
    env.block.prevrandao = Some(B256::repeat_byte(0xee));
    let result = env.run();
    assert_eq!(
        U256::from_be_slice(result.output().expect("has output")),
        U256::from(0x1234_5678_u64)
    );
}

/// MODEXP keeps the EIP-198 price: 2^3 mod 5 is `1 * 1 / 20 = 0` gas
/// (Ethereum since EIP-2565: at least 200).
#[test]
fn modexp_is_priced_with_eip198() {
    let mut data = vec![0u8; 99];
    data[31] = 1;
    data[63] = 1;
    data[95] = 1;
    data[96..].copy_from_slice(&[2, 3, 5]);
    let calldata_gas = 6 * 16 + 93 * 4;
    let ResultAndState { result, .. } = Env::new(&[]).call(Address::with_last_byte(5), data);
    assert_eq!(gas_used(&result), TX_GAS + calldata_gas);
    assert_eq!(result.output().expect("has output").as_ref(), [3]);
}

/// `<op>(address)` stored to memory and returned.
fn ext_code_query(opcode: u8, target: Address) -> Vec<u8> {
    let mut code = vec![0x73];
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[opcode, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    code
}

fn output_word(result: &ExecutionResult) -> U256 {
    assert!(result.is_success(), "unexpected result: {result:?}");
    U256::from_be_slice(result.output().expect("has output"))
}

/// Solidity checks `EXTCODESIZE` before a high level call. RSK reports
/// `2^256 - 1` for precompiled and native contracts, otherwise a call to the
/// Bridge would revert here and never be forwarded.
#[test]
fn extcodesize_of_a_native_contract_is_max() {
    let bridge = address!("0000000000000000000000000000000001000006");
    for target in [
        bridge,
        Address::with_last_byte(1),
        Address::with_last_byte(9),
    ] {
        let result = Env::new(&ext_code_query(0x3b, target)).run();
        assert_eq!(output_word(&result), U256::MAX, "{target}");
    }
    for target in [NOBODY, Address::with_last_byte(0x0a)] {
        let result = Env::new(&ext_code_query(0x3b, target)).run();
        assert_eq!(output_word(&result), U256::ZERO, "{target}");
    }
    let result = Env::new(&ext_code_query(0x3b, OTHER))
        .with_code(OTHER, &[0x00, 0x00])
        .run();
    assert_eq!(output_word(&result), U256::from(2));
}

/// `VM.doEXTCODEHASH`: zero only for an account missing from the state.
#[test]
fn extcodehash_is_zero_only_for_missing_accounts() {
    let empty_hash = U256::from_be_bytes(revm::primitives::KECCAK_EMPTY.0);
    let bridge = address!("0000000000000000000000000000000001000006");

    let native = Env::new(&ext_code_query(0x3f, bridge)).run();
    assert_eq!(output_word(&native), empty_hash);

    let missing = Env::new(&ext_code_query(0x3f, NOBODY)).run();
    assert_eq!(output_word(&missing), U256::ZERO);

    // Ethereum: zero, the account is empty
    let empty = Env::new(&ext_code_query(0x3f, NOBODY))
        .with_empty_account(NOBODY)
        .run();
    assert_eq!(output_word(&empty), empty_hash);

    let code = [0x00];
    let contract = Env::new(&ext_code_query(0x3f, OTHER))
        .with_code(OTHER, &code)
        .run();
    assert_eq!(
        output_word(&contract),
        U256::from_be_bytes(revm::primitives::keccak256(code).0)
    );
}
