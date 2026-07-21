//! Arbitrum-specific EVM instruction overrides.
//!
//! Nitro replaces two stock instructions (`go-ethereum/core/vm/instructions.go`):
//! GASPRICE routes through `TxProcessor.GasPriceOp` and returns the paid gas
//! price (the block basefee while tips are dropped), and BLOCKHASH resolves
//! against the L1 block hashes recorded in ArbOS `Blockhashes` state instead
//! of the L2 header chain.

use crate::arbitrum::arbos_state::ArbStateReader;
use crate::arbitrum::precompile::{ArbitrumContext, BATCH_POSTER_ADDRESS};
use revm::bytecode::opcode;
use revm::context::{Block, ContextTr, Transaction};
use revm::context_interface::transaction::TransactionType;
use revm::handler::instructions::EthInstructions;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::StackTr;
use revm::interpreter::{Instruction, InstructionContext, push};
use revm::primitives::U256;
use revm::{Database, DatabaseRef};

/// Nitro `ArbosVersion_3`: first version where GASPRICE returns
/// `GetPaidGasPrice` (`arbos/tx_processor.go` `GasPriceOp`).
const ARBOS_VERSION_PAID_GAS_PRICE: u64 = 3;

/// Stock mainnet table with the Arbitrum GASPRICE / BLOCKHASH overrides.
/// Static gas matches the stock entries (2 and 20).
pub(super) fn arbitrum_instructions<DB>(
    spec: revm::primitives::hardfork::SpecId,
) -> EthInstructions<EthInterpreter, ArbitrumContext<DB>>
where
    DB: Database + DatabaseRef,
{
    let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
    instructions.insert_instruction(opcode::GASPRICE, Instruction::new(gasprice, 2));
    instructions.insert_instruction(opcode::BLOCKHASH, Instruction::new(blockhash, 20));
    instructions
}

/// Nitro `TxProcessor.CollectTips`: the ArbOS state flag (v9, or v60+ with the
/// collectTips setting) plus the sequenced-block coinbase check.
pub(super) fn collects_tips<DB>(ctx: &ArbitrumContext<DB>) -> bool
where
    DB: Database + DatabaseRef,
{
    ctx.db().collect_tips() && ctx.block().beneficiary() == BATCH_POSTER_ADDRESS
}

/// Nitro `opGasprice` -> `GasPriceOp` (`arbos/tx_processor.go`): from ArbOS 3,
/// GASPRICE returns the paid gas price — the (conditionally zeroed) block
/// basefee while tips are dropped, the message's gas price when tips are
/// collected. Below ArbOS 3 it returns the message's gas price, which nitro's
/// state transition has already clamped to the basefee (tips are always
/// dropped below ArbOS 9).
fn gasprice<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    let host = &*context.host;
    let basefee = host.block().basefee() as u128;
    let price = if host.db().arbos_version() >= ARBOS_VERSION_PAID_GAS_PRICE {
        if collects_tips(host) {
            message_gas_price(host, basefee)
        } else {
            basefee
        }
    } else {
        message_gas_price(host, basefee).min(basefee)
    };
    push!(context.interpreter, U256::from(price));
}

/// Nitro `msg.GasPrice`: geth's `ToMessage` defaults an absent
/// `maxPriorityFeePerGas` to 0 — `min(feeCap, basefee)` — while revm's
/// `effective_gas_price` returns the raw fee cap in that case. Legacy and
/// EIP-2930 prices are used verbatim on both sides.
fn message_gas_price<DB>(host: &ArbitrumContext<DB>, basefee: u128) -> u128
where
    DB: Database + DatabaseRef,
{
    let tx = host.tx();
    let effective = tx.effective_gas_price(basefee);
    let fixed_price = tx.tx_type() == TransactionType::Legacy as u8
        || tx.tx_type() == TransactionType::Eip2930 as u8;
    if !fixed_price && tx.max_priority_fee_per_gas().is_none() {
        effective.min(basefee)
    } else {
        effective
    }
}

