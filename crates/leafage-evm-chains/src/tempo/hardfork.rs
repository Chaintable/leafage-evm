/// Minimal Tempo hardfork enum for leafage-evm.
/// All `is_*()` methods return true -- leafage always runs latest spec.
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

impl TempoHardfork {
    pub fn is_t1(&self) -> bool {
        true
    }
    pub fn is_t1a(&self) -> bool {
        true
    }
    pub fn is_t1b(&self) -> bool {
        true
    }
    pub fn is_t1c(&self) -> bool {
        true
    }
    pub fn is_t2(&self) -> bool {
        true
    }
    pub fn is_t3(&self) -> bool {
        true
    }
}

impl Default for TempoHardfork {
    fn default() -> Self {
        Self::T3
    }
}
