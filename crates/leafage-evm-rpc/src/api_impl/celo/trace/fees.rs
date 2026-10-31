//! Celo StateTransition fee debit/credit, around the primary call. System
//! calls share its journal but do not become transaction trace roots.
use super::input::Transaction;
use alloy::primitives::{address, keccak256, Address, Bytes, U256};
use anyhow::{ensure, Context as _, Result};
use leafage_evm_chains::celo::l1::{
    consensus::CeloChainData,
    evm::{system_call_inspected, CeloL1Context, CeloL1Evm},
    L1Fork,
};
use revm::context_interface::{
    journaled_state::account::JournaledAccountTr,
    result::{EVMError, HaltReason},
    JournalTr,
};
use revm::handler::{EthFrame, FrameResult, Handler, MainnetHandler};
use revm::inspector::InspectorHandler;
use revm::interpreter::{interpreter::EthInterpreter, InitialAndFloorGas, InterpreterResult};
use revm::{Database, Inspector};
use std::{cell::Cell, marker::PhantomData};

pub(super) fn abi(method: &str, args: &[U256]) -> Bytes {
    let mut out = keccak256(method.as_bytes())[..4].to_vec();
    for arg in args {
        out.extend_from_slice(&arg.to_be_bytes::<32>());
    }
    out.into()
}
pub(super) fn word(a: Address) -> U256 {
    U256::from_be_slice(a.as_slice())
}

pub(super) struct System<'a> {
    pub fork: L1Fork,
    pub chain: &'a dyn CeloChainData,
}
impl System<'_> {
    pub fn call<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        to: Address,
        input: Bytes,
        gas: u64,
        is_static: bool,
        unmetered: bool,
    ) -> Result<InterpreterResult> {
        system_call_inspected(
            ctx,
            self.fork,
            self.chain,
            Address::ZERO,
            to,
            input,
            gas,
            is_static,
            unmetered,
            None,
        )
        .map_err(anyhow::Error::msg)
    }
    pub fn registered<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
    ) -> Result<Option<Address>> {
        let r = self.call(
            ctx,
            address!("000000000000000000000000000000000000ce10"),
            abi(
                "getAddressFor(bytes32)",
                &[U256::from_be_bytes(keccak256(name).0)],
            ),
            100_000,
            true,
            true,
        )?;
        if !r.result.is_ok() || r.output.len() < 32 {
            return Ok(None);
        }
        let a = Address::from_slice(&r.output[12..32]);
        Ok((a != Address::ZERO).then_some(a))
    }
    pub fn query<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
        method: &str,
        args: &[U256],
        gas: u64,
    ) -> Result<Option<Bytes>> {
        let Some(to) = self.registered(ctx, name)? else {
            return Ok(None);
        };
        let r = self.call(ctx, to, abi(method, args), gas, true, false)?;
        Ok(r.result.is_ok().then_some(r.output))
    }
    pub fn scalar<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
        method: &str,
        args: &[U256],
        gas: u64,
        fallback: U256,
    ) -> Result<U256> {
        Ok(self
            .query(ctx, name, method, args, gas)?
            .filter(|v| v.len() >= 32)
            .map(|v| U256::from_be_slice(&v[..32]))
            .unwrap_or(fallback))
    }
    pub fn minimum<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        currency: Option<Address>,
    ) -> Result<U256> {
        if self.fork >= L1Fork::Gingerbread {
            let base = U256::from(ctx.block.basefee);
            let Some(currency) = currency else {
                return Ok(base);
            };
            let Some(rate) = self.query(
                ctx,
                "SortedOracles",
                "medianRate(address)",
                &[word(currency)],
                100_000,
            )?
            else {
                return Ok(U256::ZERO);
            };
            if rate.len() < 64 {
                return Ok(U256::ZERO);
            }
            let numerator = U256::from_be_slice(&rate[..32]);
            let denominator = U256::from_be_slice(&rate[32..64]);
            if numerator.is_zero() || denominator.is_zero() {
                return Ok(U256::ZERO);
            }
            // Full-width intermediate, matching Go big.Int rather than wrapping U256.
            let converted = base.widening_mul::<256, 4, 512, 8>(numerator)
                / denominator.to::<alloy::primitives::U512>();
            ensure!(
                converted.bit_len() <= 256,
                "converted base fee exceeds U256"
            );
            return Ok(converted.to::<U256>());
        }
        let currency = match currency {
            Some(a) => Some(a),
            None => self.registered(ctx, "GoldToken")?,
        };
        let Some(currency) = currency else {
            return Ok(U256::ZERO);
        };
        self.scalar(
            ctx,
            "GasPriceMinimum",
            "getGasPriceMinimum(address)",
            &[word(currency)],
            2_000_000,
            U256::ZERO,
        )
    }
    pub fn alternative_intrinsic<DB: Database>(&self, ctx: &mut CeloL1Context<DB>) -> Result<u64> {
        Ok(self
            .scalar(
                ctx,
                "BlockchainParameters",
                "intrinsicGasForAlternativeFeeCurrency()",
                &[],
                40_000,
                U256::from(50_000),
            )?
            .try_into()
            .context("alternative intrinsic gas exceeds u64")?)
    }
}

