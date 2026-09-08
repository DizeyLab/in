//! Where the bytes live.
//!
//! [`super::Store`] owns the rows; this module owns the blobs behind them.
//! `TursoStore` wrote the file tree under the storage root directly — the
//! trait here is that tree's shape as an interface, so a second
//! implementation (an S3-compatible bucket such as Cloudflare R2) is a new
//! file and nothing else.
//!
//! Keys are `<dir>/<name>`: `files/<ulid>` and `thumbs/<ulid>`, the on-disk
//! layout the local implementation has always used, so one backup unit or
//! one `rclone` of a bucket maps onto the other without translation.
//!
//! Ordering contract, shared by every implementation and load-bearing for
//! crash safety: a blob lands at its key BEFORE the row naming it commits,
//! and a blob is deleted only after its row is gone — or when no row will
//! ever name it, which is what the boot sweep does to orphans. Never the
//! other way around: a row pointing at nothing is a lie the UI can see.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use ulid::Ulid;

use super::FileSpan;

/// What a blob backend can fail with. A missing blob is not an error — the
/// getters answer `None` for it, the way a row whose file went missing is
/// "nothing to serve", not a stack.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The backend refused the read or the write.
    #[error("object storage: {0}")]
    Backend(String),
}

impl From<std::io::Error> for BlobError {
    fn from(err: std::io::Error) -> Self {
        BlobError::Backend(err.to_string())
    }
}

/// The bytes behind the rows: the file tree under the storage root, or a
/// bucket behind an S3-compatible endpoint.
#[async_trait::async_trait]
pub trait Blobs: Send + Sync + 'static {
    /// `"local"` or `"r2"` — for the boot line that says which backend
    /// holds the bytes.
    fn kind(&self) -> &'static str;

    /// Writes `bytes` at `key`, atomically from a reader's point of view:
    /// the key holds either nothing or all of `bytes`, never a prefix.
    /// Meant for thumbnails and small inserts; anything already assembled
    /// on disk uses [`Blobs::adopt`] instead.
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), BlobError>;

    /// Places an already-assembled local file at `key`, removing the staged
    /// original once it landed. The local implementation renames; a remote
    /// one streams the file up without ever holding it whole — multipart,
    /// never one allocation the file's size — and unlinks after. A staged
    /// file whose length disagrees with `expected_len` is an error and the
    /// staging is kept: the caller decides what to retry.
    async fn adopt(&self, key: &str, staged: &Path, expected_len: u64)
        -> Result<(), BlobError>;

    /// The whole blob, `None` when the key holds nothing. For thumbnails
    /// and rows small enough to buffer; a serve must use [`Blobs::span`].
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BlobError>;

    /// `len` bytes of the blob starting at `start`, as chunked frames. The
    /// span is clamped to what the blob really holds and
    /// [`FileSpan::len`] says how much the stream will yield, so a row
    /// whose size drifted from its blob cannot hang a response. `None`
    /// when the key holds nothing.
    async fn span(&self, key: &str, start: u64, len: u64) -> Result<Option<FileSpan>, BlobError>;

    /// Removes the keys, idempotently: a key holding nothing is success,
    /// not an error.
    async fn delete(&self, keys: &[&str]) -> Result<(), BlobError>;

    /// What one directory of the key space (`"files"` or `"thumbs"`) holds,
    /// temp files included — the orphan sweep's eyes, with the moment each
    /// object last changed, because the sweep only reclaims what is older
    /// than the newest row the database knows. A listing is a walk of one
    /// directory, never of the whole tree.
    async fn list_with_times(&self, dir: &str) -> Result<Vec<BlobEntry>, BlobError>;

    /// The names alone, for callers that do not weigh ages.
    async fn list(&self, dir: &str) -> Result<Vec<String>, BlobError> {
        Ok(self
            .list_with_times(dir)
            .await?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }
}

