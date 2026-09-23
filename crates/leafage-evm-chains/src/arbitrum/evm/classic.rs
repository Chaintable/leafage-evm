//! Account-state-only Classic call simulation on the shared Arbitrum EVM.
//!
//! Classic executes translated EVM bytecode inside AVM. Revm gas is only a
//! resource bound here; it cannot reproduce ArbGas, fee settlement or estimates.
//! Unsupported operations fail the whole request, including from nested calls.
use super::ArbitrumEvm;
use crate::arbitrum::precompile::ArbitrumContext;
use revm::bytecode::opcode;
use revm::context::{ContextTr, JournalTr, Transaction};
use revm::context_interface::{
    context::ContextError,
    result::{EVMError, HaltReason},
};
use revm::handler::{FrameResult, Handler, instructions::EthInstructions};
use revm::inspector::{Inspector, InspectorHandler};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{LoopControl, ReturnData, StackTr};
use revm::interpreter::{InitialAndFloorGas, Instruction, InstructionContext, as_usize_or_fail};
use revm::primitives::hardfork::SpecId;
use revm::{Database, DatabaseRef};
use std::marker::PhantomData;

pub(super) struct ClassicHandler<DB, I>(PhantomData<(DB, I)>);
impl<DB, I> ClassicHandler<DB, I> {
    pub(super) fn new() -> Self {
        Self(PhantomData)
    }
}

impl<DB: Database + DatabaseRef, I> Handler for ClassicHandler<DB, I> {
    type Evm = ArbitrumEvm<DB, I>;
    type Error = EVMError<<DB as Database>::Error>;
    type HaltReason = HaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        let tx = evm.ctx().tx();
        let unsupported = if tx.context.gas_estimation {
            Some("gas estimation requires AVM gas accounting")
        } else if tx.tx_type() != 0
            || !tx.kind().is_call()
            || tx.kind().to() == Some(&revm::primitives::Address::ZERO)
        {
            Some("only legacy call simulation is supported")
        } else if tx.gas_price() != 0 {
            Some("priced simulation requires Classic fee state; omit gasPrice or use zero")
        } else {
            None
        };
        if let Some(reason) = unsupported {
            return Err(EVMError::Custom(format!("Arbitrum Classic: {reason}")));
        }
        revm::handler::validation::validate_env(evm.ctx_mut())
    }

    fn validate_initial_tx_gas(
        &self,
        _: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        // ContractTransaction (Classic eth_call) has no Ethereum intrinsic gas.
        Ok(InitialAndFloorGas::new(0, 0))
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        // ContractTransaction has no sequence number. Do not bump the caller's
        // nonce or read/deduct Nitro poster fees from Classic account state.
        self.load_accounts(evm)?;
        Ok(0)
    }

    fn reimburse_caller(&self, _: &mut Self::Evm, _: &mut FrameResult) -> Result<(), Self::Error> {
        Ok(())
    }
    fn reward_beneficiary(
        &self,
        _: &mut Self::Evm,
        _: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<DB, I> InspectorHandler for ClassicHandler<DB, I>
where
    DB: Database + DatabaseRef,
    I: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type IT = EthInterpreter;
}

