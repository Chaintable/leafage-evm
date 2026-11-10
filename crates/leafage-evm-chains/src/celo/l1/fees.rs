use super::{
    evm::{CeloL1Context, CeloL1Evm},
    L1Fork,
};
use super::{
    system::{abi, word, System},
    tx::CeloCallTx,
};
use alloy::primitives::U256;
use revm::primitives::{AddressMap, HashSet};
use revm::{
    context_interface::{
        journaled_state::account::JournaledAccountTr,
        result::{EVMError, HaltReason, InvalidTransaction},
        JournalTr,
    },
    handler::{EthFrame, FrameResult, Handler, MainnetHandler},
    inspector::InspectorHandler,
    interpreter::{interpreter::EthInterpreter, InitialAndFloorGas},
    Database, Inspector,
};
use std::{cell::Cell, marker::PhantomData};

pub(super) struct FeeConfig {
    pub minimum: U256,
    pub intrinsic: u64,
    pub whitelisted: bool,
}
pub(super) struct CallHandler<'a, DB: Database, I> {
    pub system: System<'a>,
    pub tx: &'a CeloCallTx,
    pub config: FeeConfig,
    pub debit_refund: Cell<i64>,
    pub marker: PhantomData<(DB, I)>,
}
impl<'a, DB: Database, I: Inspector<CeloL1Context<DB>>> Handler for CallHandler<'a, DB, I> {
    type Evm = CeloL1Evm<'a, CeloL1Context<DB>, I>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        let fields = &self.tx.fields;
        if fields.eth_compatible && self.system.fork < L1Fork::Donut {
            return Err(EVMError::Custom(
                "eth-compatible transactions not supported".into(),
            ));
        }
        if fields.eth_compatible
            && (fields.fee_currency.is_some()
                || fields.gateway_fee_recipient.is_some()
                || !fields.gateway_fee.is_zero())
        {
            return Err(EVMError::Custom(
                "eth-compatible transaction contains Celo fee fields".into(),
            ));
        }
        if self.system.fork >= L1Fork::Gingerbread
            && (fields.gateway_fee_recipient.is_some() || !fields.gateway_fee.is_zero())
        {
            return Err(EVMError::Custom("gateway fee is deprecated".into()));
        }
        // Celo mainnet never activated H fork / denominated transactions.
        if fields.max_fee_in_fee_currency.is_some() {
            return Err(EVMError::Custom(
                "denominated fee transactions are not supported".into(),
            ));
        }
        if self.system.fork >= L1Fork::Espresso
            && (!self.tx.fee_cap.is_zero() || !self.tx.tip_cap.is_zero())
        {
            if self.tx.tip_cap > self.tx.fee_cap {
                return Err(EVMError::Custom(
                    "max priority fee per gas higher than max fee per gas".into(),
                ));
            }
            if self.tx.fee_cap < self.config.minimum {
                return Err(EVMError::Custom(
                    "max fee per gas less than block base fee".into(),
                ));
            }
        }
        MainnetHandler::<_, Self::Error, EthFrame>::default().validate_env(evm)
    }
    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        let mut gas =
            MainnetHandler::<_, Self::Error, EthFrame>::default().validate_initial_tx_gas(evm)?;
        // Native calls are messages, not typed transactions. Their access lists
        // are charged even before Berlin, but warmed only after Espresso.
        let access = self
            .tx
            .base
            .access_list
            .0
            .iter()
            .try_fold(0u64, |sum, item| {
                sum.checked_add(2400)?
                    .checked_add(1900u64.checked_mul(item.storage_keys.len() as u64)?)
            })
            .ok_or_else(|| EVMError::Custom("intrinsic gas overflow".into()))?;
        gas.initial_regular_gas = gas
            .initial_regular_gas
            .checked_add(access)
            .and_then(|n| n.checked_add(self.config.intrinsic))
            .ok_or_else(|| EVMError::Custom("intrinsic gas overflow".into()))?;
        if gas.initial_regular_gas > self.tx.base.gas_limit {
            return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                gas_limit: self.tx.base.gas_limit,
                initial_gas: gas.initial_regular_gas,
            }
            .into());
        }
        Ok(gas)
    }
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        self.debit(&mut evm.inner.ctx).map_err(EVMError::Custom)?;
        MainnetHandler::<_, Self::Error, EthFrame>::default()
            .validate_against_state_and_deduct_caller(evm, gas)
    }
    fn load_accounts(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        MainnetHandler::<_, Self::Error, EthFrame>::default().load_accounts(evm)?;
        if self.system.fork >= L1Fork::Espresso {
            let mut access: AddressMap<HashSet<U256>> = AddressMap::default();
            for item in &self.tx.base.access_list.0 {
                access
                    .entry(item.address)
                    .or_default()
                    .extend(item.storage_keys.iter().map(|v| U256::from_be_bytes(v.0)));
            }
            evm.inner.ctx.journaled_state.warm_access_list(access);
        }
        Ok(())
    }
    fn refund(
        &self,
        evm: &mut Self::Evm,
        result: &mut FrameResult,
        refund: i64,
    ) -> Result<(), Self::Error> {
        result.gas_mut().record_refund(self.debit_refund.get());
        MainnetHandler::<_, Self::Error, EthFrame>::default().refund(evm, result, refund)
    }
    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        if !self.tx.estimation {
            self.credit(&mut evm.inner.ctx, result.gas().spent_sub_refunded())
                .map_err(EVMError::Custom)?;
        }
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
impl<'a, DB: Database, I: Inspector<CeloL1Context<DB>>> InspectorHandler
    for CallHandler<'a, DB, I>
{
    type IT = EthInterpreter;
}

