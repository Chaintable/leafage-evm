use primitive_types::{H160, U256};
use super::pubdata::{derive_overhead, encoding_len};

#[derive(Debug, Clone)]
pub struct TransactionData {
    pub tx_type: u8,
    pub from: H160,
    pub to: Option<H160>,
    pub gas_limit: U256,
    pub pubdata_price_limit: U256,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub paymaster: H160,
    pub nonce: U256,
    pub value: U256,
    pub reserved: [U256; 4],
    pub data: Vec<u8>,
    pub signature: Vec<u8>,
    pub factory_deps: Vec<Vec<u8>>,
    pub paymaster_input: Vec<u8>,
    pub reserved_dynamic: Vec<u8>,
}

impl Default for TransactionData {
    fn default() -> Self {
        Self {
            tx_type: super::constants::EIP_712_TX_TYPE,
            from: H160::zero(),
            to: None,
            gas_limit: U256::zero(),
            pubdata_price_limit: U256::from(super::constants::MAX_GAS_PER_PUBDATA_BYTE),
            max_fee_per_gas: U256::zero(),
            max_priority_fee_per_gas: U256::zero(),
            paymaster: H160::zero(),
            nonce: U256::zero(),
            value: U256::zero(),
            reserved: [U256::zero(); 4],
            data: Vec::new(),
            signature: Vec::new(),
            factory_deps: Vec::new(),
            paymaster_input: Vec::new(),
            reserved_dynamic: Vec::new(),
        }
    }
}

fn pad_to_32_bytes(data: &[u8]) -> Vec<u8> {
    let remainder = data.len() % 32;
    if remainder == 0 {
        data.to_vec()
    } else {
        let mut padded = data.to_vec();
        padded.resize(data.len() + (32 - remainder), 0);
        padded
    }
}

fn u256_to_bytes32(val: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    val.to_big_endian(&mut b);
    b
}

fn address_to_bytes32(addr: H160) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[12..32].copy_from_slice(addr.as_bytes());
    b
}

impl TransactionData {
    pub fn encoding_len(&self) -> usize {
        encoding_len(
            self.data.len() as u64,
            self.signature.len() as u64,
            self.factory_deps.len() as u64,
            self.paymaster_input.len() as u64,
            self.reserved_dynamic.len() as u64,
        )
    }

    pub fn overhead_gas(&self) -> u32 {
        derive_overhead(self.encoding_len())
    }

    /// ABI encode the transaction matching zkSync Era bootloader specification.
    pub fn abi_encode(&self) -> Vec<u8> {
        let mut out = Vec::new();

        // 1. Initial 0x20 pointer to tuple
        out.extend_from_slice(&u256_to_bytes32(U256::from(32)));

        // 2. Tuple head: 19 words (608 bytes)
        // Head offsets relative to start of tuple
        let head_size: usize = 19 * 32;

        let data_padded = pad_to_32_bytes(&self.data);
        let data_encoded_len = 32 + data_padded.len();

        let sig_padded = pad_to_32_bytes(&self.signature);
        let sig_encoded_len = 32 + sig_padded.len();

        // Factory deps are hashes: 32 bytes count + 32 bytes per hash
        let factory_deps_encoded_len = 32 + self.factory_deps.len() * 32;

        let paymaster_input_padded = pad_to_32_bytes(&self.paymaster_input);
        let paymaster_encoded_len = 32 + paymaster_input_padded.len();

        let data_offset = head_size;
        let sig_offset = data_offset + data_encoded_len;
        let factory_deps_offset = sig_offset + sig_encoded_len;
        let paymaster_input_offset = factory_deps_offset + factory_deps_encoded_len;
        let reserved_dynamic_offset = paymaster_input_offset + paymaster_encoded_len;

        // Write head fields:
        out.extend_from_slice(&u256_to_bytes32(U256::from(self.tx_type)));
        out.extend_from_slice(&address_to_bytes32(self.from));
        out.extend_from_slice(&address_to_bytes32(self.to.unwrap_or_default()));
        out.extend_from_slice(&u256_to_bytes32(self.gas_limit));
        out.extend_from_slice(&u256_to_bytes32(self.pubdata_price_limit));
        out.extend_from_slice(&u256_to_bytes32(self.max_fee_per_gas));
        out.extend_from_slice(&u256_to_bytes32(self.max_priority_fee_per_gas));
        out.extend_from_slice(&address_to_bytes32(self.paymaster));
        out.extend_from_slice(&u256_to_bytes32(self.nonce));
        out.extend_from_slice(&u256_to_bytes32(self.value));
        for r in &self.reserved {
            out.extend_from_slice(&u256_to_bytes32(*r));
        }

        // Dynamic offsets in tuple head
        out.extend_from_slice(&u256_to_bytes32(U256::from(data_offset)));
        out.extend_from_slice(&u256_to_bytes32(U256::from(sig_offset)));
        out.extend_from_slice(&u256_to_bytes32(U256::from(factory_deps_offset)));
        out.extend_from_slice(&u256_to_bytes32(U256::from(paymaster_input_offset)));
        out.extend_from_slice(&u256_to_bytes32(U256::from(reserved_dynamic_offset)));

        // 3. Dynamic tails
        // Data
        out.extend_from_slice(&u256_to_bytes32(U256::from(self.data.len())));
        out.extend_from_slice(&data_padded);

        // Signature
        out.extend_from_slice(&u256_to_bytes32(U256::from(self.signature.len())));
        out.extend_from_slice(&sig_padded);

        // Factory deps
        out.extend_from_slice(&u256_to_bytes32(U256::from(self.factory_deps.len())));
        for dep in &self.factory_deps {
            let hash = super::artifacts::compute_bytecode_hash(dep, false);
            out.extend_from_slice(&hash);
        }

        // Paymaster input
        out.extend_from_slice(&u256_to_bytes32(U256::from(self.paymaster_input.len())));
        out.extend_from_slice(&paymaster_input_padded);

        // Reserved dynamic
        out.extend_from_slice(&u256_to_bytes32(U256::from(self.reserved_dynamic.len())));
        out.extend_from_slice(&pad_to_32_bytes(&self.reserved_dynamic));

        out
    }

    pub fn into_tokens(&self) -> Vec<U256> {
        let encoded = self.abi_encode();
        encoded
            .chunks_exact(32)
            .map(U256::from_big_endian)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_data_abi_encode() {
        let tx = TransactionData {
            tx_type: 113,
            from: H160::repeat_byte(0x11),
            to: Some(H160::repeat_byte(0x22)),
            gas_limit: U256::from(1),
            pubdata_price_limit: U256::from(1),
            max_fee_per_gas: U256::from(1),
            max_priority_fee_per_gas: U256::from(1),
            paymaster: H160::repeat_byte(0x33),
            nonce: U256::zero(),
            value: U256::zero(),
            reserved: [U256::zero(); 4],
            data: vec![0u8; 65],
            signature: vec![0u8; 75],
            factory_deps: vec![vec![0u8; 32], vec![1u8; 32]],
            paymaster_input: vec![0u8; 85],
            reserved_dynamic: vec![0u8; 32],
        };

        let assumed_len = tx.encoding_len();
        let tokens = tx.into_tokens();
        assert_eq!(assumed_len, tokens.len());
        assert_eq!(tokens[0], U256::from(32)); // 0x20 offset
    }
}
