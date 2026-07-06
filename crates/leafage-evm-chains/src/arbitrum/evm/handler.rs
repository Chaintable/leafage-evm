//! EVM handler hooks for Arbitrum Nitro transaction processing.

use super::poster_gas::ArbPosterCharge;
use super::ArbitrumEvm;
use crate::arbitrum::arbos_state::{self, ArbStateReader};
use crate::arbitrum::precompile::{
    ArbitrumContext, BATCH_POSTER_ADDRESS, L1_PRICER_FUNDS_POOL_ADDRESS,
};
use alloy::primitives::U256;
use revm::{
    context::{
        result::{EVMError, HaltReason},
        Block, ContextTr, Transaction,
    },
    context_interface::{
        journaled_state::account::JournaledAccountTr, result::InvalidTransaction,
        transaction::TransactionType, Cfg, JournalTr,
    },
    handler::{pre_execution, validation, EvmTr, FrameResult, FrameTr, Handler},
    inspector::{Inspector, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, Gas, InitialAndFloorGas},
    primitives::hardfork::SpecId,
    Database, DatabaseRef,
};

const ARBOS_VERSION_L1_PRICER_FUNDS_POOL: u64 = 2;
const ARBOS_VERSION_L1_FEES_AVAILABLE: u64 = 10;

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

    fn collect_tips(ctx: &ArbitrumContext<DB>) -> bool {
        ctx.db().collect_tips() && ctx.block().beneficiary() == BATCH_POSTER_ADDRESS
    }

    fn effective_gas_price(ctx: &ArbitrumContext<DB>) -> u128 {
        let basefee = Self::l2_basefee(ctx);
        let effective = ctx.tx().effective_gas_price(basefee);
        if Self::collect_tips(ctx) {
            effective
        } else {
            effective.min(basefee)
        }
    }

    fn paid_l1_gas_price(ctx: &ArbitrumContext<DB>, block_base_fee: u64) -> U256 {
        if Self::collect_tips(ctx) {
            let price = ctx.tx().effective_gas_price(block_base_fee as u128);
            if price != 0 {
                return U256::from(price);
            }
        }
        U256::from(block_base_fee)
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
    ) -> Result<(), EVMError<<DB as Database>::Error>> {
        let charge = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            let chain = ctx.chain();
            let l2_basefee = chain
                .current_l2_basefee()
                .unwrap_or_else(|| ctx.block().basefee());

            if l2_basefee == 0 || tx.is_retryable_redeem() {
                ArbPosterCharge::default()
            } else {
                let pricing = ctx.db().read_pricing();
                pricing
                    .as_ref()
                    .map(|pricing| {
                        pricing
                            .gas_charging_charge(&tx.base, Self::paid_l1_gas_price(ctx, l2_basefee))
                    })
                    .unwrap_or_default()
            }
        };

        evm.ctx_mut().chain_mut().set_current_poster_charge(charge);
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

        Ok(())
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
        let effective_gas_price = Self::effective_gas_price(evm.ctx());
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
        evm.ctx_mut().chain_mut().clear_current_poster_charge();

        let mut gas_limit = evm
            .ctx()
            .tx()
            .gas_limit()
            .saturating_sub(init_and_floor_gas.initial_gas);
        self.gas_charging_hook(evm, &mut gas_limit, init_and_floor_gas.initial_gas)?;

        let first_frame_input = self.first_frame_input(evm, gas_limit)?;
        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let remaining = gas.remaining();
        let refunded = gas.refunded();

        *gas = Gas::new_spent(evm.ctx().tx().gas_limit());

        if instruction_result.is_ok_or_revert() {
            gas.erase_cost(remaining);
        }

        if instruction_result.is_ok() {
            gas.record_refund(refunded);
        }
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

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if evm.ctx().cfg().is_fee_charge_disabled() {
            return Ok(());
        }

        let effective_gas_price = Self::effective_gas_price(evm.ctx());
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

        let effective_gas_price = Self::effective_gas_price(evm.ctx());
        let arbos_version = evm.ctx().db().arbos_version();
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

        let beneficiary = evm
            .ctx()
            .db()
            .network_fee_account()
            .unwrap_or_else(|| evm.ctx().block().beneficiary());
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::precompile::ArbitrumPrecompileEnv;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, Bytes, B256};
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::{Context, TxEnv};
    use revm::database::{in_memory_db::CacheDB, EmptyDB};
    use revm::MainContext;

    type TestDb = CacheDB<EmptyDB>;

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
            .with_chain(ArbitrumExecutionContext::default())
    }

    fn db_with_pricing() -> TestDb {
        let mut db = CacheDB::new(EmptyDB::default());
        let l1_pricing_key = arbos_state::child_key(&[], arbos_state::L1_PRICING_SUBSPACE);
        let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
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
            ArbitrumPrecompileEnv::default(),
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
        let batch_poster = context_with_collect_tips(BATCH_POSTER_ADDRESS);
        assert!(ArbitrumHandler::<TestDb, ()>::collect_tips(&batch_poster));
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::effective_gas_price(&batch_poster),
            200
        );
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::paid_l1_gas_price(&batch_poster, 100),
            U256::from(200)
        );

        let delayed = context_with_collect_tips(Address::with_last_byte(0x01));
        assert!(!ArbitrumHandler::<TestDb, ()>::collect_tips(&delayed));
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::effective_gas_price(&delayed),
            100
        );
        assert_eq!(
            ArbitrumHandler::<TestDb, ()>::paid_l1_gas_price(&delayed, 100),
            U256::from(100)
        );
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
            .gas_charging_hook(&mut normal_evm, &mut normal_gas_remaining, 21_000)
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
            .gas_charging_hook(&mut retryable_evm, &mut retryable_gas_remaining, 21_000)
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
}

impl<DB, INSP> InspectorHandler for ArbitrumHandler<DB, INSP>
where
    DB: Database + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type IT = EthInterpreter;
}
