use super::*;

impl<'a, DB: Database> ArbStorage<'a, ArbitrumContext<DB>> {
    pub(in crate::arbitrum::precompile) fn calldata_price_increase_enabled(
        &mut self,
    ) -> Result<bool, PrecompileError> {
        let features_key = arbos_state::child_key(&[], arbos_state::FEATURES_SUBSPACE);
        Ok(!(self.read(&features_key, 0)? & U256::from(1u8)).is_zero())
    }

    pub(in crate::arbitrum::precompile) fn collect_tips(
        &mut self,
    ) -> Result<bool, PrecompileError> {
        if self.arbos_version()? < 60 {
            return Ok(false);
        }
        Ok(!self.root(arbos_state::COLLECT_TIPS_OFFSET)?.is_zero())
    }

    pub(in crate::arbitrum::precompile) fn max_stylus_contract_fragments(
        &mut self,
    ) -> Result<u8, PrecompileError> {
        self.stylus_params().map(|params| params.max_fragment_count)
    }

    pub(in crate::arbitrum::precompile) fn stylus_params(
        &mut self,
    ) -> Result<StylusParams, PrecompileError> {
        let arbos_version = self.arbos_version()?;
        let params_key = self.stylus_params_key();
        self.burn(WARM_STORAGE_READ_GAS)?;
        let bytes = self
            .read_account_key_unmetered(
                arbos_state::ARBOS_STATE_ADDRESS,
                &params_key,
                U256::ZERO.to_be_bytes(),
            )?
            .to_be_bytes::<32>();
        Ok(Self::decode_stylus_params(bytes, arbos_version))
    }

    pub(in crate::arbitrum::precompile) fn stylus_params_concrete(
        &mut self,
        arbos_version: u64,
    ) -> Result<StylusParams, DB::Error> {
        let params_key = self.stylus_params_key();
        self.read_key_concrete(&params_key, U256::ZERO.to_be_bytes())
            .map(|value| Self::decode_stylus_params(value.to_be_bytes(), arbos_version))
    }

    fn decode_stylus_params(bytes: [u8; 32], arbos_version: u64) -> StylusParams {
        let mut cursor = 0usize;

        let take_u8 = |cursor: &mut usize| {
            let value = bytes[*cursor];
            *cursor += 1;
            value
        };
        let take_u16 = |cursor: &mut usize| {
            let value = u16::from_be_bytes([bytes[*cursor], bytes[*cursor + 1]]);
            *cursor += 2;
            value
        };
        let take_u24 = |cursor: &mut usize| {
            let value =
                u32::from_be_bytes([0, bytes[*cursor], bytes[*cursor + 1], bytes[*cursor + 2]]);
            *cursor += 3;
            value
        };
        let take_u32 = |cursor: &mut usize| {
            let value = u32::from_be_bytes([
                bytes[*cursor],
                bytes[*cursor + 1],
                bytes[*cursor + 2],
                bytes[*cursor + 3],
            ]);
            *cursor += 4;
            value
        };

        let version = take_u16(&mut cursor);
        let ink_price = take_u24(&mut cursor);
        let max_stack_depth = take_u32(&mut cursor);
        let free_pages = take_u16(&mut cursor);
        let page_gas = take_u16(&mut cursor);
        let page_limit = take_u16(&mut cursor);
        let min_init_gas = take_u8(&mut cursor);
        let min_cached_init_gas = take_u8(&mut cursor);
        let init_cost_scalar = take_u8(&mut cursor);
        let cached_cost_scalar = take_u8(&mut cursor);
        let expiry_days = take_u16(&mut cursor);
        let keepalive_days = take_u16(&mut cursor);
        let block_cache_size = take_u16(&mut cursor);
        let max_wasm_size = if arbos_version >= 40 {
            take_u32(&mut cursor)
        } else {
            128 * 1024
        };
        let max_fragment_count = if arbos_version >= 60 {
            take_u8(&mut cursor)
        } else {
            0
        };

        StylusParams {
            version,
            ink_price,
            max_stack_depth,
            free_pages,
            page_gas,
            page_limit,
            min_init_gas,
            min_cached_init_gas,
            init_cost_scalar,
            cached_cost_scalar,
            expiry_days,
            keepalive_days,
            block_cache_size,
            max_wasm_size,
            max_fragment_count,
        }
    }

