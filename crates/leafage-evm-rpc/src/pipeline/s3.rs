//! The `pipeline-compactor` target objects on S3: `{chain_id}[/{version}]/{b}`
//! and its `/validation`, `/block` and `/stateDiff` objects.

use super::bundle::Resource;
use anyhow::{Context as _, Result};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use bytes::Bytes;
use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::warn;

/// Attempts at one S3 request before the bundle fails.
const ATTEMPTS: u32 = 10;
const RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// User metadata on every uploaded object, so that a rerun can tell the
/// objects it uploaded from those of other producers, e.g. the ones being
/// replaced.
const PRODUCER_KEY: &str = "producer";
const PRODUCER: &str = "leafage-pipeline-export";
/// User metadata naming the export of the bundle an object belongs to: the
/// objects of one bundle are only complete when they all name the same, an
/// interrupted rerun may have replaced some of them.
const EXPORT_KEY: &str = "export";

/// Where and how bundles are uploaded.
pub struct S3Target {
    pub client: Client,
    /// `{chain_id}` or `{chain_id}/{version}`.
    pub prefix: String,
    /// Bucket of the block file and validation bundles.
    pub chain_table_bucket: Option<String>,
    /// Bucket of the header and state diff bundles.
    pub nodex_bucket: Option<String>,
    /// Multipart part size of the block file and state diff bundles; at
    /// least 5 MiB.
    pub part_size: usize,
    /// Parts of one object uploaded at the same time.
    pub concurrency: usize,
}

/// The SDK's own `Display` leaves out the cause ("service error").
fn sdk_error<E: std::error::Error + Send + Sync + 'static>(err: E) -> anyhow::Error {
    anyhow::anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(err))
}

/// Run `request` until it succeeds, at most [`ATTEMPTS`] times.
async fn retry<T, F, Fut>(what: &str, mut request: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1;
    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < ATTEMPTS => {
                warn!(target: "export", attempt, "retrying {what}: {err:#}");
                attempt += 1;
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
            Err(err) => return Err(err.context(what.to_string())),
        }
    }
}

impl S3Target {
    /// Bucket and key of `resource` of bundle `id`.
    pub(crate) fn object(&self, id: u64, resource: Resource) -> Result<(String, String)> {
        let base = format!("{}/{id}", self.prefix);
        let (bucket, key) = match resource {
            Resource::BlockFile => (&self.chain_table_bucket, base),
            Resource::Validation => (&self.chain_table_bucket, format!("{base}/validation")),
            Resource::Header => (&self.nodex_bucket, format!("{base}/block")),
            Resource::StateDiff => (&self.nodex_bucket, format!("{base}/stateDiff")),
        };
        let bucket = bucket
            .clone()
            .with_context(|| format!("no target bucket for the {resource} bundle"))?;
        Ok((bucket, key))
    }

