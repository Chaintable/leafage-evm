/// Hardcoded Tempo TIP-1000 gas cost overrides.
/// These replace revm 36's `GasParams` API which doesn't exist in revm 33.1.
///
/// Values sourced from `tempo-revm/crates/revm/src/gas_params.rs`.
pub struct TempoGasCosts;

impl TempoGasCosts {
    /// SSTORE set without prior load (Tempo TIP-1000: 250k vs Ethereum 20k)
    pub const SSTORE_SET: u64 = 250_000;
    /// CREATE / CREATE2 base cost (Tempo: 500k vs Ethereum 32k)
    pub const CREATE: u64 = 500_000;
    /// Transaction-level CREATE cost (Tempo: 500k vs Ethereum 32k)
    pub const TX_CREATE: u64 = 500_000;
    /// New account cost (Tempo: 250k vs Ethereum 25k)
    pub const NEW_ACCOUNT: u64 = 250_000;
    /// New account cost for SELFDESTRUCT (Tempo: 250k vs Ethereum 25k)
    pub const NEW_ACCOUNT_SELFDESTRUCT: u64 = 250_000;
    /// Code deposit per byte (Tempo: 1000 vs Ethereum 200)
    pub const CODE_DEPOSIT_PER_BYTE: u64 = 1_000;
    /// EIP-7702 auth per empty account (Tempo: 12500 vs Ethereum 25000)
    pub const EIP7702_AUTH_PER_EMPTY_ACCOUNT: u64 = 12_500;
    /// Auth account creation cost (Tempo-specific, GasId 255)
    pub const AUTH_ACCOUNT_CREATION: u64 = 250_000;
}
