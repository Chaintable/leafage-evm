use anyhow::{Context, Result};
use aws_sdk_s3::{operation::list_objects_v2::ListObjectsV2Output, Client};
use bytes::Bytes;
use std::{future::Future, num::NonZeroU64, time::Duration};

/// Keep the per-object deadline with the client, including across task clones.
#[derive(Clone)]
pub(crate) struct S3Reader {
    client: Client,
    read_timeout: Duration,
}

impl S3Reader {
    pub(crate) fn new(client: Client, timeout_secs: NonZeroU64) -> Self {
        Self {
            client,
            read_timeout: Duration::from_secs(timeout_secs.get()),
        }
    }

    /// Bundle range downloads use their existing, separate reader.
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) async fn get_bytes(&self, bucket: &str, key: &str) -> Result<Bytes> {
        let mut stage = "send";
        with_read_timeout(self.read_timeout, async {
            let object = self
                .client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await?;
            stage = "body";
            Ok(object.body.collect().await?.into_bytes())
        })
        .await
        .with_context(|| format!("S3 GET s3://{bucket}/{key}, stage={stage}"))
    }

    pub(crate) async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<ListObjectsV2Output> {
        with_read_timeout(self.read_timeout, async {
            Ok(self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .send()
                .await?)
        })
        .await
        .with_context(|| format!("S3 LIST s3://{bucket}/{prefix}, stage=send"))
    }
}

async fn with_read_timeout<T>(
    duration: Duration,
    read: impl Future<Output = Result<T>>,
) -> Result<T> {
    let start = tokio::time::Instant::now();
    // Own the request future: timing out a JoinHandle alone would detach it.
    // One deadline covers SDK retries AND body consumption after send returns.
    tokio::time::timeout(duration, read)
        .await
        .with_context(|| {
            format!(
                "S3 read timed out after {:?} (limit {:?})",
                start.elapsed(),
                duration
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::config::{retry::RetryConfig, Credentials, Region};
    use std::{
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinSet,
    };

    const LIMIT: Duration = Duration::from_millis(200);

    #[derive(Clone, Copy)]
    enum Reply {
        Pending,
        PartialBody,
        SlowBody,
        Close,
        Complete,
    }

    struct Stub {
        reader: S3Reader,
        requests: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn stub(first: Reply) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let count = count.clone();
                connections.spawn(async move {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if socket.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        request.extend(byte);
                    }
                    let reply = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        first
                    } else {
                        Reply::Complete
                    };
                    match reply {
                        Reply::Pending => std::future::pending::<()>().await,
                        Reply::PartialBody => {
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nx").await.unwrap();
                            std::future::pending::<()>().await;
                        }
                        Reply::Close => {}
                        Reply::SlowBody => {
                            tokio::time::sleep(Duration::from_millis(120)).await;
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nx").await.unwrap();
                            tokio::time::sleep(Duration::from_millis(120)).await;
                            let _ = socket.write_all(b"ata").await;
                        }
                        Reply::Complete => {
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata").await.unwrap();
                        }
                    }
                });
            }
        });
        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("test", "test", None, None, "test"))
            .endpoint_url(format!("http://{address}"))
            .force_path_style(true)
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .build();
        Stub {
            reader: S3Reader {
                client: Client::from_conf(config),
                read_timeout: LIMIT,
            },
            requests,
            task,
        }
    }

    async fn get_error(stub: &Stub) -> String {
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            stub.reader.get_bytes("source", "block"),
        )
        .await
        .expect("GET must finish")
        .unwrap_err();
        assert!(stub.requests.load(Ordering::SeqCst) > 0);
        format!("{error:#}")
    }

    #[tokio::test]
    async fn get_timeout_before_response_then_retry_succeeds() {
        let stub = stub(Reply::Pending).await;
        let error = get_error(&stub).await;
        assert!(error.contains("S3 read timed out"), "{error}");
        assert!(error.contains("s3://source/block, stage=send"), "{error}");
        let bytes = stub.reader.get_bytes("source", "block").await.unwrap();
        assert_eq!(bytes.as_ref(), b"data");
    }

    #[tokio::test]
    async fn get_timeout_in_body_then_retry_succeeds() {
        let stub = stub(Reply::PartialBody).await;
        let error = get_error(&stub).await;
        assert!(error.contains("S3 read timed out"), "{error}");
        assert!(error.contains("stage=body"), "{error}");
        assert_eq!(
            stub.reader
                .get_bytes("source", "block")
                .await
                .unwrap()
                .as_ref(),
            b"data"
        );
    }

    #[tokio::test]
    async fn connection_close_returns_error_then_retry_succeeds() {
        let stub = stub(Reply::Close).await;
        let error = get_error(&stub).await;
        assert!(error.contains("stage=send"), "{error}");
        assert_eq!(
            stub.reader
                .get_bytes("source", "block")
                .await
                .unwrap()
                .as_ref(),
            b"data"
        );
    }

    #[tokio::test]
    async fn deadline_covers_send_and_body_together_and_is_configurable() {
        let short = stub(Reply::SlowBody).await;
        let error = get_error(&short).await;
        assert!(error.contains("S3 read timed out"), "{error}");
        assert!(error.contains("stage=body"), "{error}");

        let long = stub(Reply::SlowBody).await;
        let reader = S3Reader::new(long.reader.client().clone(), NonZeroU64::new(1).unwrap());
        assert_eq!(
            reader
                .clone()
                .get_bytes("source", "block")
                .await
                .unwrap()
                .as_ref(),
            b"xata"
        );
    }

    #[tokio::test]
    async fn list_timeout_includes_prefix() {
        let stub = stub(Reply::Pending).await;
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            stub.reader.list_objects("source", "4663/7/"),
        )
        .await
        .expect("LIST must finish")
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("S3 LIST s3://source/4663/7/"), "{error}");
        assert!(error.contains("S3 read timed out"), "{error}");
        assert_eq!(stub.requests.load(Ordering::SeqCst), 1);
    }

    struct DropNotice(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropNotice {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    #[tokio::test]
    async fn deadline_and_batch_exit_drop_pending_requests() {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let notice = DropNotice(Some(done_tx));
        let error = with_read_timeout(LIMIT, async move {
            let _notice = notice;
            std::future::pending::<Result<()>>().await
        })
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("S3 read timed out"));
        tokio::time::timeout(Duration::from_secs(3), done_rx)
            .await
            .unwrap()
            .unwrap();

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let notice = DropNotice(Some(done_tx));
        let mut batch = JoinSet::new();
        batch.spawn(async move {
            let _notice = notice;
            started_tx.send(()).unwrap();
            std::future::pending::<Result<()>>().await
        });
        started_rx.await.unwrap();
        batch.spawn(async { Err::<(), _>(io::Error::other("download failed").into()) });
        assert!(batch.join_next().await.unwrap().unwrap().is_err());
        drop(batch);
        tokio::time::timeout(Duration::from_secs(3), done_rx)
            .await
            .unwrap()
            .unwrap();
    }
}
