use alloy_rlp::Decodable;
use anyhow::{bail, Context, Result};
use aws_sdk_s3::{
    error::SdkError, operation::get_object::GetObjectError, primitives::ByteStream, Client,
};
use clap::Args;
use flate2::read::GzDecoder;
use leafage_evm_types::{
    BlockInfo, BlockStorageDiff, BundleStorageDiffIndex, STATE_DIFF_ENTRY_CAPACITY,
    STATE_DIFF_INDEX_BYTES,
};
use std::{fmt, future::Future, io::Read};
use tokio::{io::AsyncReadExt, time::sleep};
use tracing::warn;

const BUNDLE_SIZE: u64 = 1_000;
const MEBIBYTE_BYTES: u64 = 1024 * 1024;
const DEFAULT_BUNDLE_RANGE_SIZE_MIB: u32 = 32;
const BODY_READ_MAX_ATTEMPTS: u32 = 3;
#[cfg(not(test))]
const BODY_READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
const BODY_READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

/// A bundle was fetched and decoded successfully, but its Header and
/// StateDiff roots do not describe one continuous state transition.
///
/// Callers that require fail-closed input handling can downcast this error;
/// transport, body-read, and decode failures deliberately keep their existing
/// error types so they remain retryable.
#[derive(Debug)]
pub(crate) struct BundleIntegrityError {
    reason: String,
}

impl BundleIntegrityError {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for BundleIntegrityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for BundleIntegrityError {}

#[derive(Debug, Clone, Copy, Args)]
pub(crate) struct BundleReadArgs {
    /// Target size limit, in MiB, when combining multiple StateDiff entries
    /// into one S3 Range request. A larger single entry is still read alone.
    #[arg(
        long = "bundle-range-size",
        default_value_t = DEFAULT_BUNDLE_RANGE_SIZE_MIB,
        value_name = "MIB"
    )]
    pub(crate) bundle_range_size_mib: u32,
}

pub(crate) fn bundle_end(block_number: u64) -> u64 {
    let bundle_id = bundle_id(block_number);
    if bundle_id == 0 {
        0
    } else {
        bundle_id.saturating_mul(BUNDLE_SIZE)
    }
}