    pub(in crate::arbitrum::precompile) fn save_stylus_params(
        &mut self,
        params: StylusParams,
    ) -> Result<(), PrecompileError> {
        let arbos_version = self.arbos_version()?;
        let mut bytes = [0u8; 32];
        let mut cursor = 0usize;

        Self::put_u16(&mut bytes, &mut cursor, params.version);
        Self::put_u24(&mut bytes, &mut cursor, params.ink_price);
        Self::put_u32(&mut bytes, &mut cursor, params.max_stack_depth);
        Self::put_u16(&mut bytes, &mut cursor, params.free_pages);
        Self::put_u16(&mut bytes, &mut cursor, params.page_gas);
        Self::put_u16(&mut bytes, &mut cursor, params.page_limit);
        Self::put_u8(&mut bytes, &mut cursor, params.min_init_gas);
        Self::put_u8(&mut bytes, &mut cursor, params.min_cached_init_gas);
        Self::put_u8(&mut bytes, &mut cursor, params.init_cost_scalar);
        Self::put_u8(&mut bytes, &mut cursor, params.cached_cost_scalar);
        Self::put_u16(&mut bytes, &mut cursor, params.expiry_days);
        Self::put_u16(&mut bytes, &mut cursor, params.keepalive_days);
        Self::put_u16(&mut bytes, &mut cursor, params.block_cache_size);
        if arbos_version >= 40 {
            Self::put_u32(&mut bytes, &mut cursor, params.max_wasm_size);
        }
        if arbos_version >= 60 {
            Self::put_u8(&mut bytes, &mut cursor, params.max_fragment_count);
        }

        let params_key = self.stylus_params_key();
        self.write(&params_key, 0, U256::from_be_bytes(bytes))
    }

    pub(in crate::arbitrum::precompile) fn wasm_activation_gas(
        &mut self,
    ) -> Result<u64, PrecompileError> {
        if self.arbos_version()? < 59 {
            return Ok(0);
        }
        let key = self.wasm_activation_gas_key();
        self.read_u64(&key, 0)
    }

    pub(in crate::arbitrum::precompile) fn account_code_hash(
        &mut self,
        account: Address,
    ) -> Result<B256, PrecompileError> {
        // Nitro's StateDB.GetCodeHash does not add the account to the EIP-2929
        // access list. Revert the journal entry created by revm's account load
        // so activation does not make the following program call warm.
        let checkpoint = self.context.journal_mut().checkpoint();
        let result = {
            self.context
                .journal_mut()
                .load_account(account)
                .map(|account| {
                    if account.data.is_selfdestructed() {
                        KECCAK_EMPTY
                    } else {
                        account
                            .data
                            .info
                            .code
                            .as_ref()
                            .map(|code| code.hash_slow())
                            .unwrap_or(account.data.info.code_hash)
                    }
                })
        };
        self.context.journal_mut().checkpoint_revert(checkpoint);
        result.map_err(fatal_db_error)
    }

    pub(in crate::arbitrum::precompile) fn account_code_and_hash(
        &mut self,
        account: Address,
    ) -> Result<(Bytes, B256), PrecompileError> {
        // Nitro's StateDB.GetCode/GetCodeHash reads do not warm the program
        // account. Keep revm's loaded code, but roll back its access-list side
        // effect before returning the owned values.
        let checkpoint = self.context.journal_mut().checkpoint();
        let result = {
            self.context
                .journal_mut()
                .load_account_with_code(account)
                .map(|loaded| {
                    let is_selfdestructed = loaded.data.is_selfdestructed();
                    let (code, code_hash) = loaded
                        .data
                        .info
                        .code
                        .as_ref()
                        .map(|code| (code.original_bytes(), code.hash_slow()))
                        .unwrap_or((Bytes::new(), loaded.data.info.code_hash));
                    (code, code_hash, is_selfdestructed)
                })
        };
        self.context.journal_mut().checkpoint_revert(checkpoint);

        let (code, code_hash, is_selfdestructed) = result.map_err(fatal_db_error)?;
        if is_selfdestructed {
            return Err(PrecompileError::other("self destructed"));
        }
        Ok((code, code_hash))
    }

