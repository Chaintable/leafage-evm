use alloy::primitives::{Address, Bytes, B256, U256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbitrumPrecompileEnv {
    pub current_arbos_version: u64,
    pub current_tx_l1_gas_fees: U256,
    pub current_l1_block_number: u64,
    pub current_retryable_ticket: Option<B256>,
    pub current_refund_to: Option<Address>,
    pub allow_debug_precompiles: bool,
    pub current_chain_config: Option<Bytes>,
}

impl Default for ArbitrumPrecompileEnv {
    fn default() -> Self {
        Self {
            current_arbos_version: 0,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
        }
    }
}

pub(super) struct ArbPrecompileInput<'a, CTX> {
    pub(super) data: &'a [u8],
    pub(super) gas: u64,
    pub(super) caller: Address,
    pub(super) value: U256,
    pub(super) is_static: bool,
    pub(super) is_valid_call_context: bool,
    pub(super) current_arbos_version: u64,
    pub(super) current_tx_l1_gas_fees: U256,
    pub(super) current_l1_block_number: u64,
    pub(super) current_retryable_ticket: Option<B256>,
    pub(super) current_refund_to: Option<Address>,
    pub(super) allow_debug_precompiles: bool,
    pub(super) current_chain_config: Option<&'a [u8]>,
    pub(super) context: &'a mut CTX,
}
