//! Leafage compatibility wrapper around the pinned official Tempo protocol definitions.
//! Mainnet schedule comes from tempo-hardfork; Default remains the legacy T10 value.
//! Genesis folds upstream Genesis/T0, and REVM36 conversion remains local.

use tempo_hardfork::TempoHardfork as OfficialHardfork;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum TempoHardfork {
    Genesis,
    T1,
    T1A,
    T1B,
    T1C,
    T2,
    T3,
    T4,
    T5,
    T6,
    T7,
    T8,
    T9,
    #[default]
    T10,
    T11,
}

impl TempoHardfork {
    /// Uses the official mainnet schedule, never latest() or the process clock.
    /// The exact pinned revision has no scheduled T12; tests enforce that invariant.
    pub fn from_timestamp(timestamp: u64) -> Self {
        let fork = OfficialHardfork::from_chain_and_timestamp(4217, timestamp)
            .expect("official Tempo mainnet schedule exists");
        Self::try_from(fork).expect("scheduled fork must be supported by the pinned adapter")
    }

    pub const fn as_official(self) -> OfficialHardfork {
        match self {
            Self::Genesis => OfficialHardfork::T0,
            Self::T1 => OfficialHardfork::T1,
            Self::T1A => OfficialHardfork::T1A,
            Self::T1B => OfficialHardfork::T1B,
            Self::T1C => OfficialHardfork::T1C,
            Self::T2 => OfficialHardfork::T2,
            Self::T3 => OfficialHardfork::T3,
            Self::T4 => OfficialHardfork::T4,
            Self::T5 => OfficialHardfork::T5,
            Self::T6 => OfficialHardfork::T6,
            Self::T7 => OfficialHardfork::T7,
            Self::T8 => OfficialHardfork::T8,
            Self::T9 => OfficialHardfork::T9,
            Self::T10 => OfficialHardfork::T10,
            Self::T11 => OfficialHardfork::T11,
        }
    }

    pub const fn is_t0(&self) -> bool {
        self.as_official().is_t0()
    }
    pub const fn is_t1(&self) -> bool {
        self.as_official().is_t1()
    }
    pub const fn is_t1a(&self) -> bool {
        self.as_official().is_t1a()
    }
    pub const fn is_t1b(&self) -> bool {
        self.as_official().is_t1b()
    }
    pub const fn is_t1c(&self) -> bool {
        self.as_official().is_t1c()
    }
    pub const fn is_t2(&self) -> bool {
        self.as_official().is_t2()
    }
    pub const fn is_t3(&self) -> bool {
        self.as_official().is_t3()
    }
    pub const fn is_t4(&self) -> bool {
        self.as_official().is_t4()
    }
    pub const fn is_t5(&self) -> bool {
        self.as_official().is_t5()
    }
    pub const fn is_t6(&self) -> bool {
        self.as_official().is_t6()
    }
    pub const fn is_t7(&self) -> bool {
        self.as_official().is_t7()
    }
    pub const fn is_t8(&self) -> bool {
        self.as_official().is_t8()
    }
    pub const fn is_t9(&self) -> bool {
        self.as_official().is_t9()
    }
    pub const fn is_t10(&self) -> bool {
        self.as_official().is_t10()
    }
    pub const fn is_t11(&self) -> bool {
        self.as_official().is_t11()
    }

    pub const fn expiring_nonce_set_capacity(&self) -> u32 {
        self.as_official().expiring_nonce_set_capacity()
    }
    pub const fn expiring_nonce_max_expiry_secs(&self) -> u64 {
        self.as_official().expiring_nonce_max_expiry_secs()
    }
    pub const fn gas_existing_nonce_key(&self) -> u64 {
        self.as_official().gas_existing_nonce_key()
    }
    pub const fn gas_new_nonce_key(&self) -> u64 {
        self.as_official().gas_new_nonce_key()
    }
}

impl TryFrom<OfficialHardfork> for TempoHardfork {
    type Error = OfficialHardfork;

