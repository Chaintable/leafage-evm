//! Historical ethermint.evm.v1.Params as returned by the archive ABCI query.
//! The query is for the parent. At an upgrade, the module migrates those
//! parameters before DeliverTx. The release table selects the resulting rules.
use anyhow::{ensure, Context, Result};
use leafage_evm_rpc::CronosEra;
use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub(super) struct EvmParams {
    #[prost(string, tag = "1")]
    denom: String,
    #[prost(bool, tag = "2")]
    enable_create: bool,
    #[prost(bool, tag = "3")]
    enable_call: bool,
    #[prost(int64, repeated, tag = "4")]
    extra_eips: Vec<i64>,
    #[prost(message, optional, tag = "5")]
    chain: Option<ChainConfig>,
    #[prost(uint64, tag = "7")]
    header_hash_num: u64,
    #[prost(uint64, tag = "8")]
    pub(super) history_serve_window: u64,
}

// Ethermint's nullable sdk.Int fields distinguish an absent fork from zero.
#[derive(Clone, PartialEq, Message)]
struct ChainConfig {
    #[prost(string, optional, tag = "1")]
    homestead: Option<String>,
    #[prost(string, optional, tag = "4")]
    eip150: Option<String>,
    #[prost(string, optional, tag = "6")]
    eip155: Option<String>,
    #[prost(string, optional, tag = "7")]
    eip158: Option<String>,
    #[prost(string, optional, tag = "8")]
    byzantium: Option<String>,
    #[prost(string, optional, tag = "9")]
    constantinople: Option<String>,
    #[prost(string, optional, tag = "10")]
    petersburg: Option<String>,
    #[prost(string, optional, tag = "11")]
    istanbul: Option<String>,
    #[prost(string, optional, tag = "13")]
    berlin: Option<String>,
    #[prost(string, optional, tag = "17")]
    london: Option<String>,
    #[prost(string, optional, tag = "21")]
    merge: Option<String>,
    #[prost(string, optional, tag = "22")]
    shanghai: Option<String>,
    #[prost(string, optional, tag = "23")]
    cancun: Option<String>,
    #[prost(string, optional, tag = "24")]
    prague: Option<String>,
    #[prost(string, optional, tag = "25")]
    osaka: Option<String>,
}

#[derive(Debug)]
pub(super) struct UnauditedParams;

impl std::fmt::Display for UnauditedParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("historical EVM parameters differ from the audited replay rule")
    }
}

impl EvmParams {
    pub(super) fn validate(&self, height: u64) -> Result<()> {
        let parent = height.checked_sub(1).context("missing parent height")?;
        let era = CronosEra::at_height(parent).context("unknown parent Cronos release")?;
        self.validate_parent(era)
            .with_context(|| format!("mainnet parent height {parent} ({era:?})"))
            .context(UnauditedParams)
    }

