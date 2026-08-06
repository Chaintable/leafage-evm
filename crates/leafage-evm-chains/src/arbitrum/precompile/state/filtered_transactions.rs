use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn is_filtered_transaction(
        &mut self,
        tx_hash: B256,
    ) -> Result<bool, PrecompileError> {
        let value = self.read_account_key(
            arbos_state::FILTERED_TRANSACTIONS_STATE_ADDRESS,
            &[],
            tx_hash.0,
        )?;
        Ok(value == U256::from(1u8))
    }

    pub(in crate::arbitrum::precompile) fn add_filtered_transaction(
        &mut self,
        tx_hash: B256,
    ) -> Result<(), PrecompileError> {
        self.write_account_key(
            arbos_state::FILTERED_TRANSACTIONS_STATE_ADDRESS,
            &[],
            tx_hash.0,
            U256::from(1u8),
        )
    }

    pub(in crate::arbitrum::precompile) fn delete_filtered_transaction(
        &mut self,
        tx_hash: B256,
    ) -> Result<(), PrecompileError> {
        self.write_account_key(
            arbos_state::FILTERED_TRANSACTIONS_STATE_ADDRESS,
            &[],
            tx_hash.0,
            U256::ZERO,
        )
    }
}