/// Read and process the requested part of one bundle.
///
/// `Ok(None)` means the StateDiff bundle object does not exist. Only that
/// definitive miss should make callers switch to the legacy per-block path.
/// A successful read returns the last processed Header so callers can reuse
/// its state root at the bundle-to-source boundary.
pub(crate) async fn s3_read_bundle<F, Fut>(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    start_block: u64,
    end_block: u64,
    bundle_range_size_mib: u32,
    mut process_block: F,
) -> Result<Option<BlockInfo>>
where
    F: FnMut(BlockInfo, BlockStorageDiff) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if start_block > end_block {
        bail!("bundle start block {start_block} is greater than end block {end_block}");
    }

    let bundle_id = bundle_id(start_block);
    if self::bundle_id(end_block) != bundle_id {
        bail!("block range {start_block}..={end_block} crosses a bundle boundary");
    }

    let state_diff_key = bundle_key(s3_chain_id, version, bundle_id, "stateDiff");
    let Some((index_bytes, state_diff_total_size)) = read_s3_range(
        s3_client,
        bucket_name,
        &state_diff_key,
        0,
        STATE_DIFF_INDEX_BYTES as u64 - 1,
        true,
    )
    .await
    .with_context(|| format!("read StateDiff bundle index s3://{bucket_name}/{state_diff_key}"))?
    else {
        return Ok(None);
    };
    let index = BundleStorageDiffIndex::decode_for_bundle(bundle_id, &index_bytes)
        .with_context(|| format!("decode StateDiff bundle {bundle_id} index"))?;
    let expected_state_diff_size = (STATE_DIFF_INDEX_BYTES as u64)
        .checked_add(
            u64::try_from(index.payload_size()?)
                .context("StateDiff bundle payload size does not fit u64")?,
        )
        .context("StateDiff bundle object size overflow")?;
    if state_diff_total_size != expected_state_diff_size {
        bail!(
            "StateDiff bundle {bundle_id} object size is {state_diff_total_size}, expected {expected_state_diff_size} from its index"
        );
    }

    // pipeline-compactor writes the header before the StateDiff object. Once
    // the index above exists, a missing header is an incomplete/corrupt bundle,
    // not the boundary at which callers should fall back to source objects.
    let header_key = bundle_key(s3_chain_id, version, bundle_id, "block");
    let header_bytes = read_s3_object(s3_client, bucket_name, &header_key)
        .await
        .with_context(|| format!("read Header bundle s3://{bucket_name}/{header_key}"))?;
    let headers = decode_bundle_headers(bundle_id, &header_bytes)?;

    let start_position = bundle_position(start_block);
    let end_position = bundle_position(end_block);
    let bundle_range_size_bytes = u64::from(bundle_range_size_mib) * MEBIBYTE_BYTES;
    let mut position = start_position;
    let mut last_block_info = None;
    while position <= end_position {
        let range_end =
            state_diff_range_end(&index, position, end_position, bundle_range_size_bytes)?;
        let (payload_start, _) = index.payload_range(position)?;
        let (_, payload_end) = index.payload_range(range_end)?;
        let object_start = (STATE_DIFF_INDEX_BYTES as u64)
            .checked_add(payload_start)
            .context("StateDiff bundle range start overflow")?;
        let object_end_exclusive = (STATE_DIFF_INDEX_BYTES as u64)
            .checked_add(payload_end)
            .context("StateDiff bundle range end overflow")?;
        let object_end = object_end_exclusive
            .checked_sub(1)
            .context("StateDiff bundle entry has an empty payload")?;
        let expected_len = usize::try_from(payload_end - payload_start)
            .context("StateDiff bundle range length does not fit usize")?;

        let (bytes, response_total_size) = read_s3_range(
            s3_client,
            bucket_name,
            &state_diff_key,
            object_start,
            object_end,
            false,
        )
            .await
            .with_context(|| {
                format!(
                    "read StateDiff bundle s3://{bucket_name}/{state_diff_key} range bytes={object_start}-{object_end}"
                )
            })?
            .context("StateDiff bundle disappeared after its index was read")?;
        if bytes.len() != expected_len {
            bail!(
                "StateDiff bundle {bundle_id} range bytes={object_start}-{object_end} returned {} bytes, expected {expected_len}",
                bytes.len()
            );
        }
        if response_total_size != state_diff_total_size {
            bail!(
                "StateDiff bundle {bundle_id} changed size between range reads: index reported {state_diff_total_size}, payload reported {response_total_size}"
            );
        }

        for entry_position in position..=range_end {
            let (entry_start, entry_end) = index.payload_range(entry_position)?;
            let local_start = usize::try_from(entry_start - payload_start)
                .context("StateDiff entry start does not fit usize")?;
            let local_end = usize::try_from(entry_end - payload_start)
                .context("StateDiff entry end does not fit usize")?;
            let mut entry_bytes = &bytes[local_start..local_end];
            let block_diff = BlockStorageDiff::decode(&mut entry_bytes).with_context(|| {
                format!("decode StateDiff bundle {bundle_id} entry {entry_position}")
            })?;
            if !entry_bytes.is_empty() {
                bail!(
                    "StateDiff bundle {bundle_id} entry {entry_position} has {} trailing bytes",
                    entry_bytes.len()
                );
            }

            let block_info = headers[entry_position].clone();
            if block_diff.hash != block_info.header.state_root {
                return Err(BundleIntegrityError::new(format!(
                    "StateDiff bundle {bundle_id} entry {entry_position} root {} does not match Header root {}",
                    block_diff.hash,
                    block_info.header.state_root
                ))
                .into());
            }
            if entry_position > 0 {
                let expected_parent_root = headers[entry_position - 1].header.state_root;
                if block_diff.parent_hash != expected_parent_root {
                    return Err(BundleIntegrityError::new(format!(
                        "StateDiff bundle {bundle_id} entry {entry_position} parent root {} does not match previous Header root {}",
                        block_diff.parent_hash,
                        expected_parent_root
                    ))
                    .into());
                }
            }

            process_block(block_info.clone(), block_diff)
                .await
                .with_context(|| format!("process bundle {bundle_id} entry {entry_position}"))?;
            last_block_info = Some(block_info);
        }

        position = range_end + 1;
    }

    Ok(last_block_info)
}

