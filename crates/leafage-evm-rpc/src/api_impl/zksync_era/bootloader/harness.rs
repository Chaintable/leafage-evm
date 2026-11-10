#![allow(dead_code)]

use alloy::primitives::Bytes;
use primitive_types::{H160, U256};
use revm::context::result::{
    EVMError, ExecutionResult, HaltReason, InvalidTransaction, Output, ResultGas, SuccessReason,
};
use revm::DatabaseRef;
use zksync_vm2::interface::{CallframeInterface, HeapId, StateInterface, Tracer};
use zksync_vm2::{ExecutionEnd, FatPointer, Program, Settings, VirtualMachine};

use super::artifacts;
use super::constants::*;
use super::hook::VmHook;
use super::layout::{self, BootloaderEnv};
use super::types::TransactionData;
use crate::api_impl::zksync_era::executor::EraStorageWorld;

#[derive(Debug)]
pub struct BootloaderExecutionOutput {
    pub result: ExecutionResult<HaltReason>,
    pub computational_gas_used: u64,
    pub pubdata_published: u64,
    pub gas_refunded: u64,
}

fn read_heap_word<T: Tracer, W: zksync_vm2::World<T>>(
    vm: &VirtualMachine<T, W>,
    word: usize,
) -> U256 {
    vm.read_heap_u256(HeapId::FIRST, (word as u32) * 32)
}

fn read_bytes_from_heap<T: Tracer, W: zksync_vm2::World<T>>(
    vm: &VirtualMachine<T, W>,
    fp: FatPointer,
) -> Vec<u8> {
    if fp.offset > fp.length {
        return Vec::new();
    }
    (fp.start + fp.offset..fp.start + fp.length)
        .map(|addr| vm.read_heap_byte(fp.memory_page, addr))
        .collect()
}

pub fn compute_refund(
    effective_gas_price: u64,
    fair_l2_gas_price: u64,
    fair_pubdata_price: u64,
    bootloader_refund: u64,
    gas_spent_on_pubdata: u64,
    tx_gas_limit: u64,
    current_ergs_per_pubdata_byte: u32,
    pubdata_published: u32,
) -> u64 {
    let total_gas_spent = tx_gas_limit.saturating_sub(bootloader_refund);
    let gas_spent_on_computation = total_gas_spent.saturating_sub(gas_spent_on_pubdata);

    let bootloader_eth_price_per_pubdata_byte =
        U256::from(effective_gas_price) * U256::from(current_ergs_per_pubdata_byte);
    let fair_eth_price_per_pubdata_byte = U256::from(fair_pubdata_price);

    let eth_price_per_pubdata_byte_for_calculation =
        bootloader_eth_price_per_pubdata_byte.min(fair_eth_price_per_pubdata_byte);

    let fair_fee_eth = U256::from(gas_spent_on_computation) * U256::from(fair_l2_gas_price)
        + U256::from(pubdata_published) * eth_price_per_pubdata_byte_for_calculation;
    let pre_paid_eth = U256::from(tx_gas_limit) * U256::from(effective_gas_price);

    if pre_paid_eth > fair_fee_eth && effective_gas_price > 0 {
        let refund_eth = pre_paid_eth - fair_fee_eth;
        let gas_price_u256 = U256::from(effective_gas_price);
        ((refund_eth + gas_price_u256 - 1) / gas_price_u256).as_u64()
    } else {
        0
    }
}

