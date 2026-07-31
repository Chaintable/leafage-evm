pub(crate) mod unsupported {
    use revm::primitives::{address, Address};
    use std::collections::HashSet;
    use std::sync::LazyLock;

    const BTC_BAL_ADDR: Address = address!("0x0000000000000000000000000000000000000040");
    const BTC_UTXOS_ADDR_LIST: Address = address!("0x0000000000000000000000000000000000000041");
    const BTC_TX_BY_TXID: Address = address!("0x0000000000000000000000000000000000000042");
    const BTC_TX_CONFIRMATIONS: Address = address!("0x0000000000000000000000000000000000000043");
    const BTC_LAST_HEADER: Address = address!("0x0000000000000000000000000000000000000044");
    const BTC_HEADER_N: Address = address!("0x0000000000000000000000000000000000000045");
    const BTC_ADDR_TO_SCRIPT: Address = address!("0x0000000000000000000000000000000000000046");
    const BTC_INPUT_BY_TXID: Address = address!("0x0000000000000000000000000000000000000047");
    const BTC_OUTPUT_BY_TXID: Address = address!("0x0000000000000000000000000000000000000048");
    const BTC_TX_GET_INPUT_WITNESS: Address =
        address!("0x0000000000000000000000000000000000000049");

    pub static UNSUPPORTED_LIST: LazyLock<HashSet<Address>> = LazyLock::new(|| {
        vec![
            BTC_BAL_ADDR,
            BTC_UTXOS_ADDR_LIST,
            BTC_TX_BY_TXID,
            BTC_TX_CONFIRMATIONS,
            BTC_LAST_HEADER,
            BTC_HEADER_N,
            BTC_ADDR_TO_SCRIPT,
            BTC_INPUT_BY_TXID,
            BTC_OUTPUT_BY_TXID,
            BTC_TX_GET_INPUT_WITNESS,
        ]
        .into_iter()
        .collect()
    });

    pub fn is_unsupported(addr: &Address) -> bool {
        UNSUPPORTED_LIST.contains(addr)
    }
}
