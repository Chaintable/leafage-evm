/// Tempo hardfork enum for leafage-evm with timestamp-based activation.
///
/// Supports both "latest spec" mode (via `Default`, returns `T3`) and
/// archive mode (via `from_timestamp`, returns the hardfork active at a
/// given block timestamp).
///
/// Mainnet activation timestamps (from `presto.json` genesis):
/// - Genesis/T0: 0
/// - T1/T1A: 1770908400 (Feb 12, 2026 15:00 UTC)
/// - T1B: 1771858800 (Feb 23, 2026 15:00 UTC)
/// - T1C: 1773327600 (Mar 12, 2026 15:00 UTC)
/// - T2: not yet scheduled
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TempoHardfork {
    Genesis,
    T1,
    T1A,
    T1B,
    T1C,
    T2,
    T3,
}

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

impl TempoHardfork {
    /// Determine the active hardfork for a given block timestamp.
    ///
    /// Uses Tempo mainnet activation timestamps. On mainnet, T0 activates at
    /// timestamp 0 (same as Genesis), so the `Genesis` variant covers both
    /// the Genesis and T0 eras.
    pub fn from_timestamp(timestamp: u64) -> Self {
        if timestamp >= MAINNET_T2_TIME {
            Self::T2
        } else if timestamp >= MAINNET_T1C_TIME {
            Self::T1C
        } else if timestamp >= MAINNET_T1B_TIME {
            Self::T1B
        } else if timestamp >= MAINNET_T1A_TIME {
            Self::T1A
        } else if timestamp >= MAINNET_T1_TIME {
            // T1 and T1A share the same timestamp on mainnet, so this branch
            // is effectively unreachable. Kept for correctness if timestamps
            // ever diverge (e.g. testnet).
            Self::T1
        } else {
            Self::Genesis
        }
    }

    pub fn is_t0(&self) -> bool {
        true // Genesis is always active
    }
    pub fn is_t1(&self) -> bool {
        *self >= Self::T1
    }
    pub fn is_t1a(&self) -> bool {
        *self >= Self::T1A
    }
    pub fn is_t1b(&self) -> bool {
        *self >= Self::T1B
    }
    pub fn is_t1c(&self) -> bool {
        *self >= Self::T1C
    }
    pub fn is_t2(&self) -> bool {
        *self >= Self::T2
    }
    pub fn is_t3(&self) -> bool {
        *self >= Self::T3
    }

    /// Gas cost for using an existing 2D nonce key (cold SLOAD + warm SSTORE reset).
    /// Ported from Tempo writer: crates/chainspec/src/spec.rs
    pub const fn gas_existing_nonce_key(&self) -> u64 {
        // T1 value: COLD_SLOAD (2100) + WARM_SSTORE_RESET (2900) = 5000
        // T2 adds 2 * WARM_SLOAD (100) = 5200
        match self {
            Self::Genesis | Self::T1 | Self::T1A | Self::T1B | Self::T1C => {
                // COLD_SLOAD_COST + WARM_SSTORE_RESET = 2100 + 2900
                5_000
            }
            Self::T2 | Self::T3 => {
                // T2 adds 2 warm SLOADs for extended nonce key lookup
                5_200
            }
        }
    }

    /// Gas cost for using a new 2D nonce key (cold SLOAD + SSTORE set for 0 -> non-zero).
    /// Ported from Tempo writer: crates/chainspec/src/spec.rs
    pub const fn gas_new_nonce_key(&self) -> u64 {
        // T1 value: COLD_SLOAD (2100) + SSTORE_SET (20000) = 22100
        // T2 adds 2 * WARM_SLOAD (100) = 22300
        match self {
            Self::Genesis | Self::T1 | Self::T1A | Self::T1B | Self::T1C => {
                // COLD_SLOAD_COST + SSTORE_SET = 2100 + 20000
                22_100
            }
            Self::T2 | Self::T3 => {
                // T2 adds 2 warm SLOADs for extended nonce key lookup
                22_300
            }
        }
    }
}

impl Default for TempoHardfork {
    /// Returns the latest hardfork (`T3`) for cases where timestamp is not
    /// available. This preserves the original "always latest spec" behavior.
    fn default() -> Self {
        Self::T3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn is_methods_on_genesis() {
        let hf = TempoHardfork::Genesis;
        assert!(hf.is_t0());
        assert!(!hf.is_t1());
        assert!(!hf.is_t1a());
        assert!(!hf.is_t1b());
        assert!(!hf.is_t1c());
        assert!(!hf.is_t2());
        assert!(!hf.is_t3());
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
    }

    #[test]
    fn default_is_latest() {
        let hf = TempoHardfork::default();
        assert_eq!(hf, TempoHardfork::T3);
        assert!(hf.is_t2());
        assert!(hf.is_t3());
    }
}
