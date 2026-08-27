use crate::bundle::{bundle_end, s3_read_bundle, BundleIntegrityError};
use crate::updater::arc_guard::{
    block_hash_at_height, validate_batch, validate_notification, validate_notification_header,
    validate_unanchored_batch, ArcInputError,
};
use crate::updater::{ArcFatalReporter, UpdaterInputPolicy};
use crate::utils::{
    s3_get_block_diff, s3_get_block_info, s3_get_block_info_and_diff_by_hash,
    s3_get_block_info_and_diff_by_number,
    s3_get_block_info_and_diff_by_number_with_parent_state_root, s3_get_block_info_by_number,
    state_diff_keyed_by_block_hash, KafkaS3Config,
};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use futures::stream::StreamExt;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{
    read_offset, write_offset, BlockContext, EvmStorageRead, EvmStorageWrite,
};
use leafage_evm_types::{
    BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, KafkaBlockChangeNotification,
    KafkaBlockContext, H256,
};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    message::BorrowedMessage,
    util::Timeout,
    ClientConfig, Message, Offset, TopicPartitionList,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::Duration;
use tokio::{sync::watch, task::JoinSet, time};
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
struct BlockContextWithOffset {
    block_diff: BlockStorageDiff,
    block_info: BlockInfo,
    first_offset: i64,
    offset: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        writes: Mutex<Vec<u64>>,
    }

    impl EvmStorageWrite for RecordingWriter {
        type Error = std::io::Error;

        fn update_block(
            &self,
            block_info: BlockInfo,
            _block_diff: BlockStorageDiff,
        ) -> Result<(), Self::Error> {
            self.writes.lock().unwrap().push(block_info.header.number);
            Ok(())
        }

        fn last_committed_block(&self) -> Result<Option<BlockInfo>, Self::Error> {
            Ok(None)
        }
    }

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

    fn context(number: u64, hash_value: u8, parent: u8) -> KafkaBlockContext {
        KafkaBlockContext {
            hash: hash(hash_value),
            parent_hash: hash(parent),
            block_number: number,
        }
    }

    fn notification(offset: i64, new_blocks: Vec<KafkaBlockContext>) -> DecodedNotification {
        DecodedNotification {
            offset,
            notification: KafkaBlockChangeNotification {
                change_type: 1,
                new_blocks,
                drop_blocks: Vec::new(),
            },
        }
    }

    #[test]
    fn arc_apply_rejects_bad_tail_before_any_state_write() {
        let writer = RecordingWriter::default();
        let anchor = block(10, 10, 9, 20);
        let batch = vec![
            (block(11, 11, 10, 21), diff(21, 20)),
            (block(12, 12, 11, 22), diff(22, 99)),
        ];

        let error = apply_arc_batch(&writer, &anchor, batch).unwrap_err();

        assert!(is_fatal_arc_input(&error));
        assert!(writer.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn arc_apply_writes_a_valid_batch_in_order_after_preflight() {
        let writer = RecordingWriter::default();
        let anchor = block(20, 20, 19, 30);
        let batch = vec![
            (block(21, 21, 20, 31), diff(31, 30)),
            (block(22, 22, 21, 32), diff(32, 31)),
            (block(23, 23, 22, 33), diff(33, 32)),
        ];

        apply_arc_batch(&writer, &anchor, batch).unwrap();

        assert_eq!(*writer.writes.lock().unwrap(), vec![21, 22, 23]);
    }

    #[test]
    fn arc_catchup_rejects_bad_handoff_before_any_write() {
        let writer = RecordingWriter::default();
        let boundary = block(10, 99, 9, 20);
        let backfill = vec![
            (block(12, 12, 11, 22), diff(22, 21)),
            (block(11, 11, 10, 21), diff(21, 20)),
        ];
        let target = context(13, 13, 12);

        let result = validate_arc_catchup_suffix(&target, &boundary, &backfill).and_then(|()| {
            apply_arc_batch(&writer, &boundary, backfill.into_iter().rev().collect())
                .map_err(|error| ArcInputError::new(format!("{error:#}")))
        });

        assert!(result.is_err());
        assert!(writer.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn arc_catchup_preflights_complete_hash_and_state_chain() {
        let boundary = block(10, 10, 9, 20);
        let backfill = vec![
            (block(12, 12, 11, 22), diff(22, 21)),
            (block(11, 11, 10, 21), diff(21, 20)),
        ];
        let target = context(13, 13, 12);

        validate_arc_catchup_suffix(&target, &boundary, &backfill).unwrap();

        let mut wrong_parent_root = backfill.clone();
        wrong_parent_root[0].1.parent_hash = hash(90);
        assert!(validate_arc_catchup_suffix(&target, &boundary, &wrong_parent_root).is_err());

        let returned = block(12, 99, 11, 22);
        assert!(validate_requested_header_hash(hash(12), &returned).is_err());
    }

    #[test]
    fn arc_catchup_without_backfill_requires_boundary_to_be_target_parent() {
        let target = context(11, 11, 10);
        validate_arc_catchup_suffix(&target, &block(10, 10, 9, 20), &[]).unwrap();
        assert!(validate_arc_catchup_suffix(&target, &block(10, 99, 9, 20), &[]).is_err());
    }

    #[test]
    fn durable_duplicate_advances_offset_without_source_data() {
        let durable = block(12, 12, 11, 22);
        let duplicate = block(10, 10, 9, 20);
        let mut prepared = HashMap::new();
        record_block_offset(&mut prepared, duplicate, BlockStorageDiff::default(), 41);

        assert_eq!(
            arc_offset_frontier(&prepared, &durable, Some(41)).unwrap(),
            Some(42)
        );
        prune_arc_durable_prefix(&mut prepared, &durable);
        assert!(prepared.is_empty());
    }

    #[test]
    fn memory_only_duplicate_waits_for_durable_bottom() {
        let durable = block(10, 10, 9, 20);
        let memory_only = block(11, 11, 10, 21);
        let mut prepared = HashMap::new();
        record_block_offset(
            &mut prepared,
            memory_only.clone(),
            BlockStorageDiff::default(),
            55,
        );

        assert_eq!(
            arc_offset_frontier(&prepared, &durable, Some(55)).unwrap(),
            Some(55)
        );
        prune_arc_durable_prefix(&mut prepared, &durable);
        assert_eq!(prepared.get(&memory_only.header.hash).unwrap().offset, 55);
    }

    #[test]
    fn crash_replays_future_after_a_later_durable_duplicate() {
        let durable = block(10, 10, 9, 20);
        let future = block(11, 11, 10, 21);
        let mut prepared = HashMap::new();
        record_block_offset(&mut prepared, future.clone(), diff(21, 20), 101);
        record_block_offset(&mut prepared, durable.clone(), diff(20, 19), 102);
        record_block_offset(&mut prepared, future.clone(), diff(21, 20), 200);

        assert_eq!(prepared.get(&future.header.hash).unwrap().first_offset, 101);
        assert_eq!(prepared.get(&future.header.hash).unwrap().offset, 200);
        let replay_offset = arc_offset_frontier(&prepared, &durable, Some(200))
            .unwrap()
            .unwrap();
        assert_eq!(replay_offset, 101);

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let offset_dir = std::env::temp_dir().join(format!(
            "leafage-arc-offset-frontier-{}-{unique}",
            std::process::id()
        ));
        write_offset(offset_dir.to_str().unwrap(), replay_offset).unwrap();
        assert_eq!(read_offset(offset_dir.to_str().unwrap()).unwrap(), 101);
        std::fs::remove_dir_all(offset_dir).unwrap();

        // A crash before the in-memory future is capped replays the same
        // message; computing the frontier again cannot move past it.
        assert_eq!(
            arc_offset_frontier(&prepared, &durable, Some(200)).unwrap(),
            Some(101)
        );
        prune_arc_durable_prefix(&mut prepared, &durable);
        assert!(prepared.contains_key(&future.header.hash));
        assert!(!prepared.contains_key(&durable.header.hash));

        assert_eq!(
            arc_offset_frontier(&prepared, &future, Some(200)).unwrap(),
            Some(201)
        );
        prune_arc_durable_prefix(&mut prepared, &future);
        assert!(prepared.is_empty());
    }

    #[test]
    fn empty_only_chunk_advances_to_next_consumed_offset() {
        let durable = block(10, 10, 9, 20);
        let prepared = HashMap::new();

        assert_eq!(
            arc_offset_frontier(&prepared, &durable, Some(77)).unwrap(),
            Some(78)
        );
    }

    #[test]
    fn durable_bottom_only_clears_the_matching_hash_at_its_height() {
        let durable = block(10, 10, 9, 20);
        let conflicting = block(10, 99, 9, 90);
        let mut prepared = HashMap::new();
        record_block_offset(&mut prepared, conflicting.clone(), diff(90, 20), 61);

        assert_eq!(
            arc_offset_frontier(&prepared, &durable, Some(61)).unwrap(),
            Some(61)
        );
        prune_arc_durable_prefix(&mut prepared, &durable);
        assert!(prepared.contains_key(&conflicting.header.hash));
    }

    #[test]
    fn planner_uses_first_future_after_leading_empty_and_durable_duplicate() {
        let latest = block(10, 10, 9, 20);
        let duplicate = context(10, 10, 9);
        let far_ahead = context(15, 15, 14);
        let decoded = vec![
            notification(40, Vec::new()),
            notification(41, vec![duplicate]),
            notification(42, vec![far_ahead.clone()]),
        ];
        let prepared = Mutex::new(HashMap::new());

        let plan = plan_arc_notifications(&decoded, &latest, &prepared, |context| {
            Ok(if context.hash == latest.header.hash {
                ArcLocalBlock {
                    exact: Some(latest.clone()),
                    at_height: Some(latest.clone()),
                }
            } else {
                ArcLocalBlock::default()
            })
        })
        .unwrap();

        assert_eq!(plan.first_future, Some(far_ahead.clone()));
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].notification.new_blocks, vec![far_ahead]);
        assert_eq!(prepared.lock().unwrap().get(&hash(10)).unwrap().offset, 41);
    }

    #[test]
    fn planner_preserves_first_pending_offset_for_frontier() {
        let latest = block(10, 10, 9, 20);
        let future = context(15, 15, 14);
        let decoded = vec![
            notification(101, vec![future.clone()]),
            notification(200, vec![future.clone()]),
        ];
        let prepared = Mutex::new(HashMap::new());

        let plan = plan_arc_notifications(&decoded, &latest, &prepared, |_| {
            Ok(ArcLocalBlock::default())
        })
        .unwrap();
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].offset, 101);

        let mut fetched = HashMap::new();
        record_block_offset(
            &mut fetched,
            block(15, 15, 14, 25),
            diff(25, 24),
            plan.pending[0].offset,
        );
        assert_eq!(
            arc_offset_frontier(&fetched, &latest, Some(200)).unwrap(),
            Some(101)
        );
    }

    #[test]
    fn catchup_mode_waits_for_a_successful_future_target() {
        let latest = block(10, 10, 9, 20);
        let prepared = Mutex::new(HashMap::new());
        let empty = vec![notification(70, Vec::new())];
        let first =
            plan_arc_notifications(&empty, &latest, &prepared, |_| Ok(ArcLocalBlock::default()))
                .unwrap();
        let mut read_from_kafka = false;
        mark_arc_catchup_succeeded(&mut read_from_kafka, first.first_future.is_some());
        assert!(!read_from_kafka);

        // A failed catch-up never calls the transition helper.
        let future = vec![notification(71, vec![context(15, 15, 14)])];
        let second =
            plan_arc_notifications(
                &future,
                &latest,
                &prepared,
                |_| Ok(ArcLocalBlock::default()),
            )
            .unwrap();
        assert!(second.first_future.is_some());
        assert!(!read_from_kafka);

        // update_from_s3_target success is the only call site that publishes
        // live mode.
        mark_arc_catchup_succeeded(&mut read_from_kafka, true);
        assert!(read_from_kafka);
    }

    #[test]
    fn arc_receive_error_discards_the_entire_chunk_and_replays_safe_offset() {
        let result = collect_complete_arc_chunk(vec![Ok(10), Err("receive"), Ok(12)]);
        assert_eq!(result, Err("receive"));

        assert_eq!(initial_arc_replay_offset(Some(101), 50, 200), 101);
        assert_eq!(initial_arc_replay_offset(Some(40), 50, 200), 200);
        assert_eq!(initial_arc_replay_offset(None, 50, 200), 200);
    }

    #[test]
    fn legacy_prepared_path_preserves_normal_behavior_and_missing_is_retryable() {
        let info = block(11, 11, 10, 21);
        let mut prepared = HashMap::new();
        record_block_offset(&mut prepared, info.clone(), diff(21, 20), 81);
        let announced = vec![context(11, 11, 10)];

        let path = prepared_update_path(&prepared, &announced, UpdaterInputPolicy::Legacy).unwrap();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].block_info, info);
        assert_eq!(path[0].offset, 81);

        let error = prepared_update_path(&HashMap::new(), &announced, UpdaterInputPolicy::Legacy)
            .unwrap_err();
        assert_eq!(classify_arc_update_failure(&error), ArcUpdateFailure::Retry);
        assert!(format!("{error:#}").contains("missing prepared block"));
    }

    #[test]
    fn planner_rejects_same_height_conflicts() {
        let latest = block(10, 10, 9, 20);
        let prepared = Mutex::new(HashMap::new());
        let different_hash = vec![
            notification(50, vec![context(15, 15, 14)]),
            notification(51, vec![context(15, 16, 14)]),
        ];
        let error = plan_arc_notifications(&different_hash, &latest, &prepared, |_| {
            Ok(ArcLocalBlock::default())
        })
        .unwrap_err();
        assert!(is_fatal_arc_input(&error));

        let different_parent = vec![
            notification(52, vec![context(15, 15, 14)]),
            notification(53, vec![context(15, 15, 13)]),
        ];
        let error = plan_arc_notifications(&different_parent, &latest, &prepared, |_| {
            Ok(ArcLocalBlock::default())
        })
        .unwrap_err();
        assert!(is_fatal_arc_input(&error));

        let wrong_local_parent = vec![notification(54, vec![context(10, 10, 8)])];
        let error = plan_arc_notifications(&wrong_local_parent, &latest, &prepared, |_| {
            Ok(ArcLocalBlock {
                exact: Some(latest.clone()),
                at_height: Some(latest.clone()),
            })
        })
        .unwrap_err();
        assert!(is_fatal_arc_input(&error));
    }

    #[test]
    fn retryable_error_is_not_classified_as_fatal_input() {
        let transient = anyhow::anyhow!("temporary S3 timeout");
        assert_eq!(
            classify_arc_update_failure(&transient),
            ArcUpdateFailure::Retry
        );

        let fatal: anyhow::Error = ArcInputError::new("conflicting parent hash").into();
        assert_eq!(classify_arc_update_failure(&fatal), ArcUpdateFailure::Fatal);

        let bundle_integrity: anyhow::Error =
            BundleIntegrityError::new("decoded roots do not connect").into();
        let mapped = Err::<(), _>(map_arc_bundle_error(bundle_integrity))
            .context("read Arc bundle")
            .unwrap_err();
        assert_eq!(
            classify_arc_update_failure(&mapped),
            ArcUpdateFailure::Fatal
        );

        let bundle_transport = map_arc_bundle_error(anyhow::anyhow!("temporary body timeout"));
        assert_eq!(
            classify_arc_update_failure(&bundle_transport),
            ArcUpdateFailure::Retry
        );
    }
}

