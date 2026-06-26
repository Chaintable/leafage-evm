use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn gas_prices_in_wei(
        &mut self,
        arbos_version: u64,
    ) -> Result<(U256, U256, U256, U256, U256, U256), PrecompileError> {
        let l1_key = self.l1_key();
        let l2_key = self.l2_key();
        let l1_price = self.read(&l1_key, arbos_state::L1_PRICE_PER_UNIT_OFFSET)?;
        let l2_price = U256::from(self.current_l2_basefee());
        let wei_for_l1_calldata = l1_price.saturating_mul(U256::from(TX_DATA_NON_ZERO_GAS));
        let per_l2_tx = wei_for_l1_calldata.saturating_mul(U256::from(ASSUMED_SIMPLE_TX_SIZE));
        let per_arb_gas_base = if arbos_version < ARBOS_VERSION_4 {
            l2_price
        } else {
            let min_base = self.read(&l2_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET)?;
            if l2_price < min_base {
                l2_price
            } else {
                min_base
            }
        };
        let per_arb_gas_congestion = if arbos_version < ARBOS_VERSION_4 {
            U256::ZERO
        } else {
            l2_price.saturating_sub(per_arb_gas_base)
        };
        let wei_for_l2_storage = l2_price.saturating_mul(U256::from(STORAGE_WRITE_COST));
        Ok((
            per_l2_tx,
            wei_for_l1_calldata,
            wei_for_l2_storage,
            per_arb_gas_base,
            per_arb_gas_congestion,
            l2_price,
        ))
    }

    pub(in crate::arbitrum::precompile) fn gas_prices_in_arb_gas(
        &mut self,
        arbos_version: u64,
    ) -> Result<(U256, U256, U256), PrecompileError> {
        let l1_key = self.l1_key();
        let l1_price = self.read(&l1_key, arbos_state::L1_PRICE_PER_UNIT_OFFSET)?;
        let l2_price = U256::from(self.current_l2_basefee());
        let wei_for_l1_calldata = l1_price.saturating_mul(U256::from(TX_DATA_NON_ZERO_GAS));
        let wei_per_l2_tx = wei_for_l1_calldata.saturating_mul(U256::from(ASSUMED_SIMPLE_TX_SIZE));
        let gas_per_l2_tx = if arbos_version < ARBOS_VERSION_4 {
            U256::from(ASSUMED_SIMPLE_TX_SIZE)
        } else if l2_price.is_zero() {
            U256::ZERO
        } else {
            wei_per_l2_tx / l2_price
        };
        let gas_for_l1_calldata = if l2_price.is_zero() {
            U256::ZERO
        } else {
            wei_for_l1_calldata / l2_price
        };
        Ok((
            gas_per_l2_tx,
            gas_for_l1_calldata,
            U256::from(STORAGE_WRITE_COST),
        ))
    }

    pub(in crate::arbitrum::precompile) fn gas_pricing_constraints(
        &mut self,
    ) -> Result<Vec<[u64; 3]>, PrecompileError> {
        let vector_key = self.gas_constraints_key();
        let len = self.read_u64(&vector_key, 0)?;
        let mut out = Vec::new();
        for i in 0..len {
            let constraint_key = self.gas_constraint_key(&vector_key, i);
            out.push([
                self.read_u64(&constraint_key, 0)?,
                self.read_u64(&constraint_key, 1)?,
                self.read_u64(&constraint_key, 2)?,
            ]);
        }
        Ok(out)
    }

    pub(in crate::arbitrum::precompile) fn multi_gas_pricing_constraints(
        &mut self,
    ) -> Result<Vec<MultiGasPricingConstraint>, PrecompileError> {
        let vector_key = self.multi_gas_constraints_key();
        let len = self.read_u64(&vector_key, 0)?;
        let mut out = Vec::new();
        for i in 0..len {
            let constraint_key = self.multi_gas_constraint_key(&vector_key, i);
            let mut resources = [0u64; NUM_RESOURCE_KIND];
            for (resource, weight) in resources.iter_mut().enumerate() {
                *weight = self.read_u64(&constraint_key, 4 + resource as u64)?;
            }
            out.push(MultiGasPricingConstraint {
                resources,
                adjustment_window_secs: self.read_u64(&constraint_key, 1)? as u32,
                target_per_sec: self.read_u64(&constraint_key, 0)?,
                backlog: self.read_u64(&constraint_key, 2)?,
            });
        }
        Ok(out)
    }

    pub(in crate::arbitrum::precompile) fn multi_gas_current_base_fees(
        &mut self,
    ) -> Result<Vec<U256>, PrecompileError> {
        let fees_key = self.multi_gas_base_fees_key();
        let l2_key = self.l2_key();
        let base_fee = self.read(&l2_key, arbos_state::L2_BASE_FEE_WEI_OFFSET)?;
        let mut out = Vec::with_capacity(NUM_RESOURCE_KIND);
        for resource in 0..NUM_RESOURCE_KIND {
            let fee = self.read(&fees_key, NUM_RESOURCE_KIND as u64 + resource as u64)?;
            if resource == RESOURCE_KIND_SINGLE_DIM || fee.is_zero() {
                out.push(base_fee);
            } else {
                out.push(fee);
            }
        }
        Ok(out)
    }

    pub(in crate::arbitrum::precompile) fn clear_gas_pricing_constraints(
        &mut self,
    ) -> Result<(), PrecompileError> {
        let vector_key = self.gas_constraints_key();
        let len = self.read_u64(&vector_key, 0)?;
        for i in (0..len).rev() {
            self.write(&vector_key, 0, U256::from(i))?;
            let constraint_key = self.gas_constraint_key(&vector_key, i);
            for offset in 0..=2 {
                self.write(&constraint_key, offset, U256::ZERO)?;
            }
        }
        Ok(())
    }

    pub(in crate::arbitrum::precompile) fn push_gas_pricing_constraint(
        &mut self,
        constraint: [u64; 3],
    ) -> Result<(), PrecompileError> {
        let vector_key = self.gas_constraints_key();
        let len = self.read_u64(&vector_key, 0)?;
        let constraint_key = self.gas_constraint_key(&vector_key, len);
        self.write(&vector_key, 0, U256::from(len.saturating_add(1)))?;
        self.write(&constraint_key, 0, U256::from(constraint[0]))?;
        self.write(&constraint_key, 1, U256::from(constraint[1]))?;
        self.write(&constraint_key, 2, U256::from(constraint[2]))
    }

    pub(in crate::arbitrum::precompile) fn clear_multi_gas_pricing_constraints(
        &mut self,
    ) -> Result<(), PrecompileError> {
        let vector_key = self.multi_gas_constraints_key();
        let len = self.read_u64(&vector_key, 0)?;
        for i in (0..len).rev() {
            self.write(&vector_key, 0, U256::from(i))?;
            let constraint_key = self.multi_gas_constraint_key(&vector_key, i);
            for offset in 0..(4 + NUM_RESOURCE_KIND as u64) {
                self.write(&constraint_key, offset, U256::ZERO)?;
            }
        }
        Ok(())
    }

    pub(in crate::arbitrum::precompile) fn push_multi_gas_pricing_constraint(
        &mut self,
        constraint: &MultiGasPricingConstraint,
    ) -> Result<(), PrecompileError> {
        let vector_key = self.multi_gas_constraints_key();
        let len = self.read_u64(&vector_key, 0)?;
        let constraint_key = self.multi_gas_constraint_key(&vector_key, len);
        self.write(&vector_key, 0, U256::from(len.saturating_add(1)))?;
        self.write(&constraint_key, 0, U256::from(constraint.target_per_sec))?;
        self.write(
            &constraint_key,
            1,
            U256::from(constraint.adjustment_window_secs),
        )?;
        self.write(&constraint_key, 2, U256::from(constraint.backlog))?;
        self.write(&constraint_key, 3, U256::from(constraint.max_weight()))?;
        for (i, weight) in constraint.resources.iter().enumerate() {
            self.write(&constraint_key, 4 + i as u64, U256::from(*weight))?;
        }
        Ok(())
    }

    pub(in crate::arbitrum::precompile::state) fn gas_constraints_key(&self) -> [u8; 32] {
        let l2_key = self.l2_key();
        arbos_state::child_key(&l2_key, GAS_CONSTRAINTS_KEY)
    }

    pub(in crate::arbitrum::precompile::state) fn gas_constraint_key(
        &self,
        vector_key: &[u8],
        index: u64,
    ) -> [u8; 32] {
        let child_id = index.to_be_bytes();
        arbos_state::child_key(vector_key, &child_id)
    }

    pub(in crate::arbitrum::precompile::state) fn multi_gas_constraints_key(&self) -> [u8; 32] {
        let l2_key = self.l2_key();
        arbos_state::child_key(&l2_key, MULTI_GAS_CONSTRAINTS_KEY)
    }

    pub(in crate::arbitrum::precompile::state) fn multi_gas_constraint_key(
        &self,
        vector_key: &[u8],
        index: u64,
    ) -> [u8; 32] {
        let child_id = index.to_be_bytes();
        arbos_state::child_key(vector_key, &child_id)
    }

    pub(in crate::arbitrum::precompile::state) fn multi_gas_base_fees_key(&self) -> [u8; 32] {
        let l2_key = self.l2_key();
        arbos_state::child_key(&l2_key, &[2])
    }

    pub(in crate::arbitrum::precompile) fn l1_pricing_surplus(
        &mut self,
    ) -> Result<I256, PrecompileError> {
        let l1_key = self.l1_key();
        let batch_poster_table_key = self.batch_poster_table_key();
        let funds_due_for_refunds = self.read(
            &batch_poster_table_key,
            arbos_state::BATCH_POSTER_TOTAL_FUNDS_DUE_OFFSET,
        )?;
        let funds_due_for_rewards =
            self.read(&l1_key, arbos_state::L1_FUNDS_DUE_FOR_REWARDS_OFFSET)?;
        let need_funds = funds_due_for_refunds.saturating_add(funds_due_for_rewards);
        let have_funds = if self.arbos_version()? < 10 {
            self.burn(STORAGE_READ_GAS)?;
            self.context
                .journal_mut()
                .load_account(L1_PRICER_FUNDS_POOL_ADDRESS)
                .map(|account| account.data.info.balance)
                .map_err(|e| PrecompileError::other(format!("{e:?}")))?
        } else {
            self.read(&l1_key, arbos_state::L1_FEES_AVAILABLE_OFFSET)?
        };

        Ok(signed_diff(have_funds, need_funds))
    }

    pub(in crate::arbitrum::precompile) fn release_l1_pricer_surplus(
        &mut self,
        max_wei_to_release: U256,
    ) -> Result<U256, PrecompileError> {
        let l1_key = self.l1_key();
        let balance = self
            .context
            .journal_mut()
            .load_account(L1_PRICER_FUNDS_POOL_ADDRESS)
            .map(|account| account.data.info.balance)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
        let recognized = self.read(&l1_key, arbos_state::L1_FEES_AVAILABLE_OFFSET)?;
        let released = balance.saturating_sub(recognized).min(max_wei_to_release);
        self.write(
            &l1_key,
            arbos_state::L1_FEES_AVAILABLE_OFFSET,
            recognized.saturating_add(released),
        )?;
        Ok(released)
    }

    pub(in crate::arbitrum::precompile) fn backlog_update_cost(
        &mut self,
    ) -> Result<u64, PrecompileError> {
        let arbos_version = self.arbos_version_unmetered()?;
        if arbos_version >= 60 {
            return Ok(STORAGE_READ_GAS + STORAGE_WRITE_COST);
        }

        let mut cost = 0u64;
        if arbos_version >= 50 {
            cost = cost.saturating_add(STORAGE_READ_GAS);
        }

        if arbos_version >= 51 {
            let vector_key = self.gas_constraints_key();
            let constraints_len = self.read_u64(&vector_key, 0)?;
            if constraints_len > 0 {
                return Ok(cost.saturating_add(STORAGE_READ_GAS).saturating_add(
                    constraints_len.saturating_mul(STORAGE_READ_GAS + STORAGE_WRITE_COST),
                ));
            }
        }

        Ok(cost
            .saturating_add(STORAGE_READ_GAS)
            .saturating_add(STORAGE_WRITE_COST))
    }

    pub(in crate::arbitrum::precompile) fn uses_fixed_backlog_update_cost(
        &mut self,
    ) -> Result<bool, PrecompileError> {
        Ok(self.arbos_version_unmetered()? >= 60)
    }

    pub(in crate::arbitrum::precompile) fn shrink_l2_backlog(
        &mut self,
        gas: u64,
    ) -> Result<(), PrecompileError> {
        let arbos_version = self.arbos_version_unmetered()?;
        let metered = arbos_version < 60;

        if arbos_version >= 60 {
            let vector_key = self.multi_gas_constraints_key();
            let len = self.read_u64_with_metering(&vector_key, 0, metered)?;
            if len > 0 {
                return self.shrink_multi_gas_backlogs(&vector_key, len, gas, metered);
            }
        }

        if arbos_version >= 50 {
            let vector_key = self.gas_constraints_key();
            let len = self.read_u64_with_metering(&vector_key, 0, metered)?;
            if len > 0 {
                return self.shrink_single_gas_backlogs(&vector_key, len, gas, metered);
            }
        }

        let l2_key = self.l2_key();
        let backlog =
            self.read_u64_with_metering(&l2_key, arbos_state::L2_GAS_BACKLOG_OFFSET, metered)?;
        self.write_with_metering(
            &l2_key,
            arbos_state::L2_GAS_BACKLOG_OFFSET,
            U256::from(backlog.saturating_sub(gas)),
            metered,
        )
    }

    pub(in crate::arbitrum::precompile::state) fn shrink_single_gas_backlogs(
        &mut self,
        vector_key: &[u8],
        len: u64,
        gas: u64,
        metered: bool,
    ) -> Result<(), PrecompileError> {
        for i in 0..len {
            let constraint_key = self.gas_constraint_key(vector_key, i);
            let backlog = self.read_u64_with_metering(&constraint_key, 2, metered)?;
            self.write_with_metering(
                &constraint_key,
                2,
                U256::from(backlog.saturating_sub(gas)),
                metered,
            )?;
        }
        Ok(())
    }

    pub(in crate::arbitrum::precompile::state) fn shrink_multi_gas_backlogs(
        &mut self,
        vector_key: &[u8],
        len: u64,
        single_dim_gas: u64,
        metered: bool,
    ) -> Result<(), PrecompileError> {
        for i in 0..len {
            let constraint_key = self.multi_gas_constraint_key(vector_key, i);
            let mut backlog = self.read_u64_with_metering(&constraint_key, 2, metered)?;
            for resource in 0..NUM_RESOURCE_KIND {
                let weight =
                    self.read_u64_with_metering(&constraint_key, 4 + resource as u64, metered)?;
                if weight == 0 {
                    continue;
                }
                let amount = if resource == RESOURCE_KIND_SINGLE_DIM {
                    single_dim_gas
                } else {
                    0
                };
                backlog = backlog.saturating_sub(amount.saturating_mul(weight));
            }
            self.write_with_metering(&constraint_key, 2, U256::from(backlog), metered)?;
        }
        Ok(())
    }
}
