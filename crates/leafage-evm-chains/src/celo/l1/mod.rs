//! Mainnet Celo before the L2 migration. The protocol reference is
//! celo-blockchain/core/vm and celo-bls-snark-rs@eca15ffed3984601cb441600ba3002d12f339e5f.

pub mod consensus;
mod crypto;
pub mod evm;
mod inspect;
mod precompile;
mod pure;
#[cfg(test)]
mod tests;

use super::{CELO_CHAIN_ID, L2_MIGRATION_BLOCK, MAX_CODE_SIZE};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::primitives::hardfork::SpecId;

pub const DONUT_BLOCK: u64 = 6_774_000;
pub const ESPRESSO_BLOCK: u64 = 11_838_440;
pub const GINGERBREAD_BLOCK: u64 = 21_616_000;
pub const EPOCH_SIZE: u64 = 17_280;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum L1Fork {
    Istanbul,
    Donut,
    Espresso,
    Gingerbread,
}

impl L1Fork {
    pub fn at(number: u64) -> Result<Self, String> {
        if number >= L2_MIGRATION_BLOCK {
            return Err(format!(
                "Celo L1 execution requires block < {L2_MIGRATION_BLOCK}"
            ));
        }
        Ok(if number >= GINGERBREAD_BLOCK {
            Self::Gingerbread
        } else if number >= ESPRESSO_BLOCK {
            Self::Espresso
        } else if number >= DONUT_BLOCK {
            Self::Donut
        } else {
            Self::Istanbul
        })
    }

    pub fn spec(self) -> SpecId {
        if self >= Self::Espresso {
            SpecId::LONDON
        } else {
            SpecId::ISTANBUL
        }
    }

    pub fn precompile_active(self, address: u8) -> bool {
        (0xf4..=0xfd).contains(&address)
            || (self >= Self::Donut && address == 0xe1)
            || (self >= Self::Donut && self < Self::Gingerbread && (0xe2..=0xf3).contains(&address))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CeloL1Config;

impl CeloL1Config {
    pub fn cfg_at(
        cfg: CfgEnv<MainnetSpecId>,
        block: &BlockEnv,
    ) -> Result<CfgEnv<MainnetSpecId>, String> {
        if cfg.chain_id != CELO_CHAIN_ID {
            return Err(format!(
                "Celo L1 execution only supports chain {CELO_CHAIN_ID}"
            ));
        }
        let number: u64 = block
            .number
            .try_into()
            .map_err(|_| "Celo block number exceeds u64")?;
        let fork = L1Fork::at(number)?;
        let mut cfg = cfg.with_spec_and_mainnet_gas_params(MainnetSpecId::from(fork.spec()));
        cfg.limit_contract_code_size = Some(MAX_CODE_SIZE);
        cfg.disable_eip3541 = fork < L1Fork::Espresso;
        Ok(cfg)
    }
}
