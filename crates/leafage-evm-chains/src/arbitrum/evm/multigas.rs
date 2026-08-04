//! ArbOS 60+ multi-dimensional gas accounting.

use crate::arbitrum::precompile::ArbitrumContext;
use alloy::primitives::{Address, U256};
use revm::bytecode::opcode;
use revm::context::{ContextTr, Host, JournalEntry};
use revm::context_interface::{
    cfg::gas_params::GasParams,
    transaction::{AccessListItemTr, TransactionType},
    Cfg, Transaction,
};
use revm::handler::instructions::EthInstructions;
use revm::interpreter::{
    instructions::{contract, host},
    interpreter::EthInterpreter,
    interpreter_types::{InputsTr, LoopControl, RuntimeFlag},
    Instruction, InstructionContext, InstructionResult, InterpreterAction,
};
use revm::primitives::hardfork::SpecId;
use revm::{Database, DatabaseRef};

pub(crate) const NUM_RESOURCE_KIND: usize = 9;
const SELFDESTRUCT_STORAGE_WRITE_GAS: u64 = 4_900;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(crate) enum ArbResourceKind {
    Computation = 1,
    HistoryGrowth = 2,
    StorageAccessRead = 3,
    StorageAccessWrite = 4,
    StorageGrowth = 5,
    SingleDim = 6,
    L2Calldata = 7,
    WasmComputation = 8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ArbMultiGas {
    resources: [u64; NUM_RESOURCE_KIND],
}

impl ArbMultiGas {
    pub(crate) fn intrinsic(tx: &impl Transaction, initial_gas: u64) -> Self {
        const TX_DATA_ZERO_GAS: u64 = 4;
        const TX_DATA_NON_ZERO_GAS: u64 = 16;
        const ACCESS_LIST_ADDRESS_GAS: u64 = 2_400;
        const ACCESS_LIST_STORAGE_GAS: u64 = 1_900;
        const AUTHORIZATION_GAS: u64 = 25_000;

        let mut gas = Self::default();
        let zero_bytes = tx.input().iter().filter(|byte| **byte == 0).count() as u64;
        let non_zero_bytes = (tx.input().len() as u64).saturating_sub(zero_bytes);
        gas.record(
            ArbResourceKind::L2Calldata,
            zero_bytes
                .saturating_mul(TX_DATA_ZERO_GAS)
                .saturating_add(non_zero_bytes.saturating_mul(TX_DATA_NON_ZERO_GAS)),
        );

        if tx.tx_type() != TransactionType::Legacy as u8 {
            let (accounts, slots) = tx
                .access_list()
                .map(|access_list| {
                    access_list.fold((0u64, 0u64), |(accounts, slots), item| {
                        (
                            accounts.saturating_add(1),
                            slots.saturating_add(item.storage_slots().count() as u64),
                        )
                    })
                })
                .unwrap_or_default();
            gas.record(
                ArbResourceKind::StorageAccessRead,
                accounts
                    .saturating_mul(ACCESS_LIST_ADDRESS_GAS)
                    .saturating_add(slots.saturating_mul(ACCESS_LIST_STORAGE_GAS)),
            );
        }

        gas.record(
            ArbResourceKind::StorageGrowth,
            (tx.authorization_list_len() as u64).saturating_mul(AUTHORIZATION_GAS),
        );
        gas.record(
            ArbResourceKind::Computation,
            initial_gas.saturating_sub(gas.total()),
        );
        gas
    }

    pub(crate) fn record(&mut self, resource: ArbResourceKind, amount: u64) {
        let current = &mut self.resources[resource as usize];
        *current = current.saturating_add(amount);
    }

    pub(crate) fn add(&mut self, other: Self) {
        for (resource, amount) in self.resources.iter_mut().zip(other.resources) {
            *resource = resource.saturating_add(amount);
        }
    }

    pub(crate) fn sstore_cost(
        gas_params: &GasParams,
        original_value: U256,
        present_value: U256,
        new_value: U256,
        is_cold: bool,
    ) -> Self {
        let mut gas = Self::default();
        if is_cold {
            gas.record(
                ArbResourceKind::StorageAccessRead,
                gas_params.cold_storage_cost(),
            );
        }
        if present_value == new_value || original_value != present_value {
            gas.record(
                ArbResourceKind::Computation,
                gas_params.warm_storage_read_cost(),
            );
        } else if original_value.is_zero() {
            gas.record(
                ArbResourceKind::StorageGrowth,
                gas_params
                    .sstore_set_without_load_cost()
                    .saturating_add(gas_params.sstore_static_gas()),
            );
        } else {
            gas.record(
                ArbResourceKind::StorageAccessWrite,
                gas_params
                    .sstore_reset_without_cold_load_cost()
                    .saturating_add(gas_params.sstore_static_gas()),
            );
        }
        gas
    }

    #[cfg(test)]
    pub(crate) fn get(&self, resource: ArbResourceKind) -> u64 {
        self.resources[resource as usize]
    }

    pub(crate) fn resources(&self) -> &[u64; NUM_RESOURCE_KIND] {
        &self.resources
    }

    pub(crate) fn total(&self) -> u64 {
        self.resources
            .iter()
            .fold(0u64, |total, gas| total.saturating_add(*gas))
    }

    pub(crate) fn single_gas(&self, refund: u64) -> u64 {
        self.total().saturating_sub(refund)
    }
}

pub(super) fn install_instruction_metering<DB>(
    instructions: &mut EthInstructions<EthInterpreter, ArbitrumContext<DB>>,
) where
    DB: Database + DatabaseRef,
{
    replace(instructions, opcode::BALANCE, balance::<DB>);
    replace(instructions, opcode::EXTCODESIZE, extcodesize::<DB>);
    replace(instructions, opcode::EXTCODEHASH, extcodehash::<DB>);
    replace(instructions, opcode::EXTCODECOPY, extcodecopy::<DB>);
    replace(instructions, opcode::SLOAD, sload::<DB>);
    replace(instructions, opcode::SSTORE, sstore::<DB>);
    replace(instructions, opcode::LOG0, log::<0, DB>);
    replace(instructions, opcode::LOG1, log::<1, DB>);
    replace(instructions, opcode::LOG2, log::<2, DB>);
    replace(instructions, opcode::LOG3, log::<3, DB>);
    replace(instructions, opcode::LOG4, log::<4, DB>);
    replace(instructions, opcode::CALL, call::<DB>);
    replace(instructions, opcode::CALLCODE, callcode::<DB>);
    replace(instructions, opcode::DELEGATECALL, delegatecall::<DB>);
    replace(instructions, opcode::STATICCALL, staticcall::<DB>);
    replace(instructions, opcode::SELFDESTRUCT, selfdestruct::<DB>);
}

fn replace<DB>(
    instructions: &mut EthInstructions<EthInterpreter, ArbitrumContext<DB>>,
    opcode: u8,
    instruction: fn(InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>),
) where
    DB: Database + DatabaseRef,
{
    let static_gas = instructions.instruction_table[opcode as usize].static_gas();
    instructions.insert_instruction(opcode, Instruction::new(instruction, static_gas));
}

fn run_stock<DB>(
    context: &mut InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
    instruction: fn(InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>),
) where
    DB: Database + DatabaseRef,
{
    instruction(InstructionContext {
        interpreter: &mut *context.interpreter,
        host: &mut *context.host,
    });
}

fn failed(interpreter: &mut revm::interpreter::Interpreter<EthInterpreter>) -> bool {
    matches!(
        interpreter.bytecode.action().as_ref(),
        Some(InterpreterAction::Return(result)) if !result.result.is_ok()
    )
}

fn journal_len<DB: Database>(context: &ArbitrumContext<DB>) -> usize {
    context.journal().journal.len()
}

fn warmed_accounts<DB: Database>(context: &ArbitrumContext<DB>, start: usize) -> u64 {
    context.journal().journal[start..]
        .iter()
        .filter(|entry| matches!(entry, JournalEntry::AccountWarmed { .. }))
        .count() as u64
}

fn meter_account_load<DB>(
    mut context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
    instruction: fn(InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>),
) where
    DB: Database + DatabaseRef,
{
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, instruction);
    }
    let start = journal_len(context.host);
    run_stock(&mut context, instruction);
    if failed(context.interpreter) {
        return;
    }
    let cold_reads = warmed_accounts(context.host, start);
    let additional = context
        .host
        .cfg()
        .gas_params()
        .cold_account_additional_cost();
    context.host.chain_mut().record_multi_gas(
        ArbResourceKind::StorageAccessRead,
        cold_reads.saturating_mul(additional),
    );
}