/// One object in a directory of the key space: its name, and when its bytes
/// last changed. `modified` is `None` when the backend will not say — an
/// object whose age is unknown is never reclaimed, because the sweep cannot
/// prove it predates the database.
#[derive(Debug, Clone)]
pub struct BlobEntry {
    /// The name under the directory, the way a row would name it.
    pub name: String,
    /// When the object last changed, as the backend reports it.
    pub modified: Option<OffsetDateTime>,
}

/// The blob tree on the local filesystem: `root/files/<id>` and
/// `root/thumbs/<id>`, the layout `TursoStore` wrote by hand before this
/// trait existed. Every operation keeps the exact shape it had there —
/// temp-plus-rename writes, rename-into-place adopts, clamped spans — so
/// a storage root with no backend configured behaves byte for byte as it
/// always did.
pub struct LocalBlobs {
    /// The storage root; every key joins onto it, and nothing else about
    /// the disk is this type's business.
    root: PathBuf,
}

impl LocalBlobs {
    /// Roots the blob tree at `root` — the storage directory `TursoStore`
    /// creates and locks down before any writer runs.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where a key lives: one join. Keys are relative by contract, so a
    /// bucket maps onto the same names without translation.
    fn path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait::async_trait]
impl Blobs for LocalBlobs {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), BlobError> {
        use std::io::Write as _;
        let path = self.path(key);
        // Temp name in the destination's own directory, then a rename: a
        // reader of the key never sees a prefix, and a crash leaves a
        // `.tmp` for the boot sweep rather than a half blob at the key.
        let tmp = path.with_extension(format!("{}.tmp", Ulid::new()));
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                Err(err.into())
            }
        }
    }

    async fn adopt(&self, key: &str, staged: &Path, expected_len: u64) -> Result<(), BlobError> {
        // The length check is the last place a disagreement between the
        // assembled bytes and the session that promised them can be caught
        // before the name becomes load-bearing. On a mismatch the staging
        // is kept: the caller decides what to retry.
        let len = std::fs::metadata(staged)?.len();
        if len != expected_len {
            return Err(BlobError::Backend(format!(
                "staged file holds {len} bytes, expected {expected_len}"
            )));
        }
        // The staging was assembled in the same tree, so same filesystem:
        // the rename is atomic and consumes the staged original, the key
        // holding nothing or the whole file.
        std::fs::rename(staged, self.path(key))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BlobError> {
        match std::fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn span(&self, key: &str, start: u64, len: u64) -> Result<Option<FileSpan>, BlobError> {
        let mut file = match tokio::fs::File::open(self.path(key)).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Clamp against the real blob, not the row: the span served is the
        // span that exists.
        let on_disk = file.metadata().await?.len();
        let len = len.min(on_disk.saturating_sub(start));
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        file.seek(std::io::SeekFrom::Start(start)).await?;
        // 64 KiB frames: big enough that a serve is a handful of reads,
        // small enough that a canceled download leaves nothing held.
        let stream = tokio_util::io::ReaderStream::with_capacity(file.take(len), 64 * 1024);
        Ok(Some(FileSpan {
            len,
            stream: Box::pin(stream),
        }))
    }

    async fn delete(&self, keys: &[&str]) -> Result<(), BlobError> {
        for key in keys {
            // A key holding nothing is success: deletes run after the row
            // is gone, and the sweep runs ahead of facts, so NotFound is
            // the ordinary outcome of a job already done.
            match std::fs::remove_file(self.path(key)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn list_with_times(&self, dir: &str) -> Result<Vec<BlobEntry>, BlobError> {
        let entries = match std::fs::read_dir(self.root.join(dir)) {
            Ok(entries) => entries,
            // A half of the tree that does not exist yet lists empty: a
            // fresh storage root has no orphans to name.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry?;
            // Files only: the sweep reads this as "what the tree holds",
            // and a directory here is not a blob it could unlink.
            if !entry.path().is_file() {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                let modified = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .map(OffsetDateTime::from);
                out.push(BlobEntry { name, modified });
            }
        }
        Ok(out)
    }
}
