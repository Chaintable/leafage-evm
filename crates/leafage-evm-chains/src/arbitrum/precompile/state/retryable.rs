use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn retryable_timeout(
        &mut self,
        ticket_id: B256,
    ) -> Result<u64, PrecompileError> {
        let retryable_key = self.retryable_key(ticket_id);
        let timeout = self.read_u64(&retryable_key, 5)?;
        let now = self.context.block().timestamp().to::<u64>();
        if timeout == 0 {
            return Err(PrecompileError::other("NoTicketWithID"));
        }
        let windows_left = if self.arbos_version()? >= 60 {
            self.read_u64(&retryable_key, 6)?
        } else {
            0
        };
        let effective_timeout =
            timeout.saturating_add(windows_left.saturating_mul(RETRYABLE_LIFETIME_SECONDS));
        if effective_timeout < now {
            return Err(PrecompileError::other("NoTicketWithID"));
        }
        Ok(effective_timeout)
    }

    pub(in crate::arbitrum::precompile) fn retryable_beneficiary(
        &mut self,
        ticket_id: B256,
    ) -> Result<Address, PrecompileError> {
        self.retryable_timeout(ticket_id)?;
        let retryable_key = self.retryable_key(ticket_id);
        self.read_address(&retryable_key, 4)
    }

    pub(in crate::arbitrum::precompile) fn delete_retryable(
        &mut self,
        ticket_id: B256,
        beneficiary: Address,
    ) -> Result<(), PrecompileError> {
        let retryable_key = self.retryable_key(ticket_id);
        for offset in 0..=6 {
            self.write(&retryable_key, offset, U256::ZERO)?;
        }

        let calldata_key = arbos_state::child_key(&retryable_key, &[1]);
        let size = self.read_u64(&calldata_key, 0)?;
        let words = size.div_ceil(32);
        for offset in 0..=words {
            self.write(&calldata_key, offset, U256::ZERO)?;
        }

        let escrow = Self::retryable_escrow_address(ticket_id);
        let balance = self
            .context
            .journal_mut()
            .load_account(escrow)
            .map(|account| account.data.info.balance)
            .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
        if !balance.is_zero() {
            self.context
                .journal_mut()
                .transfer(escrow, beneficiary, balance)
                .map_err(|e| PrecompileError::other(format!("{e:?}")))?
                .map_or(Ok(()), |err| {
                    Err(PrecompileError::other(format!("{err:?}")))
                })?;
        }
        Ok(())
    }

    pub(in crate::arbitrum::precompile) fn keepalive_retryable(
        &mut self,
        ticket_id: B256,
    ) -> Result<u64, PrecompileError> {
        let retryable_key = self.live_retryable_key(ticket_id)?;
        let timeout = self.read_u64(&retryable_key, 5)?;
        let windows_left = self.read_u64(&retryable_key, 6)?;
        let timeout =
            timeout.saturating_add(windows_left.saturating_mul(RETRYABLE_LIFETIME_SECONDS));
        let limit_before_add = self
            .context
            .block()
            .timestamp()
            .to::<u64>()
            .saturating_add(RETRYABLE_LIFETIME_SECONDS);
        if timeout > limit_before_add {
            return Err(PrecompileError::other("timeout too far into the future"));
        }

        self.put_retryable_timeout(ticket_id)?;
        self.increment_retryable_timeout_windows(&retryable_key)?;
        Ok(timeout.saturating_add(RETRYABLE_LIFETIME_SECONDS))
    }

    fn put_retryable_timeout(&mut self, ticket_id: B256) -> Result<(), PrecompileError> {
        let retryables_key = arbos_state::child_key(&[], arbos_state::RETRYABLE_SUBSPACE);
        let timeout_queue_key = arbos_state::child_key(&retryables_key, &[0]);
        let next_put = self.read_u64(&timeout_queue_key, 0)?;
        let new_next_put = next_put
            .checked_add(1)
            .ok_or_else(|| PrecompileError::other("retryable timeout queue overflow"))?;
        self.write(&timeout_queue_key, 0, U256::from(new_next_put))?;
        self.write(
            &timeout_queue_key,
            next_put,
            U256::from_be_slice(ticket_id.as_slice()),
        )
    }

    fn increment_retryable_timeout_windows(
        &mut self,
        retryable_key: &[u8],
    ) -> Result<(), PrecompileError> {
        let windows_left = self.read_u64(retryable_key, 6)?;
        let next = windows_left
            .checked_add(1)
            .ok_or_else(|| PrecompileError::other("retryable timeout window overflow"))?;
        self.write(retryable_key, 6, U256::from(next))
    }

    pub(in crate::arbitrum::precompile) fn retryable_size_bytes(
        &mut self,
        ticket_id: B256,
    ) -> Result<u64, PrecompileError> {
        let retryable_key = match self.live_retryable_key(ticket_id) {
            Ok(retryable_key) => retryable_key,
            Err(PrecompileError::Other(reason)) if reason == "NoTicketWithID" => return Ok(0),
            Err(error) => return Err(error),
        };
        let calldata_key = arbos_state::child_key(&retryable_key, &[1]);
        let calldata_size = self.read_u64(&calldata_key, 0)?;
        Ok(6 * 32 + 32 + 32 * calldata_size.div_ceil(32))
    }

    pub(in crate::arbitrum::precompile) fn retryable_redeem_info(
        &mut self,
        ticket_id: B256,
    ) -> Result<RetryableRedeemInfo, PrecompileError> {
        let retryable_key = self.live_retryable_key(ticket_id)?;
        let tries = self.read_u64(&retryable_key, 0)?;
        let next_tries = tries
            .checked_add(1)
            .ok_or_else(|| PrecompileError::other("retryable num tries overflow"))?;
        self.write(&retryable_key, 0, U256::from(next_tries))?;

        let from = self.read_address(&retryable_key, 1)?;
        let to = self.read_address_or_nil(&retryable_key, 2)?;
        let value = self.read(&retryable_key, 3)?;
        let data = self.retryable_calldata(&retryable_key)?;

        Ok(RetryableRedeemInfo {
            nonce: tries,
            from,
            to,
            value,
            data,
        })
    }

    fn live_retryable_key(&mut self, ticket_id: B256) -> Result<[u8; 32], PrecompileError> {
        let retryable_key = self.retryable_key(ticket_id);
        let timeout = self.read_u64(&retryable_key, 5)?;
        if timeout == 0 {
            return Err(PrecompileError::other("NoTicketWithID"));
        }

        let now = self.context.block().timestamp().to::<u64>();
        if timeout < now {
            let mut effective_timeout = timeout;
            if self.arbos_version_unmetered()? >= ARBOS_VERSION_60 {
                let windows_left = self.read_u64(&retryable_key, 6)?;
                effective_timeout =
                    timeout.saturating_add(windows_left.saturating_mul(RETRYABLE_LIFETIME_SECONDS));
            }
            if effective_timeout < now {
                return Err(PrecompileError::other("NoTicketWithID"));
            }
        }

        Ok(retryable_key)
    }

    fn retryable_escrow_address(ticket_id: B256) -> Address {
        let hash = keccak256([b"retryable escrow".as_slice(), ticket_id.as_slice()].concat());
        Address::from_slice(&hash.as_slice()[12..])
    }
}
