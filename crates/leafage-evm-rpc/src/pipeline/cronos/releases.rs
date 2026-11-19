//! Mainnet binary transitions from the Cronos Genesis Synchronization guide,
//! plus the v1.8 mainnet upgrade. Heights are inclusive. Dependency revisions
//! are from the respective cronosd go.mod after replace directives.
//! The guide specifies v0.6.* at genesis; its representative pins are v0.6.1.
//! https://docs.cronos.com/for-node-hosts/running-nodes/genesis-synchronization
use super::CronosEra;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct CronosRelease {
    first_height: u64,
    binary: &'static str,
    ethermint: &'static str,
    geth: &'static str,
    #[serde(skip)]
    pub(super) rules: Option<CronosEra>,
}

impl CronosRelease {
    pub fn at_height(height: u64) -> Option<&'static Self> {
        RELEASES
            .iter()
            .rev()
            .find(|release| height >= release.first_height)
    }
}

const RELEASES: &[CronosRelease] = &[
    CronosRelease {
        first_height: 1,
        binary: "v0.6.x",
        ethermint: "v0.7.2-cronos",
        geth: "cronos/v1.10.3-patched",
        rules: Some(CronosEra::BeforeHuygen),
    },
    CronosRelease {
        first_height: 2_693_800,
        binary: "v0.7.0",
        ethermint: "v0.10.0-alpha1-cronos-9",
        geth: "upstream/v1.10.15",
        rules: Some(CronosEra::Huygen),
    },
    CronosRelease {
        first_height: 3_982_500,
        binary: "v0.8.3",
        ethermint: "189ed50c7867",
        geth: "upstream/v1.10.15",
        rules: Some(CronosEra::HuygenHotfix),
    },
    CronosRelease {
        first_height: 6_542_800,
        binary: "v1.0.2",
        ethermint: "v0.20.1-cronos",
        geth: "cronos/v1.10.19-deepcopy-jumptable",
        rules: Some(CronosEra::Galileo),
    },
    CronosRelease {
        first_height: 11_608_760,
        binary: "v1.0.15",
        ethermint: "a85daedc6ffc",
        geth: "cronos/v1.10.19-deepcopy-jumptable",
        rules: Some(CronosEra::FeeMarket),
    },
    CronosRelease {
        first_height: 13_184_000,
        binary: "v1.1.0",
        ethermint: "5b153a460e7a",
        geth: "cronos/43cf32d91c3e",
        rules: Some(CronosEra::Titan),
    },
    CronosRelease {
        first_height: 13_520_000,
        binary: "v1.2.0",
        ethermint: "27de4edabc79",
        geth: "cronos/43cf32d91c3e",
        rules: Some(CronosEra::V12),
    },
    CronosRelease {
        first_height: 14_920_000,
        binary: "v1.3.0",
        ethermint: "a4e82cc4e8ed",
        geth: "cronos/43cf32d91c3e",
        rules: Some(CronosEra::V13),
    },
    CronosRelease {
        first_height: 17_155_000,
        binary: "v1.4.0",
        ethermint: "cd93407bcf05",
        geth: "cronos/d2275b4afb9a",
        rules: Some(CronosEra::Pallene),
    },
    CronosRelease {
        first_height: 38_432_212,
        binary: "v1.5.1",
        ethermint: "86f043d7a76a",
        geth: "cronos/a4fbafcae0dd",
        rules: Some(CronosEra::V15),
    },
    CronosRelease {
        first_height: 45_062_800,
        binary: "v1.6.1",
        ethermint: "10959a771228",
        geth: "cronos/a4fbafcae0dd",
        rules: Some(CronosEra::V16),
    },
    CronosRelease {
        first_height: 58_825_800,
        binary: "v1.7.0",
        ethermint: "9e97913655b0",
        geth: "cronos/a4fbafcae0dd",
        rules: Some(CronosEra::V17),
    },
    CronosRelease {
        first_height: 101_485_000,
        binary: "v1.8.0",
        ethermint: "v0.24.0",
        geth: "cronos/663dca6c618e",
        rules: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_transitions_are_inclusive() {
        let expected = [
            (1, "v0.6.x"),
            (2_693_800, "v0.7.0"),
            (3_982_500, "v0.8.3"),
            (6_542_800, "v1.0.2"),
            (11_608_760, "v1.0.15"),
            (13_184_000, "v1.1.0"),
            (13_520_000, "v1.2.0"),
            (14_920_000, "v1.3.0"),
            (17_155_000, "v1.4.0"),
            (38_432_212, "v1.5.1"),
            (45_062_800, "v1.6.1"),
            (58_825_800, "v1.7.0"),
            (101_485_000, "v1.8.0"),
        ];
        assert!(CronosRelease::at_height(0).is_none());
        for (index, (start, binary)) in expected.into_iter().enumerate() {
            assert_eq!(CronosRelease::at_height(start).unwrap().binary, binary);
            assert_eq!(CronosRelease::at_height(start + 1).unwrap().binary, binary);
            if index > 0 {
                assert_eq!(
                    CronosRelease::at_height(start - 1).unwrap().binary,
                    expected[index - 1].1
                );
            }
        }
    }
}
