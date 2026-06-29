use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn send_merkle_append(
        &mut self,
        item_hash: B256,
    ) -> Result<(u64, Vec<MerkleUpdate>), PrecompileError> {
        let merkle_key = self.send_merkle_key();
        let size = self.read_u64(&merkle_key, 0)?.saturating_add(1);
        self.write(&merkle_key, 0, U256::from(size))?;

        let mut events = Vec::new();
        let mut level = 0u64;
        let mut so_far = keccak256(item_hash.as_slice());

        loop {
            if level == Self::merkle_num_partials(size.saturating_sub(1)) {
                self.write_b256(&merkle_key, 2 + level, so_far)?;
                break;
            }

            let this_level = self.read_b256(&merkle_key, 2 + level)?;
            if this_level.is_zero() {
                self.write_b256(&merkle_key, 2 + level, so_far)?;
                break;
            }

            so_far = Self::hash_pair(this_level, so_far);
            self.write_b256(&merkle_key, 2 + level, B256::ZERO)?;
            level = level.saturating_add(1);
            events.push(MerkleUpdate {
                level,
                num_leaves: size.saturating_sub(1),
                hash: so_far,
            });
        }

        Ok((size, events))
    }

    pub(in crate::arbitrum::precompile) fn send_merkle_state(
        &mut self,
    ) -> Result<(u64, B256, Vec<B256>), PrecompileError> {
        let merkle_key = self.send_merkle_key();
        let size = self.read_u64(&merkle_key, 0)?;
        if size == 0 {
            return Ok((0, B256::ZERO, Vec::new()));
        }

        let num_partials = Self::merkle_num_partials(size);
        let mut partials = Vec::with_capacity(num_partials as usize);
        for level in 0..num_partials {
            partials.push(self.read_b256(&merkle_key, 2 + level)?);
        }

        let root = Self::merkle_root_from_partials(&partials);
        Ok((size, root, partials))
    }

    fn read_b256(&mut self, storage_key: &[u8], offset: u64) -> Result<B256, PrecompileError> {
        Ok(B256::from(
            self.read(storage_key, offset)?.to_be_bytes::<32>(),
        ))
    }

    fn write_b256(
        &mut self,
        storage_key: &[u8],
        offset: u64,
        value: B256,
    ) -> Result<(), PrecompileError> {
        self.write(storage_key, offset, U256::from_be_bytes(value.0))
    }

    fn merkle_num_partials(size: u64) -> u64 {
        u64::BITS as u64 - u64::from(size.leading_zeros())
    }

    fn hash_pair(left: B256, right: B256) -> B256 {
        let mut data = [0u8; 64];
        data[..32].copy_from_slice(left.as_slice());
        data[32..].copy_from_slice(right.as_slice());
        keccak256(data)
    }

    fn merkle_root_from_partials(partials: &[B256]) -> B256 {
        let mut hash_so_far = None;
        let mut capacity_in_hash = 0u64;
        let mut capacity = 1u64;

        for partial in partials {
            if !partial.is_zero() {
                if let Some(mut hash) = hash_so_far {
                    while capacity_in_hash < capacity {
                        hash = Self::hash_pair(hash, B256::ZERO);
                        capacity_in_hash = capacity_in_hash.saturating_mul(2);
                    }
                    hash_so_far = Some(Self::hash_pair(*partial, hash));
                    capacity_in_hash = 2 * capacity;
                } else {
                    hash_so_far = Some(*partial);
                    capacity_in_hash = capacity;
                }
            }
            capacity = capacity.saturating_mul(2);
        }

        hash_so_far.unwrap_or_default()
    }
}
