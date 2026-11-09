use crate::api_impl::core::TxSetter;
use alloy::primitives::{Address, Bytes, TxKind, B256, U256};
use revm::context::{Transaction, TxEnv};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CeloFields {
    pub fee_currency: Option<Address>,
    pub gateway_fee_recipient: Option<Address>,
    #[serde(default)]
    pub gateway_fee: U256,
    pub max_fee_in_fee_currency: Option<U256>,
    #[serde(default)]
    pub eth_compatible: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CeloCallTx {
    pub base: TxEnv,
    pub(super) fields: CeloFields,
    pub fee_cap: U256,
    pub tip_cap: U256,
    pub estimation: bool,
}

impl TxSetter for CeloCallTx {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
    fn set_gas_estimation(&mut self) {
        self.estimation = true;
        self.base.gas_price = 0;
        self.fee_cap = U256::ZERO;
        self.tip_cap = U256::ZERO;
    }
}

impl Transaction for CeloCallTx {
    type AccessListItem<'a> = <TxEnv as Transaction>::AccessListItem<'a>;
    type Authorization<'a> = <TxEnv as Transaction>::Authorization<'a>;
    fn tx_type(&self) -> u8 {
        self.base.tx_type()
    }
    fn caller(&self) -> Address {
        self.base.caller()
    }
    fn gas_limit(&self) -> u64 {
        self.base.gas_limit()
    }
    fn value(&self) -> U256 {
        self.base.value()
    }
    fn input(&self) -> &Bytes {
        self.base.input()
    }
    fn nonce(&self) -> u64 {
        Transaction::nonce(&self.base)
    }
    fn kind(&self) -> TxKind {
        self.base.kind()
    }
    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id()
    }
    fn gas_price(&self) -> u128 {
        self.base.gas_price()
    }
    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.base.access_list()
    }
    fn blob_versioned_hashes(&self) -> &[B256] {
        self.base.blob_versioned_hashes()
    }
    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas()
    }
    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list_len()
    }
    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.base.authorization_list()
    }
    fn max_fee_per_gas(&self) -> u128 {
        self.base.max_fee_per_gas()
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        None
    }
    fn effective_gas_price(&self, _: u128) -> u128 {
        self.base.gas_price
    }
}