pub(super) fn instructions<DB: Database + DatabaseRef>()
-> EthInstructions<EthInterpreter, ArbitrumContext<DB>> {
    let mut table = EthInstructions::new_mainnet_with_spec(SpecId::BERLIN);
    // NUMBER is L1, BLOCKHASH is the private inbox-accumulator hash history,
    // GASLIMIT is the current ArbOS pool limit, and both price opcodes read
    // private pricing state. None can be reconstructed from the L2 header.
    table.insert_instruction(opcode::NUMBER, Instruction::new(unavailable::<DB, 0x43>, 2));
    table.insert_instruction(
        opcode::BLOCKHASH,
        Instruction::new(unavailable::<DB, 0x40>, 20),
    );
    table.insert_instruction(
        opcode::GASLIMIT,
        Instruction::new(unavailable::<DB, 0x45>, 2),
    );
    table.insert_instruction(
        opcode::GASPRICE,
        Instruction::new(unavailable::<DB, 0x3a>, 2),
    );
    table.insert_instruction(
        opcode::BASEFEE,
        Instruction::new(unavailable::<DB, 0x48>, 2),
    );
    // Creation/destruction need Classic-specific account lifecycle handling.
    table.insert_instruction(opcode::CREATE, Instruction::new(unavailable::<DB, 0xf0>, 0));
    table.insert_instruction(
        opcode::CREATE2,
        Instruction::new(unavailable::<DB, 0xf5>, 0),
    );
    table.insert_instruction(
        opcode::SELFDESTRUCT,
        Instruction::new(unavailable::<DB, 0xff>, 0),
    );
    table.insert_instruction(opcode::RETURNDATACOPY, Instruction::new(returndatacopy, 3));
    table.insert_instruction(opcode::MSIZE, Instruction::new(msize, 2));
    macro_rules! memory_write {
        ($op:ident) => {{
            let gas = table.instruction_table[opcode::$op as usize].static_gas();
            table.insert_instruction(
                opcode::$op,
                Instruction::new(write_memory::<DB, { opcode::$op }>, gas),
            );
        }};
    }
    memory_write!(MSTORE);
    memory_write!(MSTORE8);
    memory_write!(CALLDATACOPY);
    memory_write!(CODECOPY);
    memory_write!(EXTCODECOPY);
    table
}

pub(super) fn reserved_delegate_call(inputs: &revm::interpreter::CallInputs) -> bool {
    let address = inputs.bytecode_address;
    matches!(
        inputs.scheme,
        revm::interpreter::CallScheme::DelegateCall | revm::interpreter::CallScheme::CallCode
    ) && address >= revm::primitives::Address::with_last_byte(0x64)
        && address <= revm::primitives::Address::with_last_byte(0xc8)
}

fn msize<DB: Database + DatabaseRef>(
    ctx: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
) {
    let depth = ctx.host.journal().depth();
    let size = ctx.host.chain().classic_frame(depth).memory_size;
    revm::interpreter::push!(
        ctx.interpreter,
        revm::primitives::U256::from(size.saturating_add(31) & !31)
    );
}

fn valid_copy_source(source: revm::primitives::U256, len: revm::primitives::U256) -> bool {
    let max = revm::primitives::U256::from(1u128 << 64);
    len.is_zero() || (source < max && len <= max && source.saturating_add(len) <= max)
}

fn write_memory<DB: Database + DatabaseRef, const OP: u8>(
    ctx: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
) {
    use revm::primitives::{Address, U256};
    let peek = |n| ctx.interpreter.stack.peek(n).ok();
    let fields = match OP {
        opcode::MSTORE => peek(0).map(|dest| (dest, U256::ZERO, U256::from(32), 2)),
        opcode::MSTORE8 => peek(0).map(|dest| (dest, U256::ZERO, U256::ONE, 2)),
        opcode::EXTCODECOPY => peek(1)
            .zip(peek(2))
            .zip(peek(3))
            .map(|((d, s), l)| (d, s, l, 4)),
        _ => peek(0)
            .zip(peek(1))
            .zip(peek(2))
            .map(|((d, s), l)| (d, s, l, 3)),
    };
    let Some((dest, source, len, pops)) = fields else {
        return ctx.interpreter.halt_underflow();
    };
    if !valid_copy_source(source, len) && !matches!(OP, opcode::MSTORE | opcode::MSTORE8) {
        if OP == opcode::EXTCODECOPY {
            let addr = Address::from_word(ctx.interpreter.stack.peek(0).unwrap().into());
            let account = match ctx.host.journal_mut().load_account_with_code(addr) {
                Ok(account) => account,
                Err(e) => {
                    *ctx.host.error() = Err(ContextError::Db(e));
                    return ctx.interpreter.halt_fatal();
                }
            };
            if account
                .info
                .code
                .as_ref()
                .is_none_or(|code| code.is_empty())
            {
                // Empty bytecode cannot distinguish an EOA from Classic's
                // empty-code contractInfo; these have different copy rules.
                *ctx.host.error() = Err(ContextError::Custom("Arbitrum Classic: EXTCODECOPY with an oversized source on an empty-code account requires Classic account metadata".into()));
                return ctx.interpreter.halt_fatal();
            }
        }
        // Still account for bounded destination memory expansion, but do not
        // modify bytes or the Classic ByteArray size for an invalid source.
        for _ in 0..pops {
            let _ = StackTr::popn::<1>(&mut ctx.interpreter.stack);
        }
        let len = as_usize_or_fail!(ctx.interpreter, len);
        let _ = revm::interpreter::instructions::system::copy_cost_and_memory_resize(
            ctx.interpreter,
            &ctx.host.cfg().gas_params,
            dest,
            len,
        );
        return;
    }
    revm::interpreter::instructions::instruction_table::<EthInterpreter, ArbitrumContext<DB>>()
        [OP as usize]
        .execute(InstructionContext {
            interpreter: &mut *ctx.interpreter,
            host: &mut *ctx.host,
        });
    if matches!(ctx.interpreter.bytecode.action().as_ref(), Some(revm::interpreter::InterpreterAction::Return(result)) if !result.result.is_ok())
    {
        return;
    }
    let depth = ctx.host.journal().depth();
    ctx.host.chain_mut().classic_memory_write(
        depth,
        dest.saturating_to::<usize>(),
        len.saturating_to::<usize>(),
    );
}

