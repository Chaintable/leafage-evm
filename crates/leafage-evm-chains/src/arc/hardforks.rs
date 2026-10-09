/// Arc feature hardforks.
///
/// These are independent feature flags rather than a linear execution spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArcHardfork {
    Zero3,
    Zero4,
    Zero5,
    Zero6,
    Zero7,
    Zero8,
}

impl ArcHardfork {
    const ALL: [Self; 6] = [
        Self::Zero3,
        Self::Zero4,
        Self::Zero5,
        Self::Zero6,
        Self::Zero7,
        Self::Zero8,
    ];
}

/// Activation condition for one Arc feature hardfork.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArcForkActivation {
    Block(u64),
    Timestamp(u64),
    #[default]
    Never,
}

impl ArcForkActivation {
    pub const fn is_active_at(self, block_number: u64, timestamp: u64) -> bool {
        match self {
            Self::Block(activation_block) => block_number >= activation_block,
            Self::Timestamp(activation_timestamp) => timestamp >= activation_timestamp,
            Self::Never => false,
        }
    }
}

/// Per-feature Arc hardfork activation schedule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArcHardforkSchedule {
    zero3: ArcForkActivation,
    zero4: ArcForkActivation,
    zero5: ArcForkActivation,
    zero6: ArcForkActivation,
    zero7: ArcForkActivation,
    zero8: ArcForkActivation,
}

impl ArcHardforkSchedule {
    pub const fn new(
        zero3: ArcForkActivation,
        zero4: ArcForkActivation,
        zero5: ArcForkActivation,
        zero6: ArcForkActivation,
        zero7: ArcForkActivation,
        zero8: ArcForkActivation,
    ) -> Self {
        Self {
            zero3,
            zero4,
            zero5,
            zero6,
            zero7,
            zero8,
        }
    }

    pub const fn activation(&self, hardfork: ArcHardfork) -> ArcForkActivation {
        match hardfork {
            ArcHardfork::Zero3 => self.zero3,
            ArcHardfork::Zero4 => self.zero4,
            ArcHardfork::Zero5 => self.zero5,
            ArcHardfork::Zero6 => self.zero6,
            ArcHardfork::Zero7 => self.zero7,
            ArcHardfork::Zero8 => self.zero8,
        }
    }

    pub fn flags_at(&self, block_number: u64, timestamp: u64) -> ArcHardforkFlags {
        ArcHardforkFlags::from_schedule(self, block_number, timestamp)
    }
}

/// Active Arc feature hardforks at one execution environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArcHardforkFlags {
    zero3: bool,
    zero4: bool,
    zero5: bool,
    zero6: bool,
    zero7: bool,
    zero8: bool,
}

impl ArcHardforkFlags {
    pub fn from_schedule(
        schedule: &ArcHardforkSchedule,
        block_number: u64,
        timestamp: u64,
    ) -> Self {
        let mut flags = Self::default();
        for hardfork in ArcHardfork::ALL {
            flags.set(
                hardfork,
                schedule
                    .activation(hardfork)
                    .is_active_at(block_number, timestamp),
            );
        }
        flags
    }

    pub const fn is_active(&self, hardfork: ArcHardfork) -> bool {
        match hardfork {
            ArcHardfork::Zero3 => self.zero3,
            ArcHardfork::Zero4 => self.zero4,
            ArcHardfork::Zero5 => self.zero5,
            ArcHardfork::Zero6 => self.zero6,
            ArcHardfork::Zero7 => self.zero7,
            ArcHardfork::Zero8 => self.zero8,
        }
    }

    fn set(&mut self, hardfork: ArcHardfork, active: bool) {
        match hardfork {
            ArcHardfork::Zero3 => self.zero3 = active,
            ArcHardfork::Zero4 => self.zero4 = active,
            ArcHardfork::Zero5 => self.zero5 = active,
            ArcHardfork::Zero6 => self.zero6 = active,
            ArcHardfork::Zero7 => self.zero7 = active,
            ArcHardfork::Zero8 => self.zero8 = active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_conditions_use_the_matching_dimension() {
        let block = ArcForkActivation::Block(10);
        assert!(!block.is_active_at(9, u64::MAX));
        assert!(block.is_active_at(10, 0));

        let timestamp = ArcForkActivation::Timestamp(20);
        assert!(!timestamp.is_active_at(u64::MAX, 19));
        assert!(timestamp.is_active_at(0, 20));

        assert!(!ArcForkActivation::Never.is_active_at(u64::MAX, u64::MAX));
    }

    #[test]
    fn schedule_resolves_block_and_timestamp_activations_independently() {
        let schedule = ArcHardforkSchedule::new(
            ArcForkActivation::Block(5),
            ArcForkActivation::Timestamp(50),
            ArcForkActivation::Never,
            ArcForkActivation::Timestamp(100),
            ArcForkActivation::Block(10),
            ArcForkActivation::Never,
        );

        let flags = schedule.flags_at(5, 49);
        assert!(flags.is_active(ArcHardfork::Zero3));
        assert!(!flags.is_active(ArcHardfork::Zero4));

        let flags = schedule.flags_at(4, 50);
        assert!(!flags.is_active(ArcHardfork::Zero3));
        assert!(flags.is_active(ArcHardfork::Zero4));

        let flags = schedule.flags_at(10, 100);
        assert!(flags.is_active(ArcHardfork::Zero3));
        assert!(flags.is_active(ArcHardfork::Zero4));
        assert!(!flags.is_active(ArcHardfork::Zero5));
        assert!(flags.is_active(ArcHardfork::Zero6));
        assert!(flags.is_active(ArcHardfork::Zero7));
        assert!(!flags.is_active(ArcHardfork::Zero8));
    }

    #[test]
    fn flags_do_not_imply_earlier_hardforks() {
        let schedule = ArcHardforkSchedule::new(
            ArcForkActivation::Never,
            ArcForkActivation::Block(0),
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            ArcForkActivation::Never,
        );

        let flags = schedule.flags_at(0, 0);
        assert!(!flags.is_active(ArcHardfork::Zero3));
        assert!(flags.is_active(ArcHardfork::Zero4));
    }
}
