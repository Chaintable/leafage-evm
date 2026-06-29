use revm::precompile::PrecompileError;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

type JsonObject = BTreeMap<String, Box<RawValue>>;

#[derive(Clone, Debug)]
pub(super) struct NitroChainConfig {
    chain_id: Option<JsonBigInt>,
    homestead_block: Option<JsonBigInt>,
    dao_fork_block: Option<JsonBigInt>,
    dao_fork_support: bool,
    eip150_block: Option<JsonBigInt>,
    eip155_block: Option<JsonBigInt>,
    eip158_block: Option<JsonBigInt>,
    byzantium_block: Option<JsonBigInt>,
    constantinople_block: Option<JsonBigInt>,
    petersburg_block: Option<JsonBigInt>,
    istanbul_block: Option<JsonBigInt>,
    muir_glacier_block: Option<JsonBigInt>,
    berlin_block: Option<JsonBigInt>,
    london_block: Option<JsonBigInt>,
    arrow_glacier_block: Option<JsonBigInt>,
    gray_glacier_block: Option<JsonBigInt>,
    merge_netsplit_block: Option<JsonBigInt>,
    shanghai_time: Option<u64>,
    cancun_time: Option<u64>,
    prague_time: Option<u64>,
    osaka_time: Option<u64>,
    verkle_time: Option<u64>,
    bpo1_time: Option<u64>,
    bpo2_time: Option<u64>,
    bpo3_time: Option<u64>,
    bpo4_time: Option<u64>,
    bpo5_time: Option<u64>,
    amsterdam_time: Option<u64>,
    arbitrum: NitroArbitrumChainParams,
}

#[derive(Clone, Debug, Default)]
struct NitroArbitrumChainParams {
    enable_arb_os: bool,
    genesis_block_num: u64,
    max_uncompressed_batch_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JsonBigInt {
    normalized: String,
    u64_value: Option<u64>,
    is_negative: bool,
}

impl NitroChainConfig {
    pub(super) fn parse(serialized: &[u8]) -> Result<Self, PrecompileError> {
        let object: JsonObject = serde_json::from_slice(serialized).map_err(|err| {
            PrecompileError::other(format!("invalid chain config, can't deserialize: {err}"))
        })?;
        Self::validate_known_fields(&object)?;

        Ok(Self {
            chain_id: Self::field_big_int_any(&object, &["chainId", "chainID", "ChainID"])?,
            homestead_block: Self::field_big_int(&object, "homesteadBlock")?,
            dao_fork_block: Self::field_big_int(&object, "daoForkBlock")?,
            dao_fork_support: Self::field_bool(&object, "daoForkSupport")?,
            eip150_block: Self::field_big_int(&object, "eip150Block")?,
            eip155_block: Self::field_big_int(&object, "eip155Block")?,
            eip158_block: Self::field_big_int(&object, "eip158Block")?,
            byzantium_block: Self::field_big_int(&object, "byzantiumBlock")?,
            constantinople_block: Self::field_big_int(&object, "constantinopleBlock")?,
            petersburg_block: Self::field_big_int(&object, "petersburgBlock")?,
            istanbul_block: Self::field_big_int(&object, "istanbulBlock")?,
            muir_glacier_block: Self::field_big_int(&object, "muirGlacierBlock")?,
            berlin_block: Self::field_big_int(&object, "berlinBlock")?,
            london_block: Self::field_big_int(&object, "londonBlock")?,
            arrow_glacier_block: Self::field_big_int(&object, "arrowGlacierBlock")?,
            gray_glacier_block: Self::field_big_int(&object, "grayGlacierBlock")?,
            merge_netsplit_block: Self::field_big_int(&object, "mergeNetsplitBlock")?,
            shanghai_time: Self::field_u64(&object, "shanghaiTime")?,
            cancun_time: Self::field_u64(&object, "cancunTime")?,
            prague_time: Self::field_u64(&object, "pragueTime")?,
            osaka_time: Self::field_u64(&object, "osakaTime")?,
            verkle_time: Self::field_u64(&object, "verkleTime")?,
            bpo1_time: Self::field_u64(&object, "bpo1Time")?,
            bpo2_time: Self::field_u64(&object, "bpo2Time")?,
            bpo3_time: Self::field_u64(&object, "bpo3Time")?,
            bpo4_time: Self::field_u64(&object, "bpo4Time")?,
            bpo5_time: Self::field_u64(&object, "bpo5Time")?,
            amsterdam_time: Self::field_u64(&object, "amsterdamTime")?,
            arbitrum: NitroArbitrumChainParams::parse(&object)?,
        })
    }

