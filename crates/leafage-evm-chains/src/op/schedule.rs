//! Hardfork schedules of the OP-stack chains served by the `op` evm-type.
//!
//! Every OP-stack chain runs the same fork rules but activates them at its own
//! times. A chain listed here executes each call under the fork of its block; a
//! chain that is not listed keeps the configured spec.
//!
//! Timestamps come from the superchain-registry (`superchain/configs/mainnet`)
//! and each chain's boundaries were checked against its public RPC.
//!
//! A schedule ends at the chain's latest activated fork. When a chain schedules a
//! new fork, add it here before it activates: until then its calls keep running
//! the previous fork's rules.

use super::mainnet::{op_mainnet_spec_at, OP_MAINNET_CHAIN_ID};
use leafage_evm_types::OpSpecId;

pub const SONEIUM_CHAIN_ID: u64 = 1868;
pub const UNICHAIN_CHAIN_ID: u64 = 130;
pub const CYBER_CHAIN_ID: u64 = 7560;
pub const FRAXTAL_CHAIN_ID: u64 = 252;

/// A chain that started after Bedrock: `genesis` rules from block 0, then each
/// `(activation timestamp, fork)` in ascending order.
struct TimestampSchedule {
    genesis: OpSpecId,
    forks: &'static [(u64, OpSpecId)],
}

impl TimestampSchedule {
    fn spec_at(&self, timestamp: u64) -> OpSpecId {
        self.forks
            .iter()
            .rev()
            .find(|(activation, _)| timestamp >= *activation)
            .map_or(self.genesis, |(_, spec)| *spec)
    }
}

/// Soneium: Canyon..Granite at genesis (2024-12-02).
const SONEIUM: TimestampSchedule = TimestampSchedule {
    genesis: OpSpecId::GRANITE,
    forks: &[
        (1_738_573_200, OpSpecId::HOLOCENE),
        (1_746_806_401, OpSpecId::ISTHMUS),
        (1_764_691_201, OpSpecId::JOVIAN),
        (1_783_526_401, OpSpecId::KARST),
    ],
};

/// Unichain: Canyon..Granite at genesis (2024-11-04).
const UNICHAIN: TimestampSchedule = TimestampSchedule {
    genesis: OpSpecId::GRANITE,
    forks: &[
        (1_736_445_601, OpSpecId::HOLOCENE),
        (1_746_806_401, OpSpecId::ISTHMUS),
        (1_764_691_201, OpSpecId::JOVIAN),
        (1_783_526_401, OpSpecId::KARST),
    ],
};

/// Cyber: Canyon..Ecotone at genesis (2024-04-18). Holocene (2026-01-09) is its
/// latest fork: no Isthmus, Jovian or Karst yet.
const CYBER: TimestampSchedule = TimestampSchedule {
    genesis: OpSpecId::ECOTONE,
    forks: &[
        (1_767_754_800, OpSpecId::FJORD),
        (1_767_841_200, OpSpecId::GRANITE),
        (1_767_927_600, OpSpecId::HOLOCENE),
    ],
};

/// Fraxtal: Canyon at genesis (2024-02-01). Isthmus (2025-08-20) is its latest
/// fork: no Jovian or Karst yet. The superchain-registry stops at Granite; Holocene
/// and Isthmus come from FraxFinance/fraxtal-node `mainnet/rollup.json`.
const FRAXTAL: TimestampSchedule = TimestampSchedule {
    genesis: OpSpecId::CANYON,
    forks: &[
        (1_717_009_201, OpSpecId::ECOTONE),
        (1_733_947_201, OpSpecId::FJORD),
        (1_738_958_401, OpSpecId::GRANITE),
        (1_744_052_401, OpSpecId::HOLOCENE),
        (1_755_716_401, OpSpecId::ISTHMUS),
    ],
};

