//! The R2 blob backend: the [`Blobs`](super::blobs::Blobs) trait over an
//! S3-compatible bucket.
//!
//! [`super::turso_store::TursoStore`] used to write the file tree itself;
//! this implementation holds the same `files/<id>` / `thumbs/<id>` layout in
//! a bucket instead — in practice Cloudflare R2, whose endpoint is
//! `https://{account_id}.r2.cloudflarestorage.com` and whose API is S3. The
//! layout carries over untouched, so one backup unit or one `rclone` maps
//! between the local tree and the bucket without translation.
//!
//! The ordering contract is the shared one and load-bearing here too: a blob
//! lands at its key BEFORE the row naming it commits, and a key is deleted
//! only after its row is gone. Never the other way around — a row pointing
//! at nothing is a lie the UI can see.
//!
//! [`object_store`] is the S3 client, and `From<object_store::Error>` for
//! [`BlobError`] lives in this file rather than in blobs.rs, so the trait
//! file stays free of any one backend's dependencies.

use std::path::Path;

use futures_util::StreamExt;
use futures_util::TryStreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{
    GetOptions, GetRange, MultipartUpload, ObjectStore, ObjectStoreExt, PutPayload,
};

use super::FileSpan;
use super::blobs::{BlobEntry, BlobError, Blobs};

/// The part size [`R2Blobs::adopt`] streams up in: 8 MiB, the chunk size the
/// server fixes for uploads, so the staging side and the bucket side speak
/// pieces of the same shape. S3 multipart parts must share one size until
/// the last — this loop writes full parts by construction.
const PART_SIZE: usize = 8 * 1024 * 1024;

/// The file bytes, in a bucket.
///
/// Cheap to clone — the S3 client is an [`std::sync::Arc`] underneath — so a
/// database reopen can take a second handle to the same bucket without
/// rebuilding credentials or a second connection pool.
#[derive(Clone)]
pub struct R2Blobs {
    store: object_store::aws::AmazonS3,
}

impl R2Blobs {
    /// Opens the bucket. The region is always `"auto"`: R2 answers any
    /// region string with the one that actually holds the bucket, and SigV4
    /// signing needs something in the field. `allow_http` exists for tests
    /// against a loopback S3 server; real R2 is https, and the web binary
    /// never sets it.
    pub fn open(
        endpoint: &str,
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
        allow_http: bool,
    ) -> Result<Self, BlobError> {
        let store = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_access_key_id(access_key_id)
            .with_secret_access_key(secret_access_key)
            .with_region("auto")
            .with_bucket_name(bucket)
            .with_allow_http(allow_http)
            .build()?;
        Ok(Self { store })
    }
}

/// The backend's refusals, flattened into the trait's one error shape. The
/// distinction a caller needs — "the key holds nothing" — is answered by
/// [`Blobs::get`] and friends returning `None` before this conversion ever
/// runs; everything else is text for the log.
impl From<object_store::Error> for BlobError {
    fn from(err: object_store::Error) -> Self {
        BlobError::Backend(err.to_string())
    }
}

/// Whether the error is "the key holds nothing" — the shape every getter
/// turns into `None`, because a missing blob is nothing to serve, not a
/// stack.
fn is_not_found(err: &object_store::Error) -> bool {
    matches!(err, object_store::Error::NotFound { .. })
}

