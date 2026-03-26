use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    primitives::{Address, Bytes, TxKind, B256, U256},
};

/// A single call within a Tempo batch (AA tx `Vec<Call>`).
#[derive(Clone, Debug, Default)]
pub struct TempoCall {
    pub to: TxKind,
    pub value: U256,
    pub input: Bytes,
}

/// Extended fields for Tempo transactions (type 0x76).
#[derive(Clone, Debug, Default)]
pub struct TempoTxFields {
    /// Multiple calls executed atomically.
    pub aa_calls: Vec<TempoCall>,
    /// 2D nonce key (0 = protocol nonce, non-zero = NonceManager).
    pub nonce_key: U256,
}

/// Tempo transaction environment wrapping the standard [`TxEnv`].
///
/// For non-batch transactions `tempo_fields` is `None` and all behaviour
/// is identical to a plain `TxEnv`.
#[derive(Clone, Debug, Default)]
pub struct TempoTxEnv {
    pub base: TxEnv,
    /// Present only for type-0x76 batch transactions.
    pub tempo_fields: Option<TempoTxFields>,
}

// ---------------------------------------------------------------------------
// revm Transaction trait – delegates everything to `self.base`
// ---------------------------------------------------------------------------

impl Transaction for TempoTxEnv {
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
        self.base.max_priority_fee_per_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.base.effective_gas_price(base_fee)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tempo_tx_env_default() {
        let tx = TempoTxEnv::default();
        assert!(tx.tempo_fields.is_none());
        // TxEnv defaults gas_limit to TX_GAS_LIMIT_CAP (EIP-7825).
        assert!(tx.gas_limit() > 0);
    }

    #[test]
    fn test_tempo_tx_env_delegates() {
        let tx = TempoTxEnv {
            base: TxEnv {
                tx_type: 0x76,
                gas_limit: 1_000_000,
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: vec![TempoCall {
                    to: TxKind::Create,
                    value: U256::from(42),
                    input: Bytes::new(),
                }],
                nonce_key: U256::ZERO,
            }),
        };
        assert_eq!(tx.tx_type(), 0x76);
        assert_eq!(tx.gas_limit(), 1_000_000);
        assert_eq!(tx.tempo_fields.as_ref().unwrap().aa_calls.len(), 1);
    }
}
