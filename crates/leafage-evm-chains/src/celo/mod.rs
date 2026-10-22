//! Celo L2 execution. L1 history and CIP-64 fee currencies are not handled here.

pub mod evm;

use leafage_evm_types::{BlockEnv, CfgEnv, OpSpecId};
use revm::primitives::{address, Address};

pub const CELO_CHAIN_ID: u64 = 42220;
pub const CELO_TOKEN: Address = address!("471ece3750da237f93b8e339c536989b8978a438");
pub const MAX_CODE_SIZE: usize = 65_536;
pub const L2_MIGRATION_BLOCK: u64 = 31_056_500;

// celo-org/celo-kona@9a608fbb, crates/celo-reth/res/genesis/mainnet.toml.
const ISTHMUS_TIMESTAMP: u64 = 1_752_073_200;
const JOVIAN_TIMESTAMP: u64 = 1_774_958_788;

/// Selects the mainnet Celo L2 schedule independently of plain OP execution.
#[derive(Clone, Copy, Debug)]
pub struct CeloConfig;

impl CeloConfig {
    pub fn cfg_at(cfg: CfgEnv<OpSpecId>, block: &BlockEnv) -> Result<CfgEnv<OpSpecId>, String> {
        if cfg.chain_id != CELO_CHAIN_ID {
            return Err(format!("celo evm-type only supports chain {CELO_CHAIN_ID}"));
        }
        if block.number < L2_MIGRATION_BLOCK {
            return Err(format!(
                "celo evm-type requires an L2 block (>= {L2_MIGRATION_BLOCK})"
            ));
        }
        let spec = if block.timestamp >= JOVIAN_TIMESTAMP {
            OpSpecId::JOVIAN
        } else if block.timestamp >= ISTHMUS_TIMESTAMP {
            OpSpecId::ISTHMUS
        } else {
            OpSpecId::GRANITE
        };
        let mut cfg = cfg.with_spec_and_mainnet_gas_params(spec);
        cfg.limit_contract_code_size = Some(MAX_CODE_SIZE);
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_forks_and_l2_boundary() {
        let mut cfg = CfgEnv::new_with_spec(OpSpecId::KARST);
        cfg.chain_id = CELO_CHAIN_ID;
        let mut block = BlockEnv {
            number: revm::primitives::U256::from(L2_MIGRATION_BLOCK),
            ..Default::default()
        };
        for (timestamp, spec) in [
            (ISTHMUS_TIMESTAMP - 1, OpSpecId::GRANITE),
            (ISTHMUS_TIMESTAMP, OpSpecId::ISTHMUS),
            (JOVIAN_TIMESTAMP - 1, OpSpecId::ISTHMUS),
            (JOVIAN_TIMESTAMP, OpSpecId::JOVIAN),
        ] {
            block.timestamp = revm::primitives::U256::from(timestamp);
            let actual = CeloConfig::cfg_at(cfg.clone(), &block).unwrap();
            assert_eq!(actual.spec, spec);
            assert_eq!(actual.limit_contract_code_size, Some(MAX_CODE_SIZE));
        }
        block.number -= revm::primitives::U256::from(1);
        assert!(CeloConfig::cfg_at(cfg, &block).is_err());
    }
}