#[async_trait::async_trait]
impl Blobs for R2Blobs {
    fn kind(&self) -> &'static str {
        "r2"
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), BlobError> {
        let _ = self
            .store
            .put(&ObjectPath::from(key), PutPayload::from(bytes.to_vec()))
            .await?;
        Ok(())
    }

    async fn adopt(&self, key: &str, staged: &Path, expected_len: u64) -> Result<(), BlobError> {
        let len = tokio::fs::metadata(staged).await?.len();
        if len != expected_len {
            // The staging is kept: the caller decides what to retry, and a
            // deletion here would turn a disagreement into data loss.
            return Err(BlobError::Backend(format!(
                "staged file {} holds {len} bytes, expected {expected_len}",
                staged.display()
            )));
        }
        let path = ObjectPath::from(key);
        // A zero-byte file has nothing to multipart: S3 refuses an upload of
        // zero parts, and the whole file is one empty payload anyway.
        if expected_len == 0 {
            let _ = self.store.put(&path, PutPayload::default()).await?;
            let _ = tokio::fs::remove_file(staged).await;
            return Ok(());
        }
        // The staged file streams up in parts — one PART_SIZE buffer in
        // memory at a time, never one allocation the file's size. Reads are
        // ACCUMULATED until a part is full: a `read` may return short of the
        // buffer (a regular file usually fills it, but nothing promises it),
        // and S3 refuses a complete whose non-final parts fall under the
        // 5 MiB minimum — R2 answers EntityTooSmall, the local fake does
        // not, so the sizing is made true here rather than trusted to the
        // filesystem. Only the last part is short, which is the legal shape.
        let mut file = tokio::fs::File::open(staged).await?;
        let mut upload = self.store.put_multipart(&path).await?;
        let attempt = async {
            let mut buf = vec![0u8; PART_SIZE];
            let mut filled = 0usize;
            loop {
                let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf[filled..]).await?;
                if n == 0 {
                    if filled > 0 {
                        // The tail: the one part allowed to be short.
                        upload
                            .put_part(PutPayload::from(bytes::Bytes::copy_from_slice(
                                &buf[..filled],
                            )))
                            .await?;
                    }
                    break;
                }
                filled += n;
                if filled == PART_SIZE {
                    upload
                        .put_part(PutPayload::from(bytes::Bytes::copy_from_slice(&buf)))
                        .await?;
                    filled = 0;
                }
            }
            upload.complete().await?;
            Ok::<(), BlobError>(())
        }
        .await;
        if let Err(problem) = attempt {
            // Leave no half-finished upload on the bucket; the staged file
            // stays, so the caller's retry starts from disk rather than from
            // whatever parts happened to land.
            let _ = upload.abort().await;
            return Err(problem);
        }
        // The blob landed; the staging has served its purpose.
        let _ = tokio::fs::remove_file(staged).await;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BlobError> {
        let path = ObjectPath::from(key);
        match self.store.get(&path).await {
            Ok(result) => Ok(Some(result.bytes().await?.to_vec())),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn span(&self, key: &str, start: u64, len: u64) -> Result<Option<FileSpan>, BlobError> {
        let path = ObjectPath::from(key);
        // The honest size first: a row's size can drift from its blob, and
        // S3 answers a range past the end with an error rather than a clamp,
        // which would hang a serve instead of shortening it. One HEAD buys
        // the size every clamp below is built from.
        let size = match self.store.head(&path).await {
            Ok(meta) => meta.size,
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let start = start.min(size);
        let end = start.saturating_add(len).min(size);
        if start >= end {
            return Ok(Some(FileSpan {
                len: 0,
                stream: Box::pin(futures_util::stream::empty::<std::io::Result<bytes::Bytes>>()),
            }));
        }
        let result = match self
            .store
            .get_opts(
                &path,
                GetOptions::default().with_range(Some(GetRange::Bounded(start..end))),
            )
            .await
        {
            Ok(result) => result,
            // Raced a delete between the HEAD and the read: nothing to serve.
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        // The response's own range — what the server says it served, not
        // what was asked for — is the honest length, so `FileSpan::len`
        // says exactly what the stream will yield.
        let honest = result.range.end.saturating_sub(result.range.start);
        let stream = result
            .into_stream()
            .map_err(|err| std::io::Error::other(err.to_string()));
        Ok(Some(FileSpan {
            len: honest,
            stream: Box::pin(stream),
        }))
    }

    async fn delete(&self, keys: &[&str]) -> Result<(), BlobError> {
        for key in keys {
            if let Err(err) = self.store.delete(&ObjectPath::from(*key)).await {
                // Idempotent by contract: a key holding nothing is success,
                // not an error.
                if !is_not_found(&err) {
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    async fn list_with_times(&self, dir: &str) -> Result<Vec<BlobEntry>, BlobError> {
        let separator = format!("{dir}/");
        let prefix = ObjectPath::from(separator.clone());
        let mut out = Vec::new();
        let mut listing = self.store.list(Some(&prefix));
        while let Some(entry) = listing.next().await {
            let meta = entry?;
            // The whole prefix is stripped, not just the final segment: the
            // sweep compares these names against rows, and a nested name
            // that lost its directory would lie about what it names.
            let full = meta.location.to_string();
            // `last_modified` is chrono's; the crate's own clock is `time`,
            // and a unix second crosses between them without a dependency.
            let modified = time::OffsetDateTime::from_unix_timestamp(meta.last_modified.timestamp())
                .ok();
            out.push(BlobEntry {
                name: full.strip_prefix(&separator).unwrap_or(&full).to_string(),
                modified,
            });
        }
        Ok(out)
    }
}
