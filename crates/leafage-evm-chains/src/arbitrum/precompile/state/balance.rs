use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn burn_precompile_balance(
        &mut self,
        precompile: Address,
        value: U256,
    ) -> Result<(), PrecompileError> {
        if value.is_zero() {
            return Ok(());
        }
        self.context
            .journal_mut()
            .transfer(precompile, Address::ZERO, value)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))?
            .map_or(Ok(()), |err| {
                Err(PrecompileError::other(format!("{err:?}")))
            })
    }

    pub(in crate::arbitrum::precompile) fn transfer_balance(
        &mut self,
        from: Address,
        to: Address,
        value: U256,
    ) -> Result<(), PrecompileError> {
        if value.is_zero() || from == to {
            return Ok(());
        }
        self.context
            .journal_mut()
            .transfer(from, to, value)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))?
            .map_or(Ok(()), |err| {
                Err(PrecompileError::other(format!("{err:?}")))
            })
    }

    pub(in crate::arbitrum::precompile) fn mint_balance(
        &mut self,
        account: Address,
        amount: U256,
    ) -> Result<(), PrecompileError> {
        let mut account = self
            .context
            .journal_mut()
            .load_account_mut_skip_cold_load(account, false)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
        if !account.data.incr_balance(amount) {
            return Err(PrecompileError::other("balance overflow"));
        }
        Ok(())
    }

    pub(in crate::arbitrum::precompile) fn burn_balance(
        &mut self,
        account: Address,
        amount: U256,
    ) -> Result<(), PrecompileError> {
        let mut account = self
            .context
            .journal_mut()
            .load_account_mut_skip_cold_load(account, false)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
        if !account.data.decr_balance(amount) {
            return Err(PrecompileError::other("burn amount exceeds balance"));
        }
        Ok(())
    }
}
