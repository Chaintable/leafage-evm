use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn address_table_size(
        &mut self,
    ) -> Result<U256, PrecompileError> {
        let table_key = self.address_table_key();
        self.read(&table_key, 0)
    }

    pub(in crate::arbitrum::precompile) fn address_table_lookup(
        &mut self,
        addr: Address,
    ) -> Result<Option<u64>, PrecompileError> {
        let table_key = self.address_table_key();
        let by_address_key = arbos_state::child_key(&table_key, &[]);
        let value = self.read_key(&by_address_key, address_key(addr))?;
        if value.is_zero() {
            Ok(None)
        } else {
            Ok(Some(value.to::<u64>().saturating_sub(1)))
        }
    }

    pub(in crate::arbitrum::precompile) fn address_table_lookup_index(
        &mut self,
        index: u64,
    ) -> Result<Option<Address>, PrecompileError> {
        let table_key = self.address_table_key();
        let size = self.read(&table_key, 0)?;
        if U256::from(index) >= size {
            return Ok(None);
        }
        let word = self.read(&table_key, index.saturating_add(1))?;
        Ok(Some(address_from_word(word)))
    }

    pub(in crate::arbitrum::precompile) fn address_table_compress(
        &mut self,
        addr: Address,
    ) -> Result<Vec<u8>, PrecompileError> {
        let mut out = Vec::new();
        if let Some(index) = self.address_table_lookup(addr)? {
            index.encode(&mut out);
        } else {
            addr.as_slice().encode(&mut out);
        }
        Ok(out)
    }

    pub(in crate::arbitrum::precompile) fn address_table_register(
        &mut self,
        addr: Address,
    ) -> Result<u64, PrecompileError> {
        let table_key = self.address_table_key();
        let by_address_key = arbos_state::child_key(&table_key, &[]);
        let value = self.read_key(&by_address_key, address_key(addr))?;
        if !value.is_zero() {
            return Ok(value.to::<u64>().saturating_sub(1));
        }

        let new_size = self.read_u64(&table_key, 0)?.saturating_add(1);
        let addr_word = U256::from_be_slice(addr.as_slice());
        self.write(&table_key, 0, U256::from(new_size))?;
        self.write(&table_key, new_size, addr_word)?;
        self.write_key(&by_address_key, address_key(addr), U256::from(new_size))?;
        Ok(new_size.saturating_sub(1))
    }

    pub(in crate::arbitrum::precompile) fn address_table_decompress(
        &mut self,
        buf: &[u8],
    ) -> Result<(Address, u64), PrecompileError> {
        let before = buf.len();
        let mut reader = buf;
        if let Ok(bytes) = Header::decode_bytes(&mut reader, false) {
            if bytes.len() == 20 {
                return Ok((Address::from_slice(bytes), (before - reader.len()) as u64));
            }
        }

        let mut reader = buf;
        let index = u64::decode(&mut reader).map_err(|e| PrecompileError::other(e.to_string()))?;
        let consumed = (before - reader.len()) as u64;
        let addr = self
            .address_table_lookup_index(index)?
            .ok_or_else(|| PrecompileError::other("invalid index in compressed address"))?;
        Ok((addr, consumed))
    }
}
