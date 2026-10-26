//! Arc archive replay and Writer-compatible pipeline outputs.
mod block;
mod chain;
mod convert;
mod genesis;
mod inspector;
mod replay;
mod state_diff;
pub(crate) use chain::ArcPipeline;

fn pipeline_header(block: &alloy::rpc::types::Block) -> crate::pipeline::types::PipelineHeader {
    let mut header = crate::pipeline::build::pipeline_header(block);
    header.requests_hash = header.requests_root.take();
    header
}

#[cfg(test)]
mod tests {
    #[test]
    fn arc_requests_hash_does_not_change_other_chains_header_schema() {
        let mut block: alloy::rpc::types::Block = Default::default();
        let hash = alloy::primitives::B256::repeat_byte(0x42);
        block.header.requests_hash = Some(hash);
        let shared = serde_json::to_value(crate::pipeline::build::pipeline_header(&block)).unwrap();
        assert_eq!(shared["requestsRoot"], serde_json::json!(hash));
        assert!(!shared.as_object().unwrap().contains_key("requestsHash"));
        let arc = serde_json::to_value(super::pipeline_header(&block)).unwrap();
        assert_eq!(arc["requestsHash"], serde_json::json!(hash));
        assert!(!arc.as_object().unwrap().contains_key("requestsRoot"));
        let mut expected = shared.as_object().unwrap().clone();
        expected.remove("requestsRoot");
        expected.insert("requestsHash".into(), serde_json::json!(hash));
        assert_eq!(arc, serde_json::Value::Object(expected));
    }
}