/// The fork active on `chain_id` at `(block_number, timestamp)`, or `None` for a
/// chain without a built-in schedule.
pub fn op_stack_spec_at(chain_id: u64, block_number: u64, timestamp: u64) -> Option<OpSpecId> {
    match chain_id {
        OP_MAINNET_CHAIN_ID => Some(op_mainnet_spec_at(block_number, timestamp)),
        SONEIUM_CHAIN_ID => Some(SONEIUM.spec_at(timestamp)),
        UNICHAIN_CHAIN_ID => Some(UNICHAIN.spec_at(timestamp)),
        CYBER_CHAIN_ID => Some(CYBER.spec_at(timestamp)),
        FRAXTAL_CHAIN_ID => Some(FRAXTAL.spec_at(timestamp)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_boundaries(schedule: &TimestampSchedule) {
        let mut previous = schedule.genesis;
        assert_eq!(schedule.spec_at(0), previous);
        for &(activation, spec) in schedule.forks {
            assert!(spec > previous, "forks must ascend");
            assert_eq!(schedule.spec_at(activation - 1), previous);
            assert_eq!(schedule.spec_at(activation), spec);
            previous = spec;
        }
        assert_eq!(schedule.spec_at(u64::MAX), previous);
    }

    #[test]
    fn each_fork_starts_at_its_timestamp() {
        assert_boundaries(&SONEIUM);
        assert_boundaries(&UNICHAIN);
        assert_boundaries(&CYBER);
        assert_boundaries(&FRAXTAL);
    }

    /// Real boundary blocks (number, header timestamp) from each chain.
    #[test]
    fn chain_boundary_blocks() {
        for (chain_id, block, timestamp, spec) in [
            (SONEIUM_CHAIN_ID, 1, 1_733_134_753, OpSpecId::GRANITE),
            (
                SONEIUM_CHAIN_ID,
                2_719_224,
                1_738_573_199,
                OpSpecId::GRANITE,
            ),
            (
                SONEIUM_CHAIN_ID,
                2_719_225,
                1_738_573_201,
                OpSpecId::HOLOCENE,
            ),
            (
                SONEIUM_CHAIN_ID,
                6_835_824,
                1_746_806_399,
                OpSpecId::HOLOCENE,
            ),
            (
                SONEIUM_CHAIN_ID,
                6_835_825,
                1_746_806_401,
                OpSpecId::ISTHMUS,
            ),
            (
                SONEIUM_CHAIN_ID,
                15_778_224,
                1_764_691_199,
                OpSpecId::ISTHMUS,
            ),
            (
                SONEIUM_CHAIN_ID,
                15_778_225,
                1_764_691_201,
                OpSpecId::JOVIAN,
            ),
            (
                SONEIUM_CHAIN_ID,
                25_195_824,
                1_783_526_399,
                OpSpecId::JOVIAN,
            ),
            (SONEIUM_CHAIN_ID, 25_195_825, 1_783_526_401, OpSpecId::KARST),
            // Unichain has 1s blocks.
            (UNICHAIN_CHAIN_ID, 1, 1_730_748_360, OpSpecId::GRANITE),
            (
                UNICHAIN_CHAIN_ID,
                5_697_241,
                1_736_445_600,
                OpSpecId::GRANITE,
            ),
            (
                UNICHAIN_CHAIN_ID,
                5_697_242,
                1_736_445_601,
                OpSpecId::HOLOCENE,
            ),
            (
                UNICHAIN_CHAIN_ID,
                16_058_041,
                1_746_806_400,
                OpSpecId::HOLOCENE,
            ),
            (
                UNICHAIN_CHAIN_ID,
                16_058_042,
                1_746_806_401,
                OpSpecId::ISTHMUS,
            ),
            (
                UNICHAIN_CHAIN_ID,
                33_942_841,
                1_764_691_200,
                OpSpecId::ISTHMUS,
            ),
            (
                UNICHAIN_CHAIN_ID,
                33_942_842,
                1_764_691_201,
                OpSpecId::JOVIAN,
            ),
            (
                UNICHAIN_CHAIN_ID,
                52_778_041,
                1_783_526_400,
                OpSpecId::JOVIAN,
            ),
            (
                UNICHAIN_CHAIN_ID,
                52_778_042,
                1_783_526_401,
                OpSpecId::KARST,
            ),
        ] {
            assert_eq!(
                op_stack_spec_at(chain_id, block, timestamp),
                Some(spec),
                "chain {chain_id} block {block}"
            );
        }
    }

    /// Cyber and Fraxtal are behind OP Mainnet: their head is not Karst.
    #[test]
    fn chains_behind_the_latest_fork() {
        for (chain_id, block, timestamp, spec) in [
            (CYBER_CHAIN_ID, 1, 1_713_428_569, OpSpecId::ECOTONE),
            (CYBER_CHAIN_ID, 27_163_116, 1_767_754_799, OpSpecId::ECOTONE),
            (CYBER_CHAIN_ID, 27_163_117, 1_767_754_801, OpSpecId::FJORD),
            (CYBER_CHAIN_ID, 27_206_316, 1_767_841_199, OpSpecId::FJORD),
            (CYBER_CHAIN_ID, 27_206_317, 1_767_841_201, OpSpecId::GRANITE),
            (CYBER_CHAIN_ID, 27_249_516, 1_767_927_599, OpSpecId::GRANITE),
            (
                CYBER_CHAIN_ID,
                27_249_517,
                1_767_927_601,
                OpSpecId::HOLOCENE,
            ),
            // 2026-11-16, after OP Mainnet's Karst.
            (
                CYBER_CHAIN_ID,
                40_686_578,
                1_794_801_723,
                OpSpecId::HOLOCENE,
            ),
            (FRAXTAL_CHAIN_ID, 1, 1_706_810_713, OpSpecId::CANYON),
            (FRAXTAL_CHAIN_ID, 5_099_244, 1_717_009_199, OpSpecId::CANYON),
            (
                FRAXTAL_CHAIN_ID,
                5_099_245,
                1_717_009_201,
                OpSpecId::ECOTONE,
            ),
            (
                FRAXTAL_CHAIN_ID,
                13_568_244,
                1_733_947_199,
                OpSpecId::ECOTONE,
            ),
            (FRAXTAL_CHAIN_ID, 13_568_245, 1_733_947_201, OpSpecId::FJORD),
            (FRAXTAL_CHAIN_ID, 16_073_844, 1_738_958_399, OpSpecId::FJORD),
            (
                FRAXTAL_CHAIN_ID,
                16_073_845,
                1_738_958_401,
                OpSpecId::GRANITE,
            ),
            (
                FRAXTAL_CHAIN_ID,
                18_620_844,
                1_744_052_399,
                OpSpecId::GRANITE,
            ),
            (
                FRAXTAL_CHAIN_ID,
                18_620_845,
                1_744_052_401,
                OpSpecId::HOLOCENE,
            ),
            (
                FRAXTAL_CHAIN_ID,
                24_452_844,
                1_755_716_399,
                OpSpecId::HOLOCENE,
            ),
            (
                FRAXTAL_CHAIN_ID,
                24_452_845,
                1_755_716_401,
                OpSpecId::ISTHMUS,
            ),
            // 2026-11-16, after OP Mainnet's Karst.
            (
                FRAXTAL_CHAIN_ID,
                43_995_519,
                1_794_801_749,
                OpSpecId::ISTHMUS,
            ),
        ] {
            assert_eq!(
                op_stack_spec_at(chain_id, block, timestamp),
                Some(spec),
                "chain {chain_id} block {block}"
            );
        }
    }

    #[test]
    fn op_mainnet_keeps_its_own_schedule() {
        // Pre-Bedrock, Regolith and Karst blocks.
        for (block, timestamp, spec) in [
            (105_235_062, 1_686_068_901, OpSpecId::BEDROCK),
            (105_235_063, 1_686_068_903, OpSpecId::REGOLITH),
            (153_963_812, 1_783_526_401, OpSpecId::KARST),
        ] {
            assert_eq!(
                op_stack_spec_at(OP_MAINNET_CHAIN_ID, block, timestamp),
                Some(spec)
            );
        }
    }

    #[test]
    fn unlisted_chains_have_no_schedule() {
        for chain_id in [1, 480, 4153, 5000, 8453, 57073, 59144] {
            assert_eq!(op_stack_spec_at(chain_id, 1, 1_783_526_401), None);
        }
    }
}
