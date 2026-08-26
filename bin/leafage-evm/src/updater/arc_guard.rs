use leafage_evm_types::{
    BlockInfo, BlockStorageDiff, KafkaBlockChangeNotification, KafkaBlockContext, H256,
};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArcInputError {
    reason: String,
}

impl ArcInputError {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ArcInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid Arc finalized input: {}", self.reason)
    }
}

impl std::error::Error for ArcInputError {}

pub(crate) fn validate_notification(
    offset: i64,
    notification: &KafkaBlockChangeNotification,
) -> Result<(), ArcInputError> {
    if notification.change_type != 1 {
        return Err(ArcInputError::new(format!(
            "Kafka offset {offset} has change_type {}, expected 1",
            notification.change_type
        )));
    }
    if !notification.drop_blocks.is_empty() {
        return Err(ArcInputError::new(format!(
            "Kafka offset {offset} has {} drop_blocks, expected none",
            notification.drop_blocks.len()
        )));
    }
    Ok(())
}

pub(crate) fn validate_notification_header(
    offset: i64,
    context: &KafkaBlockContext,
    block_info: &BlockInfo,
) -> Result<(), ArcInputError> {
    if block_info.header.hash != context.hash {
        return Err(ArcInputError::new(format!(
            "Kafka offset {offset} announces hash {}, fetched Header has hash {}",
            context.hash, block_info.header.hash
        )));
    }
    if block_info.header.number != context.block_number {
        return Err(ArcInputError::new(format!(
            "Kafka offset {offset} announces block {} for {}, fetched Header has block {}",
            context.block_number, context.hash, block_info.header.number
        )));
    }
    if block_info.header.parent_hash != context.parent_hash {
        return Err(ArcInputError::new(format!(
            "Kafka offset {offset} announces parent {} for block {}, fetched Header has parent {}",
            context.parent_hash, context.block_number, block_info.header.parent_hash
        )));
    }
    Ok(())
}

pub(crate) fn validate_batch<'a>(
    anchor: &BlockInfo,
    blocks: impl IntoIterator<Item = (&'a BlockInfo, &'a BlockStorageDiff)>,
) -> Result<(), ArcInputError> {
    let mut expected_number = anchor.header.number.checked_add(1).ok_or_else(|| {
        ArcInputError::new(format!(
            "block number overflows after anchor {}",
            anchor.header.number
        ))
    })?;
    let mut expected_parent_hash = anchor.header.hash;
    let mut expected_parent_state_root = anchor.header.state_root;

    for (block_info, block_diff) in blocks {
        if block_info.header.number != expected_number {
            return Err(ArcInputError::new(format!(
                "block {} has number {}, expected {}",
                block_info.header.hash, block_info.header.number, expected_number
            )));
        }
        if block_info.header.parent_hash != expected_parent_hash {
            return Err(ArcInputError::new(format!(
                "block {} parent {} does not match previous block hash {}",
                block_info.header.number, block_info.header.parent_hash, expected_parent_hash
            )));
        }
        if block_diff.hash != block_info.header.state_root {
            return Err(ArcInputError::new(format!(
                "block {} StateDiff root {} does not match Header state root {}",
                block_info.header.number, block_diff.hash, block_info.header.state_root
            )));
        }
        if block_diff.parent_hash != expected_parent_state_root {
            return Err(ArcInputError::new(format!(
                "block {} StateDiff parent root {} does not match previous state root {}",
                block_info.header.number, block_diff.parent_hash, expected_parent_state_root
            )));
        }

        expected_number = expected_number.checked_add(1).ok_or_else(|| {
            ArcInputError::new(format!(
                "block number overflows after block {}",
                block_info.header.number
            ))
        })?;
        expected_parent_hash = block_info.header.hash;
        expected_parent_state_root = block_info.header.state_root;
    }
    Ok(())
}

pub(crate) fn validate_unanchored_batch<'a>(
    blocks: impl IntoIterator<Item = (&'a BlockInfo, &'a BlockStorageDiff)>,
) -> Result<(), ArcInputError> {
    let mut previous: Option<(&BlockInfo, &BlockStorageDiff)> = None;
    for (block_info, block_diff) in blocks {
        if block_diff.hash != block_info.header.state_root {
            return Err(ArcInputError::new(format!(
                "block {} StateDiff root {} does not match Header state root {}",
                block_info.header.number, block_diff.hash, block_info.header.state_root
            )));
        }
        if let Some((previous_info, _)) = previous {
            let expected_number = previous_info.header.number.checked_add(1).ok_or_else(|| {
                ArcInputError::new(format!(
                    "block number overflows after block {}",
                    previous_info.header.number
                ))
            })?;
            if block_info.header.number != expected_number {
                return Err(ArcInputError::new(format!(
                    "block {} has number {}, expected {}",
                    block_info.header.hash, block_info.header.number, expected_number
                )));
            }
            if block_info.header.parent_hash != previous_info.header.hash {
                return Err(ArcInputError::new(format!(
                    "block {} parent {} does not match previous block hash {}",
                    block_info.header.number,
                    block_info.header.parent_hash,
                    previous_info.header.hash
                )));
            }
            if block_diff.parent_hash != previous_info.header.state_root {
                return Err(ArcInputError::new(format!(
                    "block {} StateDiff parent root {} does not match previous state root {}",
                    block_info.header.number,
                    block_diff.parent_hash,
                    previous_info.header.state_root
                )));
            }
        }
        previous = Some((block_info, block_diff));
    }
    Ok(())
}