    pub(in crate::arbitrum::precompile) fn account_code(
        &mut self,
        account: Address,
    ) -> Result<(Bytes, bool), PrecompileError> {
        self.context
            .journal_mut()
            .code(account)
            .map(|load| (load.data, load.is_cold))
            .map_err(fatal_db_error)
    }

    pub(in crate::arbitrum::precompile) fn code_by_hash(
        &self,
        code_hash: B256,
    ) -> Result<Bytes, PrecompileError>
    where
        DB: DatabaseRef,
    {
        for account in self.context.journal().state.values() {
            if account.is_selfdestructed() || account.info.code_hash != code_hash {
                continue;
            }
            if let Some(code) = &account.info.code {
                let bytes = code.original_bytes();
                if !bytes.is_empty() {
                    return Ok(bytes);
                }
            }
        }
        self.context
            .db()
            .code_by_hash_ref(code_hash)
            .map(|code| code.original_bytes())
            .map_err(fatal_db_error)
    }

    pub(in crate::arbitrum::precompile) fn account_is_warm(&self, account: Address) -> bool {
        let journal = self.context.journal();
        journal.warm_addresses.is_warm(&account)
            || journal
                .state
                .get(&account)
                .is_some_and(|account| !account.is_cold_transaction_id(journal.transaction_id))
    }

    pub(in crate::arbitrum::precompile) fn wasm_program(
        &mut self,
        code_hash: B256,
    ) -> Result<WasmProgram, PrecompileError> {
        let programs_key = self.wasm_programs_key();
        let bytes = self
            .read_key(&programs_key, code_hash.0)?
            .to_be_bytes::<32>();
        Ok(Self::decode_wasm_program(bytes))
    }

    pub(in crate::arbitrum::precompile) fn wasm_program_concrete(
        &mut self,
        code_hash: B256,
    ) -> Result<WasmProgram, DB::Error> {
        let programs_key = self.wasm_programs_key();
        self.read_key_concrete(&programs_key, code_hash.0)
            .map(|value| Self::decode_wasm_program(value.to_be_bytes()))
    }

    fn decode_wasm_program(bytes: [u8; 32]) -> WasmProgram {
        WasmProgram {
            version: u16::from_be_bytes([bytes[0], bytes[1]]),
            init_cost: u16::from_be_bytes([bytes[2], bytes[3]]),
            cached_cost: u16::from_be_bytes([bytes[4], bytes[5]]),
            footprint: u16::from_be_bytes([bytes[6], bytes[7]]),
            activated_at: u32::from_be_bytes([0, bytes[8], bytes[9], bytes[10]]),
            asm_estimate_kb: u32::from_be_bytes([0, bytes[11], bytes[12], bytes[13]]),
            cached: bytes[14] != 0,
        }
    }

    pub(in crate::arbitrum::precompile) fn active_wasm_program(
        &mut self,
        code_hash: B256,
        timestamp: u64,
        params: StylusParams,
    ) -> Result<WasmProgram, StylusProgramError> {
        let program = self.wasm_program(code_hash)?;
        if program.version == 0 {
            return Err(StylusProgramError::ProgramNotActivated);
        }
        if program.version != params.version {
            return Err(StylusProgramError::ProgramNeedsUpgrade {
                version: program.version,
                stylus_version: params.version,
            });
        }

        let age = Self::program_age(timestamp, program.activated_at);
        let expiry = u64::from(params.expiry_days) * 24 * 60 * 60;
        if age > expiry {
            return Err(StylusProgramError::ProgramExpired { age });
        }

        Ok(program)
    }

    pub(in crate::arbitrum::precompile) fn wasm_program_age(
        &self,
        timestamp: u64,
        program: WasmProgram,
    ) -> u64 {
        Self::program_age(timestamp, program.activated_at)
    }

