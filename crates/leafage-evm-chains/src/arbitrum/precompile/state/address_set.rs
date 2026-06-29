use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn address_set_contains(
        &mut self,
        set_key: &[u8],
        addr: Address,
    ) -> Result<bool, PrecompileError> {
        let by_address_key = arbos_state::child_key(set_key, &[0]);
        Ok(!self.read_key(&by_address_key, address_key(addr))?.is_zero())
    }

    pub(in crate::arbitrum::precompile) fn address_set_members(
        &mut self,
        set_key: &[u8],
    ) -> Result<Vec<Address>, PrecompileError> {
        let size = self.read_u64(set_key, 0)?.min(MAX_GET_ALL_MEMBERS);
        let mut out = Vec::with_capacity(size as usize);
        for i in 0..size {
            out.push(address_from_word(self.read(set_key, i + 1)?));
        }
        Ok(out)
    }

    pub(in crate::arbitrum::precompile) fn address_set_add(
        &mut self,
        set_key: &[u8],
        addr: Address,
    ) -> Result<(), PrecompileError> {
        if self.address_set_contains(set_key, addr)? {
            return Ok(());
        }

        let size = self.read_u64(set_key, 0)?;
        let slot = size.saturating_add(1);
        let by_address_key = arbos_state::child_key(set_key, &[0]);
        let addr_word = U256::from_be_slice(addr.as_slice());
        self.write_key(&by_address_key, address_key(addr), U256::from(slot))?;
        self.write(set_key, slot, addr_word)?;
        self.write(set_key, 0, U256::from(slot))
    }

    pub(in crate::arbitrum::precompile) fn address_set_remove(
        &mut self,
        set_key: &[u8],
        addr: Address,
    ) -> Result<(), PrecompileError> {
        let by_address_key = arbos_state::child_key(set_key, &[0]);
        let slot = self
            .read_key(&by_address_key, address_key(addr))?
            .to::<u64>();
        if slot == 0 {
            return Ok(());
        }

        self.write_key(&by_address_key, address_key(addr), U256::ZERO)?;
        let size = self.read_u64(set_key, 0)?;
        if slot < size {
            let at_size = self.read(set_key, size)?;
            self.write(set_key, slot, at_size)?;
            if self.arbos_version()? >= 11 {
                let moved_addr = address_from_word(at_size);
                self.write_key(&by_address_key, address_key(moved_addr), U256::from(slot))?;
            }
        }
        self.write(set_key, size, U256::ZERO)?;
        self.write(set_key, 0, U256::from(size.saturating_sub(1)))
    }

    pub(in crate::arbitrum::precompile) fn address_set_rectify_mapping(
        &mut self,
        set_key: &[u8],
        addr: Address,
    ) -> Result<(), PrecompileError> {
        if !self.address_set_contains(set_key, addr)? {
            return Err(PrecompileError::other(
                "RectifyMapping: Address is not an owner",
            ));
        }

        let by_address_key = arbos_state::child_key(set_key, &[0]);
        let slot = self
            .read_key(&by_address_key, address_key(addr))?
            .to::<u64>();
        let at_slot = self.read(set_key, slot)?;
        let size = self.read_u64(set_key, 0)?;
        let addr_word = U256::from_be_slice(addr.as_slice());
        if at_slot == addr_word && slot <= size {
            return Err(PrecompileError::other(
                "RectifyMapping: Owner address is correctly mapped",
            ));
        }

        self.write_key(&by_address_key, address_key(addr), U256::ZERO)?;
        self.address_set_add(set_key, addr)
    }

    pub(in crate::arbitrum::precompile) fn batch_poster_exists(
        &mut self,
        poster: Address,
    ) -> Result<bool, PrecompileError> {
        let posters_key = self.batch_poster_addresses_key();
        self.address_set_contains(&posters_key, poster)
    }

    pub(in crate::arbitrum::precompile) fn batch_posters(
        &mut self,
    ) -> Result<Vec<Address>, PrecompileError> {
        let posters_key = self.batch_poster_addresses_key();
        self.address_set_members(&posters_key)
    }

    pub(in crate::arbitrum::precompile) fn add_batch_poster(
        &mut self,
        poster: Address,
        pay_to: Address,
    ) -> Result<(), PrecompileError> {
        if self.batch_poster_exists(poster)? {
            return Err(PrecompileError::other(
                "tried to add a batch poster that already exists",
            ));
        }

        let poster_key = self.batch_poster_info_key(poster);
        self.write(
            &poster_key,
            arbos_state::BATCH_POSTER_FUNDS_DUE_OFFSET,
            U256::ZERO,
        )?;
        self.set_batch_poster_pay_to_unchecked(poster, pay_to)?;
        let posters_key = self.batch_poster_addresses_key();
        self.address_set_add(&posters_key, poster)
    }

    pub(in crate::arbitrum::precompile) fn batch_poster_pay_to(
        &mut self,
        poster: Address,
    ) -> Result<Address, PrecompileError> {
        if !self.batch_poster_exists(poster)? {
            return Err(PrecompileError::other(
                "tried to open a batch poster that does not exist",
            ));
        }
        let poster_key = self.batch_poster_info_key(poster);
        self.read_address(&poster_key, arbos_state::BATCH_POSTER_PAY_TO_OFFSET)
    }

    pub(in crate::arbitrum::precompile) fn set_batch_poster_pay_to(
        &mut self,
        poster: Address,
        pay_to: Address,
    ) -> Result<(), PrecompileError> {
        if !self.batch_poster_exists(poster)? {
            return Err(PrecompileError::other(
                "tried to open a batch poster that does not exist",
            ));
        }
        self.set_batch_poster_pay_to_unchecked(poster, pay_to)
    }

    fn set_batch_poster_pay_to_unchecked(
        &mut self,
        poster: Address,
        pay_to: Address,
    ) -> Result<(), PrecompileError> {
        let poster_key = self.batch_poster_info_key(poster);
        self.write(
            &poster_key,
            arbos_state::BATCH_POSTER_PAY_TO_OFFSET,
            U256::from_be_slice(pay_to.as_slice()),
        )
    }
}