pub(crate) fn block_hash_at_height(
    known: &mut std::collections::HashMap<u64, H256>,
    block_number: u64,
    block_hash: H256,
) -> Result<bool, ArcInputError> {
    match known.get(&block_number) {
        Some(known_hash) if *known_hash != block_hash => Err(ArcInputError::new(format!(
            "block height {block_number} contains conflicting hashes {known_hash} and {block_hash}"
        ))),
        Some(_) => Ok(false),
        None => {
            known.insert(block_number, block_hash);
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> H256 {
        H256::repeat_byte(byte)
    }

    fn block(number: u64, hash_value: u8, parent: u8, root: u8) -> BlockInfo {
        let mut block = BlockInfo::default();
        block.header.number = number;
        block.header.hash = hash(hash_value);
        block.header.parent_hash = hash(parent);
        block.header.state_root = hash(root);
        block
    }

    fn diff(root: u8, parent_root: u8) -> BlockStorageDiff {
        BlockStorageDiff {
            hash: hash(root),
            parent_hash: hash(parent_root),
            ..Default::default()
        }
    }

    #[test]
    fn accepts_only_new_block_notifications_without_drops() {
        let valid = KafkaBlockChangeNotification {
            change_type: 1,
            new_blocks: Vec::new(),
            drop_blocks: Vec::new(),
        };
        assert!(validate_notification(7, &valid).is_ok());

        let mut invalid_type = valid.clone();
        invalid_type.change_type = 2;
        assert!(validate_notification(8, &invalid_type)
            .unwrap_err()
            .to_string()
            .contains("change_type 2"));

        let mut with_drop = valid;
        with_drop.drop_blocks.push(KafkaBlockContext {
            hash: hash(2),
            parent_hash: hash(1),
            block_number: 2,
        });
        assert!(validate_notification(9, &with_drop)
            .unwrap_err()
            .to_string()
            .contains("1 drop_blocks"));
    }

    #[test]
    fn validates_notification_metadata_against_fetched_header() {
        let block = block(2, 2, 1, 12);
        let context = KafkaBlockContext {
            hash: hash(2),
            parent_hash: hash(1),
            block_number: 2,
        };
        assert!(validate_notification_header(3, &context, &block).is_ok());

        let mut wrong_number = context.clone();
        wrong_number.block_number = 3;
        assert!(validate_notification_header(3, &wrong_number, &block)
            .unwrap_err()
            .to_string()
            .contains("fetched Header has block 2"));

        let mut wrong_parent = context;
        wrong_parent.parent_hash = hash(9);
        assert!(validate_notification_header(3, &wrong_parent, &block)
            .unwrap_err()
            .to_string()
            .contains("fetched Header has parent"));
    }

    #[test]
    fn rejects_bad_tail_before_caller_applies_any_block() {
        let anchor = block(10, 10, 9, 20);
        let blocks = vec![
            (block(11, 11, 10, 21), diff(21, 20)),
            (block(12, 12, 11, 22), diff(22, 99)),
        ];
        let mut writes = 0;

        let result = validate_batch(&anchor, blocks.iter().map(|(block, diff)| (block, diff)));
        if result.is_ok() {
            for _ in &blocks {
                writes += 1;
            }
        }

        assert_eq!(writes, 0);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("block 12 StateDiff parent root"));
    }

    #[test]
    fn validates_height_block_parent_and_state_roots() {
        let anchor = block(10, 10, 9, 20);
        let valid = vec![
            (block(11, 11, 10, 21), diff(21, 20)),
            (block(12, 12, 11, 22), diff(22, 21)),
        ];
        assert!(validate_batch(&anchor, valid.iter().map(|(block, diff)| (block, diff))).is_ok());

        let mut wrong_height = valid.clone();
        wrong_height[1].0.header.number = 13;
        assert!(validate_batch(
            &anchor,
            wrong_height.iter().map(|(block, diff)| (block, diff))
        )
        .unwrap_err()
        .to_string()
        .contains("expected 12"));

        let mut wrong_parent = valid.clone();
        wrong_parent[1].0.header.parent_hash = hash(90);
        assert!(validate_batch(
            &anchor,
            wrong_parent.iter().map(|(block, diff)| (block, diff))
        )
        .unwrap_err()
        .to_string()
        .contains("previous block hash"));

        let mut wrong_first_parent_root = valid.clone();
        wrong_first_parent_root[0].1.parent_hash = hash(90);
        assert!(validate_batch(
            &anchor,
            wrong_first_parent_root
                .iter()
                .map(|(block, diff)| (block, diff))
        )
        .unwrap_err()
        .to_string()
        .contains("block 11 StateDiff parent root"));

        let mut wrong_root = valid;
        wrong_root[1].1.hash = hash(90);
        assert!(validate_batch(
            &anchor,
            wrong_root.iter().map(|(block, diff)| (block, diff))
        )
        .unwrap_err()
        .to_string()
        .contains("Header state root"));
    }

    #[test]
    fn permits_exact_duplicate_height_but_rejects_conflicting_hash() {
        let mut known = std::collections::HashMap::new();
        assert_eq!(
            block_hash_at_height(&mut known, 10, hash(10)).unwrap(),
            true
        );
        assert_eq!(
            block_hash_at_height(&mut known, 10, hash(10)).unwrap(),
            false
        );
        assert!(block_hash_at_height(&mut known, 10, hash(11))
            .unwrap_err()
            .to_string()
            .contains("conflicting hashes"));
    }
}
