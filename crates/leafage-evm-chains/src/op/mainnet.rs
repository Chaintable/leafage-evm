//! OP Mainnet hardfork schedule, mirroring op-reth `alloy-op-hardforks`
//! (`optimism/mainnet.rs`) and `spec_by_timestamp_after_bedrock`.
//!
//! The `op` evm-type serves many OP-stack chains with one configured spec; on
//! OP Mainnet a historical height needs the fork that was active at that block.

use leafage_evm_types::OpSpecId;

pub const OP_MAINNET_CHAIN_ID: u64 = 10;

/// First block executed by op-geth / op-reth. Earlier blocks are the legacy
/// l2geth (OVM 2.0) history.
pub const OP_MAINNET_BEDROCK_BLOCK: u64 = 105_235_063;
pub const OP_MAINNET_CANYON_TIMESTAMP: u64 = 1_704_992_401;
pub const OP_MAINNET_ECOTONE_TIMESTAMP: u64 = 1_710_374_401;
pub const OP_MAINNET_FJORD_TIMESTAMP: u64 = 1_720_627_201;
pub const OP_MAINNET_GRANITE_TIMESTAMP: u64 = 1_726_070_401;
pub const OP_MAINNET_HOLOCENE_TIMESTAMP: u64 = 1_736_445_601;
pub const OP_MAINNET_ISTHMUS_TIMESTAMP: u64 = 1_746_806_401;
pub const OP_MAINNET_JOVIAN_TIMESTAMP: u64 = 1_764_691_201;
pub const OP_MAINNET_KARST_TIMESTAMP: u64 = 1_783_526_401;

/// Timestamp forks, newest first; the first active one wins.
const TIMESTAMP_FORKS: [(u64, OpSpecId); 8] = [
    (OP_MAINNET_KARST_TIMESTAMP, OpSpecId::KARST),
    (OP_MAINNET_JOVIAN_TIMESTAMP, OpSpecId::JOVIAN),
    (OP_MAINNET_ISTHMUS_TIMESTAMP, OpSpecId::ISTHMUS),
    (OP_MAINNET_HOLOCENE_TIMESTAMP, OpSpecId::HOLOCENE),
    (OP_MAINNET_GRANITE_TIMESTAMP, OpSpecId::GRANITE),
    (OP_MAINNET_FJORD_TIMESTAMP, OpSpecId::FJORD),
    (OP_MAINNET_ECOTONE_TIMESTAMP, OpSpecId::ECOTONE),
    (OP_MAINNET_CANYON_TIMESTAMP, OpSpecId::CANYON),
];

/// The [`OpSpecId`] active on OP Mainnet at `(block_number, timestamp)`.
///
/// Regolith is active from the Bedrock block (timestamp 0). Pre-Bedrock blocks
/// ran on l2geth (OVM 2.0, Berlin-based), which no `OpSpecId` models; they get
/// `BEDROCK`, the oldest rules available.
pub fn op_mainnet_spec_at(block_number: u64, timestamp: u64) -> OpSpecId {
    if block_number < OP_MAINNET_BEDROCK_BLOCK {
        return OpSpecId::BEDROCK;
    }
    TIMESTAMP_FORKS
        .iter()
        .find(|(activation, _)| timestamp >= *activation)
        .map_or(OpSpecId::REGOLITH, |(_, spec)| *spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_fork_starts_at_its_timestamp() {
        let block = OP_MAINNET_BEDROCK_BLOCK + 1;
        let mut previous = OpSpecId::REGOLITH;
        for &(activation, spec) in TIMESTAMP_FORKS.iter().rev() {
            assert_eq!(op_mainnet_spec_at(block, activation - 1), previous);
            assert_eq!(op_mainnet_spec_at(block, activation), spec);
            previous = spec;
        }
        assert_eq!(op_mainnet_spec_at(block, u64::MAX), OpSpecId::KARST);
    }

    #[test]
    fn bedrock_block_starts_regolith() {
        // Bedrock block header timestamp on OP Mainnet.
        let bedrock_time = 1_686_068_903;
        assert_eq!(
            op_mainnet_spec_at(OP_MAINNET_BEDROCK_BLOCK, bedrock_time),
            OpSpecId::REGOLITH
        );
        assert_eq!(
            op_mainnet_spec_at(OP_MAINNET_BEDROCK_BLOCK - 1, bedrock_time),
            OpSpecId::BEDROCK
        );
        assert_eq!(op_mainnet_spec_at(0, 0), OpSpecId::BEDROCK);
    }

    /// Real OP Mainnet boundary blocks (header timestamps from the chain).
    #[test]
    fn mainnet_boundary_blocks() {
        for (block, timestamp, spec) in [
            (114_696_811, 1_704_992_399, OpSpecId::REGOLITH),
            (114_696_812, 1_704_992_401, OpSpecId::CANYON),
            (153_963_811, 1_783_526_399, OpSpecId::JOVIAN),
            (153_963_812, 1_783_526_401, OpSpecId::KARST),
        ] {
            assert_eq!(op_mainnet_spec_at(block, timestamp), spec, "block {block}");
        }
    }
}