    fn validate_parent(&self, era: CronosEra) -> Result<()> {
        use CronosEra::*;
        ensure!(
            self.denom == "basecro" && self.enable_create && self.enable_call,
            "unsupported EVM denom, create or call parameters"
        );
        // v1.0.2 clears extra_eips and sets LondonBlock=0. Its parent still
        // has the ordered v0.x jump-table overrides.
        let pre_galileo = era == HuygenHotfix;
        let expected_eips: &[i64] = if pre_galileo {
            &[2929, 2200, 1884, 1344]
        } else {
            &[]
        };
        ensure!(
            self.extra_eips == expected_eips,
            "unexpected extra_eips {:?}",
            self.extra_eips
        );
        let cfg = self.chain.as_ref().context("missing EVM chain_config")?;
        ensure!(
            [
                &cfg.homestead,
                &cfg.eip150,
                &cfg.eip155,
                &cfg.eip158,
                &cfg.byzantium,
                &cfg.constantinople,
                &cfg.petersburg,
                &cfg.istanbul,
                &cfg.berlin,
                &cfg.london
            ]
            .iter()
            .all(|fork| fork.as_deref() == Some("0")),
            "unexpected pre-Merge fork schedule"
        );
        let shanghai = matches!(era, Titan | V12 | V13 | Pallene | V15 | V16 | V17);
        let prague = matches!(era, V15 | V16 | V17);
        // v1.1.0 installs ShanghaiTime=0. v1.5.1 installs CancunTime=0 and
        // PragueTime=0 together. Their activation blocks query the old
        // parent schedule and execute the migrated spec from CronosEra.
        for (name, actual, active) in [
            ("merge", &cfg.merge, !pre_galileo),
            ("Shanghai", &cfg.shanghai, shanghai),
            ("Cancun", &cfg.cancun, prague),
            ("Prague", &cfg.prague, prague),
            ("Osaka", &cfg.osaka, false),
        ] {
            ensure!(
                actual.as_deref() == active.then_some("0"),
                "unexpected {name} fork schedule {actual:?}"
            );
        }
        // v1.4.0 installs the SDK 0.50 block-hash retention parameter.
        // A different keeper ring modulus can alias BLOCKHASH queries. The
        // archived canonical hash is valid for the audited mainnet window.
        ensure!(
            self.history_serve_window == 0,
            "unexpected history_serve_window {}",
            self.history_serve_window
        );
        let expected_hashes = if matches!(era, Pallene | V15 | V16 | V17) {
            256
        } else {
            0
        };
        ensure!(
            self.header_hash_num == expected_hashes,
            "unexpected header_hash_num {}",
            self.header_hash_num
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    // QueryParamsResponse.params from mainnet abci_query at each parent.
    #[test]
    fn upgrade_blocks_consume_the_pre_migration_parameters() {
        let fixtures: &[(&[u64], &str)] = &[
            (&[6_542_799], "CgdiYXNlY3JvEAEYASII8RaYEdwOwAoqawoBMBIBMBgBIgEwKkIweDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAyATA6ATBCATBKATBSATBaATBiATBqATCKAQEw"),
            (&[6_542_800, 6_542_801, 11_608_759, 11_608_760, 11_608_761, 13_183_999], "CgdiYXNlY3JvEAEYASpzCgEwEgEwGAEiATAqQjB4MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDIBMDoBMEIBMEoBMFIBMFoBMGIBMGoBMIoBATCiAQEwqgEBMA=="),
            (&[13_184_000, 13_184_001, 13_519_999, 13_520_000, 13_520_001, 14_919_999, 14_920_000, 14_920_001, 17_154_999], "CgdiYXNlY3JvEAEYASp3CgEwEgEwGAEiATAqQjB4MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDIBMDoBMEIBMEoBMFIBMFoBMGIBMGoBMIoBATCiAQEwqgEBMLIBATA="),
            (&[17_155_000, 17_155_001, 38_432_211], "CgdiYXNlY3JvEAEYASp3CgEwEgEwGAEiATAqQjB4MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDIBMDoBMEIBMEoBMFIBMFoBMGIBMGoBMIoBATCiAQEwqgEBMLIBATA4gAI="),
            (&[38_432_212, 38_432_213, 45_062_799, 45_062_800, 45_062_801, 58_825_799, 58_825_800, 58_825_801], "CgdiYXNlY3JvEAEYASp/CgEwEgEwGAEiATAqQjB4MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDIBMDoBMEIBMEoBMFIBMFoBMGIBMGoBMIoBATCiAQEwqgEBMLIBATC6AQEwwgEBMDiAAg=="),
        ];
        for &(parents, encoded) in fixtures {
            let bytes = STANDARD.decode(encoded).unwrap();
            let params = EvmParams::decode(bytes.as_slice()).unwrap();
            for &parent_height in parents {
                params.validate(parent_height + 1).unwrap();
                let mut changed = params.clone();
                changed.chain.as_mut().unwrap().shanghai = Some("9999999999".to_owned());
                assert!(changed
                    .validate(parent_height + 1)
                    .unwrap_err()
                    .is::<UnauditedParams>());
                let mut changed = params.clone();
                changed.history_serve_window = 128;
                assert!(changed
                    .validate(parent_height + 1)
                    .unwrap_err()
                    .is::<UnauditedParams>());
            }
        }
    }
}
