//! Hardfork schedules of the OP-stack chains served by the `op` evm-type.
//!
//! Every OP-stack chain runs the same fork rules but activates them at its own
//! times. A chain listed here executes each call under the fork of its block; a
//! chain that is not listed keeps the configured spec.
//!
//! Timestamps come from the superchain-registry (`superchain/configs/mainnet`)
//! and each chain's boundaries were checked against its public RPC.

use super::mainnet::{op_mainnet_spec_at, OP_MAINNET_CHAIN_ID};
use leafage_evm_types::OpSpecId;

pub const SONEIUM_CHAIN_ID: u64 = 1868;
pub const UNICHAIN_CHAIN_ID: u64 = 130;

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

/// The fork active on `chain_id` at `(block_number, timestamp)`, or `None` for a
/// chain without a built-in schedule.
pub fn op_stack_spec_at(chain_id: u64, block_number: u64, timestamp: u64) -> Option<OpSpecId> {
    match chain_id {
        OP_MAINNET_CHAIN_ID => Some(op_mainnet_spec_at(block_number, timestamp)),
        SONEIUM_CHAIN_ID => Some(SONEIUM.spec_at(timestamp)),
        UNICHAIN_CHAIN_ID => Some(UNICHAIN.spec_at(timestamp)),
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