    pub(super) fn ensure_chain_id(&self, current_chain_id: u64) -> Result<(), PrecompileError> {
        let chain_id = self
            .chain_id
            .as_ref()
            .ok_or_else(|| PrecompileError::other("chain config missing chainId"))?;
        if !chain_id.equals_u64(current_chain_id) {
            return Err(PrecompileError::other(format!(
                "chain config chainId {} does not match current chainId {current_chain_id}",
                chain_id.normalized
            )));
        }
        Ok(())
    }

    pub(super) fn check_compatible(
        &self,
        new_config: &Self,
        head_number: u64,
        head_timestamp: u64,
    ) -> Result<(), PrecompileError> {
        self.ensure_block_compatible(
            "Homestead fork block",
            &self.homestead_block,
            &new_config.homestead_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "DAO fork block",
            &self.dao_fork_block,
            &new_config.dao_fork_block,
            head_number,
        )?;
        if Self::is_block_forked(self.dao_fork_block.as_ref(), head_number)
            && self.dao_fork_support != new_config.dao_fork_support
        {
            return Self::compat_error("DAO fork support flag");
        }
        self.ensure_block_compatible(
            "EIP150 fork block",
            &self.eip150_block,
            &new_config.eip150_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "EIP155 fork block",
            &self.eip155_block,
            &new_config.eip155_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "EIP158 fork block",
            &self.eip158_block,
            &new_config.eip158_block,
            head_number,
        )?;
        if Self::is_block_forked(self.eip158_block.as_ref(), head_number)
            && self.chain_id != new_config.chain_id
        {
            return Self::compat_error("EIP158 chain ID");
        }

        self.check_arbitrum_compatible(new_config)?;
        self.ensure_block_compatible(
            "Byzantium fork block",
            &self.byzantium_block,
            &new_config.byzantium_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "Constantinople fork block",
            &self.constantinople_block,
            &new_config.constantinople_block,
            head_number,
        )?;
        if Self::is_fork_block_incompatible(
            self.petersburg_block.as_ref(),
            new_config.petersburg_block.as_ref(),
            head_number,
        ) && Self::is_fork_block_incompatible(
            self.constantinople_block.as_ref(),
            new_config.petersburg_block.as_ref(),
            head_number,
        ) {
            return Self::compat_error("Petersburg fork block");
        }
        self.ensure_block_compatible(
            "Istanbul fork block",
            &self.istanbul_block,
            &new_config.istanbul_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "Muir Glacier fork block",
            &self.muir_glacier_block,
            &new_config.muir_glacier_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "Berlin fork block",
            &self.berlin_block,
            &new_config.berlin_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "London fork block",
            &self.london_block,
            &new_config.london_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "Arrow Glacier fork block",
            &self.arrow_glacier_block,
            &new_config.arrow_glacier_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "Gray Glacier fork block",
            &self.gray_glacier_block,
            &new_config.gray_glacier_block,
            head_number,
        )?;
        self.ensure_block_compatible(
            "Merge netsplit fork block",
            &self.merge_netsplit_block,
            &new_config.merge_netsplit_block,
            head_number,
        )?;
        self.ensure_timestamp_compatible(
            "Shanghai fork timestamp",
            self.shanghai_time,
            new_config.shanghai_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "Cancun fork timestamp",
            self.cancun_time,
            new_config.cancun_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "Prague fork timestamp",
            self.prague_time,
            new_config.prague_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "Osaka fork timestamp",
            self.osaka_time,
            new_config.osaka_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "Verkle fork timestamp",
            self.verkle_time,
            new_config.verkle_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "BPO1 fork timestamp",
            self.bpo1_time,
            new_config.bpo1_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "BPO2 fork timestamp",
            self.bpo2_time,
            new_config.bpo2_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "BPO3 fork timestamp",
            self.bpo3_time,
            new_config.bpo3_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "BPO4 fork timestamp",
            self.bpo4_time,
            new_config.bpo4_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "BPO5 fork timestamp",
            self.bpo5_time,
            new_config.bpo5_time,
            head_timestamp,
        )?;
        self.ensure_timestamp_compatible(
            "Amsterdam fork timestamp",
            self.amsterdam_time,
            new_config.amsterdam_time,
            head_timestamp,
        )
    }

