#![allow(dead_code)]

use super::constants::*;
use super::types::TransactionData;
use primitive_types::{H160, H256, U256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxExecutionMode {
    VerifyExecute,
    EstimateFee,
    EthCall,
}

pub fn assemble_tx_meta(execution_mode: TxExecutionMode, execute_tx: bool) -> U256 {
    let mut output = [0u8; 32];
    output[0] = match execution_mode {
        TxExecutionMode::VerifyExecute => 0x00,
        TxExecutionMode::EstimateFee => 0x00,
        TxExecutionMode::EthCall => 0x02,
    };
    output[31] = u8::from(execute_tx);
    U256::from_big_endian(&output)
}

fn address_to_u256(addr: H160) -> U256 {
    let mut b = [0u8; 32];
    b[12..32].copy_from_slice(addr.as_bytes());
    U256::from_big_endian(&b)
}

fn h256_to_u256(h: H256) -> U256 {
    U256::from_big_endian(h.as_bytes())
}

#[derive(Debug, Clone)]
pub struct BootloaderEnv {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub prev_block_hash: H256,
    pub fee_account: H160,
    pub fair_pubdata_price: u64,
    pub fair_l2_gas_price: u64,
    pub base_fee: u64,
    pub execution_mode: TxExecutionMode,
}

impl Default for BootloaderEnv {
    fn default() -> Self {
        Self {
            block_number: 1,
            block_timestamp: 1,
            prev_block_hash: H256::zero(),
            fee_account: H160::zero(),
            fair_pubdata_price: 500,
            fair_l2_gas_price: 250_000_000,
            base_fee: 250_000_000,
            execution_mode: TxExecutionMode::EstimateFee,
        }
    }
}

/// Builds the initial bootloader heap memory layout for executing a transaction.
///
/// The offsets are the MediumInterop ones: the `fee_estimate` artifact shipped in
/// `artifacts/` is compiled against that layout, so writing the legacy offsets left
/// the bootloader reading zeroed block info and it reverted during batch setup
/// ("system context upgrade fail") on every estimate.
pub fn build_bootloader_memory(
    tx: &TransactionData,
    env: &BootloaderEnv,
    trusted_gas_limit: U256,
    settlement_layer: u64,
) -> Vec<(usize, U256)> {
    let mut memory = Vec::new();

    // 1. Initial environment header
    memory.push((OPERATOR_ADDRESS_SLOT, address_to_u256(env.fee_account)));
    memory.push((PREV_BLOCK_HASH_SLOT, h256_to_u256(env.prev_block_hash)));
    memory.push((NEW_BLOCK_TIMESTAMP_SLOT, U256::from(env.block_timestamp)));
    memory.push((NEW_BLOCK_NUMBER_SLOT, U256::from(env.block_number)));
    memory.push((FAIR_PUBDATA_PRICE_SLOT, U256::from(env.fair_pubdata_price)));
    memory.push((FAIR_L2_GAS_PRICE_SLOT, U256::from(env.fair_l2_gas_price)));
    memory.push((EXPECTED_BASE_FEE_SLOT, U256::from(env.base_fee)));
    memory.push((SHOULD_SET_NEW_BLOCK_SLOT, U256::zero()));

    // 2. L2 block info for transaction 0
    let block_pos = MEDIUM_INTEROP_TX_OPERATOR_L2_BLOCK_INFO_OFFSET;
    memory.push((block_pos, U256::from(env.block_number)));
    memory.push((block_pos + 1, U256::from(env.block_timestamp)));
    memory.push((block_pos + 2, h256_to_u256(env.prev_block_hash)));
    memory.push((block_pos + 3, U256::one())); // max_virtual_blocks_to_create

    // 3. Interop blocks and settlement layer
    memory.push((MEDIUM_INTEROP_INTEROP_BLOCKS_BEGIN_OFFSET, U256::one())); // number_of_interop_roots + 1
    memory.push((
        MEDIUM_INTEROP_SETTLEMENT_LAYER_OFFSET,
        U256::from(settlement_layer),
    ));
    memory.push((MEDIUM_INTEROP_INTEROP_FEE_OFFSET, U256::zero()));

    // 4. Tx minor description and parameters (tx_index = 0)
    let bootloader_desc_offset = MEDIUM_INTEROP_BOOTLOADER_TX_DESCRIPTION_OFFSET;
    let tx_desc_offset = MEDIUM_INTEROP_TX_DESCRIPTION_OFFSET;

    memory.push((
        bootloader_desc_offset,
        assemble_tx_meta(env.execution_mode, true),
    ));
    memory.push((
        bootloader_desc_offset + 1,
        U256::from((32 * tx_desc_offset) as u64),
    ));

    let refund_offset = MEDIUM_INTEROP_OPERATOR_REFUNDS_OFFSET;
    memory.push((refund_offset, U256::zero()));

    let overhead_offset = MEDIUM_INTEROP_TX_OVERHEAD_OFFSET;
    memory.push((overhead_offset, U256::from(tx.overhead_gas())));

    let trusted_gas_limit_offset = MEDIUM_INTEROP_TX_TRUSTED_GAS_LIMIT_OFFSET;
    memory.push((trusted_gas_limit_offset, trusted_gas_limit));

    // 5. Encoded transaction tokens at tx_description_offset
    let tokens = tx.into_tokens();
    for (i, token) in tokens.into_iter().enumerate() {
        memory.push((tx_desc_offset + i, token));
    }

    memory
}

/// Builds the bootloader heap memory layout for playground_batch.yul.zbin (MediumInterop) in eth_call mode.
pub fn build_playground_bootloader_memory(
    tx: &TransactionData,
    env: &BootloaderEnv,
    trusted_gas_limit: U256,
    settlement_layer: u64,
) -> Vec<(usize, U256)> {
    let mut memory = Vec::new();

    // 1. Initial environment header
    memory.push((OPERATOR_ADDRESS_SLOT, address_to_u256(env.fee_account)));
    memory.push((PREV_BLOCK_HASH_SLOT, h256_to_u256(env.prev_block_hash)));
    memory.push((NEW_BLOCK_TIMESTAMP_SLOT, U256::from(env.block_timestamp)));
    memory.push((NEW_BLOCK_NUMBER_SLOT, U256::from(env.block_number)));
    memory.push((FAIR_PUBDATA_PRICE_SLOT, U256::from(env.fair_pubdata_price)));
    memory.push((FAIR_L2_GAS_PRICE_SLOT, U256::from(env.fair_l2_gas_price)));
    memory.push((EXPECTED_BASE_FEE_SLOT, U256::from(env.base_fee)));
    memory.push((SHOULD_SET_NEW_BLOCK_SLOT, U256::zero()));

    // 2. L2 block info for transaction 0
    let block_pos = MEDIUM_INTEROP_TX_OPERATOR_L2_BLOCK_INFO_OFFSET;
    memory.push((block_pos, U256::from(env.block_number)));
    memory.push((block_pos + 1, U256::from(env.block_timestamp)));
    memory.push((block_pos + 2, h256_to_u256(env.prev_block_hash)));
    memory.push((block_pos + 3, U256::one())); // max_virtual_blocks_to_create

    // 3. Interop blocks and settlement layer
    memory.push((MEDIUM_INTEROP_INTEROP_BLOCKS_BEGIN_OFFSET, U256::one())); // number_of_interop_roots + 1
    memory.push((
        MEDIUM_INTEROP_SETTLEMENT_LAYER_OFFSET,
        U256::from(settlement_layer),
    ));
    memory.push((MEDIUM_INTEROP_INTEROP_FEE_OFFSET, U256::zero()));

    // 4. Tx minor description and parameters (tx_index = 0)
    let bootloader_desc_offset = MEDIUM_INTEROP_BOOTLOADER_TX_DESCRIPTION_OFFSET;
    let tx_desc_offset = MEDIUM_INTEROP_TX_DESCRIPTION_OFFSET;

    memory.push((
        bootloader_desc_offset,
        assemble_tx_meta(env.execution_mode, true),
    ));
    memory.push((
        bootloader_desc_offset + 1,
        U256::from((32 * tx_desc_offset) as u64),
    ));

    let refund_offset = MEDIUM_INTEROP_OPERATOR_REFUNDS_OFFSET;
    memory.push((refund_offset, U256::zero()));

    let overhead_offset = MEDIUM_INTEROP_TX_OVERHEAD_OFFSET;
    memory.push((overhead_offset, U256::from(tx.overhead_gas())));

    let trusted_gas_limit_offset = MEDIUM_INTEROP_TX_TRUSTED_GAS_LIMIT_OFFSET;
    memory.push((trusted_gas_limit_offset, trusted_gas_limit));

    // 5. Encoded transaction tokens at tx_description_offset
    let tokens = tx.into_tokens();
    for (i, token) in tokens.into_iter().enumerate() {
        memory.push((tx_desc_offset + i, token));
    }

    memory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootloader_memory_builder() {
        let tx = TransactionData::default();
        let env = BootloaderEnv::default();
        let mem = build_bootloader_memory(&tx, &env, U256::from(100_000), 1);

        assert!(!mem.is_empty());
        // Check slot 0 (operator address)
        assert_eq!(mem[0].0, OPERATOR_ADDRESS_SLOT);
        // Check slot 7 (should set new block)
        assert_eq!(mem[7].0, SHOULD_SET_NEW_BLOCK_SLOT);
        assert_eq!(mem[7].1, U256::zero());

        // The fee_estimate artifact is compiled against the MediumInterop
        // layout: writing the legacy offsets left it reading zeroed block info
        // and every estimate reverted with "system context upgrade fail".
        let at = |slot: usize| mem.iter().find(|(s, _)| *s == slot).map(|(_, v)| *v);
        assert_eq!(
            at(MEDIUM_INTEROP_TX_OPERATOR_L2_BLOCK_INFO_OFFSET),
            Some(U256::from(env.block_number))
        );
        assert_eq!(
            at(MEDIUM_INTEROP_OPERATOR_REFUNDS_OFFSET),
            Some(U256::zero())
        );
        assert_eq!(
            at(MEDIUM_INTEROP_INTEROP_BLOCKS_BEGIN_OFFSET),
            Some(U256::one())
        );
        assert_eq!(
            at(MEDIUM_INTEROP_SETTLEMENT_LAYER_OFFSET),
            Some(U256::from(1u64))
        );
        assert_eq!(at(MEDIUM_INTEROP_INTEROP_FEE_OFFSET), Some(U256::zero()));
        assert_eq!(
            at(MEDIUM_INTEROP_BOOTLOADER_TX_DESCRIPTION_OFFSET + 1),
            Some(U256::from(
                (32 * MEDIUM_INTEROP_TX_DESCRIPTION_OFFSET) as u64
            ))
        );
        // ... and nothing landed at the legacy slots for the same fields.
        assert_eq!(at(TX_DESCRIPTION_OFFSET), None);
        assert_eq!(at(OPERATOR_REFUNDS_OFFSET), None);
    }
}
