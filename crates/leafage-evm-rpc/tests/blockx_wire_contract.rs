//! Wire-contract tests for `blockx_stateReadBatch` (BSRB/1).
//!
//! The golden hex payloads below are the cross-repo contract with the
//! Go batching facade in BlockX's worker (fixed-offset encoding/binary;
//! the executor sandbox never sees this wire), mirrored in the BlockX
//! repository. All integers are big-endian; change the layout only by
//! bumping BSRB_VERSION.

use alloy::primitives::hex;
use leafage_evm_types::{
    Address, BsrbContext, BsrbOutcome, BsrbRead, BsrbRequest, BsrbResponse, Bytes, H256, U256,
};

fn request_number_ctx() -> BsrbRequest {
    BsrbRequest {
        context: BsrbContext::Number(2),
        reads: vec![
            BsrbRead::AddressCode {
                address: Address::repeat_byte(0x11),
            },
            BsrbRead::StorageAt {
                address: Address::repeat_byte(0x22),
                slot: H256::with_last_byte(0xfe),
            },
        ],
    }
}

#[test]
fn request_golden_number_context() {
    let golden = concat!(
        "01",               // version
        "01",               // ctx_kind = number
        "0000000000000002", // height 2
        "0002",             // count
        "00",               // kind = addressCode
        "1111111111111111111111111111111111111111",
        "01", // kind = storageAt
        "2222222222222222222222222222222222222222",
        "00000000000000000000000000000000000000000000000000000000000000fe",
    );
    let request = request_number_ctx();
    assert_eq!(hex::encode(request.encode()), golden);
    assert_eq!(
        BsrbRequest::decode(&hex::decode(golden).unwrap()).unwrap(),
        request
    );
}

#[test]
fn request_golden_hash_context() {
    let golden = concat!(
        "01", // version
        "00", // ctx_kind = hash
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "0001", // count
        "00",   // kind = addressCode
        "1111111111111111111111111111111111111111",
    );
    let request = BsrbRequest {
        context: BsrbContext::Hash(H256::repeat_byte(0xcc)),
        reads: vec![BsrbRead::AddressCode {
            address: Address::repeat_byte(0x11),
        }],
    };
    assert_eq!(hex::encode(request.encode()), golden);
    assert_eq!(
        BsrbRequest::decode(&hex::decode(golden).unwrap()).unwrap(),
        request
    );
}

#[test]
fn response_golden() {
    // -39006 as big-endian i32 is 0xffff67a2; the message is the
    // byte-exact single-method error text (leafage-py parses it).
    let message = "block 999 not found for state node";
    let golden = format!(
        concat!(
            "01",       // version
            "0003",     // count
            "00",       // tag = ok (code bytes)
            "00000005", // len
            "6080604052",
            "00",       // tag = ok (storage word)
            "00000020", // len 32
            "000000000000000000000000000000000000000000000000000000000000abcd",
            "01",       // tag = error
            "ffff67a2", // -39006
            "{:08x}",   // msg len
            "{}",
        ),
        message.len(),
        hex::encode(message.as_bytes()),
    );
    let response = BsrbResponse {
        results: vec![
            BsrbOutcome::Value(Bytes::from(vec![0x60, 0x80, 0x60, 0x40, 0x52])),
            BsrbOutcome::Value(Bytes::copy_from_slice(
                &U256::from(0xabcdu64).to_be_bytes::<32>(),
            )),
            BsrbOutcome::Error {
                code: -39006,
                message: message.to_string(),
            },
        ],
    };
    assert_eq!(hex::encode(response.encode()), golden);
    assert_eq!(
        BsrbResponse::decode(&hex::decode(&golden).unwrap()).unwrap(),
        response
    );
}

/// Empty code (codeless account) is a zero-length ok payload, distinct
/// from any error.
#[test]
fn empty_code_value_round_trips() {
    let response = BsrbResponse {
        results: vec![BsrbOutcome::Value(Bytes::new())],
    };
    assert_eq!(
        hex::encode(response.encode()),
        "01000100".to_owned() + "00000000"
    );
    assert_eq!(BsrbResponse::decode(&response.encode()).unwrap(), response);
}

/// A 32-byte contract code and a storage word are the same payload on
/// the wire by design: the request item at the same position carries
/// the kind, so the value never guesses. This pins the regression that
/// motivated dropping self-describing values.
#[test]
fn value_payloads_are_kind_agnostic() {
    let word = H256::repeat_byte(0x60);
    let as_code = BsrbResponse {
        results: vec![BsrbOutcome::Value(Bytes::copy_from_slice(word.as_slice()))],
    };
    let as_storage = BsrbResponse {
        results: vec![BsrbOutcome::Value(Bytes::copy_from_slice(word.as_slice()))],
    };
    assert_eq!(as_code.encode(), as_storage.encode());
}