impl<DB: Database, I> CallHandler<'_, DB, I> {
    fn gateway(&self) -> U256 {
        if self.tx.fields.gateway_fee_recipient.is_some() {
            self.tx.fields.gateway_fee
        } else {
            U256::ZERO
        }
    }
    fn debit(&self, ctx: &mut CeloL1Context<DB>) -> Result<(), String> {
        if !self.config.whitelisted {
            return Err("non-whitelisted fee currency address".into());
        }
        let base = &self.tx.base;
        let amount = U256::from(base.gas_price)
            .checked_mul(U256::from(base.gas_limit))
            .and_then(|v| v.checked_add(self.gateway()))
            .ok_or("fee overflow")?;
        let mut required = if self.system.fork >= L1Fork::Espresso {
            self.tx
                .fee_cap
                .checked_mul(U256::from(base.gas_limit))
                .and_then(|v| v.checked_add(self.gateway()))
                .ok_or("fee cap overflow")?
        } else {
            amount
        };
        if let Some(currency) = self.tx.fields.fee_currency {
            let balance = self.system.balance(ctx, currency, base.caller)?;
            if balance < required || (self.system.fork < L1Fork::Espresso && balance == required) {
                return Err("insufficient funds for fee currency".into());
            }
            if !self.tx.estimation {
                let r = self.system.call(
                    ctx,
                    currency,
                    abi(
                        "debitGasFees(address,uint256)",
                        &[word(base.caller), amount],
                    ),
                    1_000_000,
                    false,
                    false,
                )?;
                if !r.result.is_ok() {
                    return Err(format!("debitGasFees failed: {:?}", r.result));
                }
                self.debit_refund.set(r.gas.refunded());
            }
        } else {
            if self.system.fork >= L1Fork::Espresso {
                required = required
                    .checked_add(base.value)
                    .ok_or("fee and value overflow")?;
            }
            let mut sender = ctx
                .journaled_state
                .load_account_mut(base.caller)
                .map_err(|e| e.to_string())?;
            if *sender.balance() < required {
                return Err("insufficient funds for gas * price + value + gatewayFee".into());
            }
            if !self.tx.estimation {
                let balance = *sender.balance() - amount;
                sender.set_balance(balance);
            }
        }
        let sender = ctx
            .journaled_state
            .load_account(base.caller)
            .map_err(|e| e.to_string())?;
        if sender.info.balance < base.value {
            return Err("insufficient funds for transfer".into());
        }
        Ok(())
    }
    fn credit(&self, ctx: &mut CeloL1Context<DB>, used: u64) -> Result<(), String> {
        let tx = &self.tx.base;
        let mut refund = U256::from(tx.gas_price) * U256::from(tx.gas_limit - used);
        let mut base = self
            .config
            .minimum
            .checked_mul(U256::from(used))
            .ok_or("base fee overflow")?;
        let total = U256::from(tx.gas_price) * U256::from(used);
        let negative_tip = base > total;
        let tip = if negative_tip {
            base - total
        } else {
            total - base
        };
        let recipient = self.system.registered(
            ctx,
            if self.system.fork >= L1Fork::Gingerbread {
                "FeeHandler"
            } else {
                "Governance"
            },
        )?;
        if recipient.is_none() {
            refund = refund.checked_add(base).ok_or("refund overflow")?;
            base = U256::ZERO;
        }
        if let Some(currency) = self.tx.fields.fee_currency {
            // Native AmountToAbi uses big.Int.Bytes(), including its absolute
            // magnitude for a negative tip in zero-price eth_call simulations.
            let r=self.system.call(ctx,currency,abi("creditGasFees(address,address,address,address,uint256,uint256,uint256,uint256)",&[
                word(tx.caller),word(ctx.block.beneficiary),word(self.tx.fields.gateway_fee_recipient.unwrap_or_default()),
                word(recipient.unwrap_or_default()),refund,tip,self.tx.fields.gateway_fee,base]),1_000_000,false,false)?;
            if !r.result.is_ok() {
                return Err(format!("creditGasFees failed: {:?}", r.result));
            }
        } else {
            for (address, value, subtract) in [
                (tx.caller, refund, false),
                (ctx.block.beneficiary, tip, negative_tip),
            ] {
                let mut account = ctx
                    .journaled_state
                    .load_account_mut(address)
                    .map_err(|e| e.to_string())?;
                let balance = if subtract {
                    account.balance().wrapping_sub(value)
                } else {
                    account.balance().wrapping_add(value)
                };
                account.set_balance(balance);
            }
            for (address, value) in [
                (
                    self.tx.fields.gateway_fee_recipient,
                    self.tx.fields.gateway_fee,
                ),
                (recipient, base),
            ] {
                if let Some(address) = address {
                    let mut account = ctx
                        .journaled_state
                        .load_account_mut(address)
                        .map_err(|e| e.to_string())?;
                    let balance = account.balance().wrapping_add(value);
                    account.set_balance(balance);
                }
            }
        }
        Ok(())
    }
}
