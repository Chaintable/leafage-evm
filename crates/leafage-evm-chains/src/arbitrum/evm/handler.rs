//! EVM handler hooks for Arbitrum Nitro transaction processing.

use super::ArbitrumEvm;
use super::multigas::{ArbMultiGas, ArbResourceKind, NUM_RESOURCE_KIND};
use super::poster_gas::ArbPosterCharge;
use crate::arbitrum::arbos_state::{self, ArbPricing};
use crate::arbitrum::precompile::{ArbitrumContext, L1_PRICER_FUNDS_POOL_ADDRESS};
use crate::arbitrum::tx::nitro_message_gas_price;
use alloy::primitives::{Address, U256};
use revm::{
    Database, DatabaseRef,
    context::{
        Block, ContextTr, LocalContextTr, Transaction,
        result::{EVMError, ExecutionResult, HaltReason},
    },
    context_interface::{
        Cfg, JournalTr, journaled_state::account::JournaledAccountTr, result::InvalidTransaction,
        transaction::TransactionType,
    },
    handler::{
        EvmTr, FrameResult, FrameTr, Handler, post_execution, pre_execution, validation,
    },
    inspector::{Inspector, InspectorHandler},
    interpreter::{Gas, InitialAndFloorGas, interpreter::EthInterpreter},
    primitives::hardfork::SpecId,
};

const ARBOS_VERSION_L1_PRICER_FUNDS_POOL: u64 = 2;
const ARBOS_VERSION_L1_FEES_AVAILABLE: u64 = 10;
const ARBOS_VERSION_PER_TX_GAS_LIMIT: u64 = 50;
const ARBOS_VERSION_MULTI_GAS: u64 = 60;
const ARBOS_VERSION_MULTI_GAS_REFUND_FIX: u64 = 61;

pub struct ArbitrumHandler<DB: Database + DatabaseRef, INSP>(core::marker::PhantomData<(DB, INSP)>);

impl<DB: Database + DatabaseRef, INSP> ArbitrumHandler<DB, INSP> {
    pub fn new() -> Self {
        Self(core::marker::PhantomData)
    }
}

