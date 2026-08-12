//! Wire-contract tests for `blockx_stateReadBatch`.
//!
//! The JSON fixtures under `tests/fixtures/blockx_state_read_batch/`
//! are the cross-repo serde contract with BlockX's semantic batch
//! facade (to be vendored there when the facade lands). Structure is
//! asserted via `serde_json::Value` equality; error `message` strings
//! are asserted byte-for-byte because leafage-py parses -39006/-39007
//! message text.

use leafage_evm_types::{
    BlockId, BlockNumberOrTag, BlockType, BlockxStateRead, BlockxStateReadBatch,
    BlockxStateReadBatchResp, BlockxStateReadValue, Bytes, H256, U256,
};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let raw = match name {
        "request" => include_str!("fixtures/blockx_state_read_batch/request.json"),
        "request_number_context" => {
            include_str!("fixtures/blockx_state_read_batch/request_number_context.json")
        }
        "response_success" => {
            include_str!("fixtures/blockx_state_read_batch/response_success.json")
        }
        "response_item_error" => {
            include_str!("fixtures/blockx_state_read_batch/response_item_error.json")
        }
        _ => panic!("unknown fixture {name}"),
    };
    serde_json::from_str(raw).unwrap()
}

#[test]
fn request_fixture_roundtrips() {
    let value = fixture("request");
    let batch: BlockxStateReadBatch = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(batch.block_context.block_type, BlockType::Equals);
    assert!(matches!(batch.block_context.block_id, BlockId::Hash(_)));
    assert_eq!(batch.reads.len(), 3);
    assert!(matches!(
        &batch.reads[0],
        BlockxStateRead::AddressCode { index: 0, .. }
    ));
    // Shortest-hex position (leafage-py canonical shape) and 32-byte
    // position both parse, and as_b256 agrees with the padded form.
    match (&batch.reads[1], &batch.reads[2]) {
        (
            BlockxStateRead::StorageAt { position: p1, .. },
            BlockxStateRead::StorageAt { position: p2, .. },
        ) => {
            assert_eq!(p1.as_b256(), H256::ZERO);
            assert_eq!(p2.as_b256(), H256::with_last_byte(0xfe));
        }
        other => panic!("unexpected reads: {other:?}"),
    }

    // `reads` round-trips structurally. The block context is asserted
    // semantically instead: alloy's BlockId accepts the bare hash
    // string BlockX sends but serializes hashes as {"blockHash": ...},
    // and Leafage only ever deserializes this field.
    let reserialized = serde_json::to_value(&batch).unwrap();
    assert_eq!(reserialized["reads"], value["reads"]);
    let reparsed: BlockxStateReadBatch =
        serde_json::from_value(reserialized.clone()).unwrap();
    assert_eq!(reparsed.block_context.block_id, batch.block_context.block_id);
    assert_eq!(reserialized["blockContext"]["type"], json!("Equals"));
}

#[test]
fn number_context_fixture_roundtrips() {
    let value = fixture("request_number_context");
    let batch: BlockxStateReadBatch = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(
        batch.block_context.block_id,
        BlockId::Number(BlockNumberOrTag::Number(2))
    ));
    assert_eq!(batch.reads[0].index(), 7);
    assert_eq!(serde_json::to_value(&batch).unwrap(), value);
}

#[test]
fn response_fixtures_roundtrip() {
    for name in ["response_success", "response_item_error"] {
        let value = fixture(name);
        let resp: BlockxStateReadBatchResp = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&resp).unwrap(), value, "{name}");
    }

    // Typed checks on the success fixture: code parses as bytes,
    // storage as a 32-byte word, and absent fields stay absent.
    let resp: BlockxStateReadBatchResp =
        serde_json::from_value(fixture("response_success")).unwrap();
    assert_eq!(
        resp.results[0].value,
        Some(BlockxStateReadValue::Code(Bytes::from(vec![
            0x60, 0x80, 0x60, 0x40, 0x52
        ])))
    );
    let word: [u8; 32] = U256::from(0xabcdu64).to_be_bytes();
    assert_eq!(
        resp.results[1].value,
        Some(BlockxStateReadValue::Storage(word.into()))
    );
    assert!(resp.results.iter().all(|r| r.error.is_none()));

    // Item error carries code and byte-exact message.
    let resp: BlockxStateReadBatchResp =
        serde_json::from_value(fixture("response_item_error")).unwrap();
    let err = resp.results[1].error.as_ref().unwrap();
    assert_eq!(err.code, -39005);
    assert_eq!(err.message, "database failed");
    assert!(resp.results[1].value.is_none());
}

#[test]
fn unknown_kind_is_rejected() {
    let value = json!({
        "blockContext": {"type": "Equals", "block_id": "0x2"},
        "reads": [
            {"kind": "traceCall", "index": 0, "address": "0x1111111111111111111111111111111111111111"}
        ]
    });
    assert!(serde_json::from_value::<BlockxStateReadBatch>(value).is_err());
}

#[test]
fn malformed_address_is_rejected() {
    let value = json!({
        "blockContext": {"type": "Equals", "block_id": "0x2"},
        "reads": [
            {"kind": "addressCode", "index": 0, "address": "0x123"}
        ]
    });
    assert!(serde_json::from_value::<BlockxStateReadBatch>(value).is_err());
}

/// Storage values always serialize as full 32-byte words and code as
/// variable-length hex — the exact single-method shapes.
#[test]
fn value_shapes_match_single_methods() {
    let storage = BlockxStateReadValue::Storage(H256::with_last_byte(1));
    assert_eq!(
        serde_json::to_value(&storage).unwrap(),
        json!("0x0000000000000000000000000000000000000000000000000000000000000001")
    );
    let code = BlockxStateReadValue::Code(Bytes::new());
    assert_eq!(serde_json::to_value(&code).unwrap(), json!("0x"));
}