fn unavailable<DB: Database + DatabaseRef, const OP: u8>(
    ctx: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
) {
    let name = match OP {
        0x43 => "NUMBER",
        0x40 => "BLOCKHASH",
        0x45 => "GASLIMIT",
        0x3a => "GASPRICE",
        0x48 => "BASEFEE",
        0xf0 => "CREATE",
        0xf5 => "CREATE2",
        0xff => "SELFDESTRUCT",
        _ => unreachable!(),
    };
    *ctx.host.error() = Err(ContextError::Custom(format!(
        "Arbitrum Classic: {name} is unavailable in account-state-only simulation"
    )));
    ctx.interpreter.halt_fatal();
}

fn returndatacopy<DB: Database + DatabaseRef>(
    ctx: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
) {
    let Some([dest, offset, len]) = StackTr::popn(&mut ctx.interpreter.stack) else {
        return ctx.interpreter.halt_underflow();
    };
    let len = as_usize_or_fail!(ctx.interpreter, len);
    let Some(dest) = revm::interpreter::instructions::system::copy_cost_and_memory_resize(
        ctx.interpreter,
        &ctx.host.cfg().gas_params,
        dest,
        len,
    ) else {
        return;
    };
    let depth = ctx.host.journal().depth();
    let offset = if ctx.host.chain().classic_frame(depth).has_return_data {
        offset
    } else {
        revm::primitives::U256::ZERO
    };
    if !valid_copy_source(offset, revm::primitives::U256::from(len)) {
        return;
    }
    // ArbOS evmOps.mini zero-fills beyond return data, including no prior call.
    ctx.interpreter.memory.set_data(
        dest,
        offset.saturating_to::<usize>(),
        len,
        ctx.interpreter.return_data.buffer(),
    );
    ctx.host.chain_mut().classic_memory_write(depth, dest, len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::precompile::ArbitrumPrecompileEnv;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use crate::arbitrum::{ArbitrumExecutionMode, ArbitrumHardfork};
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::{TxEnv, result::ExecutionResult};
    use revm::database::{CacheDB, EmptyDB};
    use revm::inspector::NoOpInspector;
    use revm::primitives::{Address, Bytes, TxKind, U256};
    use revm::state::{AccountInfo, Bytecode};
    use revm::{ExecuteEvm, InspectEvm};

    const CONTRACT: Address = Address::with_last_byte(0xaa);
    const CALLER: Address = Address::with_last_byte(0xbb);
    const CHILD: Address = Address::with_last_byte(0xcc);
    fn code(db: &mut CacheDB<EmptyDB>, at: Address, bytes: &[u8]) {
        let code = Bytecode::new_raw(Bytes::copy_from_slice(bytes));
        db.insert_account_info(
            at,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
    }
    fn evm(bytes: &[u8]) -> ArbitrumEvm<CacheDB<EmptyDB>, NoOpInspector> {
        let mut db = CacheDB::new(EmptyDB::default());
        code(&mut db, CONTRACT, bytes);
        db.insert_account_info(
            CALLER,
            AccountInfo {
                nonce: 7,
                balance: U256::from(1000),
                ..Default::default()
            },
        );
        // Supplying Osaka and a high ArbOS version must not activate Nitro.
        let mut cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Osaka);
        cfg.chain_id = 42161;
        ArbitrumEvm::new(
            BlockEnv {
                number: U256::from(4198902),
                gas_limit: 30_000_000,
                ..Default::default()
            },
            cfg,
            db,
            NoOpInspector {},
            ArbitrumPrecompileEnv {
                execution_mode: ArbitrumExecutionMode::Classic,
                current_arbos_version: 60,
                ..Default::default()
            },
            ArbitrumExecutionContext::default(),
        )
    }
    fn tx() -> ArbitrumTxEnv {
        ArbitrumTxEnv::new(
            TxEnv {
                caller: CALLER,
                kind: TxKind::Call(CONTRACT),
                gas_limit: 1_000_000,
                nonce: 7,
                chain_id: Some(42161),
                ..Default::default()
            },
            Default::default(),
        )
    }
    fn output(result: ExecutionResult) -> Bytes {
        assert!(result.is_success(), "{result:?}");
        result.output().unwrap().clone()
    }
    #[test]
    fn classic_opcode_outputs_match_archive_node() {
        for (op, expected) in [
            (0x41, U256::ZERO),
            (0x44, U256::from(2_500_000_000_000_000u64)),
            (0x46, U256::from(42161)),
        ] {
            let bytes = [op, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3];
            for inspect in [false, true] {
                let mut evm = evm(&bytes);
                let result = if inspect {
                    evm.inspect_tx(tx())
                } else {
                    evm.transact(tx())
                }
                .unwrap();
                assert_eq!(U256::from_be_slice(&output(result.result)), expected);
                assert!(
                    !result
                        .state
                        .contains_key(&crate::arbitrum::arbos_state::ARBOS_STATE_ADDRESS)
                );
                if let Some(caller) = result.state.get(&CALLER) {
                    assert_eq!(caller.info.nonce, 7);
                    assert_eq!(caller.info.balance, U256::from(1000));
                }
            }
        }
    }
    #[test]
    fn classic_returndatacopy_zero_fills_and_preserves_other_memory() {
        // Store 0xff, then copy 1 byte from an empty return buffer over it.
        let mut evm = evm(&[
            0x60, 0xff, 0x60, 0, 0x53, 0x60, 1, 0x60, 0, 0x60, 0, 0x3e, 0x60, 32, 0x60, 0, 0xf3,
        ]);
        assert_eq!(
            output(evm.transact(tx()).unwrap().result),
            Bytes::from(vec![0; 32])
        );
    }
    #[test]
    fn classic_mutations_remain_visible_without_nonce_or_fee_side_effects() {
        let mut evm = evm(&[
            0x60, 1, 0x60, 0, 0x55, 0x60, 0, 0x54, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3,
        ]);
        let result = evm.transact(tx()).unwrap();
        assert_eq!(U256::from_be_slice(&output(result.result)), U256::ONE);
        assert_eq!(
            result.state[&CONTRACT].storage[&U256::ZERO].present_value(),
            U256::ONE
        );
        if let Some(caller) = result.state.get(&CALLER) {
            assert_eq!(caller.info.nonce, 7);
        }
    }
    #[test]
    fn classic_missing_data_cannot_be_caught_by_nested_calls() {
        // Parent ignores the child's success flag and returns 42.
        let parent = [
            0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0xcc, 0x5a, 0xf1, 0x50, 0x60, 42,
            0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3,
        ];
        for op in [0x43, 0x40, 0x45, 0x3a, 0x48, 0xf0, 0xf5, 0xff] {
            for inspect in [false, true] {
                let mut evm = evm(&parent);
                code(evm.ctx_mut().db_mut(), CHILD, &[op, 0]);
                let err = if inspect {
                    evm.inspect_tx(tx())
                } else {
                    evm.transact(tx())
                }
                .unwrap_err();
                assert!(err.to_string().contains("Arbitrum Classic:"), "{err}");
            }
        }
    }
    #[test]
    fn classic_does_not_enable_nitro_opcodes_or_precompiles() {
        let mut evm = evm(&[0x5f, 0]);
        assert!(!evm.transact(tx()).unwrap().result.is_success());
        let mut tx = tx();
        tx.base.kind = TxKind::Call(Address::with_last_byte(0x70));
        assert_eq!(output(evm.transact(tx).unwrap().result), Bytes::new());
    }
    #[test]
    fn classic_arbsys_dispatch_and_nonce_match_archive_node() {
        for (data, expected) in [
            ("a3b1b31d".to_string(), U256::from(4198902)),
            ("d127f54a".into(), U256::from(42161)),
            ("08bd624c".into(), U256::ZERO),
            (
                format!("23ca0cd2{:064x}", U256::from_be_slice(CALLER.as_slice())),
                U256::from(7),
            ),
        ] {
            let mut evm = evm(&[]);
            let mut tx = tx();
            tx.base.kind = TxKind::Call(Address::with_last_byte(0x64));
            tx.base.data = alloy::primitives::hex::decode(data).unwrap().into();
            assert_eq!(
                U256::from_be_slice(&output(evm.transact(tx).unwrap().result)),
                expected
            );
        }
        // Contract forwarding calldata via STATICCALL -> ArbSys.
        let mut evm = evm(&alloy::primitives::hex::decode(
            "3660006000376020600036600060645afa5060206000f3",
        )
        .unwrap());
        let mut tx = tx();
        tx.base.data = Bytes::from_static(&[0x08, 0xbd, 0x62, 0x4c]);
        assert_eq!(
            U256::from_be_slice(&output(evm.transact(tx).unwrap().result)),
            U256::ONE
        );
    }
    #[test]
    fn classic_unknown_arbsys_selector_reverts_but_missing_state_is_fatal() {
        let forwarder =
            alloy::primitives::hex::decode("3660006000376020600036600060645afa60005260206000f3")
                .unwrap();
        let mut evm = evm(&forwarder);
        let mut tx = tx();
        tx.base.data = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            U256::from_be_slice(&output(evm.transact(tx.clone()).unwrap().result)),
            U256::ZERO
        );
        tx.base.data = Bytes::from_static(&[0x05, 0x10, 0x38, 0xf2]);
        assert!(
            evm.inspect_tx(tx)
                .unwrap_err()
                .to_string()
                .contains("Arbitrum Classic:")
        );
    }

    #[test]
    fn classic_msize_tracks_writes_not_memory_reads() {
        for (code, expected) in [
            ("600051505960005260206000f3", 0),
            ("6020600020505960005260206000f3", 0),
            ("60206000a05960005260206000f3", 0),
            ("60006000525960005260206000f3", 32),
            ("60016020535960005260206000f3", 64),
            ("600160006020375960005260206000f3", 64),
            ("600160006020395960005260206000f3", 64),
            ("60016000602060aa3c5960005260206000f3", 64),
            ("6001600060203e5960005260206000f3", 64),
            // Requesting 32 output bytes from an empty account writes nothing.
            ("6020608060006000600060ee5af1505960005260206000f3", 0),
        ] {
            for inspect in [false, true] {
                let mut evm = evm(&alloy::primitives::hex::decode(code).unwrap());
                let result = if inspect {
                    evm.inspect_tx(tx())
                } else {
                    evm.transact(tx())
                }
                .unwrap();
                assert_eq!(
                    U256::from_be_slice(&output(result.result)),
                    U256::from(expected),
                    "{code}, inspected={inspect}"
                );
            }
        }
    }

    #[test]
    fn classic_msize_accounts_for_actual_child_output_and_resets_reused_frames() {
        for inspect in [false, true] {
            let parent =
                alloy::primitives::hex::decode("6020608060006000600060cc5af1505960005260206000f3")
                    .unwrap();
            let mut evm = evm(&parent);
            code(
                evm.ctx_mut().db_mut(),
                CHILD,
                &alloy::primitives::hex::decode("600160005360016000f3").unwrap(),
            );
            let result = if inspect {
                evm.inspect_tx(tx())
            } else {
                evm.transact(tx())
            }
            .unwrap();
            assert_eq!(U256::from_be_slice(&output(result.result)), U256::from(160));
        }
        let call = "6020600060006000600060cc5af150";
        let parent = alloy::primitives::hex::decode(format!("{call}{call}60206000f3")).unwrap();
        let mut evm = evm(&parent);
        code(
            evm.ctx_mut().db_mut(),
            CHILD,
            &alloy::primitives::hex::decode("5960005260206000f3").unwrap(),
        );
        assert_eq!(
            U256::from_be_slice(&output(evm.transact(tx()).unwrap().result)),
            U256::ZERO
        );
    }

    #[test]
    fn classic_copy_source_bounds_and_return_data_presence_match_archive() {
        let sentinel = "60ff600053";
        let huge_source = "68010000000000000000";
        for (copy, expected) in [
            (format!("6001{huge_source}600037"), 0xff),
            (format!("6001{huge_source}600039"), 0xff),
            (format!("6001{huge_source}600060aa3c"), 0xff),
            (format!("6001{huge_source}60003e"), 0),
            // A successful empty child changes returnInfo from None to Some(empty).
            (
                format!("6000600060006000600060ee5af1506001{huge_source}60003e"),
                0xff,
            ),
            // No-op length never needs account metadata or changes memory.
            (format!("6000{huge_source}600060ee3c"), 0xff),
        ] {
            let bytes =
                alloy::primitives::hex::decode(format!("{sentinel}{copy}60206000f3")).unwrap();
            for inspect in [false, true] {
                let mut evm = evm(&bytes);
                let result = if inspect {
                    evm.inspect_tx(tx())
                } else {
                    evm.transact(tx())
                }
                .unwrap();
                let result = output(result.result);
                assert_eq!(result[0], expected, "{copy}");
                assert!(result[1..].iter().all(|&b| b == 0));
            }
        }
    }

    #[test]
    fn classic_reserved_delegate_calls_fail_without_losing_return_data() {
        let identity = "60016000536001600060016000600060045af150";
        for address in [0x64, 0x65, 0x70, 0xc8] {
            for (opcode, value) in [("f4", ""), ("f2", "6000")] {
                for inspect in [false, true] {
                    let call = format!("6000600060006000{value}60{address:02x}5a{opcode}");
                    let bytes =
                        alloy::primitives::hex::decode(format!("{call}60005260206000f3")).unwrap();
                    let mut executor = evm(&bytes);
                    let result = if inspect {
                        executor.inspect_tx(tx())
                    } else {
                        executor.transact(tx())
                    }
                    .unwrap();
                    assert_eq!(U256::from_be_slice(&output(result.result)), U256::ZERO);
                    let bytes = alloy::primitives::hex::decode(format!(
                        "{identity}{call}503d60005260206000f3"
                    ))
                    .unwrap();
                    let mut executor = evm(&bytes);
                    let result = if inspect {
                        executor.inspect_tx(tx())
                    } else {
                        executor.transact(tx())
                    }
                    .unwrap();
                    assert_eq!(U256::from_be_slice(&output(result.result)), U256::ONE);
                }
            }
        }
    }

    #[test]
    fn classic_failed_value_calls_preserve_data_for_contracts_and_builtins() {
        for target in [1u8, 9, 0x64, 0xcc] {
            let bytes = alloy::primitives::hex::decode(format!("60016000536001600060016000600060045af1506000600060006000606560{target:02x}5af1503d60005260206000f3")).unwrap();
            for inspect in [false, true] {
                let mut evm = evm(&bytes);
                let db = evm.ctx_mut().db_mut();
                let mut account = db.basic_ref(CONTRACT).unwrap().unwrap();
                account.balance = U256::from(100);
                db.insert_account_info(CONTRACT, account);
                if target == 0xcc {
                    code(db, CHILD, &[0]);
                }
                let result = if inspect {
                    evm.inspect_tx(tx())
                } else {
                    evm.transact(tx())
                }
                .unwrap();
                assert_eq!(U256::from_be_slice(&output(result.result)), U256::ONE);
            }
        }
    }

    #[test]
    fn classic_failed_value_call_rejects_ambiguous_empty_code_accounts() {
        let bytes = alloy::primitives::hex::decode("60016000536001600060016000600060045af1506000600060006000606560cc5af1503d60005260206000f3").unwrap();
        for empty_contract in [false, true] {
            for inspect in [false, true] {
                let mut evm = evm(&bytes);
                if empty_contract {
                    code(evm.ctx_mut().db_mut(), CHILD, &[]);
                }
                let result = if inspect {
                    evm.inspect_tx(tx())
                } else {
                    evm.transact(tx())
                };
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("insufficient-balance CALL to an empty-code account")
                );
            }
        }
    }

    #[test]
    fn classic_native_hash_builtins_revert_and_can_be_caught_by_callers() {
        for (address, input) in [
            (3, Bytes::new()),
            (3, Bytes::from_static(b"abc")),
            (9, Bytes::from(vec![0; 213])),
            (9, {
                let mut data = vec![0; 213];
                data[3] = 1;
                Bytes::from(data)
            }),
        ] {
            for inspect in [false, true] {
                let mut executor = evm(&[]);
                let mut request = tx();
                request.base.kind = TxKind::Call(Address::with_last_byte(address));
                request.base.data = input.clone();
                let result = if inspect {
                    executor.inspect_tx(request)
                } else {
                    executor.transact(request)
                }
                .unwrap();
                assert!(matches!(result.result, ExecutionResult::Revert { .. }));
                // Forward valid calldata, then return the STATICCALL success bit.
                let bytes = alloy::primitives::hex::decode(format!(
                    "3660006000376000600036600060{address:02x}5afa60005260206000f3"
                ))
                .unwrap();
                let mut executor = evm(&bytes);
                let mut request = tx();
                request.base.data = input.clone();
                let result = if inspect {
                    executor.inspect_tx(request)
                } else {
                    executor.transact(request)
                }
                .unwrap();
                assert_eq!(U256::from_be_slice(&output(result.result)), U256::ZERO);
            }
        }
    }

    #[test]
    fn classic_standard_precompile_boundaries_match_archive() {
        for len in [0, 127, 128, 129] {
            let mut evm = evm(&[]);
            let mut tx = tx();
            tx.base.kind = TxKind::Call(Address::with_last_byte(1));
            tx.base.data = Bytes::from(vec![0; len]);
            let result = evm.transact(tx).unwrap().result;
            if len == 128 {
                assert_eq!(output(result), Bytes::from(vec![0; 32]));
            } else {
                assert!(matches!(result, ExecutionResult::Revert { .. }));
            }
        }
        for len in [0, 1, 191, 192, 193, 30 * 192, 31 * 192] {
            let mut evm = evm(&[]);
            let mut tx = tx();
            tx.base.gas_limit = 2_000_000;
            tx.base.kind = TxKind::Call(Address::with_last_byte(8));
            tx.base.data = Bytes::from(vec![0; len]);
            let result = evm.transact(tx).unwrap().result;
            if len < 31 * 192 {
                assert_eq!(U256::from_be_slice(&output(result)), U256::ONE);
            } else {
                assert!(matches!(result, ExecutionResult::Revert { .. }));
            }
        }
        let mut evm = evm(&[]);
        let mut tx = tx();
        tx.base.kind = TxKind::Call(Address::with_last_byte(9));
        let mut data = vec![0; 213];
        data[..4].copy_from_slice(&65536u32.to_be_bytes());
        tx.base.data = data.into();
        assert!(matches!(
            evm.transact(tx).unwrap().result,
            ExecutionResult::Revert { .. }
        ));
    }

    #[test]
    fn classic_nonzero_callcode_fails_explicitly() {
        let mut evm =
            evm(&alloy::primitives::hex::decode("6000600060006000600160cc5af200").unwrap());
        assert!(
            evm.transact(tx())
                .unwrap_err()
                .to_string()
                .contains("nonzero-value CALLCODE")
        );
    }

    #[test]
    fn classic_rejects_estimates_priced_calls_and_creation() {
        for kind in 0..3 {
            let mut evm = evm(&[0]);
            let mut tx = tx();
            match kind {
                0 => tx.context.gas_estimation = true,
                1 => tx.base.gas_price = 1,
                _ => tx.base.kind = TxKind::Create,
            }
            assert!(
                evm.transact(tx)
                    .unwrap_err()
                    .to_string()
                    .contains("Arbitrum Classic:")
            );
        }
    }
}
