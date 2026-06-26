use alloy_rlp::{BufMut, Encodable, Header, EMPTY_STRING_CODE};
use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    primitives::{keccak256, Address, Bytes, TxKind, B256, U256},
};

pub const ARBITRUM_RETRY_TX_TYPE: u8 = 0x68;
pub const ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE: u8 = 0x69;
pub const ARBITRUM_UNSIGNED_TX_TYPE: u8 = 0x65;
pub const ARBITRUM_CONTRACT_TX_TYPE: u8 = 0x66;

#[derive(Clone, Debug, Default)]
pub struct ArbitrumTxEnv {
    pub base: TxEnv,
    pub retryable: Option<ArbitrumRetryableRedeemContext>,
    pub context: ArbitrumTxContext,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArbitrumTxContext {
    pub current_l1_block_number: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbitrumRetryableRedeemContext {
    pub ticket_id: Option<B256>,
    pub refund_to: Address,
    pub zero_gas_price: bool,
}

#[derive(Clone, Debug)]
pub struct ArbitrumSubmitRetryableTx {
    pub chain_id: U256,
    pub request_id: B256,
    pub from: Address,
    pub l1_base_fee: U256,
    pub deposit_value: U256,
    pub gas_fee_cap: U256,
    pub gas: u64,
    pub retry_to: Option<Address>,
    pub retry_value: U256,
    pub beneficiary: Address,
    pub max_submission_fee: U256,
    pub fee_refund_addr: Address,
    pub retry_data: Bytes,
}

impl ArbitrumSubmitRetryableTx {
    pub fn submission_fee(calldata_len: usize, l1_base_fee: U256) -> U256 {
        let calldata_units = U256::from(calldata_len)
            .saturating_mul(U256::from(6))
            .saturating_add(U256::from(1_400));
        l1_base_fee.saturating_mul(calldata_units)
    }

    pub fn ticket_id(&self) -> B256 {
        let payload_len = self.chain_id.length()
            + self.request_id.length()
            + self.from.length()
            + self.l1_base_fee.length()
            + self.deposit_value.length()
            + self.gas_fee_cap.length()
            + self.gas.length()
            + Self::optional_address_rlp_len(&self.retry_to)
            + self.retry_value.length()
            + self.beneficiary.length()
            + self.max_submission_fee.length()
            + self.fee_refund_addr.length()
            + self.retry_data.length();

        let mut out = Vec::with_capacity(payload_len + 8);
        out.push(ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE);
        Header {
            list: true,
            payload_length: payload_len,
        }
        .encode(&mut out);
        self.chain_id.encode(&mut out);
        self.request_id.encode(&mut out);
        self.from.encode(&mut out);
        self.l1_base_fee.encode(&mut out);
        self.deposit_value.encode(&mut out);
        self.gas_fee_cap.encode(&mut out);
        self.gas.encode(&mut out);
        Self::encode_optional_address(&self.retry_to, &mut out);
        self.retry_value.encode(&mut out);
        self.beneficiary.encode(&mut out);
        self.max_submission_fee.encode(&mut out);
        self.fee_refund_addr.encode(&mut out);
        self.retry_data.encode(&mut out);
        keccak256(out)
    }

    fn optional_address_rlp_len(address: &Option<Address>) -> usize {
        address.as_ref().map_or(1, |address| address.length())
    }

    fn encode_optional_address(address: &Option<Address>, out: &mut dyn BufMut) {
        match address {
            Some(address) => address.encode(out),
            None => out.put_u8(EMPTY_STRING_CODE),
        }
    }
}

impl ArbitrumTxEnv {
    pub fn new(base: TxEnv, context: ArbitrumTxContext) -> Self {
        Self {
            base,
            retryable: None,
            context,
        }
    }

    pub fn retryable_redeem(
        mut base: TxEnv,
        ticket_id: Option<B256>,
        refund_to: Address,
        context: ArbitrumTxContext,
    ) -> Self {
        let zero_gas_price = base.gas_price == 0;
        base.tx_type = ARBITRUM_RETRY_TX_TYPE;
        Self {
            base,
            retryable: Some(ArbitrumRetryableRedeemContext {
                ticket_id,
                refund_to,
                zero_gas_price,
            }),
            context,
        }
    }

    pub fn is_retryable_redeem(&self) -> bool {
        self.retryable.is_some()
    }

    pub fn is_zero_gas_price_retryable(&self) -> bool {
        self.retryable
            .as_ref()
            .is_some_and(|retryable| retryable.zero_gas_price)
    }

    pub fn aliases_caller(&self) -> bool {
        matches!(
            self.base.tx_type,
            ARBITRUM_UNSIGNED_TX_TYPE | ARBITRUM_CONTRACT_TX_TYPE | ARBITRUM_RETRY_TX_TYPE
        )
    }
}

impl Transaction for ArbitrumTxEnv {
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
    fn retryable_redeem_marks_custom_tx_type() {
        let tx = ArbitrumTxEnv::retryable_redeem(
            TxEnv {
                gas_limit: 100_000,
                ..Default::default()
            },
            Some(B256::with_last_byte(1)),
            Address::with_last_byte(2),
            ArbitrumTxContext::default(),
        );

        assert_eq!(tx.tx_type(), ARBITRUM_RETRY_TX_TYPE);
        assert!(tx.is_retryable_redeem());
        assert!(tx.is_zero_gas_price_retryable());
        assert_eq!(tx.gas_limit(), 100_000);
    }

    #[test]
    fn retryable_redeem_tracks_nonzero_gas_price() {
        let tx = ArbitrumTxEnv::retryable_redeem(
            TxEnv {
                gas_price: 1,
                ..Default::default()
            },
            None,
            Address::ZERO,
            ArbitrumTxContext::default(),
        );

        assert!(!tx.is_zero_gas_price_retryable());
    }

    #[test]
    fn aliases_caller_matches_nitro_tx_types() {
        for tx_type in [
            ARBITRUM_UNSIGNED_TX_TYPE,
            ARBITRUM_CONTRACT_TX_TYPE,
            ARBITRUM_RETRY_TX_TYPE,
        ] {
            let tx = ArbitrumTxEnv::new(
                TxEnv {
                    tx_type,
                    ..Default::default()
                },
                ArbitrumTxContext::default(),
            );
            assert!(tx.aliases_caller(), "tx type {tx_type:#x} should alias");
        }

        for tx_type in [0, ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE] {
            let tx = ArbitrumTxEnv::new(
                TxEnv {
                    tx_type,
                    ..Default::default()
                },
                ArbitrumTxContext::default(),
            );
            assert!(
                !tx.aliases_caller(),
                "tx type {tx_type:#x} should not alias"
            );
        }
    }

    #[test]
    fn submit_retryable_ticket_id_is_typed_hash() {
        let submit = ArbitrumSubmitRetryableTx {
            chain_id: U256::ZERO,
            request_id: B256::ZERO,
            from: Address::with_last_byte(1),
            l1_base_fee: U256::from(2),
            deposit_value: U256::from(3),
            gas_fee_cap: U256::from(4),
            gas: 5,
            retry_to: Some(Address::with_last_byte(6)),
            retry_value: U256::from(7),
            beneficiary: Address::with_last_byte(8),
            max_submission_fee: U256::from(9),
            fee_refund_addr: Address::with_last_byte(10),
            retry_data: Bytes::from_static(&[11, 12]),
        };

        let mut payload = Vec::new();
        submit.chain_id.encode(&mut payload);
        submit.request_id.encode(&mut payload);
        submit.from.encode(&mut payload);
        submit.l1_base_fee.encode(&mut payload);
        submit.deposit_value.encode(&mut payload);
        submit.gas_fee_cap.encode(&mut payload);
        submit.gas.encode(&mut payload);
        submit.retry_to.unwrap().encode(&mut payload);
        submit.retry_value.encode(&mut payload);
        submit.beneficiary.encode(&mut payload);
        submit.max_submission_fee.encode(&mut payload);
        submit.fee_refund_addr.encode(&mut payload);
        submit.retry_data.encode(&mut payload);

        let mut encoded = vec![ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE];
        Header {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut encoded);
        encoded.extend_from_slice(&payload);

        assert_eq!(submit.ticket_id(), keccak256(encoded));
    }

    #[test]
    fn submit_retryable_submission_fee_matches_nitro_formula() {
        assert_eq!(
            ArbitrumSubmitRetryableTx::submission_fee(10, U256::from(3)),
            U256::from((1_400 + 6 * 10) * 3)
        );
    }
}