#[derive(Debug, Clone)]
struct DecodedNotification {
    offset: i64,
    notification: KafkaBlockChangeNotification,
}

#[derive(Debug)]
struct ArcNotificationPlan {
    pending: Vec<DecodedNotification>,
    first_future: Option<KafkaBlockContext>,
}

#[derive(Debug, Default)]
struct ArcLocalBlock {
    exact: Option<BlockInfo>,
    at_height: Option<BlockInfo>,
}

fn record_block_offset(
    prepared: &mut HashMap<H256, BlockContextWithOffset>,
    block_info: BlockInfo,
    block_diff: BlockStorageDiff,
    offset: i64,
) {
    let hash = block_info.header.hash;
    match prepared.get_mut(&hash) {
        Some(existing) => {
            existing.first_offset = existing.first_offset.min(offset);
            existing.offset = existing.offset.max(offset);
        }
        None => {
            prepared.insert(
                hash,
                BlockContextWithOffset {
                    block_diff,
                    block_info,
                    first_offset: offset,
                    offset,
                },
            );
        }
    }
}

fn arc_offset_frontier(
    prepared: &HashMap<H256, BlockContextWithOffset>,
    durable_block: &BlockInfo,
    highest_consumed_offset: Option<i64>,
) -> Result<Option<i64>> {
    let Some(highest_consumed_offset) = highest_consumed_offset else {
        return Ok(None);
    };
    let consumed_next = highest_consumed_offset
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Kafka offset overflow"))?;
    let is_durable = |block: &BlockContextWithOffset| {
        block.block_info.header.number < durable_block.header.number
            || (block.block_info.header.number == durable_block.header.number
                && block.block_info.header.hash == durable_block.header.hash)
    };
    let first_non_durable = prepared
        .values()
        .filter(|block| !is_durable(block))
        .map(|block| block.first_offset)
        .min();
    Ok(Some(first_non_durable.map_or(consumed_next, |barrier| {
        consumed_next.min(barrier)
    })))
}