    /// Whether all of `resources` of bundle `id` were uploaded by this tool,
    /// in the same export.
    pub(crate) async fn is_uploaded(
        &self,
        id: u64,
        resources: &BTreeSet<Resource>,
    ) -> Result<bool> {
        let mut export: Option<String> = None;
        for &resource in resources {
            let (bucket, key) = self.object(id, resource)?;
            let head = retry(&format!("head s3://{bucket}/{key}"), || async {
                match self
                    .client
                    .head_object()
                    .bucket(&bucket)
                    .key(&key)
                    .send()
                    .await
                {
                    Ok(head) => Ok(Some(head)),
                    Err(err) if err.as_service_error().is_some_and(|err| err.is_not_found()) => {
                        Ok(None)
                    }
                    Err(err) => Err(sdk_error(err)),
                }
            })
            .await?;
            let Some(metadata) = head.as_ref().and_then(|head| head.metadata()) else {
                return Ok(false);
            };
            if metadata.get(PRODUCER_KEY).map(String::as_str) != Some(PRODUCER) {
                return Ok(false);
            }
            let Some(this) = metadata.get(EXPORT_KEY) else {
                return Ok(false);
            };
            if *export.get_or_insert_with(|| this.clone()) != *this {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Upload `data` as `resource` of bundle `id` in `export`, in parts if
    /// it is large.
    pub(crate) async fn put(
        &self,
        id: u64,
        export: &str,
        resource: Resource,
        data: Bytes,
    ) -> Result<()> {
        if data.len() > self.part_size {
            let mut upload = self.create_multipart(id, export, resource).await?;
            let mut rest = data;
            let result = async {
                while !rest.is_empty() {
                    let part = rest.split_to(self.part_size.min(rest.len()));
                    upload.put_part(part).await?;
                }
                anyhow::Ok(())
            }
            .await;
            return match result {
                Ok(()) => upload.complete().await,
                Err(err) => {
                    upload.abort().await;
                    Err(err)
                }
            };
        }
        let (bucket, key) = self.object(id, resource)?;
        retry(&format!("put s3://{bucket}/{key}"), || async {
            self.client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .metadata(PRODUCER_KEY, PRODUCER)
                .metadata(EXPORT_KEY, export)
                .content_length(data.len() as i64)
                .body(ByteStream::from(data.clone()))
                .send()
                .await
                .map_err(sdk_error)
        })
        .await?;
        Ok(())
    }

    /// Start a multipart upload of `resource` of bundle `id` in `export`.
    pub(crate) async fn create_multipart(
        &self,
        id: u64,
        export: &str,
        resource: Resource,
    ) -> Result<MultipartUpload> {
        let (bucket, key) = self.object(id, resource)?;
        let created = retry(&format!("create upload s3://{bucket}/{key}"), || async {
            self.client
                .create_multipart_upload()
                .bucket(&bucket)
                .key(&key)
                .metadata(PRODUCER_KEY, PRODUCER)
                .metadata(EXPORT_KEY, export)
                .send()
                .await
                .map_err(sdk_error)
        })
        .await?;
        let upload_id = created
            .upload_id()
            .context("no multipart upload id")?
            .to_string();
        Ok(MultipartUpload {
            client: self.client.clone(),
            bucket,
            key,
            upload_id,
            concurrency: self.concurrency,
            next_part: 1,
            pending: JoinSet::new(),
            parts: Vec::new(),
        })
    }
}

/// A multipart upload; parts go up in the background, at most
/// `concurrency` at a time.
pub(crate) struct MultipartUpload {
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    concurrency: usize,
    next_part: i32,
    pending: JoinSet<Result<CompletedPart>>,
    parts: Vec<CompletedPart>,
}

impl MultipartUpload {
    /// Upload the next part: at least 5 MiB unless it is the last one.
    pub(crate) async fn put_part(&mut self, data: Bytes) -> Result<()> {
        while self.pending.len() >= self.concurrency {
            self.collect_one().await?;
        }
        let (client, bucket, key, upload_id) = (
            self.client.clone(),
            self.bucket.clone(),
            self.key.clone(),
            self.upload_id.clone(),
        );
        let number = self.next_part;
        self.next_part += 1;
        self.pending.spawn(async move {
            let what = format!("upload part {number} of s3://{bucket}/{key}");
            let uploaded = retry(&what, || async {
                client
                    .upload_part()
                    .bucket(&bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .part_number(number)
                    .content_length(data.len() as i64)
                    .body(ByteStream::from(data.clone()))
                    .send()
                    .await
                    .map_err(sdk_error)
            })
            .await?;
            Ok(CompletedPart::builder()
                .part_number(number)
                .set_e_tag(uploaded.e_tag)
                .build())
        });
        Ok(())
    }

    async fn collect_one(&mut self) -> Result<()> {
        if let Some(joined) = self.pending.join_next().await {
            self.parts.push(joined??);
        }
        Ok(())
    }

    /// Wait for the parts and make the object visible.
    pub(crate) async fn complete(mut self) -> Result<()> {
        let result = async {
            while !self.pending.is_empty() {
                self.collect_one().await?;
            }
            let mut parts = std::mem::take(&mut self.parts);
            parts.sort_by_key(|part| part.part_number());
            let what = format!("complete upload s3://{}/{}", self.bucket, self.key);
            retry(&what, || async {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(&self.key)
                    .upload_id(&self.upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts.clone()))
                            .build(),
                    )
                    .send()
                    .await
                    .map_err(sdk_error)
            })
            .await
        }
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) => {
                self.abort().await;
                Err(err)
            }
        }
    }