    pub(in crate::arbitrum::precompile) fn save_wasm_program(
        &mut self,
        code_hash: B256,
        program: WasmProgram,
    ) -> Result<(), PrecompileError> {
        let programs_key = self.wasm_programs_key();
        let mut bytes = [0u8; 32];
        let mut cursor = 0usize;
        Self::put_u16(&mut bytes, &mut cursor, program.version);
        Self::put_u16(&mut bytes, &mut cursor, program.init_cost);
        Self::put_u16(&mut bytes, &mut cursor, program.cached_cost);
        Self::put_u16(&mut bytes, &mut cursor, program.footprint);
        Self::put_u24(&mut bytes, &mut cursor, program.activated_at);
        Self::put_u24(&mut bytes, &mut cursor, program.asm_estimate_kb);
        Self::put_u8(&mut bytes, &mut cursor, u8::from(program.cached));
        self.write_key(&programs_key, code_hash.0, U256::from_be_bytes(bytes))
    }

    pub(in crate::arbitrum::precompile) fn save_wasm_module_hash(
        &mut self,
        code_hash: B256,
        module_hash: B256,
    ) -> Result<(), PrecompileError> {
        let module_hashes_key = self.wasm_module_hashes_key();
        self.write_key(
            &module_hashes_key,
            code_hash.0,
            U256::from_be_bytes(module_hash.0),
        )
    }

    pub(in crate::arbitrum::precompile) fn wasm_module_hash_concrete(
        &mut self,
        code_hash: B256,
    ) -> Result<B256, DB::Error> {
        let module_hashes_key = self.wasm_module_hashes_key();
        self.read_key_concrete(&module_hashes_key, code_hash.0)
            .map(|value| B256::from(value.to_be_bytes::<32>()))
    }

    pub(in crate::arbitrum::precompile) fn save_activated_wasm_program(
        &mut self,
        code_hash: B256,
        params: StylusParams,
        activation: WasmActivation,
        timestamp: u64,
        cached: bool,
    ) -> Result<U256, PrecompileError> {
        let asm_estimate_kb = activation.asm_estimate.div_ceil(1024);
        if asm_estimate_kb > MAX_UINT24 {
            return Err(PrecompileError::other("wasm asm estimate exceeds uint24"));
        }

        self.save_wasm_module_hash(code_hash, activation.module_hash)?;
        let data_fee = self.update_wasm_data_price(activation.asm_estimate, timestamp)?;
        self.save_wasm_program(
            code_hash,
            WasmProgram {
                version: params.version,
                init_cost: activation.init_cost,
                cached_cost: activation.cached_cost,
                footprint: activation.footprint,
                activated_at: Self::hours_since_arbitrum(timestamp),
                asm_estimate_kb,
                cached,
            },
        )?;
        Ok(data_fee)
    }

    pub(in crate::arbitrum::precompile) fn keepalive_wasm_program(
        &mut self,
        code_hash: B256,
        timestamp: u64,
        params: StylusParams,
    ) -> Result<U256, StylusProgramError> {
        let mut program = self.active_wasm_program(code_hash, timestamp, params)?;
        let age = Self::program_age(timestamp, program.activated_at);
        let keepalive_age = u64::from(params.keepalive_days) * 24 * 60 * 60;
        if age < keepalive_age {
            return Err(StylusProgramError::ProgramKeepaliveTooSoon { age });
        }

        let data_fee =
            self.update_wasm_data_price(program.asm_estimate_kb.saturating_mul(1024), timestamp)?;
        program.activated_at = Self::hours_since_arbitrum(timestamp);
        self.save_wasm_program(code_hash, program)?;
        Ok(data_fee)
    }

