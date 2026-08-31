use alloy::{
    primitives::{address, b256, keccak256, Address, Bytes, Log, B256, U256},
    sol,
    sol_types::{SolEvent, SolValue},
};
use revm::handler::SYSTEM_ADDRESS;

/// Arc NativeCoinControl precompile address.
pub(crate) const NATIVE_COIN_CONTROL_ADDRESS: Address =
    address!("1800000000000000000000000000000000000001");

/// Solidity mapping slot for `NativeCoinControl.isBlocklisted`.
const BLOCKLIST_MAPPING_SLOT: B256 =
    b256!("0000000000000000000000000000000000000000000000000000000000000002");

pub(crate) const ERR_BLOCKED_ADDRESS: &str = "Blocked address";
pub(crate) const ERR_ZERO_ADDRESS: &str = "Zero address not allowed";
pub(crate) const ERR_SELFDESTRUCTED_BALANCE_INCREASED: &str =
    "Cannot increase the balance of selfdestructed account";

const REVERT_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

sol! {
    #[derive(Debug, PartialEq, Eq)]
    event Transfer(address indexed from, address indexed to, uint256 amount);
}

#[inline]
pub(crate) fn blocklist_storage_slot(address: Address) -> U256 {
    let mut input = [0u8; 64];
    input[12..32].copy_from_slice(address.as_slice());
    input[32..].copy_from_slice(BLOCKLIST_MAPPING_SLOT.as_slice());
    U256::from_be_bytes(keccak256(input).0)
}

#[inline]
pub(crate) fn is_blocklisted_status(status: U256) -> bool {
    !status.is_zero()
}

pub(crate) fn revert_message(message: &str) -> Bytes {
    let encoded = message.abi_encode();
    let mut data = Vec::with_capacity(REVERT_SELECTOR.len().saturating_add(encoded.len()));
    data.extend_from_slice(&REVERT_SELECTOR);
    data.extend_from_slice(&encoded);
    data.into()
}

pub(crate) fn eip7708_transfer_log(from: Address, to: Address, amount: U256) -> Log {
    Log {
        address: SYSTEM_ADDRESS,
        data: Transfer { from, to, amount }.encode_log_data(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    #[test]
    fn blocklist_slot_matches_arc_contract_layout() {
        let account = address!("D308a07F97db36C338e8FE2AfB09267781d00811");
        let expected = b256!("c0814ebfa96e99aee5c17f259ae3205e7b664343916807a4a968c9f94e32f89b");

        assert_eq!(
            blocklist_storage_slot(account),
            U256::from_be_bytes(expected.0)
        );
        assert!(!is_blocklisted_status(U256::ZERO));
        assert!(is_blocklisted_status(U256::MAX));
    }

    #[test]
    fn eip7708_log_uses_system_emitter_and_erc20_layout() {
        let from = Address::with_last_byte(1);
        let to = Address::with_last_byte(2);
        let amount = U256::from(3);
        let log = eip7708_transfer_log(from, to, amount);

        assert_eq!(
            log.address,
            address!("fffffffffffffffffffffffffffffffffffffffe")
        );
        assert_eq!(
            log.data.topics(),
            &[
                b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                B256::left_padding_from(from.as_slice()),
                B256::left_padding_from(to.as_slice()),
            ]
        );
        assert_eq!(log.data.data.as_ref(), &amount.to_be_bytes::<32>());
    }

    #[test]
    fn revert_message_is_solidity_error_string() {
        let encoded = revert_message(ERR_BLOCKED_ADDRESS);

        assert_eq!(&encoded[..4], &REVERT_SELECTOR);
        assert!(encoded.len() >= 4 + 32 + 32 + ERR_BLOCKED_ADDRESS.len());
    }
}
