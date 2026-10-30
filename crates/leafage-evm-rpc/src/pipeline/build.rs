//! The pipeline records of a block, as `github.com/Chaintable/pipeline`
//! `util` builds them, and its receipts for the receipts root check.

use super::types::{addr_hex, hash_hex, PipelineBlock, PipelineHeader, PipelineTransaction};
use alloy::consensus::{
    Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, Transaction as _, TxEnvelope, TxType,
};
use alloy::eips::Typed2718;
use alloy::primitives::{Address, U256, U64};
use alloy::rpc::types::Block as RpcBlock;
use revm::context_interface::result::ExecutionResult;

/// The receipt of a transaction, `cumulative_gas_used` in its block.
pub(crate) fn receipt_envelope(
    tx_type: TxType,
    result: &ExecutionResult,
    cumulative_gas_used: u64,
) -> ReceiptEnvelope {
    let receipt = ReceiptWithBloom::from(Receipt {
        status: Eip658Value::Eip658(result.is_success()),
        cumulative_gas_used,
        logs: result.logs().to_vec(),
    });
    match tx_type {
        TxType::Legacy => ReceiptEnvelope::Legacy(receipt),
        TxType::Eip2930 => ReceiptEnvelope::Eip2930(receipt),
        TxType::Eip1559 => ReceiptEnvelope::Eip1559(receipt),
        TxType::Eip4844 => ReceiptEnvelope::Eip4844(receipt),
        TxType::Eip7702 => ReceiptEnvelope::Eip7702(receipt),
    }
}

/// `util.BuildPipelineTransaction`.
pub(crate) fn pipeline_transaction(
    envelope: &TxEnvelope,
    sender: Address,
    result: &ExecutionResult,
    gas_price: u128,
    index: usize,
) -> PipelineTransaction {
    let to = envelope
        .to()
        .unwrap_or_else(|| sender.create(envelope.nonce()));
    let (max_fee_per_gas, max_priority_fee_per_gas) = match envelope.ty() {
        2..=4 => (
            envelope.max_fee_per_gas(),
            envelope.max_priority_fee_per_gas().unwrap_or_default(),
        ),
        _ => (0, 0),
    };
    PipelineTransaction {
        id: hash_hex(envelope.tx_hash()),
        from_addr: addr_hex(&sender),
        to_addr: addr_hex(&to),
        gas_limit: envelope.gas_limit() as i64,
        gas_price,
        gas_used: result.tx_gas_used() as i64,
        status: result.is_success(),
        max_fee_per_gas,
        max_priority_fee_per_gas,
        input: envelope.input().clone(),
        nonce: envelope.nonce() as i64,
        idx: index as i64,
        value: envelope.value(),
    }
}

/// `util.BuildPipelineBlock`.
pub(crate) fn pipeline_block(header: &alloy::rpc::types::Header) -> PipelineBlock {
    PipelineBlock {
        id: hash_hex(&header.hash),
        height: header.number,
        parent_id: hash_hex(&header.parent_hash),
        base_fee_per_gas: header.base_fee_per_gas.unwrap_or_default() as u128,
        miner: addr_hex(&header.beneficiary),
        gas_limit: header.gas_limit as i64,
        gas_used: header.gas_used as i64,
        timestamp: header.timestamp,
        process_start_timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or_default(),
    }
}

/// `util.BuildPilelineBlockHeader`.
pub(crate) fn pipeline_header(block: &RpcBlock) -> PipelineHeader {
    let header = &block.header;
    PipelineHeader {
        number: U256::from(header.number),
        hash: header.hash,
        parent_hash: header.parent_hash,
        nonce: header.nonce,
        mix_hash: header.mix_hash,
        sha3_uncles: header.ommers_hash,
        logs_bloom: header.logs_bloom,
        state_root: header.state_root,
        miner: addr_hex(&header.beneficiary),
        difficulty: header.difficulty,
        extra_data: header.extra_data.clone(),
        gas_limit: U64::from(header.gas_limit),
        gas_used: U64::from(header.gas_used),
        timestamp: U64::from(header.timestamp),
        transactions_root: header.transactions_root,
        receipts_root: header.receipts_root,
        base_fee_per_gas: header.base_fee_per_gas.map(U256::from),
        withdrawals_root: header.withdrawals_root,
        blob_gas_used: header.blob_gas_used.map(U64::from),
        excess_blob_gas: header.excess_blob_gas.map(U64::from),
        parent_beacon_block_root: header.parent_beacon_block_root,
        requests_hash: header.requests_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    #[test]
    fn pipeline_header_uses_standard_requests_hash() {
        let mut block: RpcBlock = Default::default();
        let hash = B256::repeat_byte(0x42);
        block.header.requests_hash = Some(hash);

        let json = serde_json::to_value(pipeline_header(&block)).unwrap();
        assert_eq!(json["requestsHash"], serde_json::json!(hash));
        assert!(!json.as_object().unwrap().contains_key("requestsRoot"));

        let decoded: alloy::rpc::types::Header = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.requests_hash, Some(hash));
    }

    #[test]
    fn pipeline_header_omits_requests_hash_before_activation() {
        let block = RpcBlock::default();
        let json = serde_json::to_value(pipeline_header(&block)).unwrap();
        assert!(!json.as_object().unwrap().contains_key("requestsHash"));
        assert!(!json.as_object().unwrap().contains_key("requestsRoot"));
    }
}