fn balance<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_account_load(context, host::balance);
}

fn extcodesize<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_account_load(context, host::extcodesize);
}

fn extcodehash<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_account_load(context, host::extcodehash);
}

fn extcodecopy<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    let mut context = context;
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, host::extcodecopy);
    }
    let len = context
        .interpreter
        .stack
        .data()
        .iter()
        .rev()
        .nth(3)
        .copied()
        .unwrap_or_default()
        .saturating_to::<usize>();
    let copy_cost = context.host.cfg().gas_params().extcodecopy(len);
    let start = journal_len(context.host);
    run_stock(&mut context, host::extcodecopy);
    if failed(context.interpreter) {
        return;
    }

    let cold_reads = warmed_accounts(context.host, start);
    let additional = context
        .host
        .cfg()
        .gas_params()
        .cold_account_additional_cost();
    context.host.chain_mut().record_multi_gas(
        ArbResourceKind::StorageAccessRead,
        cold_reads
            .saturating_mul(additional)
            .saturating_add(copy_cost),
    );
}

fn sload<DB>(mut context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, host::sload);
    }
    let start = journal_len(context.host);
    run_stock(&mut context, host::sload);
    if failed(context.interpreter) {
        return;
    }
    let cold_reads = context.host.journal().journal[start..]
        .iter()
        .filter(|entry| matches!(entry, JournalEntry::StorageWarmed { .. }))
        .count() as u64;
    let additional = context
        .host
        .cfg()
        .gas_params()
        .cold_storage_additional_cost();
    context.host.chain_mut().record_multi_gas(
        ArbResourceKind::StorageAccessRead,
        cold_reads.saturating_mul(additional),
    );
}