fn prune_arc_durable_prefix(
    prepared: &mut HashMap<H256, BlockContextWithOffset>,
    durable_block: &BlockInfo,
) {
    prepared.retain(|_, block| {
        block.block_info.header.number > durable_block.header.number
            || (block.block_info.header.number == durable_block.header.number
                && block.block_info.header.hash != durable_block.header.hash)
    });
}

fn is_fatal_arc_input(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ArcInputError>().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArcUpdateFailure {
    Fatal,
    Retry,
}

fn classify_arc_update_failure(error: &anyhow::Error) -> ArcUpdateFailure {
    if is_fatal_arc_input(error) {
        ArcUpdateFailure::Fatal
    } else {
        ArcUpdateFailure::Retry
    }
}

fn map_arc_bundle_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<BundleIntegrityError>().is_some() {
        ArcInputError::new(format!("bundle integrity check failed: {error:#}")).into()
    } else {
        error
    }
}

fn collect_complete_arc_chunk<T, E>(messages: Vec<Result<T, E>>) -> Result<Vec<T>, E> {
    messages.into_iter().collect()
}

fn initial_arc_replay_offset(
    persisted_next_offset: Option<i64>,
    lowest_offset: i64,
    latest_offset: i64,
) -> i64 {
    persisted_next_offset
        .filter(|offset| *offset >= lowest_offset)
        .unwrap_or(latest_offset)
}

fn mark_arc_catchup_succeeded(read_from_kafka: &mut bool, had_target: bool) {
    if had_target {
        *read_from_kafka = true;
    }
}

fn validate_requested_header_hash(
    requested_hash: H256,
    block_info: &BlockInfo,
) -> Result<(), ArcInputError> {
    if block_info.header.hash != requested_hash {
        return Err(ArcInputError::new(format!(
            "S3 by-hash lookup requested {}, returned Header {}",
            requested_hash, block_info.header.hash
        )));
    }
    Ok(())
}

/// Validate the complete by-hash suffix and its by-number hand-off before the
/// stable prefix is allowed to mutate StateTree. `backfill` is newest-first,
/// matching the order in which parent hashes are fetched.
fn validate_arc_catchup_suffix(
    target: &KafkaBlockContext,
    boundary: &BlockInfo,
    backfill: &[(BlockInfo, BlockStorageDiff)],
) -> Result<(), ArcInputError> {
    let expected_tip = target.block_number.checked_sub(1).ok_or_else(|| {
        ArcInputError::new(format!(
            "Kafka catch-up target {} has no parent height",
            target.block_number
        ))
    })?;
    let actual_tip = backfill.first().map(|(block, _)| block).unwrap_or(boundary);
    if actual_tip.header.number != expected_tip {
        return Err(ArcInputError::new(format!(
            "S3 catch-up suffix ends at block {}, expected Kafka parent height {}",
            actual_tip.header.number, expected_tip
        )));
    }
    if actual_tip.header.hash != target.parent_hash {
        return Err(ArcInputError::new(format!(
            "S3 catch-up suffix tip {} does not match Kafka parent {}",
            actual_tip.header.hash, target.parent_hash
        )));
    }

    validate_batch(
        boundary,
        backfill
            .iter()
            .rev()
            .map(|(block_info, block_diff)| (block_info, block_diff)),
    )
}

fn missing_prepared_data(
    input_policy: UpdaterInputPolicy,
    resource: impl fmt::Display,
) -> anyhow::Error {
    if input_policy.is_arc() {
        ArcInputError::new(format!("missing {resource} in prepared Arc input")).into()
    } else {
        anyhow::anyhow!("missing {resource} in prepared Kafka input")
    }
}

fn prepared_update_path(
    prepared: &HashMap<H256, BlockContextWithOffset>,
    new_blocks: &[KafkaBlockContext],
    input_policy: UpdaterInputPolicy,
) -> Result<Vec<BlockContextWithOffset>> {
    new_blocks
        .iter()
        .map(|new_block| {
            prepared.get(&new_block.hash).cloned().ok_or_else(|| {
                missing_prepared_data(
                    input_policy,
                    format!(
                        "prepared block {} at height {}",
                        new_block.hash, new_block.block_number
                    ),
                )
            })
        })
        .collect()
}

fn plan_arc_notifications<F>(
    decoded: &[DecodedNotification],
    latest: &BlockInfo,
    prepared: &Mutex<HashMap<H256, BlockContextWithOffset>>,
    mut local_block: F,
) -> Result<ArcNotificationPlan>
where
    F: FnMut(&KafkaBlockContext) -> Result<ArcLocalBlock>,
{
    let mut known_hashes = HashMap::new();
    let mut known_parents = HashMap::new();
    let mut pending_indexes: HashMap<u64, usize> = HashMap::new();
    let mut pending: Vec<(KafkaBlockContext, i64)> = Vec::new();
    let mut previous_future: Option<KafkaBlockContext> = None;

    for item in decoded {
        validate_notification(item.offset, &item.notification)?;
        for context in &item.notification.new_blocks {
            let first_at_height =
                block_hash_at_height(&mut known_hashes, context.block_number, context.hash)?;
            if !first_at_height {
                let previous_parent =
                    known_parents.get(&context.block_number).ok_or_else(|| {
                        ArcInputError::new(format!(
                            "missing parent metadata for duplicate block height {}",
                            context.block_number
                        ))
                    })?;
                if *previous_parent != context.parent_hash {
                    return Err(ArcInputError::new(format!(
                        "block height {} hash {} contains conflicting parents {} and {}",
                        context.block_number, context.hash, previous_parent, context.parent_hash
                    ))
                    .into());
                }
                if let Some(index) = pending_indexes.get(&context.block_number).copied() {
                    // The message containing the first sighting must be
                    // replayed until this block reaches the durable bottom.
                    pending[index].1 = pending[index].1.min(item.offset);
                } else {
                    let existing = local_block(context)?.exact.ok_or_else(|| {
                        ArcInputError::new(format!(
                            "duplicate block {} at height {} is not available locally",
                            context.hash, context.block_number
                        ))
                    })?;
                    validate_notification_header(item.offset, context, &existing)?;
                    let mut prepared = prepared.lock().unwrap();
                    record_block_offset(
                        &mut prepared,
                        existing,
                        BlockStorageDiff::default(),
                        item.offset,
                    );
                }
                continue;
            }
            known_parents.insert(context.block_number, context.parent_hash);

            if context.block_number <= latest.header.number {
                let local = local_block(context)?;
                if let Some(existing) = local.exact {
                    validate_notification_header(item.offset, context, &existing)?;
                    let mut prepared = prepared.lock().unwrap();
                    record_block_offset(
                        &mut prepared,
                        existing,
                        BlockStorageDiff::default(),
                        item.offset,
                    );
                    continue;
                }
                let actual = local
                    .at_height
                    .map(|block| block.header.hash.to_string())
                    .unwrap_or_else(|| "unavailable".to_owned());
                return Err(ArcInputError::new(format!(
                    "block height {} announces hash {}, local canonical hash is {}",
                    context.block_number, context.hash, actual
                ))
                .into());
            }

            if let Some(previous) = &previous_future {
                let expected_number = previous.block_number.checked_add(1).ok_or_else(|| {
                    ArcInputError::new(format!(
                        "block number overflows after Kafka block {}",
                        previous.block_number
                    ))
                })?;
                if context.block_number != expected_number {
                    return Err(ArcInputError::new(format!(
                        "Kafka future block {} has number {}, expected {}",
                        context.hash, context.block_number, expected_number
                    ))
                    .into());
                }
                if context.parent_hash != previous.hash {
                    return Err(ArcInputError::new(format!(
                        "Kafka future block {} parent {} does not match previous hash {}",
                        context.block_number, context.parent_hash, previous.hash
                    ))
                    .into());
                }
            } else {
                let next_local = latest.header.number.checked_add(1).ok_or_else(|| {
                    ArcInputError::new(format!(
                        "block number overflows after local head {}",
                        latest.header.number
                    ))
                })?;
                if context.block_number == next_local && context.parent_hash != latest.header.hash {
                    return Err(ArcInputError::new(format!(
                        "Kafka block {} parent {} does not match local head hash {}",
                        context.block_number, context.parent_hash, latest.header.hash
                    ))
                    .into());
                }
            }
            previous_future = Some(context.clone());
            pending_indexes.insert(context.block_number, pending.len());
            pending.push((context.clone(), item.offset));
        }
    }

    let first_future = pending.first().map(|(context, _)| context.clone());
    let pending = pending
        .into_iter()
        .map(|(context, offset)| DecodedNotification {
            offset,
            notification: KafkaBlockChangeNotification {
                change_type: 1,
                new_blocks: vec![context],
                drop_blocks: Vec::new(),
            },
        })
        .collect();
    Ok(ArcNotificationPlan {
        pending,
        first_future,
    })
}

