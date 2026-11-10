use super::{CeloL1Config, GINGERBREAD_BLOCK};
use alloy::primitives::{Address, Bytes, TxKind, B256, U256};
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, MainnetSpecId};
use revm::context::{Transaction, TxEnv};
use revm::DatabaseRef;
use serde::Deserialize;

#[derive(Debug)]
pub enum CallInputError<E> {
    InvalidParams(String),
    Database(E),
}

pub fn create_txn_env<DB: DatabaseRef>(
    block: &BlockEnv,
    cfg: CfgEnv<MainnetSpecId>,
    request: CallRequest,
    db: DB,
    chain_id: u64,
) -> Result<CeloCallTx, CallInputError<DB::Error>> {
    let cfg = CeloL1Config::cfg_at(cfg, block).map_err(CallInputError::InvalidParams)?;
    let fields: CeloFields = request
        .other
        .deserialize_as()
        .map_err(|e| CallInputError::InvalidParams(e.to_string()))?;
    if request.gas_price.is_some()
        && (request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some())
    {
        return Err(CallInputError::InvalidParams(
            "both gasPrice and dynamic fee fields specified".into(),
        ));
    }
    if request.max_fee_per_blob_gas.is_some() {
        return Err(CallInputError::InvalidParams(
            "Invalid fee parameters".into(),
        ));
    }
    // Archive headers can synthesize a base fee before Celo enabled it.
    let (price, cap, tip) = if let Some(price) = request.gas_price {
        (price, price, price)
    } else if block.number >= U256::from(GINGERBREAD_BLOCK) {
        let cap = request.max_fee_per_gas.unwrap_or_default();
        let tip = request.max_priority_fee_per_gas.unwrap_or_default();
        (cap.min(tip.saturating_add(block.basefee as u128)), cap, tip)
    } else {
        (0, 0, 0)
    };
    let caller = request.from.unwrap_or_default();
    let nonce = db
        .basic_ref(caller)
        .map_err(CallInputError::Database)?
        .map(|account| account.nonce)
        .unwrap_or_default();
    let gas_cap = cfg
        .tx_gas_limit_cap
        .filter(|&cap| cap != 0)
        .unwrap_or(block.gas_limit);
    let request = request.inner;
    Ok(CeloCallTx {
        base: TxEnv {
            tx_type: 0,
            caller,
            nonce,
            gas_limit: request.gas.map_or(gas_cap, |gas| gas.min(gas_cap)),
            gas_price: price,
            gas_priority_fee: None,
            kind: request.to.unwrap_or(TxKind::Create),
            value: request.value.unwrap_or_default(),
            data: request.input.into_input().unwrap_or_default(),
            chain_id: Some(chain_id),
            access_list: request.access_list.unwrap_or_default(),
            blob_hashes: request.blob_versioned_hashes.unwrap_or_default(),
            authorization_list: request
                .authorization_list
                .unwrap_or_default()
                .into_iter()
                .map(alloy::signers::Either::Left)
                .collect(),
            ..Default::default()
        },
        fields,
        fee_cap: U256::from(cap),
        tip_cap: U256::from(tip),
        estimation: false,
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CeloFields {
    pub fee_currency: Option<Address>,
    pub gateway_fee_recipient: Option<Address>,
    #[serde(default, deserialize_with = "default_on_null")]
    pub gateway_fee: U256,
    pub max_fee_in_fee_currency: Option<U256>,
    #[serde(default, deserialize_with = "default_on_null")]
    pub eth_compatible: bool,
}

// RPC callers may explicitly send null for omitted Celo parameters.
fn default_on_null<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Clone, Debug)]
pub struct CeloCallTx {
    pub base: TxEnv,
    pub(super) fields: CeloFields,
    pub fee_cap: U256,
    pub tip_cap: U256,
    pub estimation: bool,
}

impl CeloCallTx {
    pub fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
    pub fn set_gas_estimation(&mut self) {
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