fn sstore<DB>(mut context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, host::sstore);
    }
    let stack = context.interpreter.stack.data();
    let Some((&key, &new_value)) = stack
        .len()
        .checked_sub(2)
        .map(|index| (&stack[index + 1], &stack[index]))
    else {
        return run_stock(&mut context, host::sstore);
    };
    let target = context.interpreter.input.target_address();
    let start = journal_len(context.host);
    run_stock(&mut context, host::sstore);
    if failed(context.interpreter) {
        return;
    }

    let entries = &context.host.journal().journal[start..];
    let is_cold = entries.iter().any(|entry| {
        matches!(entry, JournalEntry::StorageWarmed { address, key: warmed }
            if *address == target && *warmed == key)
    });
    let present_value = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::StorageChanged {
                address,
                key: changed,
                had_value,
            } if *address == target && *changed == key => Some(*had_value),
            _ => None,
        })
        .unwrap_or(new_value);
    let Some(original_value) = context
        .host
        .journal()
        .state
        .get(&target)
        .and_then(|account| account.storage.get(&key))
        .map(|slot| slot.original_value())
    else {
        return;
    };

    let cost = ArbMultiGas::sstore_cost(
        context.host.cfg().gas_params(),
        original_value,
        present_value,
        new_value,
        is_cold,
    );
    context.host.chain_mut().record_multi_gas_cost(cost);
}

fn log<const N: usize, DB>(mut context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, host::log::<N, _>);
    }
    let len = context
        .interpreter
        .stack
        .data()
        .iter()
        .rev()
        .nth(1)
        .copied()
        .unwrap_or_default()
        .saturating_to::<u64>();
    run_stock(&mut context, host::log::<N, _>);
    if failed(context.interpreter) {
        return;
    }
    const LOG_TOPIC_HISTORY_GAS: u64 = 32 * 8;
    const LOG_DATA_GAS: u64 = 8;
    context.host.chain_mut().record_multi_gas(
        ArbResourceKind::HistoryGrowth,
        (N as u64)
            .saturating_mul(LOG_TOPIC_HISTORY_GAS)
            .saturating_add(len.saturating_mul(LOG_DATA_GAS)),
    );
}

fn call<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_call(context, contract::call, true);
}

fn callcode<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_call(context, contract::call_code, false);
}

fn delegatecall<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_call(context, contract::delegate_call, false);
}

fn staticcall<DB>(context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    meter_call(context, contract::static_call, false);
}

fn meter_call<DB>(
    mut context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>,
    instruction: fn(InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>),
    charges_new_account: bool,
) where
    DB: Database + DatabaseRef,
{
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, instruction);
    }
    let stack = context.interpreter.stack.data();
    let target = stack.iter().rev().nth(1).copied().map(word_to_address);
    let value = charges_new_account
        .then(|| stack.iter().rev().nth(2).copied())
        .flatten()
        .unwrap_or_default();
    let start = journal_len(context.host);
    run_stock(&mut context, instruction);
    if failed(context.interpreter) {
        return;
    }

    let cold_accounts = warmed_accounts(context.host, start);
    let gas_params = context.host.cfg().gas_params();
    let mut storage_read = cold_accounts.saturating_mul(gas_params.cold_account_additional_cost());
    let mut storage_growth = 0;
    if let Some(target) = target {
        if let Some(account) = context.host.journal().state.get(&target) {
            if account
                .info
                .code
                .as_ref()
                .and_then(|code| code.eip7702_address())
                .is_some()
            {
                storage_read = storage_read.saturating_add(gas_params.warm_storage_read_cost());
            }
            if charges_new_account && !value.is_zero() && account.is_empty() {
                storage_growth = gas_params.new_account_cost(true, true);
            }
        }
    }
    let chain = context.host.chain_mut();
    chain.record_multi_gas(ArbResourceKind::StorageAccessRead, storage_read);
    chain.record_multi_gas(ArbResourceKind::StorageGrowth, storage_growth);
}

