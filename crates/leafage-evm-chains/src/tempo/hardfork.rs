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
// T2 is not yet scheduled on mainnet. Use u64::MAX as sentinel so that
// `from_timestamp` never returns T2 until an activation time is set.
const MAINNET_T2_TIME: u64 = u64::MAX;

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
    fn from_timestamp_t2_not_scheduled() {
        // T2 is not yet scheduled (sentinel u64::MAX), so T1C stays active
        assert_eq!(
            TempoHardfork::from_timestamp(MAINNET_T1C_TIME + 100_000_000),
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