    fn check_arbitrum_compatible(&self, new_config: &Self) -> Result<(), PrecompileError> {
        if self.arbitrum.enable_arb_os != new_config.arbitrum.enable_arb_os {
            return Self::compat_error("isArbitrum");
        }
        if !self.arbitrum.enable_arb_os {
            return Ok(());
        }
        if self.arbitrum.genesis_block_num != new_config.arbitrum.genesis_block_num {
            return Self::compat_error("genesisblocknum");
        }
        if self.arbitrum.max_uncompressed_batch_size
            != new_config.arbitrum.max_uncompressed_batch_size
        {
            return Self::compat_error("maxuncompressedbatchsize");
        }
        Ok(())
    }

    fn ensure_block_compatible(
        &self,
        what: &'static str,
        stored: &Option<JsonBigInt>,
        new: &Option<JsonBigInt>,
        head: u64,
    ) -> Result<(), PrecompileError> {
        if Self::is_fork_block_incompatible(stored.as_ref(), new.as_ref(), head) {
            Self::compat_error(what)
        } else {
            Ok(())
        }
    }

    fn ensure_timestamp_compatible(
        &self,
        what: &'static str,
        stored: Option<u64>,
        new: Option<u64>,
        head: u64,
    ) -> Result<(), PrecompileError> {
        if (Self::is_timestamp_forked(stored, head) || Self::is_timestamp_forked(new, head))
            && stored != new
        {
            Self::compat_error(what)
        } else {
            Ok(())
        }
    }

    fn is_fork_block_incompatible(
        stored: Option<&JsonBigInt>,
        new: Option<&JsonBigInt>,
        head: u64,
    ) -> bool {
        (Self::is_block_forked(stored, head) || Self::is_block_forked(new, head)) && stored != new
    }

    fn is_block_forked(fork_block: Option<&JsonBigInt>, head: u64) -> bool {
        fork_block.is_some_and(|fork_block| fork_block.is_forked(head))
    }

    fn is_timestamp_forked(fork_time: Option<u64>, head: u64) -> bool {
        fork_time.is_some_and(|fork_time| fork_time <= head)
    }

    fn compat_error<T>(what: &'static str) -> Result<T, PrecompileError> {
        Err(PrecompileError::other(format!(
            "invalid chain config, not compatible with previous: {what}"
        )))
    }

    fn validate_known_fields(object: &JsonObject) -> Result<(), PrecompileError> {
        Self::ensure_big_int_field(object, "terminalTotalDifficulty")?;
        Self::ensure_bool_field(object, "enableVerkleAtGenesis")?;
        Self::ensure_address_field(object, "depositContractAddress")?;
        Self::ensure_object_field(object, "ethash")?;
        Self::validate_clique_field(object)?;
        Self::validate_blob_schedule_field(object)
    }

    fn validate_clique_field(object: &JsonObject) -> Result<(), PrecompileError> {
        let Some(value) = object.get("clique") else {
            return Ok(());
        };
        let Some(clique) = Self::value_as_optional_object(value.as_ref(), "clique")? else {
            return Ok(());
        };
        Self::ensure_u64_field(&clique, "period")?;
        Self::ensure_u64_field(&clique, "epoch")
    }

    fn validate_blob_schedule_field(object: &JsonObject) -> Result<(), PrecompileError> {
        let Some(value) = object.get("blobSchedule") else {
            return Ok(());
        };
        let Some(schedule) = Self::value_as_optional_object(value.as_ref(), "blobSchedule")? else {
            return Ok(());
        };
        for name in [
            "cancun",
            "prague",
            "osaka",
            "verkle",
            "bpo1",
            "bpo2",
            "bpo3",
            "bpo4",
            "bpo5",
            "amsterdam",
        ] {
            Self::validate_blob_config_field(&schedule, name)?;
        }
        Ok(())
    }

    fn validate_blob_config_field(
        schedule: &JsonObject,
        name: &'static str,
    ) -> Result<(), PrecompileError> {
        let Some(value) = schedule.get(name) else {
            return Ok(());
        };
        let Some(config) = Self::value_as_optional_object(value.as_ref(), name)? else {
            return Ok(());
        };
        Self::ensure_int_field(&config, "target")?;
        Self::ensure_int_field(&config, "max")?;
        Self::ensure_u64_field(&config, "baseFeeUpdateFraction")
    }

    fn ensure_big_int_field(
        object: &JsonObject,
        name: &'static str,
    ) -> Result<(), PrecompileError> {
        let Some(value) = object.get(name) else {
            return Ok(());
        };
        JsonBigInt::parse_optional(value.as_ref(), name).map(|_| ())
    }

    fn ensure_bool_field(object: &JsonObject, name: &'static str) -> Result<(), PrecompileError> {
        let Some(value) = object.get(name) else {
            return Ok(());
        };
        let text = Self::raw_text(value.as_ref());
        if matches!(text, "null" | "true" | "false") {
            Ok(())
        } else {
            Err(PrecompileError::other(format!(
                "invalid chain config field {name}"
            )))
        }
    }