fn apply_arc_batch<Tree>(
    tree: &Tree,
    anchor: &BlockInfo,
    blocks: Vec<(BlockInfo, BlockStorageDiff)>,
) -> Result<()>
where
    Tree: EvmStorageWrite,
{
    validate_batch(
        anchor,
        blocks
            .iter()
            .map(|(block_info, block_diff)| (block_info, block_diff)),
    )?;
    for (block_info, block_diff) in blocks {
        info!(target:"updater", "Arc update block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
        tree.update_block(block_info, block_diff)
            .map_err(anyhow::Error::new)?;
    }
    Ok(())
}

/// [`Updater`] is used to update the snapshot tree to the latest block
pub struct Updater<Tree> {
    rpc_client: Option<HttpClient>,
    kafka_s3_cfg: KafkaS3Config,
    consumer: StreamConsumer,
    s3_client: Client,
    tree: Tree,
    max_diff_depth: usize,
    hash_to_blockctx: Mutex<HashMap<H256, BlockContextWithOffset>>,
    arc_highest_consumed_offset: Mutex<Option<i64>>,
    /// The earliest Kafka next-offset known to be safe for replay. It is set
    /// during initialization and only advanced after write_offset succeeds.
    arc_safe_next_offset: Mutex<Option<i64>>,
    read_from_kafka: bool,
    init_task_queue_size: usize,
    /// Reorg buffer depth for S3 catch-up: the number of blocks below the
    /// Kafka head that are backfilled by following the exact parent-hash chain
    /// instead of the by-number index. 0 disables it (legacy behavior).
    catchup_safe_depth: usize,
    bundle_range_size_mib: u32,
    /// Bundle reads stop permanently after the first definitive miss in this
    /// process. Retries continue from the in-memory latest block, so they do
    /// not need to reread the already-applied bundle prefix.
    read_from_bundle: AtomicBool,
    input_policy: UpdaterInputPolicy,
}

impl<Tree> Updater<Tree>
where
    Tree: EvmStorageRead
        + EvmStorageWrite<Error = <Tree as EvmStorageRead>::Error>
        + Send
        + Sync
        + 'static,
{
    pub async fn new(
        tree: Tree,
        rpc_url: Option<impl AsRef<str>>,
        kafka_s3_cfg: KafkaS3Config,
        max_diff_depth: usize,
        init_task_queue_size: usize,
        catchup_safe_depth: usize,
        bundle_range_size_mib: u32,
        input_policy: UpdaterInputPolicy,
    ) -> Result<Self> {
        let mut rpc_client = None;
        if let Some(rpc_url) = rpc_url {
            let client = HttpClientBuilder::default().build(rpc_url.as_ref())?;
            rpc_client = Some(client);
        }
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &kafka_s3_cfg.brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .set(
                "group.id",
                format!("leafage-evm-group-{}", kafka_s3_cfg.s3_chain_id),
            )
            .create()?;

        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);
        let read_from_bundle = !kafka_s3_cfg.bundle_bucket_name.is_empty();

        Ok(Self {
            rpc_client,
            kafka_s3_cfg,
            consumer,
            s3_client,
            tree,
            max_diff_depth,
            hash_to_blockctx: Mutex::new(HashMap::default()),
            arc_highest_consumed_offset: Mutex::new(None),
            arc_safe_next_offset: Mutex::new(None),
            read_from_kafka: true,
            init_task_queue_size,
            catchup_safe_depth,
            bundle_range_size_mib,
            read_from_bundle: AtomicBool::new(read_from_bundle),
            input_policy,
        })
    }

    fn set_offset(&self, offset: i64) -> Result<()> {
        let mut tpl = TopicPartitionList::with_capacity(1);
        tpl.add_partition_offset(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Offset::Offset(offset),
        )?;
        self.consumer.assign(&tpl)?;
        self.consumer.seek(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Offset::Offset(offset),
            Timeout::Never,
        )?;
        Ok(())
    }

    fn seek_arc_safe_offset(&self) -> Result<i64> {
        let offset = self
            .arc_safe_next_offset
            .lock()
            .unwrap()
            .ok_or_else(|| anyhow::anyhow!("Arc Kafka replay offset is not initialized"))?;
        self.set_offset(offset)?;
        Ok(offset)
    }

    #[inline]
    async fn get_block_info(&self, block_hash: H256) -> Result<BlockInfo> {
        if let Some(block_ctx) = self.hash_to_blockctx.lock().unwrap().get(&block_hash) {
            return Ok(block_ctx.block_info.clone());
        }
        s3_get_block_info(
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            &self.kafka_s3_cfg.version,
            block_hash,
        )
        .await
        .context(format!("s3 get block info failed, {block_hash}"))
    }

    fn clear(
        &self,
        presist_block_num: u64,
        presist_block_hash: H256,
    ) -> Option<BlockContextWithOffset> {
        let mut blocks = self.hash_to_blockctx.lock().unwrap();
        let presist_block = blocks.remove(&presist_block_hash);
        blocks.retain(|_, block| block.block_info.header.number >= presist_block_num);
        presist_block
    }

    fn decode_arc_notifications(
        &self,
        messages: &[BorrowedMessage<'_>],
    ) -> Result<Vec<DecodedNotification>> {
        let mut decoded = Vec::with_capacity(messages.len());
        for message in messages {
            let offset = message.offset();
            let payload = message.payload().ok_or_else(|| {
                ArcInputError::new(format!("Kafka offset {offset} has no payload"))
            })?;
            let notification = payload.try_into().map_err(|error| {
                ArcInputError::new(format!(
                    "Kafka offset {offset} payload cannot be decoded: {error}"
                ))
            })?;
            decoded.push(DecodedNotification {
                offset,
                notification,
            });
        }
        Ok(decoded)
    }

    fn validate_arc_notifications(
        &self,
        decoded: &[DecodedNotification],
    ) -> Result<ArcNotificationPlan> {
        let latest = self
            .tree
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or_else(|| anyhow::anyhow!("local StateTree has no latest block"))?
            .block_info()?;
        plan_arc_notifications(decoded, &latest, &self.hash_to_blockctx, |context| {
            self.arc_local_block(context)
        })
    }

    fn arc_local_block(&self, context: &KafkaBlockContext) -> Result<ArcLocalBlock> {
        let existing_at_number = self
            .tree
            .state_at(BlockId::Number(BlockNumberOrTag::Number(
                context.block_number,
            )))?
            .map(|state| state.block_info())
            .transpose()?;
        if existing_at_number
            .as_ref()
            .is_some_and(|block| block.header.hash == context.hash)
        {
            return Ok(ArcLocalBlock {
                exact: existing_at_number.clone(),
                at_height: existing_at_number,
            });
        }
        if existing_at_number.is_some() {
            return Ok(ArcLocalBlock {
                exact: None,
                at_height: existing_at_number,
            });
        }
        let existing_by_hash = self
            .tree
            .state_at(BlockId::Hash(context.hash.into()))?
            .map(|state| state.block_info())
            .transpose()?;
        let exact = existing_by_hash.filter(|block| {
            block.header.number == context.block_number && block.header.hash == context.hash
        });
        Ok(ArcLocalBlock {
            exact,
            at_height: existing_at_number,
        })
    }

    fn missing_prepared_data(&self, resource: impl fmt::Display) -> anyhow::Error {
        missing_prepared_data(self.input_policy, resource)
    }

    async fn prepare_update(
        &self,
        messages: &Vec<BorrowedMessage<'_>>,
    ) -> Result<Vec<KafkaBlockContext>> {
        let mut decoded = Vec::with_capacity(messages.len());
        for message in messages {
            let notification = message.payload().unwrap().try_into()?;
            decoded.push(DecodedNotification {
                offset: message.offset(),
                notification,
            });
        }
        self.prepare_decoded_update(&decoded).await
    }

    async fn prepare_decoded_update(
        &self,
        decoded: &[DecodedNotification],
    ) -> Result<Vec<KafkaBlockContext>> {
        let mut new_blocks = vec![];
        let mut get_block_info_join_set = JoinSet::new();
        let mut get_block_diff_join_set = JoinSet::new();

        for item in decoded {
            for new_block in &item.notification.new_blocks {
                let client = self.s3_client.clone();
                let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                let version = self.kafka_s3_cfg.version.clone();
                let hash = new_block.hash;
                get_block_info_join_set.spawn(async move {
                    (
                        hash,
                        s3_get_block_info(&client, &bucket_name, &s3_chain_id, &version, hash)
                            .await,
                    )
                });
            }
        }

        let mut blockhash_to_block_info = HashMap::new();
        let mut blockhash_to_block_diff = HashMap::new();

        let hash_keyed = state_diff_keyed_by_block_hash(&self.kafka_s3_cfg.s3_chain_id);

        // get block info first
        while let Some(res) = get_block_info_join_set.join_next().await {
            let (requested_hash, result) = res?;
            let block_info =
                result.with_context(|| format!("fetch Kafka Header {requested_hash}"))?;
            let block_hash = block_info.header.hash;
            blockhash_to_block_info.insert(block_hash, block_info.clone());

            // Block-hash-keyed chains have one object per block, so every
            // block is fetched. Elsewhere the parent is read only to decide
            // whether an object was written for this block at all.
            let diff_key = if hash_keyed {
                Some(block_hash)
            } else {
                let parent_hash = block_info.header.parent_hash;
                let parent_block_info =
                    if let Some(parent_block_info) = blockhash_to_block_info.get(&parent_hash) {
                        parent_block_info.clone()
                    } else {
                        let parent_block_info = self.get_block_info(parent_hash).await?;
                        blockhash_to_block_info.insert(parent_hash, parent_block_info.clone());
                        parent_block_info
                    };
                (parent_block_info.header.state_root != block_info.header.state_root)
                    .then_some(block_info.header.state_root)
            };

            if let Some(diff_key) = diff_key {
                let client = self.s3_client.clone();
                let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                let version = self.kafka_s3_cfg.version.clone();
                get_block_diff_join_set.spawn(async move {
                    (
                        block_hash,
                        s3_get_block_diff(&client, &bucket_name, &s3_chain_id, &version, diff_key)
                            .await,
                    )
                });
            }
        }

        // get block diff
        while let Some(res) = get_block_diff_join_set.join_next().await {
            let (block_hash, result) = res?;
            let block_diff =
                result.with_context(|| format!("fetch Kafka StateDiff for block {block_hash}"))?;
            blockhash_to_block_diff.insert(block_hash, block_diff);
        }

        let mut block_contexts = Vec::new();
        for item in decoded {
            debug!(target:"updater", "get block_change_notification {:?}, offset {:?}", item.notification, item.offset);
            for new_block in &item.notification.new_blocks {
                let block_info = blockhash_to_block_info
                    .get(&new_block.hash)
                    .ok_or_else(|| {
                        self.missing_prepared_data(format!(
                            "Header {} for block {}",
                            new_block.hash, new_block.block_number
                        ))
                    })?
                    .clone();
                if self.input_policy.is_arc() {
                    validate_notification_header(item.offset, new_block, &block_info)?;
                }

                let block_diff = match blockhash_to_block_diff.get(&new_block.hash) {
                    Some(block_diff) => block_diff.clone(),
                    None => {
                        let parent_block_info = blockhash_to_block_info
                            .get(&new_block.parent_hash)
                            .ok_or_else(|| {
                                self.missing_prepared_data(format!(
                                    "parent Header {} for block {}",
                                    new_block.parent_hash, new_block.block_number
                                ))
                            })?;
                        BlockStorageDiff {
                            hash: block_info.header.state_root,
                            parent_hash: parent_block_info.header.state_root,
                            ..Default::default()
                        }
                    }
                };
                if self.input_policy.is_arc() {
                    let parent_block_info = blockhash_to_block_info
                        .get(&new_block.parent_hash)
                        .ok_or_else(|| {
                            self.missing_prepared_data(format!(
                                "parent Header {} for block {}",
                                new_block.parent_hash, new_block.block_number
                            ))
                        })?;
                    validate_batch(
                        parent_block_info,
                        std::iter::once((&block_info, &block_diff)),
                    )?;
                }

                let block_ctx_with_offset = BlockContextWithOffset {
                    block_diff,
                    block_info,
                    first_offset: item.offset,
                    offset: item.offset,
                };

                block_contexts.push((new_block.hash, block_ctx_with_offset));
                new_blocks.push(new_block.clone());
            }
        }
        let mut prepared = self.hash_to_blockctx.lock().unwrap();
        if self.input_policy.is_arc() {
            for (_, block) in block_contexts {
                record_block_offset(
                    &mut prepared,
                    block.block_info,
                    block.block_diff,
                    block.offset,
                );
            }
        } else {
            prepared.extend(block_contexts);
        }
        Ok(new_blocks)
    }

    async fn update_range_from_s3(
        &self,
        start_block_number: u64,
        end_block_number: u64,
    ) -> Result<()> {
        let mut get_block_info_diff_join_set = JoinSet::new();
        for block_number in start_block_number..=end_block_number {
            let rpc_client = self.rpc_client.clone();
            let client = self.s3_client.clone();
            let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
            let outer_bucket_name = self.kafka_s3_cfg.outer_bucket_name.clone();
            let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
            let version = self.kafka_s3_cfg.version.clone();
            get_block_info_diff_join_set.spawn(async move {
                (
                    block_number,
                    s3_get_block_info_and_diff_by_number(
                        &rpc_client,
                        &client,
                        &bucket_name,
                        &outer_bucket_name,
                        &s3_chain_id,
                        &version,
                        block_number,
                    )
                    .await,
                )
            });
        }
        if self.input_policy.is_arc() {
            let mut all_results = Vec::new();
            while let Some(result) = get_block_info_diff_join_set.join_next().await {
                all_results.push(result.context("join Arc S3 catch-up task")?);
            }
            all_results.sort_by_key(|(block_number, _)| *block_number);
            let mut blocks = Vec::with_capacity(all_results.len());
            for (block_number, result) in all_results {
                match result {
                    Ok(block) => blocks.push(block),
                    Err(error) => {
                        return Err(
                            error.context(format!("fetch S3 catch-up block {block_number}"))
                        );
                    }
                }
            }
            let anchor = self
                .tree
                .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                .ok_or_else(|| anyhow::anyhow!("local StateTree has no latest block"))?
                .block_info()?;
            apply_arc_batch(&self.tree, &anchor, blocks)?;
            info!(target:"updater", "update from s3, start block number {}, end block number {}", start_block_number, end_block_number);
            return Ok(());
        }
        let mut all_results = get_block_info_diff_join_set.join_all().await;
        all_results.sort_by_key(|(i, _)| *i);
        for (_, res) in all_results {
            match res {
                Ok((block_info, block_diff)) => {
                    info!(target:"updater", "update block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
                    self.tree.update_block(block_info.clone(), block_diff)?;
                }
                Err(e) => {
                    error!(target: "etl", "Join error: {}", e);
                    return Err(anyhow::anyhow!("Failed to join tasks: {}", e));
                }
            }
        }
        info!(target:"updater", "update from s3, start block number {}, end block number {}", start_block_number, end_block_number);
        Ok(())
    }

    /// Catch up the stable by-number segment from compacted bundles first.
    /// After the first missing bundle, all remaining blocks and retries use
    /// the original per-block reads without probing bundle storage again.
    async fn update_bundle_range(
        &self,
        start_block_number: u64,
        end_block_number: u64,
    ) -> Result<Option<BlockInfo>> {
        if self.input_policy.is_arc() {
            let mut blocks = Vec::new();
            let last_bundle_block = s3_read_bundle(
                &self.s3_client,
                &self.kafka_s3_cfg.bundle_bucket_name,
                &self.kafka_s3_cfg.s3_chain_id,
                &self.kafka_s3_cfg.version,
                start_block_number,
                end_block_number,
                self.bundle_range_size_mib,
                |block_info, block_diff| {
                    blocks.push((block_info, block_diff));
                    std::future::ready(Ok(()))
                },
            )
            .await
            .map_err(map_arc_bundle_error)
            .with_context(|| {
                format!("read bundle blocks {start_block_number}..={end_block_number}")
            })?;
            if last_bundle_block.is_some() {
                let anchor = self
                    .tree
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .ok_or_else(|| anyhow::anyhow!("local StateTree has no latest block"))?
                    .block_info()?;
                apply_arc_batch(&self.tree, &anchor, blocks)?;
            }
            return Ok(last_bundle_block);
        }

        s3_read_bundle(
            &self.s3_client,
            &self.kafka_s3_cfg.bundle_bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            &self.kafka_s3_cfg.version,
            start_block_number,
            end_block_number,
            self.bundle_range_size_mib,
            |block_info, block_diff| async move {
                info!(target:"updater", "update bundle block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
                self.tree.update_block(block_info, block_diff)?;
                Ok(())
            },
        )
        .await
    }

    async fn update_stable_range_from_s3(
        &self,
        start_block_number: u64,
        end_block_number: u64,
        mut parent_state_root: H256,
    ) -> Result<()> {
        let mut next_block_number = start_block_number;

        while self.read_from_bundle.load(Ordering::Relaxed) && next_block_number <= end_block_number
        {
            let current_bundle_end = bundle_end(next_block_number).min(end_block_number);
            let last_bundle_block = self
                .update_bundle_range(next_block_number, current_bundle_end)
                .await?;

            let Some(last_bundle_block) = last_bundle_block else {
                self.read_from_bundle.store(false, Ordering::Relaxed);
                info!(target: "updater",
                    "Bundle containing block {} is not available; switching to per-block reads for the rest of this catch-up",
                    next_block_number);
                break;
            };
            parent_state_root = last_bundle_block.header.state_root;

            info!(target: "updater",
                "Updated blocks {}..={} from bundle storage",
                next_block_number, current_bundle_end);
            if current_bundle_end == end_block_number {
                return Ok(());
            }
            next_block_number = current_bundle_end + 1;
        }

        // The first source block can follow a compacted block whose source
        // Header has already been deleted. Use the root retained from the DB
        // or last bundle for this hand-off block.
        if next_block_number <= end_block_number {
            let result = s3_get_block_info_and_diff_by_number_with_parent_state_root(
                &self.rpc_client,
                &self.s3_client,
                &self.kafka_s3_cfg.bucket_name,
                &self.kafka_s3_cfg.outer_bucket_name,
                &self.kafka_s3_cfg.s3_chain_id,
                &self.kafka_s3_cfg.version,
                next_block_number,
                parent_state_root,
            )
            .await;
            let (block_info, block_diff) = match result {
                Ok(block) => block,
                Err(error) => {
                    return Err(error.context(format!(
                        "fetch first per-block catch-up block {next_block_number}"
                    )));
                }
            };
            if self.input_policy.is_arc() {
                let anchor = self
                    .tree
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .ok_or_else(|| anyhow::anyhow!("local StateTree has no latest block"))?
                    .block_info()?;
                apply_arc_batch(&self.tree, &anchor, vec![(block_info, block_diff)])?;
            } else {
                info!(target:"updater", "update first per-block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
                self.tree.update_block(block_info, block_diff)?;
            }
            if next_block_number == end_block_number {
                return Ok(());
            }
            next_block_number += 1;
        }

        let batch_size = std::cmp::max(1, self.init_task_queue_size as u64);
        while next_block_number <= end_block_number {
            let current_end = std::cmp::min(
                next_block_number.saturating_add(batch_size - 1),
                end_block_number,
            );
            self.update_range_from_s3(next_block_number, current_end)
                .await?;
            if current_end == end_block_number {
                break;
            }
            next_block_number = current_end + 1;
        }
        Ok(())
    }

    async fn update_from_s3(&self, messages: &Vec<BorrowedMessage<'_>>) -> Result<()> {
        let block_change_notification: KafkaBlockChangeNotification =
            messages[0].payload().unwrap().try_into()?;
        let target_block = block_change_notification
            .new_blocks
            .first()
            .ok_or_else(|| anyhow::anyhow!("No new blocks in the message"))?
            .clone();
        self.update_from_s3_target(&target_block).await
    }

    async fn arc_handoff_boundary(
        &self,
        boundary_number: u64,
        last_applied_block: &BlockInfo,
    ) -> Result<BlockInfo> {
        if boundary_number == last_applied_block.header.number {
            return Ok(last_applied_block.clone());
        }

        // Compacted source Headers may already be gone. Prefer the same
        // by-number bundle source as phase 1, but do not mutate StateTree.
        if self.read_from_bundle.load(Ordering::Relaxed)
            && !self.kafka_s3_cfg.bundle_bucket_name.is_empty()
        {
            let boundary = s3_read_bundle(
                &self.s3_client,
                &self.kafka_s3_cfg.bundle_bucket_name,
                &self.kafka_s3_cfg.s3_chain_id,
                &self.kafka_s3_cfg.version,
                boundary_number,
                boundary_number,
                self.bundle_range_size_mib,
                |_, _| std::future::ready(Ok(())),
            )
            .await
            .map_err(map_arc_bundle_error)
            .with_context(|| {
                format!("pre-read Arc catch-up hand-off block {boundary_number} from bundle")
            })?;
            if let Some(boundary) = boundary {
                return Ok(boundary);
            }
        }

        s3_get_block_info_by_number(
            &self.rpc_client,
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.outer_bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            &self.kafka_s3_cfg.version,
            boundary_number,
        )
        .await
        .with_context(|| format!("pre-read Arc catch-up hand-off block {boundary_number}"))
    }

    async fn prepare_arc_catchup_suffix(
        &self,
        target_block: &KafkaBlockContext,
        by_number_target: u64,
        last_applied_block: &BlockInfo,
    ) -> Result<(BlockInfo, Vec<(BlockInfo, BlockStorageDiff)>)> {
        let tip_block_number = target_block.block_number.checked_sub(1).ok_or_else(|| {
            ArcInputError::new(format!(
                "Kafka catch-up target {} has no parent height",
                target_block.block_number
            ))
        })?;
        let max_hops = tip_block_number
            .checked_sub(by_number_target)
            .ok_or_else(|| {
                ArcInputError::new(format!(
                    "Arc catch-up boundary {} is above Kafka parent height {}",
                    by_number_target, tip_block_number
                ))
            })?;

        // Fetch the exact suffix newest-first. No stable-prefix write is
        // allowed until every requested hash and the complete suffix have
        // passed validation.
        let mut requested_hash = target_block.parent_hash;
        let mut backfill = Vec::new();
        for _ in 0..max_hops {
            let (block_info, block_diff) = s3_get_block_info_and_diff_by_hash(
                &self.s3_client,
                &self.kafka_s3_cfg.bucket_name,
                &self.kafka_s3_cfg.s3_chain_id,
                &self.kafka_s3_cfg.version,
                requested_hash,
            )
            .await
            .with_context(|| format!("fetch Arc catch-up block by hash {requested_hash}"))?;
            validate_requested_header_hash(requested_hash, &block_info)?;
            requested_hash = block_info.header.parent_hash;
            backfill.push((block_info, block_diff));
        }

        let boundary = self
            .arc_handoff_boundary(by_number_target, last_applied_block)
            .await?;
        if boundary.header.number != by_number_target {
            return Err(ArcInputError::new(format!(
                "S3 by-number hand-off requested block {}, returned Header at {}",
                by_number_target, boundary.header.number
            ))
            .into());
        }
        validate_arc_catchup_suffix(target_block, &boundary, &backfill)?;
        Ok((boundary, backfill))
    }

    async fn update_from_s3_target(&self, target_block: &KafkaBlockContext) -> Result<()> {
        let last_applied_block = self
            .tree
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or_else(|| anyhow::anyhow!("No latest block in StateTree"))?
            .block_info()?;
        let last_applied_number = last_applied_block.header.number;
        let tip_block_number = target_block.block_number.saturating_sub(1);

        // The by-number S3 index can resolve the wrong branch around the chain
        // tip during a reorg, which leaves the hand-off block disconnected from
        // the Kafka stream. So only trust by-number for the stable segment and
        // leave a `catchup_safe_depth` buffer below the Kafka head; that buffer
        // must exceed the chain's maximum reorg depth so the by-number hand-off
        // block is always canonical. The buffered tip is then backfilled along
        // the exact parent-hash links from Kafka (phase 2 below). A depth of 0
        // disables the buffer entirely, falling back to the legacy
        // by-number-only catch-up.
        let depth = self.catchup_safe_depth as u64;
        // Backfill the `depth` blocks immediately below the Kafka head (the tip
        // and the `depth - 1` blocks beneath it), so the by-number hand-off
        // block sits `depth` blocks below the tip — outside a reorg of depth
        // `<= depth`. Basing this on `tip` (not the Kafka head) keeps the flag
        // honest: `depth = 1` protects exactly the tip, `depth = 0` protects
        // nothing (legacy by-number-only catch-up).
        let by_number_target = tip_block_number
            .saturating_sub(depth)
            .max(last_applied_number)
            .min(tip_block_number);

        // Arc validates the by-hash suffix and its by-number hand-off before
        // phase 1 performs any StateTree write. The suffix is bounded by
        // catchup_safe_depth; the potentially large stable prefix remains
        // streamed in preflighted apply batches.
        let arc_backfill = if self.input_policy.is_arc() {
            let (_, backfill) = self
                .prepare_arc_catchup_suffix(target_block, by_number_target, &last_applied_block)
                .await?;
            Some(backfill)
        } else {
            None
        };

        // Phase 1: by-number catch-up over the stable segment. Prefer compacted
        // bundles until their first definitive miss, then retain the legacy
        // per-block batching for the remainder.
        let start_block_number = last_applied_number + 1;
        info!(target:"updater", "update from s3 by number, start block number {}, target block number {}", start_block_number, by_number_target);
        if start_block_number <= by_number_target {
            self.update_stable_range_from_s3(
                start_block_number,
                by_number_target,
                last_applied_block.header.state_root,
            )
            .await?;
        }

        if let Some(backfill) = arc_backfill {
            let anchor = self
                .tree
                .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                .ok_or_else(|| anyhow::anyhow!("local StateTree has no latest block"))?
                .block_info()?;
            apply_arc_batch(&self.tree, &anchor, backfill.into_iter().rev().collect())?;
            return Ok(());
        }

        // Legacy phase 2: backfill (by_number_target, tip] by walking the parent-hash
        // chain from the Kafka head, reading each block strictly by hash so a
        // tip reorg cannot swap in a sibling from the wrong branch.
        let mut backfill = Vec::new();
        if tip_block_number > by_number_target {
            let mut parent_hash = target_block.parent_hash;
            // A healthy parent-hash chain decrements the block number by one per
            // hop, so it reaches `by_number_target` within exactly this many
            // blocks. The bound guards against an unbounded walk (and S3 request
            // storm) should the chain data be corrupt or non-decreasing.
            let max_hops = tip_block_number - by_number_target;
            loop {
                let (block_info, block_diff) = s3_get_block_info_and_diff_by_hash(
                    &self.s3_client,
                    &self.kafka_s3_cfg.bucket_name,
                    &self.kafka_s3_cfg.s3_chain_id,
                    &self.kafka_s3_cfg.version,
                    parent_hash,
                )
                .await?;
                if block_info.header.number <= by_number_target {
                    break;
                }
                parent_hash = block_info.header.parent_hash;
                backfill.push((block_info, block_diff));
                if backfill.len() as u64 >= max_hops {
                    break;
                }
            }
        }
        // A reorg deeper than the buffer would leave the by-number hand-off
        // block on a stale branch: the oldest backfilled block then links to a
        // parent that isn't in the tree, and update_block would fail with an
        // opaque ParentBlockHashNotFound. Detect it here and report the real
        // cause (and the fix) instead. The chain anchor is the oldest block's
        // parent_hash, so no extra S3 read is needed.
        if let Some((oldest, _)) = backfill.last() {
            let tree_anchor_hash = self
                .tree
                .state_at(BlockId::Number(BlockNumberOrTag::Number(by_number_target)))?
                .map(|s| s.block_info())
                .transpose()?
                .map(|b| b.header.hash);
            if tree_anchor_hash != Some(oldest.header.parent_hash) {
                let reason = format!(
                    "S3 catch-up hand-off mismatch at block {}: by-number anchor {:?} != Kafka chain parent {}; \
                     reorg is deeper than --catchup-safe-depth ({}), increase it",
                    by_number_target,
                    tree_anchor_hash,
                    oldest.header.parent_hash,
                    depth
                );
                if self.input_policy.is_arc() {
                    return Err(ArcInputError::new(reason).into());
                }
                return Err(anyhow::anyhow!(reason));
            }
        }
        for (block_info, block_diff) in backfill.into_iter().rev() {
            info!(target:"updater", "update from s3 by hash, block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
            self.tree.update_block(block_info, block_diff)?;
        }
        Ok(())
    }

    async fn update_from_kafka(&self, messages: &Vec<BorrowedMessage<'_>>) -> Result<()> {
        let new_blocks = self.prepare_update(messages).await?;
        let mut update_path = {
            let blocks = self.hash_to_blockctx.lock().unwrap();
            prepared_update_path(&blocks, &new_blocks, self.input_policy)?
        };
        for block in update_path.drain(..) {
            let block_storage_diff = block.block_diff;
            let block_info = block.block_info;
            let block_hash = block_info.header.hash;
            let block_num = block_info.header.number;
            let new_accounts_num = block_storage_diff.new_accounts.len();
            let deleted_accounts_num = block_storage_diff.deleted_accounts.len();
            let new_codes_num = block_storage_diff.new_codes.len();
            self.tree.update_block(block_info, block_storage_diff)?;
            info!(target:"updater", "update block hash {}, block num {}, new accounts num {}, deleted accounts num {}, new codes num {}",
                                        block_hash, block_num, new_accounts_num, deleted_accounts_num, new_codes_num);
        }
        self.commit_offset()
    }

    async fn update_arc_messages(
        &self,
        messages: &[BorrowedMessage<'_>],
        read_from_kafka: &mut bool,
    ) -> Result<()> {
        let decoded = self.decode_arc_notifications(messages)?;
        let plan = self.validate_arc_notifications(&decoded)?;
        let new_blocks = self.prepare_decoded_update(&plan.pending).await?;
        let update_path = {
            let prepared = self.hash_to_blockctx.lock().unwrap();
            prepared_update_path(&prepared, &new_blocks, self.input_policy)?
        };
        validate_unanchored_batch(
            update_path
                .iter()
                .map(|block| (&block.block_info, &block.block_diff)),
        )?;

        if !*read_from_kafka {
            if let Some(target) = &plan.first_future {
                self.update_from_s3_target(target).await?;
                // This assignment is deliberately after the await: an empty
                // or duplicate-only chunk, and a failed catch-up, both remain
                // in catch-up mode for the next future notification.
                mark_arc_catchup_succeeded(read_from_kafka, true);
            }
        }

        let anchor = self
            .tree
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or_else(|| anyhow::anyhow!("local StateTree has no latest block"))?
            .block_info()?;
        let blocks = update_path
            .into_iter()
            .map(|block| (block.block_info, block.block_diff))
            .collect();
        apply_arc_batch(&self.tree, &anchor, blocks)?;
        self.commit_arc_offset(&decoded)
    }

    fn commit_arc_offset(&self, decoded: &[DecodedNotification]) -> Result<()> {
        let chunk_highest = decoded.iter().map(|item| item.offset).max();
        let previous_highest = *self.arc_highest_consumed_offset.lock().unwrap();
        let proposed_highest = match (previous_highest, chunk_highest) {
            (Some(previous), Some(current)) => Some(previous.max(current)),
            (previous, current) => previous.or(current),
        };
        let durable_block = self
            .tree
            .last_committed_block()?
            .ok_or_else(|| anyhow::anyhow!("No last committed block in StateTree"))?;
        let next_offset = {
            let prepared = self.hash_to_blockctx.lock().unwrap();
            arc_offset_frontier(&prepared, &durable_block, proposed_highest)?
        };
        if let Some(next_offset) = next_offset {
            write_offset(&self.kafka_s3_cfg.offset_dir, next_offset)?;
            *self.arc_safe_next_offset.lock().unwrap() = Some(next_offset);
        }

        // Only publish the in-process frontier after the durable offset write
        // succeeds. A failed write retries the same Kafka chunk with both the
        // previous frontier and every replay barrier intact.
        {
            let mut prepared = self.hash_to_blockctx.lock().unwrap();
            prune_arc_durable_prefix(&mut prepared, &durable_block);
        }
        *self.arc_highest_consumed_offset.lock().unwrap() = proposed_highest;
        Ok(())
    }

    fn commit_offset(&self) -> Result<()> {
        let presist_block = self.tree.last_committed_block()?.unwrap();
        let presist_block_num = presist_block.header.number;
        let presist_block_hash = presist_block.header.hash;
        // clear block context before presist block
        let presist_block = self.clear(presist_block_num, presist_block_hash);
        if let Some(presist_block) = presist_block {
            debug!(target:"updater", "clear block hash {}, block num {}", presist_block.block_info.header.hash, presist_block.block_info.header.number);
            write_offset(&self.kafka_s3_cfg.offset_dir, presist_block.offset + 1)?;
        }
        Ok(())
    }

    async fn get_offset(&self) -> Result<(i64, i64)> {
        let (low, high) = self.consumer.fetch_watermarks(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Duration::from_secs(1),
        )?;
        if low == high {
            return Err(anyhow::anyhow!("No messages in the topic"));
        }
        return Ok((low, high - 1));
    }

    async fn init_offset(&mut self) {
        let offset = read_offset(&self.kafka_s3_cfg.offset_dir).ok();
        let (lowest_offset, latest_offset) = self
            .get_offset()
            .await
            .expect("Failed to get latest offset");
        let arc_replay_offset = initial_arc_replay_offset(offset, lowest_offset, latest_offset);
        match offset {
            Some(offset)
                if offset >= lowest_offset && !self.kafka_s3_cfg.bundle_bucket_name.is_empty() =>
            {
                // A still-valid Kafka offset can nevertheless point at source
                // objects already removed by the compactor. With bundle
                // storage enabled, catch up from the latest notification via
                // bundle/block reads first, then resume live Kafka updates.
                info!(target: "updater", "kafka offset {} is valid, but bundle storage is enabled; catching up from bundle/block storage", offset);
                self.read_from_kafka = false;
                self.set_offset(latest_offset)
                    .expect("Failed to set latest offset");
            }
            Some(offset) if offset >= lowest_offset => {
                self.set_offset(offset).expect("Failed to set offset");
                info!(target: "updater", "kafka updater start with offset {}", offset);
            }
            Some(offset) => {
                info!(target: "updater", "offset {} is smaller than lowest offset {}, will read from s3/rpc", offset, lowest_offset);
                self.read_from_kafka = false;
                self.set_offset(latest_offset)
                    .expect("Failed to set latest offset");
            }
            None => {
                info!(target: "updater", "kafka updater start with no offset, will read from s3/rpc");
                self.read_from_kafka = false;
                self.set_offset(latest_offset)
                    .expect("Failed to set latest offset");
            }
        }
        if self.input_policy.is_arc() {
            *self.arc_safe_next_offset.lock().unwrap() = Some(arc_replay_offset);
        }
    }

    pub fn start(mut self, mut fatal_reporter: Option<ArcFatalReporter>) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        tokio::spawn(async move {
            self.init_offset().await;
            let mut arc_read_from_kafka = self.read_from_kafka;
            let stream = self.consumer.stream();
            let mut chunk = stream.ready_chunks(std::cmp::max(1, self.max_diff_depth));
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        info!(target:"updater", "stop updater");
                        break;
                    }
                    messages = chunk.next() => {
                        let messages = messages.expect("kafka stream next failed");
                        if self.input_policy.is_arc() {
                            let msgs = match collect_complete_arc_chunk(messages) {
                                Ok(messages) => messages,
                                Err(error) => {
                                    error!(target:"updater", "Failed to receive complete Arc Kafka chunk: {:?}; replaying from the last safe next-offset", error);
                                    loop {
                                        match self.seek_arc_safe_offset() {
                                            Ok(offset) => {
                                                info!(target:"updater", "Arc Kafka consumer reset to safe offset {}", offset);
                                                break;
                                            }
                                            Err(error) => {
                                                error!(target:"updater", "Failed to reset Arc Kafka consumer; retrying without committing offset: {:#}", error);
                                                tokio::select! {
                                                    _ = rx.changed() => return,
                                                    _ = time::sleep(time::Duration::from_secs(1)) => {}
                                                }
                                            }
                                        }
                                    }
                                    continue;
                                }
                            };
                            if msgs.is_empty() {
                                continue;
                            }
                            loop {
                                match self
                                    .update_arc_messages(&msgs, &mut arc_read_from_kafka)
                                    .await
                                {
                                    Ok(()) => break,
                                    Err(error) => match classify_arc_update_failure(&error) {
                                        ArcUpdateFailure::Fatal => {
                                            error!(target:"updater", "Fatal Arc input: {:#}", error);
                                            if let Some(reporter) = fatal_reporter.as_mut() {
                                                reporter.report(error);
                                            }
                                            return;
                                        }
                                        ArcUpdateFailure::Retry => {
                                            error!(target:"updater", "Failed to update Arc input, retrying without committing offset: {:#}", error);
                                            time::sleep(time::Duration::from_secs(1)).await;
                                        }
                                    },
                                }
                            }
                            continue;
                        }

                        let mut msgs = vec![];
                        for message in messages {
                            match message {
                                Ok(message) => msgs.push(message),
                                Err(error) => {
                                    error!(target:"updater", "Failed to receive message: {:?}", error);
                                    break;
                                }
                            }
                        }
                        if msgs.is_empty() {
                            continue
                        }
                        if !self.read_from_kafka {
                            loop {
                                if let Err(e) = self.update_from_s3(&msgs).await {
                                    error!(target:"updater", "Failed to update from S3: {:?}", e);
                                    time::sleep(time::Duration::from_secs(1)).await
                                } else {
                                    self.read_from_kafka = true;
                                    break;
                                }
                            }
                        }
                        if self.read_from_kafka {
                            loop {
                                if let Err(e) = self.update_from_kafka(&msgs).await {
                                    error!(target:"updater", "Failed to update: {:?}", e);
                                    self.commit_offset().expect("Failed to commit offset");
                                    time::sleep(time::Duration::from_secs(1)).await
                                } else {
                                    break;
                                }
                            }
                        }

                    }
                }
            }
        });

        tx
    }
}
