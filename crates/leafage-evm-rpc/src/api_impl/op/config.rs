use anyhow::{anyhow, bail, Result};
use leafage_evm_types::{CfgEnv, OpSpecId};
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InitcodeLimit {
    Bytes(usize),
    Keyword(String),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OpCustomConfig {
    op_spec_id: Option<String>,
    limit_contract_code_size: Option<usize>,
    limit_contract_initcode_size: Option<InitcodeLimit>,
}

/// Builds the OP `CfgEnv` from `--evm-custom-config`.
///
/// `op_spec_id` is an OP fork name (e.g. "Jovian"); omitted keeps Osaka.
/// Without an initcode override, revm uses 2 * code size, matching geth.
pub fn build_op_custom_config(json: Option<&str>) -> Result<CfgEnv<OpSpecId>> {
    let json = json.unwrap_or("{}");
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| anyhow!("cannot parse op custom evm config: {err}"))?;
    if !value.is_object() {
        bail!("op custom evm config must be a JSON object");
    }
    let custom: OpCustomConfig = serde_json::from_value(value)
        .map_err(|err| anyhow!("cannot parse op custom evm config: {err}"))?;
    let spec = custom
        .op_spec_id
        .map(|name| {
            OpSpecId::from_str(&name).map_err(|_| {
                anyhow!("invalid op_spec_id {name:?} in op custom evm config (expected an OP fork name such as \"Jovian\")")
            })
        })
        .transpose()?
        .unwrap_or(OpSpecId::OSAKA);
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.limit_contract_code_size = custom.limit_contract_code_size;
    if let Some(limit) = custom.limit_contract_initcode_size {
        cfg.limit_contract_initcode_size = Some(match limit {
            InitcodeLimit::Bytes(size) => size,
            InitcodeLimit::Keyword(value) if value == "unlimited" => usize::MAX,
            InitcodeLimit::Keyword(_) => bail!("op initcode limit must be bytes or \"unlimited\""),
        });
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context_interface::Cfg;

    fn config(json: &str) -> CfgEnv<OpSpecId> {
        build_op_custom_config(Some(json)).unwrap()
    }

    #[test]
    fn op_spec_id_takes_fork_names() {
        use OpSpecId::*;
        for spec in [
            BEDROCK, REGOLITH, CANYON, ECOTONE, FJORD, GRANITE, HOLOCENE, ISTHMUS, JOVIAN, INTEROP,
            OSAKA,
        ] {
            let name: &str = spec.into();
            let cfg = config(&format!(r#"{{"op_spec_id":"{name}"}}"#));
            assert_eq!(cfg.spec, spec);
            assert_eq!(cfg.gas_params, CfgEnv::new_with_spec(spec).gas_params);
        }
        assert_eq!(build_op_custom_config(None).unwrap().spec, OSAKA);
        for json in ["{}", r#"{"op_spec_id":null}"#] {
            assert_eq!(config(json).spec, OSAKA);
        }
    }

    #[test]
    fn size_overrides() {
        for json in [
            "{}",
            r#"{"limit_contract_code_size":null,"limit_contract_initcode_size":null}"#,
        ] {
            let cfg = config(json);
            assert_eq!(cfg.limit_contract_code_size, None);
            assert_eq!(cfg.limit_contract_initcode_size, None);
        }
        let code = config(r#"{"limit_contract_code_size":262144}"#);
        assert_eq!(
            (code.max_code_size(), code.max_initcode_size()),
            (262144, 524288)
        );
        let init = config(r#"{"limit_contract_initcode_size":524288}"#);
        assert_eq!(
            (init.max_code_size(), init.max_initcode_size()),
            (24576, 524288)
        );
        let rise = config(
            r#"{"op_spec_id":"Jovian","limit_contract_code_size":262144,"limit_contract_initcode_size":524288}"#,
        );
        assert_eq!(rise.spec, OpSpecId::JOVIAN);
        assert_eq!(
            (rise.max_code_size(), rise.max_initcode_size()),
            (262144, 524288)
        );
        let metis = config(
            r#"{"limit_contract_code_size":2457600,"limit_contract_initcode_size":"unlimited"}"#,
        );
        assert_eq!(
            (metis.max_code_size(), metis.max_initcode_size()),
            (2457600, usize::MAX)
        );
        let zero = config(r#"{"limit_contract_code_size":0,"limit_contract_initcode_size":0}"#);
        assert_eq!((zero.max_code_size(), zero.max_initcode_size()), (0, 0));
        assert_eq!(rise.tx_gas_limit_cap, config("{}").tx_gas_limit_cap);
    }

    #[test]
    fn invalid_op_config_is_rejected() {
        for json in [
            "null",
            "[]",
            "42",
            "{",
            r#"{"typo":1}"#,
            r#"{"spec_id":"Jovian"}"#,
            r#"{"op_spec_id":108}"#,
            r#"{"op_spec_id":"108"}"#,
            r#"{"op_spec_id":"jovian"}"#,
            r#"{"op_spec_id":"Prague"}"#,
            r#"{"limit_contract_code_size":-1}"#,
            r#"{"limit_contract_code_size":1.5}"#,
            r#"{"limit_contract_code_size":"unlimited"}"#,
            r#"{"limit_contract_initcode_size":"infinite"}"#,
            r#"{"limit_contract_initcode_size":{"unlimited":null}}"#,
            r#"{"limit_contract_initcode_size":18446744073709551616}"#,
        ] {
            assert!(
                build_op_custom_config(Some(json)).is_err(),
                "accepted {json}"
            );
        }
    }
}