    fn ensure_u64_field(object: &JsonObject, name: &'static str) -> Result<(), PrecompileError> {
        Self::field_u64(object, name).map(|_| ())
    }

    fn ensure_int_field(object: &JsonObject, name: &'static str) -> Result<(), PrecompileError> {
        let Some(value) = object.get(name) else {
            return Ok(());
        };
        let text = Self::raw_text(value.as_ref());
        if text == "null" || Self::parse_i64_text(text).is_some() {
            Ok(())
        } else {
            Err(PrecompileError::other(format!(
                "invalid chain config field {name}"
            )))
        }
    }

    fn ensure_address_field(
        object: &JsonObject,
        name: &'static str,
    ) -> Result<(), PrecompileError> {
        let Some(value) = object.get(name) else {
            return Ok(());
        };
        let Ok(value) = serde_json::from_str::<String>(value.get()) else {
            return Err(PrecompileError::other(format!(
                "invalid chain config field {name}"
            )));
        };
        if Self::is_hex_address(&value) {
            Ok(())
        } else {
            Err(PrecompileError::other(format!(
                "invalid chain config field {name}"
            )))
        }
    }

    fn ensure_object_field(object: &JsonObject, name: &'static str) -> Result<(), PrecompileError> {
        object.get(name).map_or(Ok(()), |value| {
            Self::value_as_optional_object(value.as_ref(), name).map(|_| ())
        })
    }

    fn value_as_optional_object(
        value: &RawValue,
        name: &'static str,
    ) -> Result<Option<JsonObject>, PrecompileError> {
        if Self::raw_text(value) == "null" {
            return Ok(None);
        }
        serde_json::from_str(value.get())
            .map(Some)
            .map_err(|_| PrecompileError::other(format!("invalid chain config field {name}")))
    }