pub fn execute_bootloader<DB: DatabaseRef>(
    db: DB,
    tx: &TransactionData,
    env: &BootloaderEnv,
    gas_limit: u64,
) -> Result<BootloaderExecutionOutput, EVMError<DB::Error, InvalidTransaction>> {
    let mut world = EraStorageWorld::new(db);
    world.set_expected_caller(tx.from);

    if env.block_number > 1 {
        let prev_num = (env.block_number - 1) as u128;
        let prev_ts = (env.block_timestamp.saturating_sub(1)) as u128;
        let slot9_val = (U256::from(prev_num) << 128) | U256::from(prev_ts);
        world.set_system_context_override(U256::from(9), slot9_val);
        world.set_system_context_override(U256::from(10), U256::zero());
        let prev_hash_slot = 11 + ((prev_num.saturating_sub(1)) % 257);
        world.set_system_context_override(U256::from(prev_hash_slot), U256::zero());
    }

    let bootloader_code = artifacts::FEE_ESTIMATE_BYTECODE;
    let bootloader_program = Program::new(bootloader_code, true);

    let bootloader_gas = BATCH_COMPUTATIONAL_GAS_LIMIT;

    let settings = Settings {
        default_aa_code_hash: artifacts::DEFAULT_AA_CODE_HASH,
        evm_interpreter_code_hash: artifacts::DEFAULT_AA_CODE_HASH,
        hook_address: VM_HOOK_ADDRESS,
    };

    let mut vm = VirtualMachine::new(
        BOOTLOADER_ADDRESS,
        bootloader_program,
        H160::zero(),
        &[],
        bootloader_gas,
        settings,
    );

    vm.current_frame().set_stack_pointer(0);
    vm.current_frame().set_heap_bound(u32::MAX);
    vm.current_frame().set_aux_heap_bound(u32::MAX);
    vm.current_frame().set_exception_handler(INITIAL_FRAME_FORMAL_EH_LOCATION);

    // Populate initial bootloader memory in heap
    let trusted_gas_limit = U256::from(MAX_L2_TX_GAS_LIMIT).min(tx.gas_limit);
    let memory = layout::build_bootloader_memory(tx, env, trusted_gas_limit);
    for (slot, value) in memory {
        let start_address = (slot as u32) * 32;
        vm.write_heap_u256(HeapId::FIRST, start_address, value);
    }

    // Decommit bootloader entry point
    let bootloader_hash = artifacts::fee_estimate_hash_u256();
    let mut tracer = ();
    vm.manually_decommit(&mut world, &mut tracer, bootloader_hash);

    let mut account_validation_split: Option<(u32, u32)> = None;
    let mut last_tx_result: Option<ExecutionResult<HaltReason>> = None;
    let mut gas_refunded: u64 = 0;

    let execution_result = loop {
        match vm.run(&mut world, &mut tracer) {
            ExecutionEnd::SuspendedOnHook(raw_hook) => {
                let hook = VmHook::from_raw(raw_hook);
                match hook {
                    VmHook::DebugLog | VmHook::DebugReturnData => {}
                    VmHook::AccountValidationEntered => {
                        let gas = vm.current_frame().gas();
                        let gas_given = gas.min(BATCH_COMPUTATIONAL_GAS_LIMIT);
                        let gas_hidden = gas - gas_given;
                        account_validation_split = Some((gas_given, gas_hidden));
                        vm.current_frame().set_gas(gas_given);
                    }
                    VmHook::ValidationExited => {
                        if let Some((_, gas_hidden)) = account_validation_split.take() {
                            let gas_left = vm.current_frame().gas();
                            vm.current_frame().set_gas(gas_left + gas_hidden);
                        }
                    }
                    VmHook::AskOperatorForRefund => {
                        let bootloader_refund =
                            read_heap_word(&vm, VM_HOOK_PARAMS_START_POSITION as usize).as_u64();
                        let gas_spent_on_pubdata =
                            read_heap_word(&vm, (VM_HOOK_PARAMS_START_POSITION + 1) as usize).as_u64();
                        let gas_per_pubdata_byte =
                            read_heap_word(&vm, (VM_HOOK_PARAMS_START_POSITION + 2) as usize).low_u32();

                        let pubdata_published =
                            vm.world_diff().pubdata_costs().iter().sum::<i32>().max(0) as u32;

                        let refund = compute_refund(
                            env.base_fee,
                            env.fair_l2_gas_price,
                            env.fair_pubdata_price,
                            bootloader_refund,
                            gas_spent_on_pubdata,
                            tx.gas_limit.as_u64(),
                            gas_per_pubdata_byte,
                            pubdata_published,
                        );

                        let refund_slot = OPERATOR_REFUNDS_OFFSET;
                        vm.write_heap_u256(
                            HeapId::FIRST,
                            (refund_slot as u32) * 32,
                            U256::from(refund),
                        );
                    }
                    VmHook::NotifyAboutRefund => {
                        gas_refunded =
                            read_heap_word(&vm, VM_HOOK_PARAMS_START_POSITION as usize).low_u64();
                    }
                    VmHook::PostResult => {
                        let result_param =
                            read_heap_word(&vm, VM_HOOK_PARAMS_START_POSITION as usize);
                        let result_success = !result_param.is_zero();
                        let value =
                            read_heap_word(&vm, (VM_HOOK_PARAMS_START_POSITION + 1) as usize);
                        let fp = FatPointer::from(value);
                        let return_data = read_bytes_from_heap(&vm, fp);

                        last_tx_result = Some(if result_success {
                            ExecutionResult::Success {
                                reason: SuccessReason::Return,
                                gas: ResultGas::default(),
                                logs: vec![],
                                output: Output::Call(Bytes::from(return_data)),
                            }
                        } else {
                            ExecutionResult::Revert {
                                gas: ResultGas::default(),
                                logs: vec![],
                                output: Bytes::from(return_data),
                            }
                        });
                    }
                    VmHook::TxHasEnded => {
                        let res = last_tx_result.take().unwrap_or_else(|| {
                            let tx_success =
                                !read_heap_word(&vm, RESULT_SUCCESS_FIRST_SLOT as usize).is_zero();
                            if tx_success {
                                ExecutionResult::Success {
                                    reason: SuccessReason::Return,
                                    gas: ResultGas::default(),
                                    logs: vec![],
                                    output: Output::Call(Bytes::new()),
                                }
                            } else {
                                ExecutionResult::Revert {
                                    gas: ResultGas::default(),
                                    logs: vec![],
                                    output: Bytes::new(),
                                }
                            }
                        });
                        break res;
                    }
                    _ => {
                        // Other hooks: DebugLog, DebugReturnData, NearCallCatch, etc. Continue
                    }
                }
            }
            ExecutionEnd::ProgramFinished(out) => {
                break ExecutionResult::Success {
                    reason: SuccessReason::Return,
                    gas: ResultGas::default(),
                    logs: vec![],
                    output: Output::Call(Bytes::from(out)),
                };
            }
            ExecutionEnd::Reverted(out) => {
                break ExecutionResult::Revert {
                    gas: ResultGas::default(),
                    logs: vec![],
                    output: Bytes::from(out),
                };
            }
            ExecutionEnd::Panicked => {
                break ExecutionResult::Halt {
                    reason: HaltReason::OpcodeNotFound,
                    gas: ResultGas::default().with_total_gas_spent(gas_limit),
                    logs: vec![],
                };
            }
            ExecutionEnd::StoppedByTracer => {
                break ExecutionResult::Halt {
                    reason: HaltReason::PrecompileError,
                    gas: ResultGas::default().with_total_gas_spent(gas_limit),
                    logs: vec![],
                };
            }
        }
    };

    let gas_remaining = vm.current_frame().gas();
    let computational_gas_used = (bootloader_gas as u64).saturating_sub(gas_remaining as u64);
    let pubdata_published = vm.world_diff().pubdata_costs().iter().sum::<i32>().max(0) as u64;

    Ok(BootloaderExecutionOutput {
        result: execution_result,
        computational_gas_used,
        pubdata_published,
        gas_refunded,
    })
}