/// Nitro `opBlockhash`: the 256-block window is anchored at the ArbOS-recorded
/// L1 block number and hashes come from ArbOS `Blockhashes` state, not the L2
/// header chain.
fn blockhash<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    let Some(([], number)) = StackTr::popn_top(&mut context.interpreter.stack) else {
        return context.interpreter.halt_underflow();
    };

    let Some(upper) = context.host.db().blockhashes_l1_block_number() else {
        return context.interpreter.halt_fatal();
    };
    let requested = number.saturating_to::<u64>();
    let lower = upper.saturating_sub(256);
    if requested >= lower && requested < upper {
        let Some(hash) = context.host.db().l1_block_hash(requested) else {
            return context.interpreter.halt_fatal();
        };
        *number = U256::from_be_bytes(hash.0);
    } else {
        *number = U256::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::evm::{ArbitrumEvm, ArbitrumExecutionContext};
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::precompile::ArbitrumPrecompileEnv;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, B256, Bytes, address};
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::ExecuteEvm;
    use revm::context::TxEnv;
    use revm::context::result::{ExecutionResult, Output};
    use revm::database::{EmptyDB, in_memory_db::CacheDB};
    use revm::primitives::TxKind;
    use revm::state::{AccountInfo, Bytecode};

    type TestDb = CacheDB<EmptyDB>;

    const CONTRACT: Address = address!("00000000000000000000000000000000000c0de0");
    const CALLER: Address = address!("0000000000000000000000000000000000ca11e4");
    // GASPRICE; MSTORE(0); RETURN(0, 32)
    const GASPRICE_CODE: &[u8] = &[0x3a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];
    // CALLDATALOAD(0); BLOCKHASH; MSTORE(0); RETURN(0, 32)
    const BLOCKHASH_CODE: &[u8] = &[0x5f, 0x35, 0x40, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

    fn db_with(arbos_version: u64, code: &'static [u8]) -> TestDb {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::ARBOS_VERSION_OFFSET),
            U256::from(arbos_version),
        )
        .expect("write ArbOS version");
        let bytecode = Bytecode::new_raw(Bytes::from_static(code));
        db.insert_account_info(
            CONTRACT,
            AccountInfo {
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                ..Default::default()
            },
        );
        db.insert_account_info(
            CALLER,
            AccountInfo {
                balance: U256::from(10u128.pow(18)),
                ..Default::default()
            },
        );
        db
    }

    fn test_tx(data: Bytes) -> TxEnv {
        TxEnv {
            caller: CALLER,
            kind: TxKind::Call(CONTRACT),
            gas_limit: 500_000,
            gas_price: 250,
            data,
            ..Default::default()
        }
    }

    fn call_contract(db: TestDb, beneficiary: Address, tx_env: TxEnv) -> Bytes {
        let block_env = BlockEnv {
            basefee: 100,
            gas_limit: 30_000_000,
            beneficiary,
            ..Default::default()
        };
        let mut execution_context = ArbitrumExecutionContext::default();
        execution_context.set_current_l2_context(block_env.number, block_env.basefee);
        let mut evm = ArbitrumEvm::new(
            block_env,
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            db,
            (),
            ArbitrumPrecompileEnv::default(),
            execution_context,
        );
        let tx = ArbitrumTxEnv::new(tx_env, Default::default());
        match evm.transact(tx).expect("transact").result {
            ExecutionResult::Success {
                output: Output::Call(bytes),
                ..
            } => bytes,
            other => panic!("expected successful call, got {other:?}"),
        }
    }

    #[test]
    fn gasprice_returns_basefee_while_tips_are_dropped() {
        // ArbOS 40, no collectTips flag: nitro GasPriceOp returns the basefee,
        // not the tx's own gas price.
        let output = call_contract(
            db_with(40, GASPRICE_CODE),
            Address::ZERO,
            test_tx(Bytes::new()),
        );
        assert_eq!(U256::from_be_slice(&output), U256::from(100));
    }

    fn collect_tips_db() -> TestDb {
        let mut db = db_with(60, GASPRICE_CODE);
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::COLLECT_TIPS_OFFSET),
            U256::ONE,
        )
        .expect("write collectTips");
        db
    }

    #[test]
    fn gasprice_returns_effective_price_when_tips_are_collected() {
        // EIP-1559 with an explicit tip: min(fee cap 250, basefee 100 + tip 50)
        // = 150, distinct from both the basefee and the fee cap.
        let tx = TxEnv {
            tx_type: TransactionType::Eip1559 as u8,
            gas_priority_fee: Some(50),
            ..test_tx(Bytes::new())
        };
        let output = call_contract(collect_tips_db(), BATCH_POSTER_ADDRESS, tx);
        assert_eq!(U256::from_be_slice(&output), U256::from(150));
    }

    #[test]
    fn gasprice_defaults_absent_priority_fee_to_zero_when_tips_are_collected() {
        // geth's ToMessage treats a missing maxPriorityFeePerGas as 0, so
        // nitro's msg.GasPrice is min(fee cap 250, basefee 100) = 100.
        let tx = TxEnv {
            tx_type: TransactionType::Eip1559 as u8,
            gas_priority_fee: None,
            ..test_tx(Bytes::new())
        };
        let output = call_contract(collect_tips_db(), BATCH_POSTER_ADDRESS, tx);
        assert_eq!(U256::from_be_slice(&output), U256::from(100));
    }

    #[test]
    fn gasprice_clamps_to_basefee_below_arbos_3() {
        // Pre-v3 nitro returns evm.GasPrice, already basefee-clamped by the
        // state transition while tips are dropped.
        let output = call_contract(
            db_with(2, GASPRICE_CODE),
            Address::ZERO,
            test_tx(Bytes::new()),
        );
        assert_eq!(U256::from_be_slice(&output), U256::from(100));
    }

    #[test]
    fn blockhash_reads_arbos_recorded_l1_hash() {
        let mut db = db_with(40, BLOCKHASH_CODE);
        let blockhashes_key = arbos_state::child_key(&[], arbos_state::BLOCKHASHES_SUBSPACE);
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&blockhashes_key, 0),
            U256::from(1_000u64),
        )
        .expect("write l1BlockNumber");
        let hash = B256::repeat_byte(0xab);
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&blockhashes_key, 1 + 999 % 256),
            U256::from_be_bytes(hash.0),
        )
        .expect("write recorded hash");

        let arg = |n: u64| Bytes::copy_from_slice(&U256::from(n).to_be_bytes::<32>());
        let output = call_contract(db.clone(), Address::ZERO, test_tx(arg(999)));
        assert_eq!(output.as_ref(), hash.as_slice());

        // At or above the recorded L1 number, and below the 256-block window:
        // zero, per nitro's opBlockhash.
        let output = call_contract(db.clone(), Address::ZERO, test_tx(arg(1_000)));
        assert_eq!(U256::from_be_slice(&output), U256::ZERO);
        let output = call_contract(db, Address::ZERO, test_tx(arg(743)));
        assert_eq!(U256::from_be_slice(&output), U256::ZERO);
    }
}
