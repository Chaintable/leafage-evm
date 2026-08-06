use alloy_rlp::{BufMut, Encodable, Header, EMPTY_STRING_CODE};
use revm::{
    context::TxEnv,
    context_interface::transaction::{Transaction, TransactionType},
    primitives::{keccak256, Address, Bytes, TxKind, B256, U256},
};

pub const ARBITRUM_RETRY_TX_TYPE: u8 = 0x68;
pub const ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE: u8 = 0x69;
pub const ARBITRUM_UNSIGNED_TX_TYPE: u8 = 0x65;
pub const ARBITRUM_CONTRACT_TX_TYPE: u8 = 0x66;

/// Nitro `msg.GasPrice`: geth's `ToMessage` defaults an absent
/// `maxPriorityFeePerGas` to 0 — `min(feeCap, basefee)` — while revm's
/// `effective_gas_price` returns the raw fee cap in that case. Legacy and
/// EIP-2930 prices are used verbatim on both sides.
pub fn nitro_message_gas_price(tx: &impl Transaction, basefee: u128) -> u128 {
    let effective = tx.effective_gas_price(basefee);
    let fixed_price = tx.tx_type() == TransactionType::Legacy as u8
        || tx.tx_type() == TransactionType::Eip2930 as u8;
    if !fixed_price && tx.max_priority_fee_per_gas().is_none() {
        effective.min(basefee)
    } else {
        effective
    }
}