fn selfdestruct<DB>(mut context: InstructionContext<'_, ArbitrumContext<DB>, EthInterpreter>)
where
    DB: Database + DatabaseRef,
{
    if context.host.chain().multi_gas_arbos_version().is_none() {
        return run_stock(&mut context, host::selfdestruct);
    }
    if context.interpreter.runtime_flag.is_static() {
        context
            .interpreter
            .halt(InstructionResult::StateChangeDuringStaticCall);
        return;
    }
    let target = match context.interpreter.stack.pop() {
        Ok(target) => word_to_address(target),
        Err(result) => {
            context.interpreter.halt(result);
            return;
        }
    };
    let spec = context.interpreter.runtime_flag.spec_id();
    let gas_params = context.host.cfg().gas_params().clone();
    let cold_cost = gas_params.selfdestruct_cold_cost();
    let skip_cold_load = context.interpreter.gas.remaining() < cold_cost;
    let result = match context.host.selfdestruct(
        context.interpreter.input.target_address(),
        target,
        skip_cold_load,
    ) {
        Ok(result) => result,
        Err(revm::context_interface::host::LoadError::ColdLoadSkipped) => {
            context.interpreter.halt_oog();
            return;
        }
        Err(revm::context_interface::host::LoadError::DBError) => {
            context.interpreter.halt_fatal();
            return;
        }
    };
    let topup = if spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) {
        result.data.had_value && !result.data.target_exists
    } else {
        !result.data.target_exists
    };
    if !context
        .interpreter
        .gas
        .record_cost(gas_params.selfdestruct_cost(topup, result.is_cold))
    {
        context.interpreter.halt_oog();
        return;
    }
    if result.is_cold {
        context.host.chain_mut().record_multi_gas(
            ArbResourceKind::StorageAccessRead,
            gas_params
                .cold_account_additional_cost()
                .saturating_add(gas_params.warm_storage_read_cost()),
        );
    }
    context.host.chain_mut().record_multi_gas(
        ArbResourceKind::StorageAccessWrite,
        SELFDESTRUCT_STORAGE_WRITE_GAS,
    );
    if topup {
        context.host.chain_mut().record_multi_gas(
            ArbResourceKind::StorageGrowth,
            gas_params.new_account_cost(true, true),
        );
    }
    if !result.data.previously_destroyed {
        context
            .interpreter
            .gas
            .record_refund(gas_params.selfdestruct_refund());
    }
    context.interpreter.halt(InstructionResult::SelfDestruct);
}

fn word_to_address(word: U256) -> Address {
    let bytes = word.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::TxEnv;
    use revm::primitives::{Bytes, TxKind};

    #[test]
    fn intrinsic_gas_matches_nitro_resource_split() {
        let tx = TxEnv {
            kind: TxKind::Create,
            data: Bytes::from_static(&[0, 1, 2]),
            ..Default::default()
        };
        let gas = ArbMultiGas::intrinsic(&tx, 53_050);

        assert_eq!(gas.get(ArbResourceKind::L2Calldata), 36);
        assert_eq!(gas.get(ArbResourceKind::Computation), 53_014);
        assert_eq!(gas.total(), 53_050);
    }

    #[test]
    fn sstore_gas_includes_revm_static_cost_in_the_write_resource() {
        let gas_params = GasParams::new_spec(SpecId::PRAGUE);

        let new_slot =
            ArbMultiGas::sstore_cost(&gas_params, U256::ZERO, U256::ZERO, U256::ONE, true);
        assert_eq!(new_slot.get(ArbResourceKind::StorageAccessRead), 2_100);
        assert_eq!(new_slot.get(ArbResourceKind::StorageGrowth), 20_000);
        assert_eq!(new_slot.total(), 22_100);

        let existing_slot =
            ArbMultiGas::sstore_cost(&gas_params, U256::ONE, U256::ONE, U256::from(2), false);
        assert_eq!(
            existing_slot.get(ArbResourceKind::StorageAccessWrite),
            2_900
        );
        assert_eq!(existing_slot.total(), 2_900);

        let dirty_slot =
            ArbMultiGas::sstore_cost(&gas_params, U256::ONE, U256::from(2), U256::from(3), false);
        assert_eq!(dirty_slot.get(ArbResourceKind::Computation), 100);
        assert_eq!(dirty_slot.total(), 100);
    }
}