impl<DB: Database + DatabaseRef, INSP> Default for ArbitrumHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB, INSP> ArbitrumHandler<DB, INSP>
where
    DB: Database + DatabaseRef,
{
    fn poster_gas(evm: &ArbitrumEvm<DB, INSP>) -> u64 {
        evm.ctx()
            .chain()
            .current_poster_charge()
            .map(|charge| charge.poster_gas)
            .unwrap_or_default()
    }

    fn l2_basefee(ctx: &ArbitrumContext<DB>) -> u128 {
        ctx.chain()
            .current_l2_basefee()
            .unwrap_or_else(|| ctx.block().basefee()) as u128
    }

    fn collect_tips(
        ctx: &mut ArbitrumContext<DB>,
    ) -> Result<bool, EVMError<<DB as Database>::Error>> {
        super::instructions::collects_tips(ctx).map_err(EVMError::Database)
    }

    fn effective_gas_price(
        ctx: &mut ArbitrumContext<DB>,
    ) -> Result<u128, EVMError<<DB as Database>::Error>> {
        let basefee = Self::l2_basefee(ctx);
        if Self::collect_tips(ctx)? {
            Ok(nitro_message_gas_price(ctx.tx(), basefee))
        } else {
            Ok(nitro_message_gas_price(ctx.tx(), basefee).min(basefee))
        }
    }

    fn paid_l1_gas_price(
        ctx: &mut ArbitrumContext<DB>,
        block_base_fee: u64,
    ) -> Result<U256, EVMError<<DB as Database>::Error>> {
        if Self::collect_tips(ctx)? {
            let price = nitro_message_gas_price(ctx.tx(), block_base_fee as u128);
            if price != 0 {
                return Ok(U256::from(price));
            }
        }
        Ok(U256::from(block_base_fee))
    }

    fn effective_balance_spending(
        tx: &impl Transaction,
        effective_gas_price: u128,
        blob_price: u128,
    ) -> Result<U256, InvalidTransaction> {
        let mut spending = (tx.gas_limit() as u128)
            .checked_mul(effective_gas_price)
            .and_then(|gas_cost| U256::from(gas_cost).checked_add(tx.value()))
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;

        if tx.tx_type() == TransactionType::Eip4844 {
            let blob_gas = tx.total_blob_gas() as u128;
            spending = spending
                .checked_add(U256::from(blob_price.saturating_mul(blob_gas)))
                .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
        }

        Ok(spending)
    }

    fn calculate_caller_fee(
        balance: U256,
        tx: &impl Transaction,
        cfg: &impl Cfg,
        effective_gas_price: u128,
        blob_price: u128,
    ) -> Result<U256, InvalidTransaction> {
        if cfg.is_fee_charge_disabled() {
            return Ok(balance);
        }

        if !cfg.is_balance_check_disabled() {
            tx.ensure_enough_balance(balance)?;
        }

        let gas_balance_spending =
            Self::effective_balance_spending(tx, effective_gas_price, blob_price)? - tx.value();
        let mut new_balance = balance.saturating_sub(gas_balance_spending);

        if cfg.is_balance_check_disabled() {
            new_balance = new_balance.max(tx.value());
        }

        Ok(new_balance)
    }

    fn validate_l2_basefee(ctx: &ArbitrumContext<DB>) -> Result<(), InvalidTransaction> {
        if ctx.cfg().is_base_fee_check_disabled() {
            return Ok(());
        }

        let basefee = Self::l2_basefee(ctx);
        if basefee == 0 {
            return Ok(());
        }

        let effective = ctx.tx().effective_gas_price(basefee);
        if effective != 0 && effective < basefee {
            return Err(InvalidTransaction::GasPriceLessThanBasefee);
        }
        Ok(())
    }

    fn l1_pricing_slot(offset: u64) -> U256 {
        let l1_pricing_key = arbos_state::child_key(&[], arbos_state::L1_PRICING_SUBSPACE);
        arbos_state::slot_at(&l1_pricing_key, offset)
    }

    fn add_to_l1_pricing_slot(
        ctx: &mut ArbitrumContext<DB>,
        offset: u64,
        delta: U256,
    ) -> Result<(), EVMError<<DB as Database>::Error>> {
        if delta.is_zero() {
            return Ok(());
        }

        let slot = Self::l1_pricing_slot(offset);
        ctx.journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)?;
        let current = ctx
            .journal_mut()
            .sload(arbos_state::ARBOS_STATE_ADDRESS, slot)?
            .data;
        ctx.journal_mut().sstore(
            arbos_state::ARBOS_STATE_ADDRESS,
            slot,
            current.saturating_add(delta),
        )?;
        Ok(())
    }

    fn gas_charging_hook(
        &self,
        evm: &mut ArbitrumEvm<DB, INSP>,
        gas_remaining: &mut u64,
        intrinsic_gas: u64,
        arbos_version: u64,
    ) -> Result<u64, EVMError<<DB as Database>::Error>> {
        let (l2_basefee, is_retryable_redeem, gas_estimation) = {
            let ctx = evm.ctx();
            (
                ctx.chain()
                    .current_l2_basefee()
                    .unwrap_or_else(|| ctx.block().basefee()),
                ctx.tx().is_retryable_redeem(),
                ctx.tx().context.gas_estimation,
            )
        };
        let pricing = if l2_basefee == 0 || is_retryable_redeem {
            None
        } else {
            ArbPricing::read_from_db(evm.ctx_mut().db_mut())?
        };
        let charge = if let Some(pricing) = pricing {
            let paid_l1_gas_price = Self::paid_l1_gas_price(evm.ctx_mut(), l2_basefee)?;
            let ctx = evm.ctx();
            pricing.gas_charging_charge(
                &ctx.tx().base,
                paid_l1_gas_price,
                gas_estimation,
            )
        } else {
            ArbPosterCharge::default()
        };

        evm.ctx_mut().chain_mut().set_current_poster_charge(charge);
        evm.ctx_mut()
            .chain_mut()
            .record_multi_gas(ArbResourceKind::SingleDim, charge.poster_gas);
        Self::add_to_l1_pricing_slot(
            evm.ctx_mut(),
            arbos_state::L1_UNITS_SINCE_UPDATE_OFFSET,
            U256::from(charge.calldata_units),
        )?;

        if *gas_remaining < charge.poster_gas {
            return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                initial_gas: intrinsic_gas.saturating_add(charge.poster_gas),
                gas_limit: evm.ctx().tx().gas_limit(),
            }
            .into());
        }

        *gas_remaining -= charge.poster_gas;

        if !gas_estimation {
            return Ok(0);
        }

        let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
        let limit_offset = if arbos_version < ARBOS_VERSION_PER_TX_GAS_LIMIT {
            arbos_state::L2_PER_BLOCK_GAS_LIMIT_OFFSET
        } else {
            arbos_state::L2_PER_TX_GAS_LIMIT_OFFSET
        };
        let mut computation_limit = evm
            .ctx_mut()
            .db_mut()
            .storage(
                arbos_state::ARBOS_STATE_ADDRESS,
                arbos_state::slot_at(&l2_pricing_key, limit_offset),
            )?
            .wrapping_to::<u64>();
        if arbos_version >= ARBOS_VERSION_PER_TX_GAS_LIMIT {
            computation_limit = computation_limit.saturating_sub(intrinsic_gas);
        }

        let held_gas = gas_remaining.saturating_sub(computation_limit);
        *gas_remaining = (*gas_remaining).min(computation_limit);
        Ok(held_gas)
    }

    fn read_arbos_value(
        ctx: &mut ArbitrumContext<DB>,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<U256, EVMError<<DB as Database>::Error>> {
        ctx.journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)?;
        Self::read_loaded_arbos_value(ctx, storage_key, offset)
    }

    fn read_loaded_arbos_value(
        ctx: &mut ArbitrumContext<DB>,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<U256, EVMError<<DB as Database>::Error>> {
        ctx.journal_mut()
            .sload(
                arbos_state::ARBOS_STATE_ADDRESS,
                arbos_state::slot_at(storage_key, offset),
            )
            .map(|value| value.data)
            .map_err(Into::into)
    }

    fn network_fee_account(
        ctx: &mut ArbitrumContext<DB>,
    ) -> Result<Address, EVMError<<DB as Database>::Error>> {
        let value = Self::read_arbos_value(ctx, &[], arbos_state::NETWORK_FEE_ACCOUNT_OFFSET)?;
        let bytes = value.to_be_bytes::<32>();
        Ok(Address::from_slice(&bytes[12..]))
    }

    fn multi_gas_price(
        ctx: &mut ArbitrumContext<DB>,
    ) -> Result<Option<U256>, EVMError<<DB as Database>::Error>> {
        let Some(arbos_version) = ctx.chain().multi_gas_arbos_version() else {
            return Ok(None);
        };
        if arbos_version < ARBOS_VERSION_MULTI_GAS {
            return Ok(None);
        }
        ctx.journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)?;

        let l2_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
        if arbos_version >= ARBOS_VERSION_MULTI_GAS_REFUND_FIX {
            let constraints_key = arbos_state::child_key(&l2_key, &[1]);
            if Self::read_loaded_arbos_value(ctx, &constraints_key, 0)?.is_zero() {
                return Ok(None);
            }
        }

        let block_basefee = if arbos_version >= ARBOS_VERSION_MULTI_GAS_REFUND_FIX {
            U256::from(Self::l2_basefee(ctx))
        } else {
            Self::read_loaded_arbos_value(ctx, &l2_key, arbos_state::L2_BASE_FEE_WEI_OFFSET)?
        };
        let fees_key = arbos_state::child_key(&l2_key, &[2]);
        let resources = *ctx.chain().multi_gas().resources();
        let mut total = U256::ZERO;
        for (resource, amount) in resources.into_iter().enumerate() {
            if amount == 0 {
                continue;
            }
            let resource_fee = Self::read_loaded_arbos_value(
                ctx,
                &fees_key,
                NUM_RESOURCE_KIND as u64 + resource as u64,
            )?;
            let fee = if resource == ArbResourceKind::SingleDim as usize
                || resource_fee.is_zero()
            {
                block_basefee
            } else {
                resource_fee
            };
            total = total.saturating_add(fee.saturating_mul(U256::from(amount)));
        }
        Ok(Some(total))
    }

    fn apply_multi_gas_refund(
        evm: &mut ArbitrumEvm<DB, INSP>,
        exec_result: &FrameResult,
        network_fee_account: alloy::primitives::Address,
    ) -> Result<(), EVMError<<DB as Database>::Error>> {
        // Retryable redeems route refunds through their L1 deposit accounting.
        // ArbitrumTxEnv intentionally does not carry MaxRefund, so applying the
        // normal sender refund here would be observably wrong.
        if evm.ctx().tx().is_retryable_redeem() {
            return Ok(());
        }
        let Some(multi_gas_cost) = Self::multi_gas_price(evm.ctx_mut())? else {
            return Ok(());
        };
        let basefee = U256::from(Self::l2_basefee(evm.ctx()));
        let single_gas_cost = basefee.saturating_mul(U256::from(exec_result.gas().used()));
        if single_gas_cost <= multi_gas_cost {
            return Ok(());
        }
        let refund = single_gas_cost - multi_gas_cost;
        let caller = evm.ctx().tx().caller();
        if !evm
            .ctx_mut()
            .journal_mut()
            .load_account_mut(network_fee_account)?
            .decr_balance(refund)
        {
            return Ok(());
        }
        evm.ctx_mut()
            .journal_mut()
            .load_account_mut(caller)?
            .incr_balance(refund);
        Ok(())
    }

    fn finish_frame_result(
        evm: &mut ArbitrumEvm<DB, INSP>,
        frame_result: &mut FrameResult,
        held_gas: u64,
    ) {
        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let execution_remaining = gas.remaining();
        let refunded = gas.refunded();

        *gas = Gas::new_spent(evm.ctx().tx().gas_limit());
        gas.erase_cost(held_gas);

        if instruction_result.is_ok_or_revert() {
            gas.erase_cost(execution_remaining);
        }

        if instruction_result.is_ok() {
            gas.record_refund(refunded);
        }
    }

    fn start_tx_hook(evm: &mut ArbitrumEvm<DB, INSP>, intrinsic_gas: u64) -> u64 {
        evm.ctx_mut().chain_mut().clear_open_contract_frames();
        evm.ctx_mut().chain_mut().clear_current_poster_charge();

        let arbos_version = evm.ctx().chain().current_arbos_version();
        let intrinsic = ArbMultiGas::intrinsic(evm.ctx().tx(), intrinsic_gas);
        evm.ctx_mut()
            .chain_mut()
            .begin_multi_gas(arbos_version, intrinsic);
        arbos_version
    }
}