#[derive(Clone, Debug, Default)]
pub struct ArbitrumTxEnv {
    pub base: TxEnv,
    pub variant: ArbitrumTxVariant,
    pub context: ArbitrumTxContext,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArbitrumTxContext {
    pub current_l1_block_number: u64,
    /// Nitro run-context analog (`runCtx.IsGasEstimation()`): gas-estimation
    /// runs apply the L1 poster padding (×1.10 cost, 7/8 price adjustment)
    /// that eth_call-style runs must not.
    pub gas_estimation: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ArbitrumTxVariant {
    #[default]
    Ethereum,
    SubmitRetryable(ArbitrumSubmitRetryableTx),
    RetryableRedeem(ArbitrumRetryTx),
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbitrumRetryTx {
    pub chain_id: U256,
    pub nonce: u64,
    pub from: Address,
    pub gas_fee_cap: U256,
    pub gas: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    pub ticket_id: B256,
    pub refund_to: Address,
    pub max_refund: U256,
    pub submission_fee_refund: U256,
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

    pub fn retry_tx(
        &self,
        ticket_id: B256,
        sequence_num: u64,
        gas_fee_cap: U256,
        max_refund: U256,
        submission_fee_refund: U256,
    ) -> ArbitrumRetryTx {
        ArbitrumRetryTx {
            chain_id: self.chain_id,
            nonce: sequence_num,
            from: self.from,
            gas_fee_cap,
            gas: self.gas,
            to: self.retry_to,
            value: self.retry_value,
            data: self.retry_data.clone(),
            ticket_id,
            refund_to: self.fee_refund_addr,
            max_refund,
            submission_fee_refund,
        }
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

impl ArbitrumRetryTx {
    pub fn hash(&self) -> B256 {
        let payload_len = self.chain_id.length()
            + self.nonce.length()
            + self.from.length()
            + self.gas_fee_cap.length()
            + self.gas.length()
            + ArbitrumSubmitRetryableTx::optional_address_rlp_len(&self.to)
            + self.value.length()
            + self.data.length()
            + self.ticket_id.length()
            + self.refund_to.length()
            + self.max_refund.length()
            + self.submission_fee_refund.length();

        let mut out = Vec::with_capacity(payload_len + 8);
        out.push(ARBITRUM_RETRY_TX_TYPE);
        Header {
            list: true,
            payload_length: payload_len,
        }
        .encode(&mut out);
        self.chain_id.encode(&mut out);
        self.nonce.encode(&mut out);
        self.from.encode(&mut out);
        self.gas_fee_cap.encode(&mut out);
        self.gas.encode(&mut out);
        ArbitrumSubmitRetryableTx::encode_optional_address(&self.to, &mut out);
        self.value.encode(&mut out);
        self.data.encode(&mut out);
        self.ticket_id.encode(&mut out);
        self.refund_to.encode(&mut out);
        self.max_refund.encode(&mut out);
        self.submission_fee_refund.encode(&mut out);
        keccak256(out)
    }
}

impl ArbitrumTxEnv {
    pub fn new(base: TxEnv, context: ArbitrumTxContext) -> Self {
        Self {
            base,
            variant: ArbitrumTxVariant::Ethereum,
            context,
        }
    }

    pub fn submit_retryable(mut source: Self, submit_retryable: ArbitrumSubmitRetryableTx) -> Self {
        source.base.tx_type = ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE;
        source.base.caller = submit_retryable.from;
        source.base.gas_limit = submit_retryable.gas;
        source.base.gas_price = submit_retryable.gas_fee_cap.try_into().unwrap_or(u128::MAX);
        source.base.gas_priority_fee = Some(0);
        source.base.kind = submit_retryable
            .retry_to
            .map_or(TxKind::Create, TxKind::Call);
        source.base.value = submit_retryable.retry_value;
        source.base.data = submit_retryable.retry_data.clone();
        source.base.nonce = 0;
        source.variant = ArbitrumTxVariant::SubmitRetryable(submit_retryable);
        source
    }

    pub fn retryable_redeem(
        mut base: TxEnv,
        ticket_id: Option<B256>,
        refund_to: Address,
        context: ArbitrumTxContext,
    ) -> Self {
        base.tx_type = ARBITRUM_RETRY_TX_TYPE;
        let retryable = ArbitrumRetryTx {
            chain_id: base.chain_id.map(U256::from).unwrap_or_default(),
            nonce: base.nonce,
            from: base.caller,
            gas_fee_cap: U256::from(base.gas_price),
            gas: base.gas_limit,
            to: base.kind.to().copied(),
            value: base.value,
            data: base.data.clone(),
            ticket_id: ticket_id.unwrap_or_default(),
            refund_to,
            max_refund: U256::MAX,
            submission_fee_refund: U256::ZERO,
        };
        Self {
            base,
            variant: ArbitrumTxVariant::RetryableRedeem(retryable),
            context,
        }
    }

    pub fn from_retryable(retryable: ArbitrumRetryTx, context: ArbitrumTxContext) -> Self {
        let base = TxEnv {
            tx_type: ARBITRUM_RETRY_TX_TYPE,
            caller: retryable.from,
            gas_limit: retryable.gas,
            gas_price: retryable.gas_fee_cap.try_into().unwrap_or(u128::MAX),
            gas_priority_fee: Some(0),
            kind: retryable.to.map_or(TxKind::Create, TxKind::Call),
            value: retryable.value,
            data: retryable.data.clone(),
            nonce: retryable.nonce,
            chain_id: if retryable.chain_id.is_zero() {
                None
            } else {
                retryable.chain_id.try_into().ok()
            },
            ..Default::default()
        };
        Self {
            base,
            variant: ArbitrumTxVariant::RetryableRedeem(retryable),
            context,
        }
    }

    pub fn submit_retryable_tx(&self) -> Option<&ArbitrumSubmitRetryableTx> {
        match &self.variant {
            ArbitrumTxVariant::SubmitRetryable(tx) => Some(tx),
            _ => None,
        }
    }

    pub fn retryable_redeem_tx(&self) -> Option<&ArbitrumRetryTx> {
        match &self.variant {
            ArbitrumTxVariant::RetryableRedeem(tx) => Some(tx),
            _ => None,
        }
    }

    pub fn is_submit_retryable(&self) -> bool {
        self.submit_retryable_tx().is_some()
    }

    pub fn is_retryable_redeem(&self) -> bool {
        self.retryable_redeem_tx().is_some()
    }

    pub fn is_zero_gas_price_retryable(&self) -> bool {
        self.retryable_redeem_tx()
            .is_some_and(|retryable| retryable.gas_fee_cap.is_zero())
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
    use leafage_evm_types::hex;

    #[test]
    fn nitro_message_gas_price_matches_geth_default_tip_semantics() {
        let missing_tip = TxEnv {
            tx_type: TransactionType::Eip1559 as u8,
            gas_price: 250,
            gas_priority_fee: None,
            ..Default::default()
        };
        assert_eq!(nitro_message_gas_price(&missing_tip, 100), 100);

        let explicit_tip = TxEnv {
            tx_type: TransactionType::Eip1559 as u8,
            gas_price: 250,
            gas_priority_fee: Some(50),
            ..Default::default()
        };
        assert_eq!(nitro_message_gas_price(&explicit_tip, 100), 150);

        let legacy = TxEnv {
            gas_price: 250,
            ..Default::default()
        };
        assert_eq!(nitro_message_gas_price(&legacy, 100), 250);
    }

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
    fn retryable_nil_chain_id_stays_unset_in_tx_env() {
        let submit = ArbitrumSubmitRetryableTx {
            chain_id: U256::ZERO,
            request_id: B256::ZERO,
            from: Address::with_last_byte(1),
            l1_base_fee: U256::ZERO,
            deposit_value: U256::ZERO,
            gas_fee_cap: U256::ZERO,
            gas: 100_000,
            retry_to: Some(Address::with_last_byte(2)),
            retry_value: U256::ZERO,
            beneficiary: Address::with_last_byte(3),
            max_submission_fee: U256::ZERO,
            fee_refund_addr: Address::with_last_byte(4),
            retry_data: Bytes::new(),
        };
        let retryable = submit.retry_tx(
            B256::with_last_byte(5),
            0,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
        );

        let tx = ArbitrumTxEnv::from_retryable(retryable, ArbitrumTxContext::default());

        assert_eq!(tx.chain_id(), None);
        assert_eq!(tx.tx_type(), ARBITRUM_RETRY_TX_TYPE);
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
    fn arbitrum_transaction_hashes_match_nitro_vectors() {
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
        assert_eq!(
            submit.ticket_id(),
            B256::from(hex!(
                "b5cc7f02d7439838cbee893675f3fdddba261626a579726892a434e9bb3e2190"
            ))
        );

        let retryable = ArbitrumRetryTx {
            chain_id: U256::ZERO,
            nonce: 9,
            from: Address::with_last_byte(1),
            gas_fee_cap: U256::from(4),
            gas: 50_000,
            to: Some(Address::with_last_byte(6)),
            value: U256::from(7),
            data: Bytes::from_static(&[11, 12]),
            ticket_id: B256::with_last_byte(5),
            refund_to: Address::with_last_byte(10),
            max_refund: U256::from(123),
            submission_fee_refund: U256::from(456),
        };
        assert_eq!(
            retryable.hash(),
            B256::from(hex!(
                "1da44f94407051bf4eb3eafabdc5cd2c7b45700d7997da04ffc14fb3f198fbd3"
            ))
        );
    }

    #[test]
    fn submit_retryable_submission_fee_matches_nitro_formula() {
        assert_eq!(
            ArbitrumSubmitRetryableTx::submission_fee(10, U256::from(3)),
            U256::from((1_400 + 6 * 10) * 3)
        );
    }
}
