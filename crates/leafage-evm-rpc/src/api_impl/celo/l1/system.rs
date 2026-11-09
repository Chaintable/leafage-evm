//! Native Celo configuration and fee contract calls. Configuration queries use
//! a separate journal after Espresso; fee debit/credit use the caller's journal.
use alloy::primitives::{address, keccak256, Address, Bytes, U256, U512};
use leafage_evm_chains::celo::l1::{
    consensus::CeloChainData,
    evm::{system_call_result, CeloL1Context},
    L1Fork,
};
use revm::{interpreter::InterpreterResult, Database};

pub(super) fn abi(method: &str, args: &[U256]) -> Bytes {
    let mut data = keccak256(method.as_bytes())[..4].to_vec();
    for arg in args {
        data.extend_from_slice(&arg.to_be_bytes::<32>());
    }
    data.into()
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
    ) -> Result<InterpreterResult, String> {
        system_call_result(
            ctx,
            self.fork,
            self.chain,
            Address::ZERO,
            to,
            input,
            gas,
            is_static,
            unmetered,
        )
    }
    pub fn registered<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
    ) -> Result<Option<Address>, String> {
        self.lookup(ctx, name, true)
    }
    fn lookup<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
        strict: bool,
    ) -> Result<Option<Address>, String> {
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
        if r.result == revm::interpreter::InstructionResult::Revert
            || (r.result.is_ok() && r.output.is_empty())
        {
            return Ok(None);
        }
        if !r.result.is_ok() || r.output.len() < 32 || r.output.len() % 32 != 0 {
            return if strict {
                Err(format!(
                    "Registry lookup failed for {name}: {:?}, {} output bytes",
                    r.result,
                    r.output.len()
                ))
            } else {
                Ok(None)
            };
        }
        let address = Address::from_slice(&r.output[12..32]);
        Ok((address != Address::ZERO).then_some(address))
    }
    pub fn query<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
        method: &str,
        args: &[U256],
        gas: u64,
    ) -> Result<Option<Bytes>, String> {
        // Configuration readers fall back on contract errors, but database
        // failures from the EVM runner still propagate to the RPC caller.
        let Some(to) = self.lookup(ctx, name, false)? else {
            return Ok(None);
        };
        let r = self.call(ctx, to, abi(method, args), gas, true, false)?;
        Ok((r.result.is_ok() && r.output.len() % 32 == 0).then_some(r.output))
    }
    fn scalar<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        name: &str,
        method: &str,
        args: &[U256],
        gas: u64,
        fallback: U256,
    ) -> Result<U256, String> {
        Ok(self
            .query(ctx, name, method, args, gas)?
            .filter(|v| v.len() >= 32)
            .map(|v| U256::from_be_slice(&v[..32]))
            .unwrap_or(fallback))
    }
    pub fn alternative_intrinsic<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
    ) -> Result<u64, String> {
        self.scalar(
            ctx,
            "BlockchainParameters",
            "intrinsicGasForAlternativeFeeCurrency()",
            &[],
            40_000,
            U256::from(50_000),
        )?
        .try_into()
        .map_err(|_| "alternative intrinsic gas exceeds u64".into())
    }
    pub fn whitelisted<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        currency: Address,
    ) -> Result<bool, String> {
        let data = self.query(ctx, "FeeCurrencyWhitelist", "getWhitelist()", &[], 200_000)?;
        let addresses = data.as_deref().and_then(|data| {
            if data.len() < 64 {
                return None;
            }
            let offset = usize::try_from(U256::from_be_slice(&data[..32])).ok()?;
            let length_end = offset.checked_add(32)?;
            let count = usize::try_from(U256::from_be_slice(data.get(offset..length_end)?)).ok()?;
            let end = length_end.checked_add(count.checked_mul(32)?)?;
            Some(
                data.get(length_end..end)?
                    .chunks_exact(32)
                    .map(|v| Address::from_slice(&v[12..]))
                    .collect::<Vec<_>>(),
            )
        });
        // Before Espresso the native CurrencyWhitelist lookup fails open.
        Ok(addresses
            .map(|a| a.contains(&currency))
            .unwrap_or(self.fork < L1Fork::Espresso))
    }
    pub fn minimum<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        currency: Option<Address>,
    ) -> Result<U256, String> {
        if self.fork >= L1Fork::Gingerbread {
            let base = U256::from(ctx.block.basefee);
            let Some(currency) = currency else {
                return Ok(base);
            };
            // Native GetExchangeRate falls back to a 1:1 rate on failed
            // contract lookup/query/ABI decoding. A successfully decoded zero
            // rate instead fails validation and yields a zero minimum.
            let Some(rate) = self.query(
                ctx,
                "SortedOracles",
                "medianRate(address)",
                &[word(currency)],
                100_000,
            )?
            else {
                return Ok(base);
            };
            if rate.len() < 64 {
                return Ok(base);
            }
            let numerator = U256::from_be_slice(&rate[..32]);
            let denominator = U256::from_be_slice(&rate[32..64]);
            if numerator.is_zero() || denominator.is_zero() {
                return Ok(U256::ZERO);
            }
            let result = base.widening_mul::<256, 4, 512, 8>(numerator) / denominator.to::<U512>();
            if result.bit_len() > 256 {
                return Err("converted base fee exceeds U256".into());
            }
            return Ok(result.to::<U256>());
        }
        let currency = match currency {
            Some(a) => Some(a),
            None => self.lookup(ctx, "GoldToken", false)?,
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
    pub fn balance<DB: Database>(
        &self,
        ctx: &mut CeloL1Context<DB>,
        currency: Address,
        owner: Address,
    ) -> Result<U256, String> {
        // Native EVMRunner creates the balance query with ORIGIN=0/GASPRICE=0.
        let old = ctx.tx.clone();
        ctx.tx.caller = Address::ZERO;
        ctx.tx.gas_price = 0;
        let result = self.call(
            ctx,
            currency,
            abi("balanceOf(address)", &[word(owner)]),
            100_000,
            true,
            false,
        );
        ctx.tx = old;
        let r = result?;
        if !r.result.is_ok() || r.output.len() < 32 || r.output.len() % 32 != 0 {
            return Err("fee currency balanceOf failed".into());
        }
        Ok(U256::from_be_slice(&r.output[..32]))
    }
}