    fn try_from(fork: OfficialHardfork) -> Result<Self, Self::Error> {
        Ok(match fork {
            OfficialHardfork::Genesis | OfficialHardfork::T0 => Self::Genesis,
            OfficialHardfork::T1 => Self::T1,
            OfficialHardfork::T1A => Self::T1A,
            OfficialHardfork::T1B => Self::T1B,
            OfficialHardfork::T1C => Self::T1C,
            OfficialHardfork::T2 => Self::T2,
            OfficialHardfork::T3 => Self::T3,
            OfficialHardfork::T4 => Self::T4,
            OfficialHardfork::T5 => Self::T5,
            OfficialHardfork::T6 => Self::T6,
            OfficialHardfork::T7 => Self::T7,
            OfficialHardfork::T8 => Self::T8,
            OfficialHardfork::T9 => Self::T9,
            OfficialHardfork::T10 => Self::T10,
            OfficialHardfork::T11 => Self::T11,
            _ => return Err(fork),
        })
    }
}

impl From<TempoHardfork> for revm::primitives::hardfork::SpecId {
    fn from(_: TempoHardfork) -> Self {
        // Tempo keeps Osaka EVM semantics for every Tempo hardfork. The
        // Ethereum built-in precompile set is selected independently in
        // TempoEvm::new: Prague before T1C, Osaka from T1C onward.
        Self::OSAKA
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Frozen pre-migration timestamps, independent of the upstream schedule implementation.
    /// Tempo mainnet activation timestamps (from `presto.json` genesis config).
    ///
    /// Note: T1 and T1A share the same activation timestamp on mainnet.
    /// The `Genesis` variant covers both the Genesis and T0 eras (T0 activates
    /// at timestamp 0 on mainnet, same as Genesis).
    const MAINNET_T1_TIME: u64 = 1_770_908_400;
    const MAINNET_T1A_TIME: u64 = 1_770_908_400;
    const MAINNET_T1B_TIME: u64 = 1_771_858_800;
    const MAINNET_T1C_TIME: u64 = 1_773_327_600;
    // T2 activated on mainnet: 2026-03-31 14:00 UTC (from presto.json genesis).
    const MAINNET_T2_TIME: u64 = 1_774_965_600;
    // T3 activated on mainnet: 2026-04-27 14:00 UTC (from presto.json genesis).
    const MAINNET_T3_TIME: u64 = 1_777_298_400;
    // T4 activates on mainnet: 2026-05-18 14:00 UTC (from presto.json genesis).
    const MAINNET_T4_TIME: u64 = 1_779_112_800;
    const MAINNET_T5_TIME: u64 = 1_781_013_600;
    const MAINNET_T6_TIME: u64 = 1_782_223_200;
    const MAINNET_T7_TIME: u64 = 1_783_605_600;
    const MAINNET_T8_TIME: u64 = 1_785_420_000;
    const MAINNET_T9_TIME: u64 = 1_786_024_800;
    const MAINNET_T10_TIME: u64 = 1_787_320_800;
    const MAINNET_T11_TIME: u64 = 1_789_048_800;

    #[test]
    fn official_mapping_preserves_legacy_default_and_rejects_unimplemented_forks() {
        assert_eq!(TempoHardfork::default(), TempoHardfork::T10);
        assert_eq!(
            TempoHardfork::try_from(OfficialHardfork::T0),
            Ok(TempoHardfork::Genesis)
        );
        assert_eq!(
            TempoHardfork::try_from(OfficialHardfork::Genesis),
            Ok(TempoHardfork::Genesis)
        );
        assert_eq!(
            TempoHardfork::try_from(OfficialHardfork::T12),
            Err(OfficialHardfork::T12)
        );
        assert_eq!(OfficialHardfork::T12.mainnet_activation_timestamp(), None);
        assert_eq!(
            OfficialHardfork::from_chain_and_timestamp(4217, u64::MAX),
            Some(OfficialHardfork::T11)
        );
        assert_eq!(
            OfficialHardfork::from_chain_and_timestamp(999, u64::MAX),
            None
        );
        for fork in OfficialHardfork::VARIANTS {
            if let Ok(local) = TempoHardfork::try_from(*fork) {
                assert_eq!(
                    local.as_official(),
                    if *fork == OfficialHardfork::Genesis {
                        OfficialHardfork::T0
                    } else {
                        *fork
                    }
                );
            }
        }
    }

    #[test]
    fn from_timestamp_genesis() {
        assert_eq!(TempoHardfork::from_timestamp(0), TempoHardfork::Genesis);
        assert_eq!(TempoHardfork::from_timestamp(1000), TempoHardfork::Genesis);
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1_TIME - 1),
            TempoHardfork::Genesis
        );
    }

    #[test]
    fn from_timestamp_t1a() {
        // T1 and T1A share the same activation timestamp on mainnet,
        // so from_timestamp returns T1A (not T1) at the activation boundary.
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1A_TIME),
            TempoHardfork::T1A
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1A_TIME + 1),
            TempoHardfork::T1A
        );
    }

    #[test]
    fn from_timestamp_t1b() {
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1B_TIME - 1),
            TempoHardfork::T1A
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1B_TIME),
            TempoHardfork::T1B
        );
    }

    #[test]
    fn from_timestamp_t1c() {
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1C_TIME - 1),
            TempoHardfork::T1B
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1C_TIME),
            TempoHardfork::T1C
        );
    }

    #[test]
    fn from_timestamp_t2_activated() {
        // T2 activated at 1774965600 (2026-03-31 14:00 UTC)
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T2_TIME),
            TempoHardfork::T2
        );
        // Before T2: still T1C
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T2_TIME - 1),
            TempoHardfork::T1C
        );
    }

    #[test]
    fn from_timestamp_t3_activated() {
        // T3 activated at 1777298400 (2026-04-27 14:00 UTC)
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T3_TIME - 1),
            TempoHardfork::T2
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T3_TIME),
            TempoHardfork::T3
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T3_TIME + 1),
            TempoHardfork::T3
        );
    }

    #[test]
    fn from_timestamp_t4_activated() {
        // T4 activates at 1779112800 (2026-05-18 14:00 UTC)
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T4_TIME - 1),
            TempoHardfork::T3
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T4_TIME),
            TempoHardfork::T4
        );
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T4_TIME + 1),
            TempoHardfork::T4
        );
    }

    #[test]
    fn from_timestamp_t5_through_t11_boundaries() {
        let boundaries = [
            (MAINNET_T5_TIME, TempoHardfork::T4, TempoHardfork::T5),
            (MAINNET_T6_TIME, TempoHardfork::T5, TempoHardfork::T6),
            (MAINNET_T7_TIME, TempoHardfork::T6, TempoHardfork::T7),
            (MAINNET_T8_TIME, TempoHardfork::T7, TempoHardfork::T8),
            (MAINNET_T9_TIME, TempoHardfork::T8, TempoHardfork::T9),
            (MAINNET_T10_TIME, TempoHardfork::T9, TempoHardfork::T10),
            (MAINNET_T11_TIME, TempoHardfork::T10, TempoHardfork::T11),
        ];

        for (timestamp, before, active) in boundaries {
            assert_eq!(TempoHardfork::from_timestamp(timestamp - 1), before);
            assert_eq!(TempoHardfork::from_timestamp(timestamp), active);
            assert_eq!(TempoHardfork::from_timestamp(timestamp + 1), active);
        }

        // Future timestamps select the latest scheduled fork, never an unscheduled one.
        assert_eq!(TempoHardfork::from_timestamp(u64::MAX), TempoHardfork::T11);
    }

    #[test]
    fn is_methods_on_genesis() {
        let hf = TempoHardfork::Genesis;
        assert!(hf.is_t0());
        assert!(!hf.is_t1());
        assert!(!hf.is_t1a());
        assert!(!hf.is_t1b());
        assert!(!hf.is_t1c());
        assert!(!hf.is_t2());
        assert!(!hf.is_t3());
        assert!(!hf.is_t4());
        assert!(!hf.is_t5());
        assert!(!hf.is_t6());
        assert!(!hf.is_t7());
        assert!(!hf.is_t8());
        assert!(!hf.is_t9());
        assert!(!hf.is_t10());
        assert!(!hf.is_t11());
    }

    #[test]
    fn is_methods_on_t1c() {
        let hf = TempoHardfork::T1C;
        assert!(hf.is_t0());
        assert!(hf.is_t1());
        assert!(hf.is_t1a());
        assert!(hf.is_t1b());
        assert!(hf.is_t1c());
        assert!(!hf.is_t2());
        assert!(!hf.is_t3());
        assert!(!hf.is_t4());
        assert!(!hf.is_t5());
        assert!(!hf.is_t10());
        assert!(!hf.is_t11());
    }

    #[test]
    fn is_methods_on_t3() {
        let hf = TempoHardfork::T3;
        assert!(hf.is_t0());
        assert!(hf.is_t1());
        assert!(hf.is_t1a());
        assert!(hf.is_t1b());
        assert!(hf.is_t1c());
        assert!(hf.is_t2());
        assert!(hf.is_t3());
        assert!(!hf.is_t4());
        assert!(!hf.is_t5());
        assert!(!hf.is_t10());
        assert!(!hf.is_t11());
    }

    #[test]
    fn is_methods_on_t4() {
        let hf = TempoHardfork::T4;
        assert!(hf.is_t0());
        assert!(hf.is_t1());
        assert!(hf.is_t1a());
        assert!(hf.is_t1b());
        assert!(hf.is_t1c());
        assert!(hf.is_t2());
        assert!(hf.is_t3());
        assert!(hf.is_t4());
        assert!(!hf.is_t5());
        assert!(!hf.is_t10());
        assert!(!hf.is_t11());
    }

    #[test]
    fn is_methods_on_t10() {
        let hf = TempoHardfork::T10;
        assert!(hf.is_t0());
        assert!(hf.is_t1());
        assert!(hf.is_t2());
        assert!(hf.is_t3());
        assert!(hf.is_t4());
        assert!(hf.is_t5());
        assert!(hf.is_t6());
        assert!(hf.is_t7());
        assert!(hf.is_t8());
        assert!(hf.is_t9());
        assert!(hf.is_t10());
        assert!(!hf.is_t11());
    }

    #[test]
    fn is_methods_on_t11() {
        let hf = TempoHardfork::T11;
        assert!(hf.is_t0());
        assert!(hf.is_t1());
        assert!(hf.is_t2());
        assert!(hf.is_t3());
        assert!(hf.is_t4());
        assert!(hf.is_t5());
        assert!(hf.is_t6());
        assert!(hf.is_t7());
        assert!(hf.is_t8());
        assert!(hf.is_t9());
        assert!(hf.is_t10());
        assert!(hf.is_t11());
    }

    #[test]
    fn default_is_latest_activated() {
        let hf = TempoHardfork::default();
        assert_eq!(hf, TempoHardfork::T10);
        assert!(hf.is_t10());
        assert!(!hf.is_t11());
    }

    #[test]
    fn t3_through_t11_gas_matches_t2() {
        // T3-T11 inherit T2 nonce gas (no schedule change).
        for hardfork in [
            TempoHardfork::T3,
            TempoHardfork::T4,
            TempoHardfork::T5,
            TempoHardfork::T6,
            TempoHardfork::T7,
            TempoHardfork::T8,
            TempoHardfork::T9,
            TempoHardfork::T10,
            TempoHardfork::T11,
        ] {
            assert_eq!(
                hardfork.gas_existing_nonce_key(),
                TempoHardfork::T2.gas_existing_nonce_key()
            );
            assert_eq!(
                hardfork.gas_new_nonce_key(),
                TempoHardfork::T2.gas_new_nonce_key()
            );
        }
    }

    #[test]
    fn every_tempo_hardfork_uses_osaka_evm_spec() {
        use revm::primitives::hardfork::SpecId;

        for hardfork in [
            TempoHardfork::Genesis,
            TempoHardfork::T1,
            TempoHardfork::T1A,
            TempoHardfork::T1B,
            TempoHardfork::T1C,
            TempoHardfork::T2,
            TempoHardfork::T3,
            TempoHardfork::T4,
            TempoHardfork::T5,
            TempoHardfork::T6,
            TempoHardfork::T7,
            TempoHardfork::T8,
            TempoHardfork::T9,
            TempoHardfork::T10,
            TempoHardfork::T11,
        ] {
            assert_eq!(SpecId::from(hardfork), SpecId::OSAKA);
        }
    }

    #[test]
    fn expiring_nonce_parameters_activate_at_t11() {
        assert_eq!(TempoHardfork::T10.expiring_nonce_set_capacity(), 300_000);
        assert_eq!(TempoHardfork::T10.expiring_nonce_max_expiry_secs(), 30);
        assert_eq!(TempoHardfork::T11.expiring_nonce_set_capacity(), 3_000_000);
        assert_eq!(TempoHardfork::T11.expiring_nonce_max_expiry_secs(), 300);
    }
}