    /// Drop the upload and the parts sent so far.
    pub(crate) async fn abort(mut self) {
        self.pending.abort_all();
        while self.pending.join_next().await.is_some() {}
        let what = format!("abort upload s3://{}/{}", self.bucket, self.key);
        let aborted = retry(&what, || async {
            self.client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.key)
                .upload_id(&self.upload_id)
                .send()
                .await
                .map_err(sdk_error)
        })
        .await;
        if let Err(err) = aborted {
            warn!(target: "export", "{err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through an S3-compatible store at `PIPELINE_EXPORT_S3_ENDPOINT`
    /// with buckets `chain-table` and `nodex`, e.g.
    /// `docker run -p 5055:5055 motoserver/moto -p 5055`.
    #[tokio::test]
    #[ignore = "needs an S3-compatible endpoint"]
    async fn uploads_round_trip() {
        let endpoint = std::env::var("PIPELINE_EXPORT_S3_ENDPOINT").unwrap();
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .build();
        let target = S3Target {
            client: Client::from_conf(config),
            prefix: format!("56/test-{}", std::process::id()),
            chain_table_bucket: Some("chain-table".to_string()),
            nodex_bucket: Some("nodex".to_string()),
            part_size: 5 << 20,
            concurrency: 2,
        };
        let all: BTreeSet<_> = Resource::ALL.into_iter().collect();
        assert!(!target.is_uploaded(7, &all).await.unwrap());

        let small = Bytes::from_static(br#"{"txs_count":1}"#);
        let large: Bytes = (0..(3 << 20) as u32)
            .flat_map(|i| i.to_le_bytes())
            .collect::<Vec<u8>>()
            .into();
        // Streamed in parts, as the block file is.
        let mut upload = target
            .create_multipart(7, "a", Resource::BlockFile)
            .await
            .unwrap();
        for part in large.chunks(5 << 20) {
            upload.put_part(Bytes::copy_from_slice(part)).await.unwrap();
        }
        upload.complete().await.unwrap();
        target
            .put(7, "a", Resource::Validation, small.clone())
            .await
            .unwrap();
        target
            .put(7, "a", Resource::Header, small.clone())
            .await
            .unwrap();
        // Over the part size: split up by `put`.
        target
            .put(7, "a", Resource::StateDiff, large.clone())
            .await
            .unwrap();
        assert!(target.is_uploaded(7, &all).await.unwrap());
        // A rerun that only got as far as the validation.
        target
            .put(7, "b", Resource::Validation, small.clone())
            .await
            .unwrap();
        assert!(!target.is_uploaded(7, &all).await.unwrap());
        let one = [Resource::Validation].into_iter().collect();
        assert!(target.is_uploaded(7, &one).await.unwrap());

        for (resource, expected) in [
            (Resource::BlockFile, &large),
            (Resource::Validation, &small),
            (Resource::Header, &small),
            (Resource::StateDiff, &large),
        ] {
            let (bucket, key) = target.object(7, resource).unwrap();
            let object = target
                .client
                .get_object()
                .bucket(bucket)
                .key(&key)
                .send()
                .await
                .map_err(sdk_error)
                .unwrap();
            let data = object.body.collect().await.unwrap().into_bytes();
            assert!(data == *expected, "{key}");
        }
    }
}