async fn read_s3_range(
    s3_client: &Client,
    bucket_name: &str,
    key: &str,
    start: u64,
    end: u64,
    not_found_is_none: bool,
) -> Result<Option<(Vec<u8>, u64)>> {
    let byte_range = format!("bytes={start}-{end}");
    let expected_len = usize::try_from(
        end.checked_sub(start)
            .and_then(|length| length.checked_add(1))
            .context("invalid S3 byte range")?,
    )
    .context("S3 byte range length does not fit usize")?;

    for attempt in 1..=BODY_READ_MAX_ATTEMPTS {
        let object = match s3_client
            .get_object()
            .bucket(bucket_name)
            .key(key)
            .range(&byte_range)
            .send()
            .await
        {
            Ok(output) => output,
            Err(error) if not_found_is_none && is_not_found(&error) => return Ok(None),
            Err(error)
                if error.as_service_error().is_none() && attempt < BODY_READ_MAX_ATTEMPTS =>
            {
                warn!(
                    "get s3://{bucket_name}/{key} range {byte_range} failed (attempt {attempt}/{BODY_READ_MAX_ATTEMPTS}): {error}; retrying"
                );
                sleep(BODY_READ_RETRY_DELAY).await;
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("get s3://{bucket_name}/{key} range {byte_range}"));
            }
        };
        let total_size =
            validate_range_response(object.content_length(), object.content_range(), start, end)
                .with_context(|| format!("validate s3://{bucket_name}/{key} range {byte_range}"))?;

        match collect_range_body(object.body, expected_len).await {
            Ok(bytes) => return Ok(Some((bytes, total_size))),
            Err(error) if attempt < BODY_READ_MAX_ATTEMPTS => {
                warn!(
                    "read s3://{bucket_name}/{key} range {byte_range} body failed (attempt {attempt}/{BODY_READ_MAX_ATTEMPTS}): {error:#}; retrying"
                );
                sleep(BODY_READ_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the finite range read loop always returns")
}

async fn read_s3_object(s3_client: &Client, bucket_name: &str, key: &str) -> Result<Vec<u8>> {
    for attempt in 1..=BODY_READ_MAX_ATTEMPTS {
        let object = match s3_client
            .get_object()
            .bucket(bucket_name)
            .key(key)
            .send()
            .await
        {
            Ok(output) => output,
            Err(error)
                if error.as_service_error().is_none() && attempt < BODY_READ_MAX_ATTEMPTS =>
            {
                warn!(
                    "get s3://{bucket_name}/{key} failed (attempt {attempt}/{BODY_READ_MAX_ATTEMPTS}): {error}; retrying"
                );
                sleep(BODY_READ_RETRY_DELAY).await;
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("get s3://{bucket_name}/{key}"));
            }
        };
        match object.body.collect().await {
            Ok(bytes) => return Ok(bytes.into_bytes().to_vec()),
            Err(error) if attempt < BODY_READ_MAX_ATTEMPTS => {
                warn!(
                    "read s3://{bucket_name}/{key} body failed (attempt {attempt}/{BODY_READ_MAX_ATTEMPTS}): {error}; retrying"
                );
                sleep(BODY_READ_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("the finite object read loop always returns")
}

fn bundle_id(block_number: u64) -> u64 {
    if block_number == 0 {
        0
    } else {
        (block_number - 1) / BUNDLE_SIZE + 1
    }
}

fn bundle_position(block_number: u64) -> usize {
    if block_number == 0 {
        0
    } else {
        ((block_number - 1) % BUNDLE_SIZE) as usize
    }
}

fn bundle_key(s3_chain_id: &str, version: &str, bundle_id: u64, resource: &str) -> String {
    if version.is_empty() {
        format!("{s3_chain_id}/{bundle_id}/{resource}")
    } else {
        format!("{s3_chain_id}/{version}/{bundle_id}/{resource}")
    }
}

fn is_not_found(error: &SdkError<GetObjectError>) -> bool {
    error.as_service_error().is_some_and(|error| {
        error.is_no_such_key() || matches!(error.meta().code(), Some("NoSuchKey" | "NotFound"))
    })
}

fn validate_range_response(
    content_length: Option<i64>,
    content_range: Option<&str>,
    expected_start: u64,
    expected_end: u64,
) -> Result<u64> {
    let expected_len = expected_end
        .checked_sub(expected_start)
        .and_then(|length| length.checked_add(1))
        .context("invalid expected S3 byte range")?;
    if content_length != i64::try_from(expected_len).ok() {
        bail!("range response Content-Length is {content_length:?}, expected {expected_len}");
    }

    let content_range = content_range.context("range response is missing Content-Range")?;
    let (returned_range, total_size) = content_range
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .with_context(|| format!("invalid Content-Range {content_range:?}"))?;
    let (returned_start, returned_end) = returned_range
        .split_once('-')
        .with_context(|| format!("invalid Content-Range {content_range:?}"))?;
    let returned_start: u64 = returned_start
        .parse()
        .with_context(|| format!("invalid Content-Range start in {content_range:?}"))?;
    let returned_end: u64 = returned_end
        .parse()
        .with_context(|| format!("invalid Content-Range end in {content_range:?}"))?;
    let total_size: u64 = total_size
        .parse()
        .with_context(|| format!("invalid Content-Range total in {content_range:?}"))?;
    if returned_start != expected_start || returned_end != expected_end {
        bail!(
            "range response returned bytes {returned_start}-{returned_end}, expected {expected_start}-{expected_end}"
        );
    }
    if total_size <= returned_end {
        bail!("range response total size {total_size} does not include byte {returned_end}");
    }
    Ok(total_size)
}

async fn collect_range_body(body: ByteStream, expected_len: usize) -> Result<Vec<u8>> {
    let read_limit = u64::try_from(expected_len)
        .context("range length does not fit u64")?
        .checked_add(1)
        .context("range read limit overflow")?;
    let mut reader = body.into_async_read().take(read_limit);
    let mut bytes = Vec::with_capacity(expected_len);
    reader.read_to_end(&mut bytes).await?;
    if bytes.len() != expected_len {
        bail!(
            "range response body is {} bytes, expected {expected_len}",
            bytes.len()
        );
    }
    Ok(bytes)
}

fn decode_bundle_headers(bundle_id: u64, bytes: &[u8]) -> Result<Vec<BlockInfo>> {
    let mut decoder = GzDecoder::new(bytes);
    let mut json = Vec::new();
    decoder
        .read_to_end(&mut json)
        .with_context(|| format!("decompress Header bundle {bundle_id}"))?;
    let headers: Vec<BlockInfo> = serde_json::from_slice(&json)
        .with_context(|| format!("decode Header bundle {bundle_id}"))?;
    let expected_count = if bundle_id == 0 {
        1
    } else {
        STATE_DIFF_ENTRY_CAPACITY
    };
    if headers.len() != expected_count {
        bail!(
            "Header bundle {bundle_id} contains {} entries, expected {expected_count}",
            headers.len()
        );
    }

    let first_block = if bundle_id == 0 {
        0
    } else {
        (bundle_id - 1)
            .checked_mul(BUNDLE_SIZE)
            .and_then(|height| height.checked_add(1))
            .context("Header bundle height overflow")?
    };
    for (position, header) in headers.iter().enumerate() {
        let expected_number = first_block
            .checked_add(position as u64)
            .context("Header bundle height overflow")?;
        if header.header.number != expected_number {
            bail!(
                "Header bundle {bundle_id} entry {position} has block number {}, expected {expected_number}",
                header.header.number
            );
        }
        if position > 0 && header.header.parent_hash != headers[position - 1].header.hash {
            bail!(
                "Header bundle {bundle_id} entry {position} parent {} does not match previous Header hash {}",
                header.header.parent_hash,
                headers[position - 1].header.hash
            );
        }
    }
    Ok(headers)
}

fn state_diff_range_end(
    index: &BundleStorageDiffIndex,
    start_position: usize,
    end_position: usize,
    group_range_limit_bytes: u64,
) -> Result<usize> {
    let (payload_start, _) = index.payload_range(start_position)?;
    let mut range_end = start_position;
    for position in start_position + 1..=end_position {
        let (_, payload_end) = index.payload_range(position)?;
        // The limit only bounds ranges that combine multiple entries. A single
        // entry may itself be larger and must still be fetched on its own.
        if payload_end - payload_start > group_range_limit_bytes {
            break;
        }
        range_end = position;
    }
    Ok(range_end)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_rlp::Encodable;
    use aws_sdk_s3::config::{Credentials, Region};
    use axum::{
        body::{Body, Bytes},
        extract::State,
        http::{Request, Response, StatusCode},
        Router,
    };
    use flate2::{write::GzEncoder, Compression};
    use leafage_evm_types::H256;
    use std::{
        collections::HashMap,
        io::Write,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    const BITMAP_BYTES: usize = 125;
    type RecordedRequests = Arc<Mutex<Vec<(String, Option<String>)>>>;

    #[derive(Clone, Default)]
    struct MockS3 {
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        requests: RecordedRequests,
        missing_error_code: Option<&'static str>,
        ignore_range: bool,
        truncate_range_bodies: Arc<AtomicUsize>,
    }

    async fn mock_s3_get(State(state): State<MockS3>, request: Request<Body>) -> Response<Body> {
        let path = request.uri().path().trim_start_matches('/');
        let (_, key) = path.split_once('/').unwrap_or(("", path));
        let range = request
            .headers()
            .get("range")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        state
            .requests
            .lock()
            .unwrap()
            .push((key.to_owned(), range.clone()));

        let Some(data) = state.objects.lock().unwrap().get(key).cloned() else {
            let code = state.missing_error_code.unwrap_or("NoSuchKey");
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/xml")
                .body(Body::from(format!(
                    "<Error><Code>{code}</Code><Message>missing</Message></Error>"
                )))
                .unwrap();
        };

        let (status, mut body, content_range) = match (range, state.ignore_range) {
            (Some(_), true) => (StatusCode::OK, data, None),
            (Some(range), false) => {
                let range = range.strip_prefix("bytes=").unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse().unwrap();
                (
                    StatusCode::PARTIAL_CONTENT,
                    data[start..=end].to_vec(),
                    Some(format!("bytes {start}-{end}/{}", data.len())),
                )
            }
            (None, _) => (StatusCode::OK, data, None),
        };
        let advertised_content_length = body.len();
        let truncate_body = content_range.is_some()
            && state
                .truncate_range_bodies
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        if truncate_body {
            body.pop();
        }
        let mut response = Response::builder()
            .status(status)
            .header("content-length", advertised_content_length);
        if let Some(content_range) = content_range {
            response = response.header("content-range", content_range);
        }
        if truncate_body {
            let body = futures::stream::iter([
                Ok(Bytes::from(body)),
                Err(std::io::Error::other("truncated mock response")),
            ]);
            response.body(Body::from_stream(body)).unwrap()
        } else {
            response.body(Body::from(body)).unwrap()
        }
    }

    async fn mock_client(state: MockS3) -> (Client, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(mock_s3_get).with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("test", "test", None, None, "test"))
            .endpoint_url(format!("http://{address}"))
            .force_path_style(true)
            .build();
        (Client::from_conf(config), server)
    }

    fn genesis_bundle_objects() -> HashMap<String, Vec<u8>> {
        let block = BlockInfo::default();
        let state_root = block.header.state_root;
        let mut header_encoder = GzEncoder::new(Vec::new(), Compression::default());
        header_encoder
            .write_all(&serde_json::to_vec(&vec![block]).unwrap())
            .unwrap();
        let header = header_encoder.finish().unwrap();

        let diff = BlockStorageDiff {
            hash: state_root,
            ..Default::default()
        };
        let mut payload = Vec::new();
        diff.encode(&mut payload);
        let mut state_diff = vec![0; STATE_DIFF_INDEX_BYTES];
        state_diff[0] = 1;
        let offset_one = BITMAP_BYTES + 8;
        state_diff[offset_one..offset_one + 8]
            .copy_from_slice(&(payload.len() as u64).to_be_bytes());
        state_diff.extend_from_slice(&payload);

        HashMap::from([
            ("1/0/block".to_owned(), header),
            ("1/0/stateDiff".to_owned(), state_diff),
        ])
    }

    fn test_hash(value: u64) -> H256 {
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&value.to_be_bytes());
        H256::from(bytes)
    }

    fn full_bundle_objects() -> HashMap<String, Vec<u8>> {
        let mut headers = Vec::with_capacity(STATE_DIFF_ENTRY_CAPACITY);
        let mut state_diff = vec![0xff; STATE_DIFF_INDEX_BYTES];
        state_diff[BITMAP_BYTES..].fill(0);
        let mut payload = Vec::new();

        for position in 0..STATE_DIFF_ENTRY_CAPACITY {
            let number = position as u64 + 1;
            let mut block = BlockInfo::default();
            block.header.number = number;
            block.header.hash = test_hash(number);
            block.header.parent_hash = test_hash(number - 1);
            block.header.state_root = test_hash(10_000 + number);
            headers.push(block.clone());

            let diff = BlockStorageDiff {
                hash: block.header.state_root,
                parent_hash: test_hash(10_000 + number - 1),
                ..Default::default()
            };
            diff.encode(&mut payload);
            let offset_start = BITMAP_BYTES + (position + 1) * 8;
            state_diff[offset_start..offset_start + 8]
                .copy_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        state_diff.extend_from_slice(&payload);

        let mut header_encoder = GzEncoder::new(Vec::new(), Compression::default());
        header_encoder
            .write_all(&serde_json::to_vec(&headers).unwrap())
            .unwrap();
        let header = header_encoder.finish().unwrap();

        HashMap::from([
            ("1/1/block".to_owned(), header),
            ("1/1/stateDiff".to_owned(), state_diff),
        ])
    }

    fn index_with_sizes(sizes: &[u64]) -> BundleStorageDiffIndex {
        let mut bytes = vec![0; STATE_DIFF_INDEX_BYTES];
        let mut offset = 0_u64;
        for position in 0..STATE_DIFF_ENTRY_CAPACITY {
            let size = sizes.get(position).copied().unwrap_or(1);
            offset += size;
            let start = BITMAP_BYTES + (position + 1) * 8;
            bytes[start..start + 8].copy_from_slice(&offset.to_be_bytes());
        }
        BundleStorageDiffIndex::decode(&bytes).unwrap()
    }

    #[test]
    fn maps_block_numbers_to_pipeline_compactor_bundles() {
        let cases = [
            (0, 0, 0, 0),
            (1, 1, 0, 1_000),
            (1_000, 1, 999, 1_000),
            (1_001, 2, 0, 2_000),
            (2_000, 2, 999, 2_000),
        ];
        for (height, expected_id, expected_position, expected_end) in cases {
            assert_eq!(bundle_id(height), expected_id);
            assert_eq!(bundle_position(height), expected_position);
            assert_eq!(bundle_end(height), expected_end);
        }
    }

    #[test]
    fn selects_state_diff_ranges() {
        let limit = 4;
        let index = index_with_sizes(&[limit + 1, limit / 2, limit / 2, 1]);

        assert_eq!(state_diff_range_end(&index, 0, 3, limit).unwrap(), 0);
        assert_eq!(state_diff_range_end(&index, 1, 3, limit).unwrap(), 2);
        assert_eq!(state_diff_range_end(&index, 3, 3, limit).unwrap(), 3);
    }

    #[tokio::test]
    async fn reads_genesis_with_index_and_payload_ranges() {
        let state = MockS3 {
            objects: Arc::new(Mutex::new(genesis_bundle_objects())),
            ..Default::default()
        };
        let requests = state.requests.clone();
        let (client, server) = mock_client(state).await;
        let seen = Arc::new(Mutex::new(Vec::new()));

        let found = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            0,
            0,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            {
                let seen = seen.clone();
                move |block_info, block_diff| {
                    let seen = seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push((block_info.header.number, block_diff.hash));
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(found.unwrap().header.number, 0);
        assert_eq!(seen.lock().unwrap().len(), 1);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0],
            ("1/0/stateDiff".to_owned(), Some("bytes=0-8132".to_owned()))
        );
        assert_eq!(requests[1], ("1/0/block".to_owned(), None));
        assert!(requests[2].1.as_deref().unwrap().starts_with("bytes=8133-"));
        server.abort();
    }

    #[tokio::test]
    async fn decoded_root_mismatch_has_typed_integrity_error() {
        let mut objects = genesis_bundle_objects();
        let mut mismatched_header = BlockInfo::default();
        mismatched_header.header.state_root = test_hash(99);
        let mut header_encoder = GzEncoder::new(Vec::new(), Compression::default());
        header_encoder
            .write_all(&serde_json::to_vec(&vec![mismatched_header]).unwrap())
            .unwrap();
        objects.insert("1/0/block".to_owned(), header_encoder.finish().unwrap());
        let state = MockS3 {
            objects: Arc::new(Mutex::new(objects)),
            ..Default::default()
        };
        let (client, server) = mock_client(state).await;

        let error = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            0,
            0,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap_err();

        assert!(error.downcast_ref::<BundleIntegrityError>().is_some());
        server.abort();
    }

    #[tokio::test]
    async fn decoded_parent_root_mismatch_has_typed_integrity_error() {
        let mut objects = full_bundle_objects();
        let state_diff = objects.get_mut("1/1/stateDiff").unwrap();
        let index = BundleStorageDiffIndex::decode(&state_diff[..STATE_DIFF_INDEX_BYTES]).unwrap();
        let (entry_start, entry_end) = index.payload_range(1).unwrap();
        let entry_start = STATE_DIFF_INDEX_BYTES + entry_start as usize;
        let entry_end = STATE_DIFF_INDEX_BYTES + entry_end as usize;
        let mut bytes = &state_diff[entry_start..entry_end];
        let mut block_diff = BlockStorageDiff::decode(&mut bytes).unwrap();
        block_diff.parent_hash = test_hash(99_999);
        let mut encoded = Vec::new();
        block_diff.encode(&mut encoded);
        assert_eq!(encoded.len(), entry_end - entry_start);
        state_diff[entry_start..entry_end].copy_from_slice(&encoded);

        let state = MockS3 {
            objects: Arc::new(Mutex::new(objects)),
            ..Default::default()
        };
        let (client, server) = mock_client(state).await;
        let error = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            2,
            2,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap_err();

        assert!(error.downcast_ref::<BundleIntegrityError>().is_some());
        server.abort();
    }

    #[tokio::test]
    async fn reads_a_partial_full_bundle_with_an_exact_payload_range() {
        let objects = full_bundle_objects();
        let state_diff = &objects["1/1/stateDiff"];
        let index = BundleStorageDiffIndex::decode(&state_diff[..STATE_DIFF_INDEX_BYTES]).unwrap();
        let (payload_start, _) = index.payload_range(9).unwrap();
        let (_, payload_end) = index.payload_range(11).unwrap();
        let expected_range = format!(
            "bytes={}-{}",
            STATE_DIFF_INDEX_BYTES as u64 + payload_start,
            STATE_DIFF_INDEX_BYTES as u64 + payload_end - 1
        );
        let state = MockS3 {
            objects: Arc::new(Mutex::new(objects)),
            ..Default::default()
        };
        let requests = state.requests.clone();
        let (client, server) = mock_client(state).await;
        let seen = Arc::new(Mutex::new(Vec::new()));

        let last = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            10,
            12,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            {
                let seen = seen.clone();
                move |block_info, _| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(block_info.header.number);
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(last.header.number, 12);
        assert_eq!(*seen.lock().unwrap(), vec![10, 11, 12]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].1.as_deref(), Some(expected_range.as_str()));
        server.abort();
    }

    #[tokio::test]
    async fn retries_a_truncated_range_body_before_processing_entries() {
        let state = MockS3 {
            objects: Arc::new(Mutex::new(genesis_bundle_objects())),
            truncate_range_bodies: Arc::new(AtomicUsize::new(1)),
            ..Default::default()
        };
        let requests = state.requests.clone();
        let (client, server) = mock_client(state).await;

        let last = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            0,
            0,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(last.header.number, 0);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0], requests[1]);
        server.abort();
    }

    #[tokio::test]
    async fn returns_not_found_without_reading_header() {
        let state = MockS3::default();
        let requests = state.requests.clone();
        let (client, server) = mock_client(state).await;

        let found = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            1,
            1,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            |_, _| async { unreachable!() },
        )
        .await
        .unwrap();

        assert!(found.is_none());
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn does_not_treat_no_such_bucket_as_bundle_miss() {
        let state = MockS3 {
            missing_error_code: Some("NoSuchBucket"),
            ..Default::default()
        };
        let (client, server) = mock_client(state).await;

        let error = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            1,
            1,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            |_, _| async { unreachable!() },
        )
        .await
        .unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("NoSuchBucket"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn rejects_a_server_that_ignores_range() {
        let state = MockS3 {
            objects: Arc::new(Mutex::new(genesis_bundle_objects())),
            ignore_range: true,
            ..Default::default()
        };
        let (client, server) = mock_client(state).await;

        let error = s3_read_bundle(
            &client,
            "bundle",
            "1",
            "",
            0,
            0,
            DEFAULT_BUNDLE_RANGE_SIZE_MIB,
            |_, _| async { unreachable!() },
        )
        .await
        .unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("Content-Length"), "{error}");
        server.abort();
    }
}