pub(super) struct CeloTransactionHandler<'a, DB: Database, I> {
    pub system: System<'a>,
    pub tx: &'a Transaction,
    pub minimum: U256,
    pub price: u128,
    pub alternative_intrinsic: u64,
    pub debit_refund: Cell<i64>,
    pub marker: PhantomData<(DB, I)>,
}
impl<'a, DB: Database, I: Inspector<CeloL1Context<DB>>> Handler
    for CeloTransactionHandler<'a, DB, I>
{
    type Evm = CeloL1Evm<'a, CeloL1Context<DB>, I>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        let mut gas =
            MainnetHandler::<_, Self::Error, EthFrame>::default().validate_initial_tx_gas(evm)?;
        gas.initial_regular_gas = gas
            .initial_regular_gas
            .checked_add(self.alternative_intrinsic)
            .ok_or_else(|| EVMError::Custom("intrinsic gas overflow".into()))?;
        if gas.initial_regular_gas > self.tx.gas.to::<u64>() {
            return Err(EVMError::Custom("insufficient intrinsic gas".into()));
        }
        Ok(gas)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        let result = self
            .debit(&mut evm.inner.ctx)
            .map_err(|e| EVMError::Custom(format!("Celo fee debit: {e:#}")));
        result?;
        // Fee charging is disabled in this mainnet helper; retain nonce checks
        // and the CALL nonce bump. Celo deliberately allows a contract sender.
        MainnetHandler::<_, Self::Error, EthFrame>::default()
            .validate_against_state_and_deduct_caller(evm, gas)
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
        self.credit(&mut evm.inner.ctx, result.gas().spent_sub_refunded())
            .map_err(|e| EVMError::Custom(format!("Celo fee credit: {e:#}")))
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
    for CeloTransactionHandler<'a, DB, I>
{
    type IT = EthInterpreter;
}

impl<DB: Database, I> CeloTransactionHandler<'_, DB, I> {
    fn gateway(&self) -> U256 {
        if self.tx.gateway_fee_recipient.is_some() {
            self.tx.gateway_fee
        } else {
            U256::ZERO
        }
    }
    fn debit(&self, ctx: &mut CeloL1Context<DB>) -> Result<()> {
        let amount = U256::from(self.price)
            .checked_mul(U256::from(self.tx.gas.to::<u64>()))
            .and_then(|v| v.checked_add(self.gateway()))
            .context("fee overflow")?;
        if let Some(currency) = self.tx.fee_currency {
            // Native canPayFee executes balanceOf on the same StateDB before
            // debitGasFees. Its accesses remain warm for the primary call.
            // Preserve the query even though signed canonical inputs already
            // identify a valid transaction: its access-list effects matter.
            let previous_tx = ctx.tx.clone();
            ctx.tx.caller = Address::ZERO;
            ctx.tx.gas_price = 0;
            let balance = self.system.call(
                ctx,
                currency,
                abi("balanceOf(address)", &[word(self.tx.from)]),
                100_000,
                true,
                false,
            );
            ctx.tx = previous_tx;
            let balance = balance?;
            ensure!(
                balance.result.is_ok() && balance.output.len() >= 32,
                "fee currency balanceOf failed"
            );
            let balance = U256::from_be_slice(&balance.output[..32]);
            let required = if self.system.fork >= L1Fork::Espresso {
                self.tx
                    .max_fee_per_gas
                    .unwrap_or(U256::from(self.price))
                    .checked_mul(U256::from(self.tx.gas.to::<u64>()))
                    .and_then(|v| v.checked_add(self.gateway()))
                    .context("fee cap overflow")?
            } else {
                amount
            };
            ensure!(
                if self.system.fork >= L1Fork::Espresso {
                    balance >= required
                } else {
                    balance > required
                },
                "insufficient fee currency balance"
            );
            let r = self.system.call(
                ctx,
                currency,
                abi(
                    "debitGasFees(address,uint256)",
                    &[word(self.tx.from), amount],
                ),
                1_000_000,
                false,
                false,
            )?;
            ensure!(r.result.is_ok(), "debitGasFees failed: {:?}", r.result);
            self.debit_refund.set(r.gas.refunded());
        } else {
            let mut caller = ctx.journaled_state.load_account_mut(self.tx.from)?;
            ensure!(
                *caller.balance() >= amount,
                "insufficient native fee balance"
            );
            let balance = *caller.balance() - amount;
            caller.set_balance(balance);
        }
        Ok(())
    }
    fn credit(&self, ctx: &mut CeloL1Context<DB>, used: u64) -> Result<()> {
        let mut refund = U256::from(self.price) * U256::from(self.tx.gas.to::<u64>() - used);
        let mut base = self
            .minimum
            .checked_mul(U256::from(used))
            .context("base fee overflow")?;
        let tip = U256::from(self.price) * U256::from(used) - base;
        let fee_handler = self.system.registered(
            ctx,
            if self.system.fork >= L1Fork::Gingerbread {
                "FeeHandler"
            } else {
                "Governance"
            },
        )?;
        if fee_handler.is_none() {
            refund += base;
            base = U256::ZERO;
        }
        if let Some(currency) = self.tx.fee_currency {
            let r = self.system.call(ctx, currency,
                abi("creditGasFees(address,address,address,address,uint256,uint256,uint256,uint256)", &[
                    word(self.tx.from), word(ctx.block.beneficiary), word(self.tx.gateway_fee_recipient.unwrap_or_default()),
                    word(fee_handler.unwrap_or_default()), refund, tip, self.tx.gateway_fee, base]), 1_000_000, false, false)?;
            ensure!(r.result.is_ok(), "creditGasFees failed: {:?}", r.result);
        } else {
            for (to, value) in [(self.tx.from, refund), (ctx.block.beneficiary, tip)] {
                ensure!(
                    ctx.journaled_state
                        .load_account_mut(to)?
                        .incr_balance(value),
                    "fee balance overflow"
                );
            }
            if let Some(to) = self.tx.gateway_fee_recipient {
                ensure!(
                    ctx.journaled_state
                        .load_account_mut(to)?
                        .incr_balance(self.tx.gateway_fee),
                    "gateway fee overflow"
                );
            }
            if let Some(to) = fee_handler {
                ensure!(
                    ctx.journaled_state.load_account_mut(to)?.incr_balance(base),
                    "base fee overflow"
                );
            }
        }
        Ok(())
    }
}
