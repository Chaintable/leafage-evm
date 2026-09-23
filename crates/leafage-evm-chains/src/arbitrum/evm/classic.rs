//! Account-state-only Classic call simulation on the shared Arbitrum EVM.
//!
//! Classic executes translated EVM bytecode inside AVM. Revm gas is only a
//! resource bound here; it cannot reproduce ArbGas, fee settlement or estimates.
//! Unsupported operations fail the whole request, including from nested calls.
use super::ArbitrumEvm;
use crate::arbitrum::precompile::ArbitrumContext;
use revm::bytecode::opcode;
use revm::context::{ContextTr, Transaction};
use revm::context_interface::{
    context::ContextError,
    result::{EVMError, HaltReason},
};
use revm::handler::{FrameResult, Handler, instructions::EthInstructions};
use revm::inspector::{Inspector, InspectorHandler};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{ReturnData, StackTr};
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
    table
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
    // ArbOS evmOps.mini zero-fills beyond return data, including no prior call.
    ctx.interpreter.memory.set_data(
        dest,
        offset.saturating_to::<usize>(),
        len,
        ctx.interpreter.return_data.buffer(),
    );
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