    pub(in crate::arbitrum::precompile) fn update_wasm_data_price(
        &mut self,
        temp_bytes: u32,
        timestamp: u64,
    ) -> Result<U256, PrecompileError> {
        const DEMAND_OFFSET: u64 = 0;
        const BYTES_PER_SECOND_OFFSET: u64 = 1;
        const LAST_UPDATE_TIME_OFFSET: u64 = 2;
        const MIN_PRICE_OFFSET: u64 = 3;
        const INERTIA_OFFSET: u64 = 4;

        let key = self.wasm_data_pricer_key();
        let demand = self.read_u64(&key, DEMAND_OFFSET)?.min(u64::from(u32::MAX)) as u32;
        let bytes_per_second = self
            .read_u64(&key, BYTES_PER_SECOND_OFFSET)?
            .min(u64::from(u32::MAX)) as u32;
        let last_update_time = self.read_u64(&key, LAST_UPDATE_TIME_OFFSET)?;
        let min_price = self
            .read_u64(&key, MIN_PRICE_OFFSET)?
            .min(u64::from(u32::MAX)) as u32;
        let inertia = self
            .read_u64(&key, INERTIA_OFFSET)?
            .min(u64::from(u32::MAX)) as u32;
        if inertia == 0 {
            return Err(PrecompileError::other("wasm data pricer inertia is zero"));
        }

        let passed = timestamp
            .saturating_sub(last_update_time)
            .min(u64::from(u32::MAX)) as u32;
        let credit = bytes_per_second.saturating_mul(passed);
        let demand = demand.saturating_sub(credit).saturating_add(temp_bytes);

        self.write(&key, DEMAND_OFFSET, U256::from(demand))?;
        self.write(&key, LAST_UPDATE_TIME_OFFSET, U256::from(timestamp))?;

        let exponent = 10_000u64.saturating_mul(u64::from(demand)) / u64::from(inertia);
        let multiplier = Self::approx_exp_basis_points(exponent, 12);
        let cost_per_byte = u64::from(min_price).saturating_mul(multiplier) / 10_000;
        Ok(U256::from(
            cost_per_byte.saturating_mul(u64::from(temp_bytes)),
        ))
    }

    pub(in crate::arbitrum::precompile) fn wasm_program_cached(
        &mut self,
        code_hash: B256,
    ) -> Result<bool, PrecompileError> {
        self.wasm_program(code_hash).map(|program| program.cached)
    }

    pub(in crate::arbitrum::precompile) fn set_wasm_program_cached(
        &mut self,
        code_hash: B256,
        cached: bool,
    ) -> Result<(), PrecompileError> {
        let programs_key = self.wasm_programs_key();
        let mut bytes = self
            .read_key(&programs_key, code_hash.0)?
            .to_be_bytes::<32>();
        bytes[14] = u8::from(cached);
        self.write_key(&programs_key, code_hash.0, U256::from_be_bytes(bytes))
    }

    fn put_u8(bytes: &mut [u8; 32], cursor: &mut usize, value: u8) {
        bytes[*cursor] = value;
        *cursor += 1;
    }

    fn put_u16(bytes: &mut [u8; 32], cursor: &mut usize, value: u16) {
        bytes[*cursor..*cursor + 2].copy_from_slice(&value.to_be_bytes());
        *cursor += 2;
    }

    fn put_u24(bytes: &mut [u8; 32], cursor: &mut usize, value: u32) {
        let encoded = value.to_be_bytes();
        bytes[*cursor..*cursor + 3].copy_from_slice(&encoded[1..]);
        *cursor += 3;
    }

    fn put_u32(bytes: &mut [u8; 32], cursor: &mut usize, value: u32) {
        bytes[*cursor..*cursor + 4].copy_from_slice(&value.to_be_bytes());
        *cursor += 4;
    }

    fn hours_since_arbitrum(timestamp: u64) -> u32 {
        timestamp
            .saturating_sub(ARBITRUM_START_TIME)
            .checked_div(3600)
            .unwrap_or_default()
            .min(0x00ff_ffff) as u32
    }

    fn program_age(timestamp: u64, activated_at_hours: u32) -> u64 {
        let seconds = u64::from(activated_at_hours).saturating_mul(3600);
        let activated_at = ARBITRUM_START_TIME.saturating_add(seconds);
        timestamp.saturating_sub(activated_at)
    }

    fn approx_exp_basis_points(value: u64, accuracy: u64) -> u64 {
        const ONE_IN_BIPS: u64 = 10_000;

        if accuracy == 0 {
            return ONE_IN_BIPS;
        }

        let mut res = ONE_IN_BIPS + value / accuracy;
        for i in (1..accuracy).rev() {
            res = ONE_IN_BIPS
                + res
                    .saturating_mul(value)
                    .checked_div(i.saturating_mul(ONE_IN_BIPS))
                    .unwrap_or(u64::MAX);
        }
        res.min(i64::MAX as u64)
    }
}