impl<DB, INSP> Handler for ArbitrumHandler<DB, INSP>
where
    DB: Database + DatabaseRef,
{
    type Evm = ArbitrumEvm<DB, INSP>;
    type Error = EVMError<<DB as Database>::Error>;
    type HaltReason = HaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        validation::validate_env::<_, Self::Error>(evm.ctx())?;
        Self::validate_l2_basefee(evm.ctx())?;
        Ok(())
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        let effective_gas_price = Self::effective_gas_price(evm.ctx_mut())?;
        let blob_price = evm.ctx().block().blob_gasprice().unwrap_or_default();
        let (block, tx, cfg, journal, _, _) = evm.ctx_mut().all_mut();

        let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;
        pre_execution::validate_account_nonce_and_code_with_components(
            &caller.account().info,
            tx,
            cfg,
        )?;

        let new_balance = Self::calculate_caller_fee(
            *caller.balance(),
            tx,
            cfg,
            effective_gas_price,
            block.blob_gasprice().unwrap_or(blob_price),
        )?;

        caller.set_balance(new_balance);
        if tx.kind().is_call() {
            caller.bump_nonce();
        }
        Ok(())
    }

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let arbos_version = Self::start_tx_hook(evm, init_and_floor_gas.initial_gas);

        let mut gas_limit = evm
            .ctx()
            .tx()
            .gas_limit()
            .saturating_sub(init_and_floor_gas.initial_gas);
        let held_gas = self.gas_charging_hook(
            evm,
            &mut gas_limit,
            init_and_floor_gas.initial_gas,
            arbos_version,
        )?;

        let first_frame_input = self.first_frame_input(evm, gas_limit)?;
        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;
        Self::finish_frame_result(evm, &mut frame_result, held_gas);
        Ok(frame_result)
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        Self::finish_frame_result(evm, frame_result, 0);
        Ok(())
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) {
        let spec: SpecId = (*evm.ctx().cfg().spec()).into();
        let gas = exec_result.gas_mut();
        gas.record_refund(eip7702_refund);

        let max_refund_quotient = if spec.is_enabled_in(SpecId::LONDON) {
            5
        } else {
            2
        };
        let refundable_spent = gas.spent().saturating_sub(Self::poster_gas(evm));
        let max_refund = refundable_spent / max_refund_quotient;
        let refund = (gas.refunded() as u64).min(max_refund);
        gas.set_refund(refund as i64);
    }

    fn eip7623_check_gas_floor(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
    ) {
        let gas = exec_result.gas();
        let gross_gas_used = gas.spent();
        let net_gas_used = gas.spent_sub_refunded();
        let refund = gas.refunded() as u64;
        evm.ctx_mut()
            .chain_mut()
            .finalize_multi_gas(gross_gas_used);
        let used_multi_gas = evm.ctx().chain().multi_gas().single_gas(refund);
        if net_gas_used < init_and_floor_gas.floor_gas {
            evm.ctx_mut().chain_mut().record_multi_gas(
                ArbResourceKind::L2Calldata,
                init_and_floor_gas.floor_gas.saturating_sub(used_multi_gas),
            );
        }
        post_execution::eip7623_check_gas_floor(exec_result.gas_mut(), init_and_floor_gas);
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if evm.ctx().cfg().is_fee_charge_disabled() {
            return Ok(());
        }

        let effective_gas_price = Self::effective_gas_price(evm.ctx_mut())?;
        let gas = exec_result.gas();
        let refund_gas = gas.remaining().saturating_add(gas.refunded() as u64);
        let refund = U256::from(effective_gas_price.saturating_mul(refund_gas as u128));
        let caller = evm.ctx().tx().caller();

        evm.ctx_mut()
            .journal_mut()
            .load_account_mut(caller)?
            .incr_balance(refund);
        Ok(())
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        if evm.ctx().cfg().is_fee_charge_disabled() {
            return Ok(());
        }

        let effective_gas_price = Self::effective_gas_price(evm.ctx_mut())?;
        let arbos_version = evm.ctx().chain().current_arbos_version();
        let poster_gas = Self::poster_gas(evm);
        let compute_gas = exec_result.gas().used().saturating_sub(poster_gas);
        let compute_fee = U256::from(effective_gas_price.saturating_mul(compute_gas as u128));
        let poster_fee = if effective_gas_price == 0 {
            U256::ZERO
        } else {
            evm.ctx()
                .chain()
                .current_poster_charge()
                .map(|charge| charge.poster_fee)
                .unwrap_or_default()
        };

        let beneficiary = Self::network_fee_account(evm.ctx_mut())?;
        evm.ctx_mut()
            .journal_mut()
            .load_account_mut(beneficiary)?
            .incr_balance(compute_fee);

        if !poster_fee.is_zero() {
            let poster_fee_destination = if arbos_version < ARBOS_VERSION_L1_PRICER_FUNDS_POOL {
                evm.ctx().block().beneficiary()
            } else {
                L1_PRICER_FUNDS_POOL_ADDRESS
            };
            evm.ctx_mut()
                .journal_mut()
                .load_account_mut(poster_fee_destination)?
                .incr_balance(poster_fee);
        }
        if arbos_version >= ARBOS_VERSION_L1_FEES_AVAILABLE {
            Self::add_to_l1_pricing_slot(
                evm.ctx_mut(),
                arbos_state::L1_FEES_AVAILABLE_OFFSET,
                poster_fee,
            )?;
        }
        Self::apply_multi_gas_refund(evm, exec_result, beneficiary)?;
        Ok(())
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        evm.ctx_mut().chain_mut().clear_open_contract_frames();
        evm.ctx_mut().chain_mut().clear_multi_gas();
        evm.ctx_mut().local_mut().clear();
        evm.ctx_mut().journal_mut().discard_tx();
        evm.frame_stack().clear();
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::precompile::ArbitrumPrecompileEnv;
    use crate::arbitrum::precompile::BATCH_POSTER_ADDRESS;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, B256, Bytes};
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::MainContext;
    use revm::context::{Context, TxEnv};
    use revm::database::{EmptyDB, in_memory_db::CacheDB};

    type TestDb = CacheDB<EmptyDB>;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ExpectedDbError;

    impl core::fmt::Display for ExpectedDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("expected database error")
        }
    }

    impl std::error::Error for ExpectedDbError {}
    impl revm::database_interface::DBErrorMarker for ExpectedDbError {}

    #[derive(Clone)]
    struct FailingArbosStorage {
        slot: U256,
    }

    impl DatabaseRef for FailingArbosStorage {
        type Error = ExpectedDbError;

        fn basic_ref(
            &self,
            address: Address,
        ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok((address == arbos_state::ARBOS_STATE_ADDRESS).then(Default::default))
        }

        fn code_by_hash_ref(&self, _: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
            Ok(Default::default())
        }

        fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == arbos_state::ARBOS_STATE_ADDRESS && index == self.slot {
                return Err(ExpectedDbError);
            }
            Ok(U256::ZERO)
        }

        fn block_hash_ref(&self, _: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    fn context() -> ArbitrumContext<TestDb> {
        Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default())
    }

    fn context_with_collect_tips(beneficiary: Address) -> ArbitrumContext<TestDb> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::ARBOS_VERSION_OFFSET),
            U256::from(60),
        )
        .expect("write ArbOS version");
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::COLLECT_TIPS_OFFSET),
            U256::ONE,
        )
        .expect("write collectTips");

        let mut execution_context = ArbitrumExecutionContext::default();
        execution_context.set_current_arbos_version(60);
        Context::mainnet()
            .with_tx(ArbitrumTxEnv::new(
                TxEnv {
                    gas_price: 200,
                    ..Default::default()
                },
                Default::default(),
            ))
            .with_block(BlockEnv {
                beneficiary,
                basefee: 100,
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(execution_context)
    }

    fn db_with_pricing() -> TestDb {
        let mut db = CacheDB::new(EmptyDB::default());
        let l1_pricing_key = arbos_state::child_key(&[], arbos_state::L1_PRICING_SUBSPACE);
        let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::ARBOS_VERSION_OFFSET),
            U256::from(51),
        )
        .expect("write ArbOS version");
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&l2_pricing_key, arbos_state::L2_PER_TX_GAS_LIMIT_OFFSET),
            U256::from(32_000_000u64),
        )
        .expect("write per-transaction gas limit");
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&l1_pricing_key, arbos_state::L1_PRICE_PER_UNIT_OFFSET),
            U256::from(1_000u64),
        )
        .expect("write L1 price per unit");
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&l2_pricing_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET),
            U256::ONE,
        )
        .expect("write L2 minimum base fee");
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&[], arbos_state::BROTLI_COMPRESSION_LEVEL_OFFSET),
            U256::ZERO,
        )
        .expect("write brotli compression level");
        db
    }

    fn evm_with_tx(tx: ArbitrumTxEnv) -> ArbitrumEvm<TestDb, ()> {
        let mut execution_context = ArbitrumExecutionContext::default();
        execution_context.set_current_l2_context(U256::ZERO, 100);
        let mut evm = ArbitrumEvm::new(
            BlockEnv {
                basefee: 100,
                gas_limit: 1_000_000,
                ..Default::default()
            },
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            db_with_pricing(),
            (),
            ArbitrumPrecompileEnv {
                current_arbos_version: 51,
                ..Default::default()
            },
            execution_context,
        );
        evm.inner.ctx.tx = tx;
        evm
    }

    fn read_l1_pricing_slot(ctx: &mut ArbitrumContext<TestDb>, offset: u64) -> U256 {
        let slot = ArbitrumHandler::<TestDb, ()>::l1_pricing_slot(offset);
        ctx.journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        ctx.journal_mut()
            .sload(arbos_state::ARBOS_STATE_ADDRESS, slot)
            .expect("read ArbOS L1 pricing slot")
            .data
    }

    #[test]
    fn add_to_l1_pricing_slot_loads_account_and_accumulates() {
        let mut ctx = context();

        ArbitrumHandler::<TestDb, ()>::add_to_l1_pricing_slot(
            &mut ctx,
            arbos_state::L1_UNITS_SINCE_UPDATE_OFFSET,
            U256::from(7),
        )
        .expect("add initial units");
        ArbitrumHandler::<TestDb, ()>::add_to_l1_pricing_slot(
            &mut ctx,
            arbos_state::L1_UNITS_SINCE_UPDATE_OFFSET,
            U256::from(5),
        )
        .expect("add more units");

        assert_eq!(
            read_l1_pricing_slot(&mut ctx, arbos_state::L1_UNITS_SINCE_UPDATE_OFFSET),
            U256::from(12)
        );
    }

    #[test]
    fn delayed_message_blocks_do_not_collect_tips() {
        let mut batch_poster = context_with_collect_tips(BATCH_POSTER_ADDRESS);
        assert!(ArbitrumHandler::<TestDb, ()>::collect_tips(&mut batch_poster).unwrap());
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::effective_gas_price(&mut batch_poster).unwrap(),
            200
        );
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::paid_l1_gas_price(&mut batch_poster, 100).unwrap(),
            U256::from(200)
        );

        let mut delayed = context_with_collect_tips(Address::with_last_byte(0x01));
        assert!(!ArbitrumHandler::<TestDb, ()>::collect_tips(&mut delayed).unwrap());
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::effective_gas_price(&mut delayed).unwrap(),
            100
        );
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::paid_l1_gas_price(&mut delayed, 100).unwrap(),
            U256::from(100)
        );
    }

    #[test]
    fn collect_tips_defaults_absent_priority_fee_to_zero() {
        let mut batch_poster = context_with_collect_tips(BATCH_POSTER_ADDRESS);
        batch_poster.tx = ArbitrumTxEnv::new(
            TxEnv {
                tx_type: TransactionType::Eip1559 as u8,
                gas_price: 250,
                gas_priority_fee: None,
                ..Default::default()
            },
            Default::default(),
        );

        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::effective_gas_price(&mut batch_poster).unwrap(),
            100
        );
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::paid_l1_gas_price(&mut batch_poster, 100).unwrap(),
            U256::from(100)
        );
    }

    #[test]
    fn collect_tips_error_aborts_transaction() {
        use revm::ExecuteEvm;
        use revm::state::AccountInfo;

        let caller = Address::with_last_byte(0xc0);
        let mut db = CacheDB::new(FailingArbosStorage {
            slot: arbos_state::slot_at(&[], arbos_state::COLLECT_TIPS_OFFSET),
        });
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10u128.pow(18)),
                ..Default::default()
            },
        );
        let mut evm = ArbitrumEvm::new(
            BlockEnv {
                beneficiary: BATCH_POSTER_ADDRESS,
                gas_limit: 1_000_000,
                ..Default::default()
            },
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            db,
            (),
            ArbitrumPrecompileEnv {
                current_arbos_version: 60,
                ..Default::default()
            },
            ArbitrumExecutionContext::default(),
        );

        let error = evm
            .transact(ArbitrumTxEnv::new(
                TxEnv {
                    caller,
                    gas_limit: 100_000,
                    kind: revm::primitives::TxKind::Call(Address::with_last_byte(0xee)),
                    ..Default::default()
                },
                Default::default(),
            ))
            .expect_err("collectTips failure must abort execution");
        assert!(matches!(error, EVMError::Database(ExpectedDbError)));
    }

    #[test]
    fn network_fee_account_error_aborts_transaction() {
        use revm::ExecuteEvm;
        use revm::state::AccountInfo;

        let caller = Address::with_last_byte(0xc1);
        let mut db = CacheDB::new(FailingArbosStorage {
            slot: arbos_state::slot_at(&[], arbos_state::NETWORK_FEE_ACCOUNT_OFFSET),
        });
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10u128.pow(18)),
                ..Default::default()
            },
        );
        let mut evm = ArbitrumEvm::new(
            BlockEnv {
                gas_limit: 1_000_000,
                ..Default::default()
            },
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            db,
            (),
            ArbitrumPrecompileEnv {
                current_arbos_version: 40,
                ..Default::default()
            },
            ArbitrumExecutionContext::default(),
        );

        let error = evm
            .transact(ArbitrumTxEnv::new(
                TxEnv {
                    caller,
                    gas_limit: 100_000,
                    kind: revm::primitives::TxKind::Call(Address::with_last_byte(0xee)),
                    ..Default::default()
                },
                Default::default(),
            ))
            .expect_err("network fee account failure must abort execution");
        assert!(matches!(error, EVMError::Database(ExpectedDbError)));
    }

    #[test]
    fn retryable_redeem_skips_l1_poster_charge_even_with_nonzero_gas_price() {
        let handler = ArbitrumHandler::<TestDb, ()>::new();
        let data = Bytes::from(vec![0xab; 100]);
        let normal_tx = ArbitrumTxEnv::new(
            TxEnv {
                gas_limit: 1_000_000,
                gas_price: 100,
                data: data.clone(),
                ..Default::default()
            },
            Default::default(),
        );
        let mut normal_evm = evm_with_tx(normal_tx);
        let mut normal_gas_remaining = 900_000;

        handler
            .gas_charging_hook(&mut normal_evm, &mut normal_gas_remaining, 21_000, 51)
            .expect("normal transaction poster gas should be chargeable");
        assert!(
            normal_evm
                .ctx()
                .chain()
                .current_poster_charge()
                .expect("normal poster charge should be recorded")
                .poster_gas
                > 0
        );

        let retryable_tx = ArbitrumTxEnv::retryable_redeem(
            TxEnv {
                gas_limit: 1_000_000,
                gas_price: 100,
                data,
                ..Default::default()
            },
            Some(B256::with_last_byte(1)),
            Address::with_last_byte(2),
            Default::default(),
        );
        let mut retryable_evm = evm_with_tx(retryable_tx);
        let mut retryable_gas_remaining = 900_000;

        handler
            .gas_charging_hook(&mut retryable_evm, &mut retryable_gas_remaining, 21_000, 51)
            .expect("retryable redeem should not charge L1 poster gas");
        assert_eq!(
            retryable_evm
                .ctx()
                .chain()
                .current_poster_charge()
                .expect("retryable poster charge should be recorded")
                .poster_gas,
            0
        );
        assert_eq!(retryable_gas_remaining, 900_000);
    }

    /// The tx-level gas-estimation flag switches poster charging between
    /// nitro's padded estimation mode and the unpadded call mode.
    #[test]
    fn gas_charging_hook_pads_only_estimation_runs() {
        use crate::arbitrum::tx::ArbitrumTxContext;

        let handler = ArbitrumHandler::<TestDb, ()>::new();
        let base = TxEnv {
            gas_limit: 1_000_000,
            gas_price: 100,
            data: Bytes::from(vec![0xab; 100]),
            ..Default::default()
        };

        let poster_gas_for = |gas_estimation: bool| {
            let tx = ArbitrumTxEnv::new(
                base.clone(),
                ArbitrumTxContext {
                    gas_estimation,
                    ..Default::default()
                },
            );
            let mut evm = evm_with_tx(tx);
            let mut gas_remaining = 900_000;
            handler
                .gas_charging_hook(&mut evm, &mut gas_remaining, 21_000, 51)
                .expect("poster gas should be chargeable");
            evm.ctx()
                .chain()
                .current_poster_charge()
                .expect("poster charge should be recorded")
                .poster_gas
        };

        let call_gas = poster_gas_for(false);
        let estimation_gas = poster_gas_for(true);
        assert!(call_gas > 0);
        assert_ne!(
            estimation_gas, call_gas,
            "estimation padding must not leak into call runs"
        );
    }

    #[test]
    fn gas_estimation_caps_computation_by_arbos_version() {
        use crate::arbitrum::tx::ArbitrumTxContext;

        let handler = ArbitrumHandler::<TestDb, ()>::new();
        let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
        let cases = [
            (
                49,
                arbos_state::L2_PER_BLOCK_GAS_LIMIT_OFFSET,
                80_000,
                80_000,
            ),
            (51, arbos_state::L2_PER_TX_GAS_LIMIT_OFFSET, 100_000, 79_000),
        ];

        for (version, offset, stored_limit, expected_execution_limit) in cases {
            let tx = ArbitrumTxEnv::new(
                TxEnv {
                    gas_limit: 500_000,
                    gas_price: 100,
                    ..Default::default()
                },
                ArbitrumTxContext {
                    gas_estimation: true,
                    ..Default::default()
                },
            );
            let mut evm = evm_with_tx(tx);
            evm.ctx_mut()
                .db_mut()
                .insert_account_storage(
                    arbos_state::ARBOS_STATE_ADDRESS,
                    arbos_state::slot_at(&[], arbos_state::ARBOS_VERSION_OFFSET),
                    U256::from(version),
                )
                .expect("write ArbOS version");
            evm.ctx_mut()
                .db_mut()
                .insert_account_storage(
                    arbos_state::ARBOS_STATE_ADDRESS,
                    arbos_state::slot_at(&l2_pricing_key, offset),
                    U256::from(stored_limit),
                )
                .expect("write computation gas limit");

            let mut gas_remaining = 479_000;
            let held = handler
                .gas_charging_hook(&mut evm, &mut gas_remaining, 21_000, version)
                .expect("gas charging hook");
            let poster = ArbitrumHandler::<TestDb, ()>::poster_gas(&evm);

            assert_eq!(gas_remaining, expected_execution_limit);
            assert_eq!(held + gas_remaining + poster, 479_000);
        }

        let tx = ArbitrumTxEnv::new(
            TxEnv {
                gas_limit: 500_000,
                gas_price: 100,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let mut evm = evm_with_tx(tx);
        let mut gas_remaining = 479_000;
        let held = handler
            .gas_charging_hook(&mut evm, &mut gas_remaining, 21_000, 51)
            .expect("call-mode gas charging hook");
        assert_eq!(held, 0);
        assert_eq!(
            gas_remaining + ArbitrumHandler::<TestDb, ()>::poster_gas(&evm),
            479_000
        );
    }

    #[test]
    fn multi_gas_price_applies_arbos_60_and_61_fork_rules() {
        let l2_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
        let constraints_key = arbos_state::child_key(&l2_key, &[1]);
        let make_context = |arbos_version, constraints_len| {
            let mut ctx = context();
            ctx.chain_mut().set_current_l2_context(U256::ZERO, 11);
            ctx.db_mut()
                .insert_account_storage(
                    arbos_state::ARBOS_STATE_ADDRESS,
                    arbos_state::slot_at(&l2_key, arbos_state::L2_BASE_FEE_WEI_OFFSET),
                    U256::from(7),
                )
                .expect("write stored L2 base fee");
            ctx.db_mut()
                .insert_account_storage(
                    arbos_state::ARBOS_STATE_ADDRESS,
                    arbos_state::slot_at(&constraints_key, 0),
                    U256::from(constraints_len),
                )
                .expect("write constraint count");
            ctx.chain_mut()
                .begin_multi_gas(arbos_version, Default::default());
            ctx.chain_mut()
                .record_multi_gas(ArbResourceKind::Computation, 2);
            ctx
        };

        let mut arbos_60 = make_context(60, 0);
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::multi_gas_price(&mut arbos_60)
                .expect("price ArbOS 60 resources"),
            Some(U256::from(14))
        );

        let mut arbos_61_without_constraints = make_context(61, 0);
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::multi_gas_price(
                &mut arbos_61_without_constraints,
            )
            .expect("price unconstrained ArbOS 61 resources"),
            None
        );

        let mut arbos_61_with_constraints = make_context(61, 1);
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::multi_gas_price(
                &mut arbos_61_with_constraints,
            )
            .expect("price constrained ArbOS 61 resources"),
            Some(U256::from(22))
        );
    }

    #[test]
    fn computation_hold_gas_is_returned_after_execution() {
        use crate::arbitrum::tx::ArbitrumTxContext;
        use revm::ExecuteEvm;
        use revm::state::AccountInfo;

        let caller = Address::with_last_byte(0xc2);
        let mut db = db_with_pricing();
        let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            arbos_state::slot_at(&l2_pricing_key, arbos_state::L2_PER_TX_GAS_LIMIT_OFFSET),
            U256::from(100_000u64),
        )
        .expect("write low per-transaction gas limit");
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10u128.pow(18)),
                ..Default::default()
            },
        );

        let mut execution_context = ArbitrumExecutionContext::default();
        execution_context.set_current_l2_context(U256::ZERO, 100);
        let mut evm = ArbitrumEvm::new(
            BlockEnv {
                basefee: 100,
                gas_limit: 1_000_000,
                ..Default::default()
            },
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            db,
            (),
            ArbitrumPrecompileEnv {
                current_arbos_version: 51,
                ..Default::default()
            },
            execution_context,
        );
        let result = evm
            .transact(ArbitrumTxEnv::new(
                TxEnv {
                    caller,
                    kind: revm::primitives::TxKind::Call(Address::with_last_byte(0xee)),
                    gas_limit: 500_000,
                    gas_price: 100,
                    ..Default::default()
                },
                ArbitrumTxContext {
                    gas_estimation: true,
                    ..Default::default()
                },
            ))
            .expect("gas-estimation execution");
        let poster = ArbitrumHandler::<TestDb, ()>::poster_gas(&evm);

        assert!(result.result.is_success());
        assert_eq!(result.result.gas_used(), 21_000 + poster);
    }

    /// Pins the `inspect_execution` override: both execution paths must run the
    /// same Arbitrum transaction hooks for an identical transaction.
    #[test]
    fn inspected_execution_runs_arbitrum_tx_hooks_like_transact() {
        use revm::inspector::NoOpInspector;
        use revm::primitives::TxKind;
        use revm::state::AccountInfo;
        use revm::{ExecuteEvm, InspectEvm};

        let caller = Address::with_last_byte(0xc1);
        let make_tx = || {
            ArbitrumTxEnv::new(
                TxEnv {
                    caller,
                    kind: TxKind::Call(Address::with_last_byte(0xee)),
                    gas_limit: 1_000_000,
                    gas_price: 100,
                    data: Bytes::from(vec![0xab; 100]),
                    ..Default::default()
                },
                Default::default(),
            )
        };
        let funded_db = || {
            let mut db = db_with_pricing();
            let l2_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
            db.insert_account_storage(
                arbos_state::ARBOS_STATE_ADDRESS,
                arbos_state::slot_at(&[], arbos_state::ARBOS_VERSION_OFFSET),
                U256::from(60),
            )
            .expect("write ArbOS version");
            db.insert_account_storage(
                arbos_state::ARBOS_STATE_ADDRESS,
                arbos_state::slot_at(&l2_key, arbos_state::L2_BASE_FEE_WEI_OFFSET),
                U256::from(100),
            )
            .expect("write L2 base fee");
            db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::from(10u128.pow(18)),
                    ..Default::default()
                },
            );
            db
        };
        let block_env = || BlockEnv {
            basefee: 100,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let execution_context = || {
            let mut context = ArbitrumExecutionContext::default();
            context.set_current_l2_context(U256::ZERO, 100);
            context
        };

        let mut transact_evm = ArbitrumEvm::new(
            block_env(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            funded_db(),
            NoOpInspector {},
            ArbitrumPrecompileEnv {
                current_arbos_version: 60,
                ..Default::default()
            },
            execution_context(),
        );
        let transact_result = transact_evm.transact(make_tx()).expect("transact");
        let transact_poster = transact_evm
            .ctx()
            .chain()
            .current_poster_charge()
            .expect("transact path records the poster charge")
            .poster_gas;
        let transact_multi_gas = transact_evm.ctx().chain().multi_gas();
        assert!(transact_poster > 0);
        assert_eq!(
            transact_evm.ctx().chain().multi_gas_arbos_version(),
            Some(60)
        );

        let mut inspect_evm = ArbitrumEvm::new(
            block_env(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            funded_db(),
            NoOpInspector {},
            ArbitrumPrecompileEnv {
                current_arbos_version: 60,
                ..Default::default()
            },
            execution_context(),
        );
        let inspect_result = inspect_evm.inspect_one_tx(make_tx()).expect("inspect");
        let inspect_poster = inspect_evm
            .ctx()
            .chain()
            .current_poster_charge()
            .expect("inspected path records the poster charge")
            .poster_gas;
        let inspect_multi_gas = inspect_evm.ctx().chain().multi_gas();

        assert_eq!(inspect_poster, transact_poster);
        assert_eq!(inspect_multi_gas, transact_multi_gas);
        assert_eq!(inspect_result.gas_used(), transact_result.result.gas_used());
    }

    #[test]
    fn calldata_floor_uses_classified_multi_gas_after_refund() {
        use revm::interpreter::{CallOutcome, InstructionResult, InterpreterResult};

        let mut evm = evm_with_tx(ArbitrumTxEnv::default());
        evm.ctx_mut()
            .chain_mut()
            .begin_multi_gas(60, ArbMultiGas::default());
        evm.ctx_mut()
            .chain_mut()
            .record_multi_gas(ArbResourceKind::Computation, 80);
        evm.ctx_mut()
            .chain_mut()
            .leave_multi_gas_unattributed(10);

        let mut gas = Gas::new(100);
        assert!(gas.record_cost(100));
        gas.record_refund(20);
        let mut result = FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Return, Bytes::new(), gas),
            0..0,
        ));

        ArbitrumHandler::<TestDb, ()>::new().eip7623_check_gas_floor(
            &mut evm,
            &mut result,
            InitialAndFloorGas::new(0, 100),
        );

        let multi_gas = evm.ctx().chain().multi_gas();
        assert_eq!(multi_gas.get(ArbResourceKind::Computation), 90);
        assert_eq!(multi_gas.get(ArbResourceKind::L2Calldata), 30);
        assert_eq!(multi_gas.single_gas(20), 100);
    }

    #[test]
    fn calldata_floor_condition_uses_transaction_net_gas() {
        use revm::interpreter::{CallOutcome, InstructionResult, InterpreterResult};

        let mut evm = evm_with_tx(ArbitrumTxEnv::default());
        evm.ctx_mut()
            .chain_mut()
            .begin_multi_gas(60, ArbMultiGas::default());
        evm.ctx_mut()
            .chain_mut()
            .record_multi_gas(ArbResourceKind::Computation, 80);
        evm.ctx_mut()
            .chain_mut()
            .leave_multi_gas_unattributed(10);

        let mut gas = Gas::new(100);
        assert!(gas.record_cost(100));
        let mut result = FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Return, Bytes::new(), gas),
            0..0,
        ));

        ArbitrumHandler::<TestDb, ()>::new().eip7623_check_gas_floor(
            &mut evm,
            &mut result,
            InitialAndFloorGas::new(0, 100),
        );

        let multi_gas = evm.ctx().chain().multi_gas();
        assert_eq!(multi_gas.get(ArbResourceKind::Computation), 90);
        assert_eq!(multi_gas.get(ArbResourceKind::L2Calldata), 0);
    }

    #[test]
    fn calldata_floor_does_not_underflow_when_classification_is_over_attributed() {
        use revm::interpreter::{CallOutcome, InstructionResult, InterpreterResult};

        let mut evm = evm_with_tx(ArbitrumTxEnv::default());
        evm.ctx_mut()
            .chain_mut()
            .begin_multi_gas(60, ArbMultiGas::default());
        evm.ctx_mut()
            .chain_mut()
            .record_multi_gas(ArbResourceKind::Computation, 120);

        let mut gas = Gas::new(100);
        assert!(gas.record_cost(100));
        let mut result = FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Return, Bytes::new(), gas),
            0..0,
        ));

        ArbitrumHandler::<TestDb, ()>::new().eip7623_check_gas_floor(
            &mut evm,
            &mut result,
            InitialAndFloorGas::new(0, 110),
        );

        assert_eq!(
            evm.ctx()
                .chain()
                .multi_gas()
                .get(ArbResourceKind::L2Calldata),
            0
        );
    }

    #[test]
    fn catch_error_clears_open_contract_frame_counts() {
        let address = Address::with_last_byte(0x42);
        let mut evm = evm_with_tx(ArbitrumTxEnv::default());
        evm.ctx_mut().chain_mut().enter_contract_frame(address);
        evm.ctx_mut()
            .chain_mut()
            .begin_multi_gas(60, Default::default());
        evm.ctx_mut()
            .chain_mut()
            .record_multi_gas(ArbResourceKind::Computation, 1);
        assert_eq!(evm.ctx().chain().open_contract_frame_count(address), 1);

        let error: EVMError<<TestDb as Database>::Error> =
            EVMError::Custom("synthetic execution error".to_owned());
        let result = ArbitrumHandler::<TestDb, ()>::new().catch_error(&mut evm, error);

        assert!(
            matches!(result, Err(EVMError::Custom(message)) if message == "synthetic execution error")
        );
        assert_eq!(evm.ctx().chain().open_contract_frame_count(address), 0);
        assert_eq!(evm.ctx().chain().multi_gas_arbos_version(), None);
        assert_eq!(evm.ctx().chain().multi_gas().total(), 0);
    }
}

impl<DB, INSP> InspectorHandler for ArbitrumHandler<DB, INSP>
where
    DB: Database + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type IT = EthInterpreter;

    /// Mirrors this handler's `execution` override on the inspected path.
    /// `inspect_run` calls `inspect_execution`, not `Handler::execution`, so
    /// without this override every traced run (pre_traceCall / pre_traceMany /
    /// simulateTransactions) would skip nitro's L1 poster-gas charging that
    /// the untraced path applies.
    fn inspect_execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let arbos_version = Self::start_tx_hook(evm, init_and_floor_gas.initial_gas);

        let mut gas_limit = evm
            .ctx()
            .tx()
            .gas_limit()
            .saturating_sub(init_and_floor_gas.initial_gas);
        let held_gas = self.gas_charging_hook(
            evm,
            &mut gas_limit,
            init_and_floor_gas.initial_gas,
            arbos_version,
        )?;

        let first_frame_input = self.first_frame_input(evm, gas_limit)?;
        let mut frame_result = self.inspect_run_exec_loop(evm, first_frame_input)?;
        Self::finish_frame_result(evm, &mut frame_result, held_gas);
        Ok(frame_result)
    }
}