    fn is_hex_address(value: &str) -> bool {
        let Some(raw) = value.strip_prefix("0x") else {
            return false;
        };
        raw.len() == 40 && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn field_u64(object: &JsonObject, name: &'static str) -> Result<Option<u64>, PrecompileError> {
        Self::field_u64_any(object, &[name])
    }

    fn field_big_int(
        object: &JsonObject,
        name: &'static str,
    ) -> Result<Option<JsonBigInt>, PrecompileError> {
        Self::field_big_int_any(object, &[name])
    }

    fn field_big_int_any(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<Option<JsonBigInt>, PrecompileError> {
        let mut parsed = None;
        for (name, value) in object {
            if names.contains(&name.as_str()) {
                parsed = JsonBigInt::parse_optional(value.as_ref(), name.as_str())?;
            }
        }
        Ok(parsed)
    }

    fn field_u64_any(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<Option<u64>, PrecompileError> {
        let mut parsed = None;
        for (name, value) in object {
            if names.contains(&name.as_str()) {
                parsed = Self::value_as_u64(value.as_ref())?;
            }
        }
        Ok(parsed)
    }

    fn field_bool(object: &JsonObject, name: &'static str) -> Result<bool, PrecompileError> {
        let Some(value) = object.get(name) else {
            return Ok(false);
        };
        match Self::raw_text(value.as_ref()) {
            "null" => Ok(false),
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(PrecompileError::other(format!(
                "invalid chain config field {name}"
            ))),
        }
    }

    fn value_as_u64(value: &RawValue) -> Result<Option<u64>, PrecompileError> {
        let text = Self::raw_text(value);
        if text == "null" {
            return Ok(None);
        }
        Self::parse_u64_text(text)
            .map(Some)
            .ok_or_else(|| PrecompileError::other("invalid chain config numeric field"))
    }

    fn raw_text(value: &RawValue) -> &str {
        value.get().trim()
    }

    fn parse_u64_text(text: &str) -> Option<u64> {
        if text.is_empty()
            || text.starts_with('-')
            || !text.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        text.parse().ok()
    }

    fn parse_i64_text(text: &str) -> Option<i64> {
        let digits = text.strip_prefix('-').unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    }
}

impl JsonBigInt {
    fn parse_optional(value: &RawValue, name: &str) -> Result<Option<Self>, PrecompileError> {
        let text = NitroChainConfig::raw_text(value);
        if text == "null" {
            return Ok(None);
        }

        let (negative, digits) = text
            .strip_prefix('-')
            .map_or((false, text), |digits| (true, digits));
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(PrecompileError::other(format!(
                "invalid chain config field {name}"
            )));
        }

        let digits = digits.trim_start_matches('0');
        let normalized = if digits.is_empty() {
            "0".to_string()
        } else if negative {
            format!("-{digits}")
        } else {
            digits.to_string()
        };
        let is_negative = negative && normalized != "0";
        let u64_value = (!is_negative)
            .then(|| normalized.parse::<u64>().ok())
            .flatten();

        Ok(Some(Self {
            normalized,
            u64_value,
            is_negative,
        }))
    }

    fn equals_u64(&self, value: u64) -> bool {
        !self.is_negative && self.u64_value == Some(value)
    }

    fn is_forked(&self, head: u64) -> bool {
        self.is_negative || self.u64_value.is_some_and(|fork_block| fork_block <= head)
    }
}

impl NitroArbitrumChainParams {
    fn parse(object: &JsonObject) -> Result<Self, PrecompileError> {
        let Some(value) = object.get("arbitrum") else {
            return Ok(Self::default());
        };
        if NitroChainConfig::raw_text(value.as_ref()) == "null" {
            return Ok(Self::default());
        }
        let object = NitroChainConfig::value_as_optional_object(value.as_ref(), "arbitrum")?
            .ok_or_else(|| PrecompileError::other("invalid chain config arbitrum field"))?;
        Self::validate_known_fields(&object)?;

        Ok(Self {
            enable_arb_os: Self::field_bool_any(&object, &["EnableArbOS", "enableArbOS"])?,
            genesis_block_num: Self::field_u64_any(
                &object,
                &["GenesisBlockNum", "genesisBlockNum"],
            )?
            .unwrap_or_default(),
            max_uncompressed_batch_size: Self::field_u64_any(
                &object,
                &["MaxUncompressedBatchSize", "maxUncompressedBatchSize"],
            )?
            .unwrap_or_default(),
        })
    }

    fn validate_known_fields(object: &JsonObject) -> Result<(), PrecompileError> {
        Self::ensure_bool_field(object, &["EnableArbOS", "enableArbOS"])?;
        Self::ensure_bool_field(object, &["AllowDebugPrecompiles", "allowDebugPrecompiles"])?;
        Self::ensure_bool_field(
            object,
            &["DataAvailabilityCommittee", "dataAvailabilityCommittee"],
        )?;
        Self::ensure_u64_field(object, &["InitialArbOSVersion", "initialArbOSVersion"])?;
        Self::ensure_address_field(object, &["InitialChainOwner", "initialChainOwner"])?;
        Self::ensure_u64_field(object, &["GenesisBlockNum", "genesisBlockNum"])?;
        Self::ensure_u64_field(object, &["MaxCodeSize", "maxCodeSize"])?;
        Self::ensure_u64_field(object, &["MaxInitCodeSize", "maxInitCodeSize"])?;
        Self::ensure_u64_field(
            object,
            &["MaxUncompressedBatchSize", "maxUncompressedBatchSize"],
        )
    }

    fn ensure_bool_field(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<(), PrecompileError> {
        for (name, value) in object {
            if names.contains(&name.as_str()) {
                match NitroChainConfig::raw_text(value.as_ref()) {
                    "null" | "true" | "false" => {}
                    _ => {
                        return Err(PrecompileError::other("invalid chain config boolean field"));
                    }
                }
            }
        }
        Ok(())
    }

    fn ensure_u64_field(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<(), PrecompileError> {
        Self::field_u64_any(object, names).map(|_| ())
    }

    fn ensure_address_field(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<(), PrecompileError> {
        for (name, value) in object {
            if names.contains(&name.as_str()) {
                let Ok(value) = serde_json::from_str::<String>(value.get()) else {
                    return Err(PrecompileError::other("invalid chain config address field"));
                };
                if !NitroChainConfig::is_hex_address(&value) {
                    return Err(PrecompileError::other("invalid chain config address field"));
                }
            }
        }
        Ok(())
    }

    fn field_u64_any(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<Option<u64>, PrecompileError> {
        NitroChainConfig::field_u64_any(object, names)
    }

    fn field_bool_any(
        object: &JsonObject,
        names: &[&'static str],
    ) -> Result<bool, PrecompileError> {
        let mut parsed = None;
        for (name, value) in object {
            if names.contains(&name.as_str()) {
                let value = match NitroChainConfig::raw_text(value.as_ref()) {
                    "null" => false,
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(PrecompileError::other("invalid chain config boolean field"));
                    }
                };
                parsed = Some(value);
            }
        }
        Ok(parsed.unwrap_or(false))
    }
}
