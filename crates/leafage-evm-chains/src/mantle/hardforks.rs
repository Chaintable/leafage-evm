use leafage_evm_types::{CfgEnv, OpSpecId};
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MantleHardfork(OpSpecId);

impl Deref for MantleHardfork {
    type Target = OpSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MantleHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<OpSpecId> for MantleHardfork {
    fn from(spec: OpSpecId) -> Self {
        Self(spec)
    }
}

impl From<MantleHardfork> for OpSpecId {
    fn from(spec: MantleHardfork) -> Self {
        spec.0
    }
}

impl MantleHardfork {
    pub fn convert_cfg_env(cfg: CfgEnv<MantleHardfork>) -> CfgEnv<OpSpecId> {
        let mut op_cfg = CfgEnv::new_with_spec(cfg.spec.into());
        op_cfg.disable_balance_check = cfg.disable_balance_check;
        op_cfg.disable_eip3607 = cfg.disable_eip3607;
        op_cfg.disable_block_gas_limit = cfg.disable_block_gas_limit;
        op_cfg.disable_base_fee = cfg.disable_base_fee;
        op_cfg.chain_id = cfg.chain_id;
        op_cfg.tx_gas_limit_cap = cfg.tx_gas_limit_cap;
        op_cfg.tx_chain_id_check = cfg.tx_chain_id_check;
        op_cfg.limit_contract_code_size = cfg.limit_contract_code_size;
        op_cfg.limit_contract_initcode_size = cfg.limit_contract_initcode_size;
        op_cfg.disable_nonce_check = cfg.disable_nonce_check;
        op_cfg.max_blobs_per_tx = cfg.max_blobs_per_tx;
        op_cfg.blob_base_fee_update_fraction = cfg.blob_base_fee_update_fraction;
        op_cfg
    }
}
