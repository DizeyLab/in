//! Turso implementation of [`Store`].
//!
//! Turso is in-process and SQLite-compatible, so the schema is plain SQL and
//! there is no server to run alongside the binary. It ships no migration
//! runner of its own; [`TursoStore::open`] applies the numbered files in
//! `crates/in-core/migrations` at boot and verifies the result.

use async_trait::async_trait;
use rand::Rng;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Row, params};
use ulid::Ulid;

use super::blobs::{Blobs, LocalBlobs};
use super::secret;
use super::{
    CHUNK_SIZE, CreatedLink, File, Folder, Listing, Result, ShareKind, ShareLink, ShareUser,
    SharedItem, Store, StoreError, ThumbState, UPLOAD_TTL_HOURS, UploadSession, UploadState, User,
};
use super::{ReconcileOptions, reconcile, schema, sniff};
use crate::live::{Change, Topic};
use crate::thumbs;

use super::previews;

/// Where one file's bytes live: `<storage>/files/<id>`, named by the row's
/// own id. What an upload carried never decides a path here — the name is
/// always store-made, so `name` stays a label.
pub(crate) const FILES_DIR: &str = "files";
/// Where thumbnails live: `<storage>/thumbs/<id>`, webp, longest edge 512.
const THUMBS_DIR: &str = "thumbs";
/// Where upload chunks stage: `<storage>/uploads/<session-id>/<n>`.
const UPLOADS_DIR: &str = "uploads";
/// Where preview derivatives live: `previews/<id>`, the playable stand-in
/// a media route builds for a video whose stored tracks no browser demuxes.
/// Deleted beside `files/` and `thumbs/` in every purge, swept at boot with
/// the same watermark rule.
const PREVIEWS_DIR: &str = previews::PREVIEWS_DIR;

/// Thumbnails are attempted only below this source size: 64 MiB. Past it the
/// row wears `failed` rather than pinning memory the size of the file.
const THUMB_SOURCE_CAP: u64 = 64 * 1024 * 1024;

const USER_COLUMNS: &str = "id, oidc_sub, email, display_name, admin, disabled, quota_bytes, used_bytes, ui, created_at, last_seen_at, theme, language, photo_version";
const FOLDER_COLUMNS: &str = "id, owner_id, parent_id, name, created_at, deleted_at";
const FILE_COLUMNS: &str = "id, owner_id, folder_id, name, mime, size_bytes, thumb_state, created_at, updated_at, deleted_at, download_count";
const LINK_COLUMNS: &str = "id, token_hash, kind, target_id, created_by, can_download, created_at, expires_at, revoked_at, password_hash";
const SESSION_COLUMNS: &str = "id, owner_id, folder_id, name, size_bytes, chunk_size, received_bytes, state, created_at, expires_at";

pub struct TursoStore {
    /// Shared by every single-statement call. Turso serialises statements on a
    /// connection, so this is safe; transactions are the exception and take a
    /// connection of their own.
    conn: tokio::sync::Mutex<Connection>,
    db: turso::Database,
    /// Where committed writes are announced. Held as the sender so the store
    /// can hand out a receiver per subscriber; nothing is ever read from here
    /// by the store itself.
    live: tokio::sync::broadcast::Sender<Change>,
    /// Root of the file tree the binary payloads live under: `files/` for
    /// file bytes, `thumbs/` for thumbnails, `uploads/` for staged chunks —
    /// one raw file per row, named by the row's own id. The database keeps
    /// the facts and this tree keeps the bytes; a boot sweep deletes
    /// whichever half outlives the other.
    storage: std::path::PathBuf,
    /// The backend holding the bytes that `files/<id>` and `thumbs/<id>`
    /// name. The local tree by default; a swap of backends is a
    /// constructor's decision ([`Self::open_with_blobs`]), never a change
    /// to a call site — every files/ and thumbs/ byte path below routes
    /// through this.
    blobs: std::sync::Arc<dyn Blobs>,
}

impl TursoStore {
    /// Announces a committed write. Called only once the write is durable —
    /// after commit for a transaction, and never on a path that returned an
    /// error — because a subscriber's whole job is to re-read, and waking it
    /// before the row lands makes it read the past.
    ///
    /// A send with no subscribers returns `Err`, which is the ordinary state
    /// of a server nobody is looking at. It is dropped on purpose: an
    /// announcement nobody is waiting for is not a problem, and logging it
    /// would fill the log with the sound of an idle app.
    fn announce(&self, topics: impl IntoIterator<Item = Topic>) {
        for topic in topics {
            let _ = self.live.send(Change {
                topic,
                seq: crate::live::next_seq(),
            });
        }
    }

    /// Opens (creating if needed) the database at `path` and brings it up to
    /// date. `storage` is the root of the file tree the binary payloads live
    /// under — created here if missing, derived to sit beside the database
    /// when `None`.
    ///
    /// The boot, in order: create the storage tree, rebuild a stale database
    /// before any long-lived handle opens it, and migrate an empty one.
    /// That is all a first request needs. The hygiene sweeps — aborting
    /// expired upload sessions, deleting files no row names — wait in
    /// [`Self::boot_sweeps`], which the server runs as a background task
    /// the moment `open` returns: a restart is not a reason to make the
    /// first page wait behind a walk of every file on disk. Trash is never
    /// purged here — see the note below.
    pub async fn open(database: &str, storage: Option<&std::path::Path>) -> Result<Self> {
        let storage_buf;
        let storage: &std::path::Path = match storage {
            Some(dir) => dir,
            None => {
                // Beside the database: the two are one backup unit, so the
                // default keeps them siblings.
                storage_buf = default_storage(database);
                &storage_buf
            }
        };
        Self::open_with_blobs(
            database,
            Some(storage),
            // No backend configured: the bytes live in the local tree, the
            // only place they have ever lived.
            std::sync::Arc::new(LocalBlobs::new(storage)),
        )
        .await
    }

    /// [`Self::open`] with the blob backend named instead of implied. The
    /// `storage` tree stays local either way: upload chunks stage there and
    /// an assembled file waits there for placement — the sniffer and the
    /// thumbnailer need a real local file — so even a bucket-backed store
    /// keeps a local staging floor.
    pub async fn open_with_blobs(
        database: &str,
        storage: Option<&std::path::Path>,
        blobs: std::sync::Arc<dyn Blobs>,
    ) -> Result<Self> {
        let storage_buf;
        let storage: &std::path::Path = match storage {
            Some(dir) => dir,
            None => {
                // Beside the database: the two are one backup unit, so the
                // default keeps them siblings.
                storage_buf = default_storage(database);
                &storage_buf
            }
        };
        // Before anything else, including the reconcile below that may need
        // this tree: a fresh storage path is a normal first boot, and every
        // writer past this point assumes the tree is there.
        ensure_storage_dirs(storage)?;
        let existed = database != ":memory:" && std::path::Path::new(database).exists();
        // Before anything holds this file open: a database of an older shape
        // is rebuilt now, while no handle of ours points at the file that the
        // rebuild is about to replace.
        Self::repair_if_stale(database, storage).await?;
        let db = Builder::new_local(database)
            .build()
            .await
            .map_err(backend)?;
        let conn = db.connect().map_err(backend)?;
        // Turso is a single-writer engine. Two connections on one Database
        // handle serialise by themselves, but a second handle on the same file
        // (a second process, or a careless second open) fails outright with
        // "database is locked" and silently drops the write unless a busy
        // timeout is set. Both pragmas are set on every connection we hand out.
        for pragma in ["PRAGMA foreign_keys = ON", "PRAGMA busy_timeout = 5000"] {
            conn.execute(pragma, ()).await.map_err(backend)?;
        }
        // The database file sits beside other local users; 0600 right after
        // creation closes the window between "file exists" and "file is ours
        // alone" — a WAL file, if this engine leaves one beside the main
        // file, gets the same restriction while it's still there to restrict.
        if !existed && database != ":memory:" {
            restrict_if_present(std::path::Path::new(database))?;
            restrict_if_present(&sibling(database, "-wal"))?;
            restrict_if_present(&sibling(database, "-shm"))?;
        }
        // 256 is deep enough that a client which pauses for a moment catches
        // up without noticing, and shallow enough that one wedged subscriber
        // cannot pin an unbounded backlog. Overflowing it is not a failure:
        // the reader is told it lagged and resyncs, which is cheaper than the
        // memory a larger buffer would cost to avoid saying so.
        let (live, _) = tokio::sync::broadcast::channel(256);
        let store = Self {
            conn: tokio::sync::Mutex::new(conn),
            db,
            live,
            storage: storage.to_path_buf(),
            blobs,
        };
        store.migrate(database).await?;
        // Trash is deliberately NOT swept here either: the age cutoff is
        // the deployment's `purge_after_days`, which this signature never
        // sees. The server purges with its configured cutoff in the same
        // background pass that runs [`Self::boot_sweeps`].
        // The file may have been rebuilt from a backup; its permissions and
        // any transient WAL/SHM siblings should still be private.
        if database != ":memory:" {
            restrict_if_present(std::path::Path::new(database))?;
            restrict_if_present(&sibling(database, "-wal"))?;
            restrict_if_present(&sibling(database, "-shm"))?;
        }
        Ok(store)
    }

    /// The hygiene a boot owes but a first request does not wait for:
    /// abort upload sessions past their expiry, then sweep files no row
    /// names. The server runs this as a background task the moment `open`
    /// returns, so the port answers while it works; a request racing the
    /// sweep still sees a consistent store, because the sweep's deletions
    /// are decided under the database's write lock (see
    /// [`Self::sweep_orphan_files`]).
    pub async fn boot_sweeps(&self) -> Result<()> {
        self.prune_expired_uploads(OffsetDateTime::now_utc())
            .await?;
        self.sweep_orphan_files().await
    }

    /// Sets the storage tree against the database, once per boot. The
    /// database and the tree are two halves of one state, and a crash between
    /// a row write and its file write — either order — leaves exactly one
    /// half behind. A file no row names goes, including a `.tmp` a crash
    /// abandoned mid-write and a staged chunk whose session is gone; a row
    /// whose file is gone is only said out loud and otherwise kept, because
    /// deleting the row would turn a lost file into a lost fact — the row is
    /// still what screens list, and a re-upload replaces it cleanly.
    ///
    /// The store outlives any one copy of the database now that the bytes
    /// sit in a shared bucket, so an unknown object is reclaimed ONLY when
    /// it is no newer than the newest moment the database knows (the latest
    /// `file.created_at` / `file.updated_at` / `upload_session.created_at`);
    /// anything newer is kept and said out loud. A crash orphan is older
    /// than the next successful upload and goes on a later boot, while
    /// uploads a stale database never heard of are always newer than its
    /// newest row and are never touched — a database three days behind the
    /// bucket once cost four real uploads here. An empty database knows no
    /// moment at all, which is the stale case at its worst: it deletes
    /// nothing.
    async fn sweep_orphan_files(&self) -> Result<()> {
        // The whole pass — the id reads and the directory walks — holds the
        // database's write lock. A writer lands a new file while holding
        // that same lock (the bytes go down before the row, inside one
        // IMMEDIATE transaction), so a file the walk cannot name was either
        // written entirely before this lock was taken — and then its row is
        // in the id set — or is written entirely after the lock is released.
        // A sweep that could run between a file's bytes and its row would
        // delete an upload the server had already accepted; while the sweeps
        // ran inside `open`, before any request could arrive, the boot order
        // made that impossible, and this lock is what keeps it impossible
        // now that the server answers while the sweep runs.
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let files = known_ids(&tx, "SELECT id FROM file").await?;
        let thumbs = known_ids(&tx, "SELECT id FROM file WHERE thumb_state = 'ready'").await?;
        let sessions =
            known_ids(&tx, "SELECT id FROM upload_session WHERE state = 'active'").await?;
        // The watermark: the newest moment any row carries. Read under the
        // same lock as the ids, so a writer that commits mid-sweep either
        // is in both sets or in neither.
        let watermark = newest_known(&tx).await?;
        let mut kept = 0usize;
        // Whether an unknown object may be reclaimed: only one no newer
        // than the database's newest moment. An age the backend will not
        // report, and an empty database, both answer no.
        let reclaimable = |modified: Option<OffsetDateTime>| match (watermark, modified) {
            (Some(newest), Some(at)) => at <= newest,
            _ => false,
        };
        for (dir, known, kind) in [
            (FILES_DIR, &files, "file"),
            (THUMBS_DIR, &thumbs, "thumbnail"),
            // A derivative may exist for any live file row — it is built on
            // demand, not at ingest — so its known set is the same ids.
            (PREVIEWS_DIR, &files, "preview"),
        ] {
            // One listing per half: the names the tree holds, temp files
            // included — a `.tmp` is an orphan like any other — each with
            // the moment it last changed.
            let entries = self
                .blobs
                .list_with_times(dir)
                .await
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            let names: std::collections::HashSet<&str> =
                entries.iter().map(|entry| entry.name.as_str()).collect();
            for id in known.iter() {
                if !names.contains(id.as_str()) {
                    eprintln!("{kind} {id} names a file that is not there");
                }
            }
            for entry in &entries {
                if known.contains(&entry.name) {
                    continue;
                }
                if !reclaimable(entry.modified) {
                    kept += 1;
                    eprintln!(
                        "{kind} {} is newer than anything the database knows and was kept — \
                         the database may be behind the store",
                        entry.name
                    );
                    continue;
                }
                if let Err(e) = self.blobs.delete(&[&format!("{dir}/{}", entry.name)]).await {
                    eprintln!(
                        "could not delete orphaned storage file {dir}/{}: {e}",
                        entry.name
                    );
                }
            }
        }
        // A staged-chunk directory outlives its session only through a crash
        // between the session write and the first chunk, or a removed session
        // row — either way nothing will ever finish it, so it goes, under the
        // same watermark rule and with the directory's own mtime as its age.
        let uploads = self.storage.join(UPLOADS_DIR);
        let entries = match std::fs::read_dir(&uploads) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report_kept(kept);
                tx.commit().await.map_err(backend)?;
                return Ok(());
            }
            Err(e) => return Err(backend(e)),
        };
        for entry in entries {
            let entry = entry.map_err(backend)?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if sessions.contains(&name) {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .map(OffsetDateTime::from);
            if !reclaimable(modified) {
                kept += 1;
                eprintln!(
                    "staged upload {name} is newer than anything the database knows and was kept — \
                     the database may be behind the store"
                );
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(&path) {
                eprintln!(
                    "could not delete orphaned upload directory {}: {e}",
                    path.display()
                );
            }
        }
        report_kept(kept);
        // Nothing was written through the transaction: it exists to hold
        // the write lock across the walk, and committing it ends the pass.
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    /// Brings the database to the declared schema.
    ///
    /// - An empty database is created from the migrations.
    /// - A database that already matches the declared schema is left alone.
    /// - A stale database is backed up, rebuilt, and re-verified once.
    ///   If the rebuilt database still does not match, the process stops
    ///   with the original backed up and untouched.
    ///
    /// SQLite's DDL is transactional, so a schema that dies halfway leaves
    /// nothing behind: a half-created database is a boot that starts over,
    /// not a database with a hole in it.
    async fn migrate(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                (),
            )
            .await
            .map_err(backend)?;

        let empty = match rows.next().await.map_err(backend)? {
            Some(row) => row.get::<i64>(0).map_err(backend)? == 0,
            None => true,
        };
        drop(rows);

        if empty {
            conn.execute("BEGIN IMMEDIATE", ()).await.map_err(backend)?;
            if let Err(e) = conn.execute_batch(&super::schema::schema_sql()).await {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(backend(e));
            }
            return conn
                .execute("COMMIT", ())
                .await
                .map_err(backend)
                .map(|_| ());
        }

        if path == ":memory:" {
            // Tests own an in-memory database; its schema is whatever the
            // test created, and the open should not try to reconcile it.
            return Ok(());
        }

        // `repair_if_stale` ran before this handle was ever opened, so by now
        // the file on disk matches. Saying so here is cheap and turns a
        // reordering mistake into a refusal to start rather than a store
        // running against a schema the code does not expect.
        let have = schema::fingerprint(&conn).await.map_err(backend)?;
        let want = schema::declared_fingerprint().await.map_err(backend)?;
        if have != want {
            return Err(StoreError::Backend(format!(
                "database does not match the declared schema and was not repaired; \
                 do not restart. diff:\n{}",
                schema::diff_report(&have, &want)
            )));
        }
        Ok(())
    }

    /// Brings a database of an older shape onto the declared schema, BEFORE
    /// any long-lived handle is opened on it.
    ///
    /// The order matters and is the whole reason this is not part of
    /// `migrate`: `reconcile` swaps a rebuilt file into place, so a
    /// `turso::Database` opened beforehand still refers to the file that is
    /// now the backup. Reconnecting such a handle reads the old database and
    /// makes a successful rebuild look like a failed one.
    async fn repair_if_stale(path: &str, storage: &std::path::Path) -> Result<()> {
        if path == ":memory:" || !std::path::Path::new(path).exists() {
            return Ok(());
        }
        let (have, empty) = {
            let db = Builder::new_local(path).build().await.map_err(backend)?;
            let conn = db.connect().map_err(backend)?;
            let mut rows = conn
                .query(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                    (),
                )
                .await
                .map_err(backend)?;
            let empty = match rows.next().await.map_err(backend)? {
                Some(row) => row.get::<i64>(0).map_err(backend)? == 0,
                None => true,
            };
            drop(rows);
            let have = if empty {
                String::new()
            } else {
                schema::fingerprint(&conn).await.map_err(backend)?
            };
            (have, empty)
        };
        if empty {
            return Ok(());
        }
        let want = schema::declared_fingerprint().await.map_err(backend)?;
        if have == want {
            return Ok(());
        }

        eprintln!(
            "database schema differs from the declared schema; rebuilding automatically\n{}",
            schema::diff_report(&have, &want)
        );
        reconcile(
            path,
            Some(storage),
            ReconcileOptions {
                dry_run: false,
                yes: false,
                auto: true,
            },
        )
        .await?;

        // Check the result once, on a handle opened after the swap. A
        // normalisation bug that rebuilt forever would otherwise write a
        // full-size backup on every restart until the disk filled.
        let db = Builder::new_local(path).build().await.map_err(backend)?;
        let conn = db.connect().map_err(backend)?;
        let after = schema::fingerprint(&conn).await.map_err(backend)?;
        if after != want {
            return Err(StoreError::Backend(format!(
                "rebuild did not match the declared schema; the original is backed up beside it \
                 and this is not retried. diff:\n{}",
                schema::diff_report(&after, &want)
            )));
        }
        Ok(())
    }

    /// A connection of its own, for work that opens a transaction.
    /// `Connection::transaction` takes `&mut self`, and a transaction on the
    /// shared connection would swallow everyone else's statements.
    async fn tx_conn(&self) -> Result<Connection> {
        let conn = self.db.connect().map_err(backend)?;
        for pragma in ["PRAGMA foreign_keys = ON", "PRAGMA busy_timeout = 5000"] {
            conn.execute(pragma, ()).await.map_err(backend)?;
        }
        Ok(conn)
    }

    async fn one_row(&self, sql: &str, args: impl turso::IntoParams) -> Result<Option<Row>> {
        let conn = self.conn.lock().await;
        let mut rows = conn.query(sql, args).await.map_err(backend)?;
        rows.next().await.map_err(backend)
    }
}

/// Recomputes one account's usage from the rows: the sum of every file size
/// it holds, trashed or not. Runs inside the caller's transaction, so the
/// mutation and the total commit as one — usage can never drift from the
/// rows, because it is never carried anywhere except from them.
async fn refresh_usage(conn: &Connection, owner_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE user SET used_bytes = \
         (SELECT COALESCE(SUM(size_bytes), 0) FROM file WHERE owner_id = ?1) \
         WHERE id = ?1",
        params![owner_id],
    )
    .await
    .map_err(backend)?;
    Ok(())
}

/// Refuses the write while there is still nothing on disk to clean up: the
/// owner's used bytes plus `extra` must fit the ceiling.
async fn check_quota(conn: &Connection, owner_id: &str) -> Result<(u64, u64)> {
    let mut rows = conn
        .query(
            "SELECT quota_bytes, used_bytes FROM user WHERE id = ?1",
            params![owner_id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => {
            let quota = row.get::<i64>(0).map_err(backend)?.max(0) as u64;
            let used = row.get::<i64>(1).map_err(backend)?.max(0) as u64;
            Ok((quota, used))
        }
        None => Err(StoreError::NotFound),
    }
}

fn fit_quota(quota: u64, used: u64, extra: u64) -> Result<()> {
    // A zero-byte write costs nothing, so it always fits — even under an
    // account whose quota was lowered below current usage, which must still
    // be able to trash and rearrange.
    if extra > 0 && used.saturating_add(extra) > quota {
        return Err(StoreError::QuotaExceeded);
    }
    Ok(())
}

/// The label an uploaded or typed name becomes: the last path segment, with
/// control characters stripped and surrounding whitespace trimmed. A name
/// that is a path (`../../etc`) is a valid file name and a terrible path, so
/// only the tail survives; a name that sanitises to nothing becomes
/// `Untitled` rather than failing the upload.
pub(crate) fn label_of(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    let trimmed = clean.trim().to_string();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed
    }
}
/// The longest name the store keeps whole: the drive forms carry
/// `maxlength="255"`, so a postfix (` (2)`, `.txt`) truncates the base to
/// fit rather than growing the name past what the UI accepts.
pub(crate) const MAX_NAME_CHARS: usize = 255;

/// The first `max` characters of `s`, on a character boundary — never
/// splitting a multi-byte character the way a byte slice would.
fn truncate_chars(s: &str, max: usize) -> &str {
    if s.chars().count() <= max {
        return s;
    }
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Splits a file name at its LAST dot into stem and extension (dot kept on
/// the extension): `report.txt` is `("report", ".txt")`, `archive.tar.gz`
/// is `("archive.tar", ".gz")`. No dot, a leading dot only (`.gitignore`),
/// or a trailing dot (`foo.`) is no split — the whole name is the stem —
/// so the postfix never lands in front of a dotfile's only dot.
fn split_file_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 && at + 1 < name.len() => (&name[..at], &name[at..]),
        _ => (name, ""),
    }
}

/// `report.txt` at 2 is `report (2).txt`; `name` at 2 is `name (2)`. The
/// stem is truncated so stem + postfix + extension fits [`MAX_NAME_CHARS`].
fn postfixed_file_name(want: &str, n: u32) -> String {
    let suffix = format!(" ({n})");
    let (stem, ext) = split_file_name(want);
    let keep = MAX_NAME_CHARS.saturating_sub(suffix.chars().count() + ext.chars().count());
    format!("{}{}{}", truncate_chars(stem, keep), suffix, ext)
}

/// `Name` at 2 is `Name (2)`, truncated to fit [`MAX_NAME_CHARS`].
fn postfixed_folder_name(want: &str, n: u32) -> String {
    let suffix = format!(" ({n})");
    let keep = MAX_NAME_CHARS.saturating_sub(suffix.chars().count());
    format!("{}{}", truncate_chars(want, keep), suffix)
}

/// Addresses match case-insensitively: the provider may capitalise what
/// the row stored, so both sides fold before comparing. Provisioning folds
/// on the way in, and every lookup folds the question.
fn fold_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Escapes a substring search for LIKE: backslash first, then the two
/// wildcards, so `100%` searches for a percent rather than everything.
fn like_pattern(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('%');
    for c in query.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// A fresh public-link token: 32 random bytes, base64url without padding.
/// Shown at creation and sealed by the web layer; only its hash is stored here.
fn new_token() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The token's hash, as stored: base64url SHA-256. Constant-length and
/// content-hiding — the row answers equality and nothing else.
///
/// Public because resolving is by hash: the route holding the plaintext token
/// hashes it with this and calls [`Store::resolve_share_link`]. The two must
/// never disagree about the function, so there is exactly one.
pub fn hash_share_token(token: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(token.as_bytes()))
}

/// Argon2id's cost, mirrored from im's account hasher: 19 MiB of memory,
/// two iterations, one lane. Slow enough that a guess costs real work,
/// cheap enough that unlocking a link never reads as an outage.
pub const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
pub const ARGON2_ITERATIONS: u32 = 2;
pub const ARGON2_PARALLELISM: u32 = 1;

fn link_argon2() -> argon2::Argon2<'static> {
    let params = argon2::Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        None,
    )
    .expect("argon2 parameters are constants and are in range");
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
}

/// Hashes a link password into a PHC string, salt included. The parameters
/// ride in the string itself, so raising the cost later does not lock
/// holders of the link out.
///
/// CPU-slow on purpose: the caller owns the async boundary and runs this
/// under `tokio::task::spawn_blocking`.
pub fn hash_link_password(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let salt = SaltString::generate(&mut OsRng);
    link_argon2()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| StoreError::Backend(format!("password hashing failed: {e}")))
}

/// Checks a candidate password against a stored PHC string. A malformed
/// hash answers false — a gate that could panic would be a gate an attacker
/// could aim.
pub fn link_password_matches(phc: &str, candidate: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    match PasswordHash::new(phc) {
        Ok(parsed) => link_argon2()
            .verify_password(candidate.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// The unlock proof a visitor's browser carries as `in_link_{id}`: the
/// token and the stored hash bound together, so a proof stolen from one
/// link is worthless on another and a re-keyed password voids every cookie
/// minted before. Same encode as [`hash_share_token`].
pub fn link_unlock_proof(token: &str, password_hash: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let bound = format!("{token}:{password_hash}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(bound.as_bytes()))
}
/// zero-byte upload has no chunks; its finish assembles the empty file.
fn chunk_count(size_bytes: u64) -> u64 {
    if size_bytes == 0 {
        0
    } else {
        size_bytes.div_ceil(CHUNK_SIZE)
    }
}

/// The exact bytes chunk `index` must carry: full [`CHUNK_SIZE`] except the
/// final one, which carries the remainder.
fn expected_chunk_len(size_bytes: u64, index: u64) -> u64 {
    let full_before = index.saturating_mul(CHUNK_SIZE);
    (size_bytes.saturating_sub(full_before)).min(CHUNK_SIZE)
}

/// Where file `id`'s bytes live.
fn file_path(storage: &std::path::Path, id: &str) -> std::path::PathBuf {
    storage.join(FILES_DIR).join(id)
}

/// Where session `id`'s chunks stage.
fn session_dir(storage: &std::path::Path, id: &str) -> std::path::PathBuf {
    storage.join(UPLOADS_DIR).join(id)
}

/// Where chunk `index` of session `id` stages.
fn chunk_path(storage: &std::path::Path, id: &str, index: u64) -> std::path::PathBuf {
    session_dir(storage, id).join(index.to_string())
}

/// The staged bytes a session holds, summed from the chunk files on disk —
/// never from a counter a re-sent chunk could have double-counted.
fn staged_received(storage: &std::path::Path, id: &str) -> u64 {
    let mut total = 0u64;
    let dir = session_dir(storage, id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        if entry.path().is_file() {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
    total
}

/// The indexes of the chunk files a session holds, unsorted — the same
/// on-disk truth `staged_received` sums.
fn staged_indexes(storage: &std::path::Path, id: &str) -> Vec<u64> {
    let dir = session_dir(storage, id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
        .collect()
}
/// Creates the storage tree — the root and one directory per payload kind —
/// and locks each to 0700 on unix. Every writer past `open` assumes the tree
/// is there.
fn ensure_storage_dirs(storage: &std::path::Path) -> Result<()> {
    for dir in [
        storage.to_path_buf(),
        storage.join(FILES_DIR),
        storage.join(THUMBS_DIR),
        storage.join(UPLOADS_DIR),
        storage.join(PREVIEWS_DIR),
    ] {
        std::fs::create_dir_all(&dir)
            .map_err(|e| StoreError::Backend(format!("could not create {}: {e}", dir.display())))?;
        restrict_dir(&dir)?;
    }
    Ok(())
}

/// 0700 on a directory holding someone's files.
#[cfg(unix)]
fn restrict_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(backend)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Where binary files live when no storage directory is handed in: beside
/// the database file. The two are one backup unit — a backup that takes the
/// database but not the files beside it restores a drive whose contents are
/// gone — so the default keeps them siblings.
fn default_storage(database: &str) -> std::path::PathBuf {
    std::path::Path::new(database)
        .parent()
        .map(|parent| parent.join("storage"))
        .unwrap_or_else(|| std::path::PathBuf::from("storage"))
}

/// Writes `bytes` to `path` through a temp name in the same directory and a
/// rename, so a reader of the final name never sees a partial file — and a
/// crash mid-write leaves a temp name the boot sweep deletes rather than a
/// half file wearing a real one. The caller owns what happens next: the row
/// goes in after, and a failed row write unlinks this file best-effort.
fn write_file_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("{}.tmp", Ulid::new()));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.flush()?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Reads up to `buf.len()` bytes from the start of the file at `path`. What
/// the sniffer and the thumbnailer decide from when the whole file is not
/// yet — or not affordably — in memory.
fn read_up_to(path: &std::path::Path, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Every id `sql` hands back — the set of files the sweep must keep.
async fn known_ids(conn: &Connection, sql: &str) -> Result<std::collections::HashSet<String>> {
    let mut rows = conn.query(sql, ()).await.map_err(backend)?;
    let mut out = std::collections::HashSet::new();
    while let Some(row) = rows.next().await.map_err(backend)? {
        out.insert(row.get::<String>(0).map_err(backend)?);
    }
    Ok(out)
}

/// The newest moment the database knows: the latest of the file rows'
/// stamps and the upload sessions'. `None` for a database with no rows at
/// all, which is what tells the sweep it may reclaim nothing.
async fn newest_known(conn: &Connection) -> Result<Option<OffsetDateTime>> {
    let mut newest: Option<OffsetDateTime> = None;
    for sql in [
        "SELECT MAX(created_at) FROM file",
        "SELECT MAX(updated_at) FROM file",
        "SELECT MAX(created_at) FROM upload_session",
    ] {
        let mut rows = conn.query(sql, ()).await.map_err(backend)?;
        if let Some(row) = rows.next().await.map_err(backend)?
            && let Some(raw) = row.get::<Option<String>>(0).map_err(backend)?
        {
            let at = parse_stamp(&raw)?;
            if newest.is_none_or(|seen| at > seen) {
                newest = Some(at);
            }
        }
    }
    Ok(newest)
}

/// The one summary line a sweep that kept something owes the log; a sweep
/// that kept nothing says nothing.
fn report_kept(kept: usize) {
    if kept > 0 {
        eprintln!(
            "boot sweep kept {kept} unknown object(s) newer than the database — \
             this database may be behind the store"
        );
    }
}

/// A database sidecar (`in.db-wal`, `in.db-shm`): the file beside the main
/// one, wearing the same name plus the suffix.
fn sibling(path: &str, suffix: &str) -> std::path::PathBuf {
    let base = std::path::Path::new(path);
    let mut name = base
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    base.parent()
        .map(|parent| parent.join(&name))
        .unwrap_or_else(|| std::path::PathBuf::from(name))
}

/// 0600 on a file holding live state, when it is there to restrict.
fn restrict_if_present(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        secret::restrict(path).map_err(backend)?;
    }
    Ok(())
}

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn now_text() -> Result<String> {
    stamp(OffsetDateTime::now_utc())
}

/// An RFC 3339 UTC timestamp: lexicographically sortable, which is what lets
/// expiry and purge comparisons live in SQL string order.
fn stamp(at: OffsetDateTime) -> Result<String> {
    at.format(&Rfc3339)
        .map_err(|e| StoreError::Corrupt(format!("timestamp: {e}")))
}

fn parse_stamp(raw: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|e| StoreError::Corrupt(format!("timestamp {raw:?}: {e}")))
}
fn opt_stamp(row: &Row, idx: usize) -> Result<Option<OffsetDateTime>> {
    match row.get::<Option<String>>(idx).map_err(backend)? {
        Some(raw) => Ok(Some(parse_stamp(&raw)?)),
        None => Ok(None),
    }
}

/// One user row, in [`USER_COLUMNS`] order.
fn user_from(row: &Row) -> Result<User> {
    Ok(User {
        id: text(row, 0)?,
        oidc_sub: text(row, 1)?,
        email: text(row, 2)?,
        display_name: text(row, 3)?,
        admin: row.get::<i64>(4).map_err(backend)? != 0,
        disabled: row.get::<i64>(5).map_err(backend)? != 0,
        quota_bytes: row.get::<i64>(6).map_err(backend)?.max(0) as u64,
        used_bytes: row.get::<i64>(7).map_err(backend)?.max(0) as u64,
        ui: text(row, 8)?,
        theme: text(row, 11)?,
        language: text(row, 12)?,
        created_at: parse_stamp(&text(row, 9)?)?,
        last_seen_at: opt_stamp(row, 10)?,
        photo_version: row.get::<i64>(13).map_err(backend)?.max(0) as u64,
    })
}

/// One folder row, in [`FOLDER_COLUMNS`] order.
fn folder_from(row: &Row) -> Result<Folder> {
    Ok(Folder {
        id: text(row, 0)?,
        owner_id: text(row, 1)?,
        parent_id: opt_text(row, 2)?,
        name: text(row, 3)?,
        created_at: parse_stamp(&text(row, 4)?)?,
        deleted_at: opt_stamp(row, 5)?,
    })
}

/// One file row, in [`FILE_COLUMNS`] order — still without its bytes, which
/// no listing carries.
fn file_from(row: &Row) -> Result<File> {
    Ok(File {
        id: text(row, 0)?,
        owner_id: text(row, 1)?,
        folder_id: opt_text(row, 2)?,
        name: text(row, 3)?,
        mime: text(row, 4)?,
        size_bytes: row.get::<i64>(5).map_err(backend)?.max(0) as u64,
        download_count: row.get::<i64>(10).map_err(backend)?.max(0) as u64,
        thumb_state: ThumbState::parse(&text(row, 6)?)?,
        created_at: parse_stamp(&text(row, 7)?)?,
        updated_at: parse_stamp(&text(row, 8)?)?,
        deleted_at: opt_stamp(row, 9)?,
    })
}

/// One share-link row, in [`LINK_COLUMNS`] order.
fn link_from(row: &Row) -> Result<ShareLink> {
    Ok(ShareLink {
        id: text(row, 0)?,
        token_hash: text(row, 1)?,
        kind: ShareKind::parse(&text(row, 2)?)?,
        target_id: text(row, 3)?,
        created_by: text(row, 4)?,
        can_download: row.get::<i64>(5).map_err(backend)? != 0,
        created_at: parse_stamp(&text(row, 6)?)?,
        expires_at: opt_stamp(row, 7)?,
        revoked_at: opt_stamp(row, 8)?,
        password_hash: opt_text(row, 9)?,
    })
}

/// One upload-session row, in [`SESSION_COLUMNS`] order.
fn session_from(row: &Row) -> Result<UploadSession> {
    Ok(UploadSession {
        id: text(row, 0)?,
        owner_id: text(row, 1)?,
        folder_id: opt_text(row, 2)?,
        name: text(row, 3)?,
        size_bytes: row.get::<i64>(4).map_err(backend)?.max(0) as u64,
        chunk_size: row.get::<i64>(5).map_err(backend)?.max(0) as u64,
        received_bytes: row.get::<i64>(6).map_err(backend)?.max(0) as u64,
        state: UploadState::parse(&text(row, 7)?)?,
        created_at: parse_stamp(&text(row, 8)?)?,
        expires_at: parse_stamp(&text(row, 9)?)?,
    })
}

fn text(row: &Row, idx: usize) -> Result<String> {
    row.get::<String>(idx).map_err(backend)
}

fn opt_text(row: &Row, idx: usize) -> Result<Option<String>> {
    row.get::<Option<String>>(idx).map_err(backend)
}

#[async_trait]
impl Store for TursoStore {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change> {
        self.live.subscribe()
    }

    async fn user_by_oidc_sub(&self, sub: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {USER_COLUMNS} FROM user WHERE oidc_sub = ?1");
        match self.one_row(&sql, params![sub]).await? {
            Some(row) => Ok(Some(user_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {USER_COLUMNS} FROM user WHERE email = ?1");
        match self.one_row(&sql, params![fold_email(email)]).await? {
            Some(row) => Ok(Some(user_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn user(&self, id: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {USER_COLUMNS} FROM user WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => Ok(Some(user_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn users(&self) -> Result<Vec<User>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {USER_COLUMNS} FROM user ORDER BY created_at, id");
        let mut rows = conn.query(&sql, ()).await.map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(user_from(&row)?);
        }
        Ok(out)
    }

    async fn provision_user(
        &self,
        sub: &str,
        email: &str,
        display_name: &str,
        admin: Option<bool>,
        photo_version: u64,
        default_quota_bytes: u64,
    ) -> Result<User> {
        let email = fold_email(email);
        // IMMEDIATE: "first user ever" is read-then-write. Two concurrent
        // first sign-ins each count zero users, each claim admin, and the
        // deployment starts with two admins instead of one.
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let sql = format!("SELECT {USER_COLUMNS} FROM user WHERE oidc_sub = ?1");
        let mut rows = tx.query(&sql, params![sub]).await.map_err(backend)?;
        if let Some(row) = rows.next().await.map_err(backend)? {
            let known = user_from(&row)?;
            drop(rows);
            // A returning person: the row follows the provider — address,
            // name, photo stamp, and the admin flag when this sight carries
            // the directory's word on it. The quota is ours and this write
            // touches neither it nor a flag nobody spoke about.
            let admin_changed = admin.is_some_and(|flag| flag != known.admin);
            tx.execute(
                "UPDATE user SET email = ?1, display_name = ?2, photo_version = ?3, \
                 admin = CASE WHEN ?4 IS NULL THEN admin ELSE ?4 END, last_seen_at = ?5 \
                 WHERE id = ?6",
                params![
                    email,
                    display_name,
                    photo_version as i64,
                    admin.map(|flag| if flag { 1 } else { 0 }),
                    now_text()?,
                    known.id
                ],
            )
            .await
            .map_err(backend)?;
            let mut back = tx.query(&sql, params![sub]).await.map_err(backend)?;
            let row = back
                .next()
                .await
                .map_err(backend)?
                .ok_or(StoreError::NotFound)?;
            let user = user_from(&row)?;
            drop(back);
            tx.commit().await.map_err(backend)?;
            // A mirrored field moved: every signed-in topbar may want the
            // new face or name. A mere last-seen stamp announces nothing —
            // a quiet return is not news.
            if user.email != known.email
                || user.display_name != known.display_name
                || user.photo_version != known.photo_version
                || admin_changed
            {
                self.announce([Topic::Profile(user.id.clone())]);
            }
            return Ok(user);
        }
        drop(rows);
        let mut count = tx
            .query("SELECT COUNT(*) FROM user", ())
            .await
            .map_err(backend)?;
        let n = match count.next().await.map_err(backend)? {
            Some(row) => row.get::<i64>(0).map_err(backend)?,
            None => 0,
        };
        drop(count);
        // The provider's word on the person is the admin flag when it has
        // one; a sight with no directory behind it falls back to the
        // bootstrap rule — the first account ever provisioned is the admin,
        // because a deployment with no admin is a deployment nobody can
        // administer.
        let admin = admin.unwrap_or(n == 0);
        let id = Ulid::new().to_string();
        let now = now_text()?;
        tx.execute(
            "INSERT INTO user (id, oidc_sub, email, display_name, admin, disabled, \
             quota_bytes, used_bytes, ui, theme, language, created_at, last_seen_at, \
             photo_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, 0, 'instrument', 'dark', 'en', ?7, ?7, ?8)",
            params![
                id.clone(),
                sub,
                email,
                display_name,
                if admin { 1 } else { 0 },
                default_quota_bytes as i64,
                now,
                photo_version as i64
            ],
        )
        .await
        .map_err(backend)?;
        let mut back = tx
            .query(
                &format!("SELECT {USER_COLUMNS} FROM user WHERE id = ?1"),
                params![id],
            )
            .await
            .map_err(backend)?;
        let row = back
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let user = user_from(&row)?;
        drop(back);
        tx.commit().await.map_err(backend)?;
        self.announce([
            Topic::Admin(user.id.clone()),
            Topic::Profile(user.id.clone()),
        ]);
        Ok(user)
    }
    async fn set_user_quota(&self, user_id: &str, quota_bytes: u64) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET quota_bytes = ?1 WHERE id = ?2",
                params![quota_bytes as i64, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Admin(user_id.to_string())]);
        Ok(())
    }

    async fn set_user_disabled(&self, user_id: &str, disabled: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET disabled = ?1 WHERE id = ?2",
                params![if disabled { 1 } else { 0 }, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Admin(user_id.to_string())]);
        Ok(())
    }

    async fn set_preferences(&self, id: &str, theme: &str, language: &str, ui: &str) -> Result<()> {
        if theme != "light" && theme != "dark" {
            return Err(StoreError::Corrupt(format!("theme {theme:?}")));
        }
        if language != "en" && language != "tr" {
            return Err(StoreError::Corrupt(format!("language {language:?}")));
        }
        if ui != "ledger" && ui != "instrument" {
            return Err(StoreError::Corrupt(format!("ui {ui:?}")));
        }
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET theme = ?1, language = ?2, ui = ?3 WHERE id = ?4",
                params![theme, language, ui, id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Admin(id.to_string())]);
        Ok(())
    }

    async fn create_folder(
        &self,
        owner_id: &str,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<Folder> {
        let name = label_of(name);
        // IMMEDIATE: the parent check, the name search and the insert are
        // one write set, so a parent trashed mid-create still refuses. A
        // live sibling wearing the name is no refusal: the folder takes the
        // first free `name (2)` postfix instead.
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        check_live_parent(&tx, owner_id, parent_id).await?;
        let name = free_folder_name(&tx, owner_id, parent_id, &name, None).await?;
        let id = Ulid::new().to_string();
        let now = now_text()?;
        tx.execute(
            "INSERT INTO folder (id, owner_id, parent_id, name, created_at, deleted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![id.clone(), owner_id, parent_id, name, now],
        )
        .await
        .map_err(backend)?;
        let mut rows = tx
            .query(
                &format!("SELECT {FOLDER_COLUMNS} FROM folder WHERE id = ?1"),
                params![id],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let folder = folder_from(&row)?;
        drop(rows);
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(owner_id.to_string())]);
        Ok(folder)
    }

    async fn folder(&self, id: &str) -> Result<Option<Folder>> {
        let sql = format!("SELECT {FOLDER_COLUMNS} FROM folder WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => Ok(Some(folder_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn rename_folder(&self, id: &str, name: &str) -> Result<()> {
        let name = label_of(name);
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let folder = folder_row(&tx, id).await?.ok_or(StoreError::NotFound)?;
        if folder.deleted_at.is_some() {
            return Err(StoreError::NotFound);
        }
        // A live sibling wearing the name is no refusal: the rename lands
        // on the first free `name (2)` postfix instead.
        let name = free_folder_name(
            &tx,
            &folder.owner_id,
            folder.parent_id.as_deref(),
            &name,
            Some(id),
        )
        .await?;
        tx.execute(
            "UPDATE folder SET name = ?1 WHERE id = ?2",
            params![name, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &folder.owner_id).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(folder.owner_id)]);
        Ok(())
    }

    async fn move_folder(&self, id: &str, parent_id: Option<&str>) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let folder = folder_row(&tx, id).await?.ok_or(StoreError::NotFound)?;
        if folder.deleted_at.is_some() {
            return Err(StoreError::NotFound);
        }
        // The destination is vetted before the cycle walk: a stranger's, a
        // missing, or a trashed folder refuses before ancestry is even asked.
        check_live_parent(&tx, &folder.owner_id, parent_id).await?;
        // The cycle walk: the destination's ancestry must not contain the
        // folder being moved. Moving onto itself is the one-step circle.
        let mut cursor = parent_id.map(str::to_string);
        while let Some(current) = cursor {
            if current == id {
                return Err(StoreError::Cycle);
            }
            cursor = parent_of(&tx, &current).await?;
        }
        // A live child of the destination wearing the folder's name is no
        // obstacle: the move lands on the first free `name (2)` postfix.
        let name =
            free_folder_name(&tx, &folder.owner_id, parent_id, &folder.name, Some(id)).await?;
        tx.execute(
            "UPDATE folder SET parent_id = ?1, name = ?2 WHERE id = ?3",
            params![parent_id, name, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &folder.owner_id).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(folder.owner_id)]);
        Ok(())
    }

    async fn delete_folder(&self, id: &str) -> Result<()> {
        let owner = {
            let folder = self.folder(id).await?.ok_or(StoreError::NotFound)?;
            if folder.deleted_at.is_some() {
                return Ok(());
            }
            folder.owner_id.clone()
        };
        let now = now_text()?;
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        // One timestamp for the whole subtree: the trash lists one moment,
        // and the purge takes it in one pass. Rows already trashed keep
        // their own earlier moment — a file trashed on its own before the
        // folder must still wear its own timestamp after, so a later
        // restore of the folder can tell the cascade apart from it.
        tx.execute(
            "WITH RECURSIVE sub(id) AS ( \
               SELECT ?1 \
               UNION ALL \
               SELECT f.id FROM folder f JOIN sub s ON f.parent_id = s.id \
             ) \
             UPDATE folder SET deleted_at = ?2 WHERE owner_id = ?3 AND deleted_at IS NULL \
             AND id IN (SELECT id FROM sub)",
            params![id, now.as_str(), owner.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "WITH RECURSIVE sub(id) AS ( \
               SELECT ?1 \
               UNION ALL \
               SELECT f.id FROM folder f JOIN sub s ON f.parent_id = s.id \
             ) \
             UPDATE file SET deleted_at = ?2, updated_at = ?2 \
             WHERE owner_id = ?3 AND deleted_at IS NULL AND folder_id IN (SELECT id FROM sub)",
            params![id, now.as_str(), owner.as_str()],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &owner).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(owner.clone()), Topic::Trash(owner)]);
        Ok(())
    }

    async fn list_children(&self, owner_id: &str, parent_id: Option<&str>) -> Result<Listing> {
        let conn = self.conn.lock().await;
        let mut folders = Vec::new();
        let mut files = Vec::new();
        match parent_id {
            Some(parent) => {
                let mut rows = conn
                    .query(
                        &format!(
                            "SELECT {FOLDER_COLUMNS} FROM folder \
                             WHERE owner_id = ?1 AND parent_id = ?2 AND deleted_at IS NULL \
                             ORDER BY name, id"
                        ),
                        params![owner_id, parent],
                    )
                    .await
                    .map_err(backend)?;
                while let Some(row) = rows.next().await.map_err(backend)? {
                    folders.push(folder_from(&row)?);
                }
                let mut rows = conn
                    .query(
                        &format!(
                            "SELECT {FILE_COLUMNS} FROM file \
                             WHERE owner_id = ?1 AND folder_id = ?2 AND deleted_at IS NULL \
                             ORDER BY name, id"
                        ),
                        params![owner_id, parent],
                    )
                    .await
                    .map_err(backend)?;
                while let Some(row) = rows.next().await.map_err(backend)? {
                    files.push(file_from(&row)?);
                }
            }
            None => {
                let mut rows = conn
                    .query(
                        &format!(
                            "SELECT {FOLDER_COLUMNS} FROM folder \
                             WHERE owner_id = ?1 AND parent_id IS NULL AND deleted_at IS NULL \
                             ORDER BY name, id"
                        ),
                        params![owner_id],
                    )
                    .await
                    .map_err(backend)?;
                while let Some(row) = rows.next().await.map_err(backend)? {
                    folders.push(folder_from(&row)?);
                }
                let mut rows = conn
                    .query(
                        &format!(
                            "SELECT {FILE_COLUMNS} FROM file \
                             WHERE owner_id = ?1 AND folder_id IS NULL AND deleted_at IS NULL \
                             ORDER BY name, id"
                        ),
                        params![owner_id],
                    )
                    .await
                    .map_err(backend)?;
                while let Some(row) = rows.next().await.map_err(backend)? {
                    files.push(file_from(&row)?);
                }
            }
        }
        Ok(Listing { folders, files })
    }
    async fn insert_file(
        &self,
        owner_id: &str,
        folder_id: Option<&str>,
        name: &str,
        bytes: &[u8],
    ) -> Result<File> {
        let name = label_of(name);
        let size = bytes.len() as u64;
        // IMMEDIATE: the quota read, the name check and the row write are one
        // write set. Two concurrent uploads each read room for themselves and
        // the pair lands over quota.
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        check_live_parent(&tx, owner_id, folder_id).await?;
        let (quota, used) = check_quota(&tx, owner_id).await?;
        fit_quota(quota, used, size)?;
        // A live sibling wearing the name is no refusal: the file takes the
        // first free `stem (2).ext` postfix instead.
        let name = free_file_name(&tx, owner_id, folder_id, &name, None).await?;
        let mime = sniff::sniff(bytes).to_string();
        let (thumb_state, thumb_bytes) = thumb_for(&mime, size, Some(bytes));
        // The bytes land first, then the row says they are there: a crash
        // in between leaves an orphan blob the boot sweep deletes, never a
        // row pointing at nothing.
        let id = Ulid::new().to_string();
        let file_key = format!("files/{id}");
        let thumb_key = format!("thumbs/{id}");
        self.blobs
            .put(&file_key, bytes)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let mut thumb_placed = false;
        if let Some(thumb) = thumb_bytes.as_ref() {
            // A thumbnail that cannot be written is a miss, not a failed
            // upload: the file is what matters, and the row says so.
            if self.blobs.put(&thumb_key, thumb).await.is_ok() {
                thumb_placed = true;
            } else {
                let _ = self.blobs.delete(&[&thumb_key]).await;
            }
        }
        let stored_thumb = if thumb_placed {
            ThumbState::Ready
        } else if thumb_state == ThumbState::Ready {
            ThumbState::Failed
        } else {
            thumb_state
        };
        let now = now_text()?;
        let written = tx
            .execute(
                "INSERT INTO file (id, owner_id, folder_id, name, mime, size_bytes, \
                 thumb_state, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, NULL)",
                params![
                    id.clone(),
                    owner_id,
                    folder_id,
                    name,
                    mime,
                    size as i64,
                    stored_thumb.as_str(),
                    now
                ],
            )
            .await;
        if let Err(e) = written {
            // The row was never born, so the bytes must not outlive it under
            // a name nothing points at.
            let _ = self.blobs.delete(&[&file_key, &thumb_key]).await;
            return Err(backend(e));
        }
        refresh_usage(&tx, owner_id).await?;
        let mut rows = tx
            .query(
                &format!("SELECT {FILE_COLUMNS} FROM file WHERE id = ?1"),
                params![id],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let file = file_from(&row)?;
        drop(rows);
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(owner_id.to_string())]);
        Ok(file)
    }

    async fn file(&self, id: &str) -> Result<Option<File>> {
        let sql = format!("SELECT {FILE_COLUMNS} FROM file WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => Ok(Some(file_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn rename_file(&self, id: &str, name: &str) -> Result<()> {
        let name = label_of(name);
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let file = file_row(&tx, id).await?.ok_or(StoreError::NotFound)?;
        if file.deleted_at.is_some() {
            return Err(StoreError::NotFound);
        }
        // A live sibling wearing the name is no refusal: the rename lands
        // on the first free `stem (2).ext` postfix instead.
        let name = free_file_name(
            &tx,
            &file.owner_id,
            file.folder_id.as_deref(),
            &name,
            Some(id),
        )
        .await?;
        let now = now_text()?;
        tx.execute(
            "UPDATE file SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, now, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &file.owner_id).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(file.owner_id)]);
        Ok(())
    }

    async fn move_file(&self, id: &str, folder_id: Option<&str>) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let file = file_row(&tx, id).await?.ok_or(StoreError::NotFound)?;
        if file.deleted_at.is_some() {
            return Err(StoreError::NotFound);
        }
        check_live_parent(&tx, &file.owner_id, folder_id).await?;
        // A live child of the destination wearing the file's name is no
        // obstacle: the move lands on the first free `stem (2).ext` postfix.
        let name = free_file_name(&tx, &file.owner_id, folder_id, &file.name, Some(id)).await?;
        let now = now_text()?;
        tx.execute(
            "UPDATE file SET folder_id = ?1, name = ?2, updated_at = ?3 WHERE id = ?4",
            params![folder_id, name, now, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &file.owner_id).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Library(file.owner_id)]);
        Ok(())
    }

    async fn delete_file(&self, id: &str) -> Result<()> {
        let owner = {
            let file = self.file(id).await?.ok_or(StoreError::NotFound)?;
            if file.deleted_at.is_some() {
                return Ok(());
            }
            file.owner_id.clone()
        };
        let now = now_text()?;
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&conn, &owner).await?;
        drop(conn);
        self.announce([Topic::Library(owner.clone()), Topic::Trash(owner)]);
        Ok(())
    }

    async fn file_bytes(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT 1 FROM file WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let known = rows.next().await.map_err(backend)?.is_some();
        drop(conn);
        if !known {
            return Ok(None);
        }
        // A row whose file went missing is reported at boot; a missing blob
        // answers "nothing to serve" rather than failing the panel.
        self.blobs
            .get(&format!("files/{id}"))
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn file_stream(&self, id: &str, start: u64, len: u64) -> Result<Option<super::FileSpan>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT 1 FROM file WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let known = rows.next().await.map_err(backend)?.is_some();
        drop(conn);
        if !known {
            return Ok(None);
        }
        // Same answer as `file_bytes`: a missing file is "nothing to
        // serve", not a stack. The span clamps against the real blob, not
        // the row: the span served is the span that exists.
        self.blobs
            .span(&format!("files/{id}"), start, len)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn record_download(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT owner_id FROM file WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let owner = match rows.next().await.map_err(backend)? {
            Some(row) => text(&row, 0)?,
            None => return Err(StoreError::NotFound),
        };
        drop(rows);
        conn.execute(
            "UPDATE file SET download_count = download_count + 1 WHERE id = ?1",
            params![id],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Library(owner)]);
        Ok(())
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        match self
            .one_row("SELECT value FROM setting WHERE key = ?1", params![key])
            .await?
        {
            Some(row) => Ok(Some(text(&row, 0)?)),
            None => Ok(None),
        }
    }

    async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn delete_setting(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM setting WHERE key = ?1", params![key])
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn thumb_bytes(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT thumb_state FROM file WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let ready = match rows.next().await.map_err(backend)? {
            Some(row) => text(&row, 0)? == ThumbState::Ready.as_str(),
            None => return Ok(None),
        };
        drop(conn);
        if !ready {
            return Ok(None);
        }
        self.blobs
            .get(&format!("thumbs/{id}"))
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn media_derivative(&self, id: &str) -> Result<Option<super::MediaDerivative>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT mime FROM file WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let mime = match rows.next().await.map_err(backend)? {
            Some(row) => text(&row, 0)?,
            None => return Ok(None),
        };
        drop(conn);
        // The build's misses are the module's own log lines; the store's
        // answer to every one of them is "serve the original".
        Ok(previews::ensure(&self.storage, self.blobs.as_ref(), id, &mime).await)
    }

    async fn derivative_stream(
        &self,
        id: &str,
        start: u64,
        len: u64,
    ) -> Result<Option<super::FileSpan>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT 1 FROM file WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let known = rows.next().await.map_err(backend)?.is_some();
        drop(conn);
        if !known {
            return Ok(None);
        }
        // Same answer as `file_stream`: a missing derivative is "nothing
        // to serve", not a stack. The span clamps against the real blob.
        Ok(previews::stream(self.blobs.as_ref(), id, start, len).await)
    }

    async fn list_trash(&self, owner_id: &str) -> Result<Listing> {
        let conn = self.conn.lock().await;
        let mut folders = Vec::new();
        let mut files = Vec::new();
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {FOLDER_COLUMNS} FROM folder \
                     WHERE owner_id = ?1 AND deleted_at IS NOT NULL \
                     ORDER BY deleted_at DESC, id"
                ),
                params![owner_id],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            folders.push(folder_from(&row)?);
        }
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {FILE_COLUMNS} FROM file \
                     WHERE owner_id = ?1 AND deleted_at IS NOT NULL \
                     ORDER BY deleted_at DESC, id"
                ),
                params![owner_id],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            files.push(file_from(&row)?);
        }
        Ok(Listing { folders, files })
    }

    async fn restore_file(&self, id: &str) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let file = file_row(&tx, id).await?.ok_or(StoreError::NotFound)?;
        if file.deleted_at.is_none() {
            return Ok(());
        }
        // A file restored under trash would be trashed the moment it
        // arrives: every ancestor folder must be live first.
        if let Some(folder_id) = file.folder_id.as_deref() {
            ancestor_trashed(&tx, folder_id).await?;
        }
        // A live sibling wearing the name is no refusal: the restore lands
        // on the first free `stem (2).ext` postfix instead.
        let name = free_file_name(
            &tx,
            &file.owner_id,
            file.folder_id.as_deref(),
            &file.name,
            Some(id),
        )
        .await?;
        let now = now_text()?;
        tx.execute(
            "UPDATE file SET name = ?1, deleted_at = NULL, updated_at = ?2 WHERE id = ?3",
            params![name, now, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &file.owner_id).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([
            Topic::Library(file.owner_id.clone()),
            Topic::Trash(file.owner_id),
        ]);
        Ok(())
    }

    async fn restore_folder(&self, id: &str) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let folder = folder_row(&tx, id).await?.ok_or(StoreError::NotFound)?;
        let Some(trashed_at) = folder.deleted_at else {
            return Ok(());
        };
        let trashed_at = stamp(trashed_at)?;
        // Same refusal as the file: no resurrection into a trash that never
        // left. Only the ancestors above the folder are asked — the subtree
        // below comes back with it.
        if let Some(parent_id) = folder.parent_id.as_deref() {
            ancestor_trashed(&tx, parent_id).await?;
        }
        let now = now_text()?;
        // Only the cascade comes back: descendants wear the folder's own
        // trash timestamp when the folder took them with it, while anything
        // trashed individually before keeps its earlier moment and stays
        // trashed.
        tx.execute(
            "WITH RECURSIVE sub(id) AS ( \
               SELECT ?1 \
               UNION ALL \
               SELECT f.id FROM folder f JOIN sub s ON f.parent_id = s.id \
             ) \
             UPDATE folder SET deleted_at = NULL \
             WHERE id IN (SELECT id FROM sub) AND deleted_at = ?2",
            params![id, trashed_at.clone()],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "WITH RECURSIVE sub(id) AS ( \
               SELECT ?1 \
               UNION ALL \
               SELECT f.id FROM folder f JOIN sub s ON f.parent_id = s.id \
             ) \
             UPDATE file SET deleted_at = NULL, updated_at = ?2 \
             WHERE folder_id IN (SELECT id FROM sub) AND deleted_at = ?3",
            params![id, now, trashed_at],
        )
        .await
        .map_err(backend)?;
        // A live sibling wearing the folder's name is no refusal: the
        // restored folder takes the first free `name (2)` postfix. The
        // cascade below needs no fix-up — no live row can sit under a
        // trashed folder, so only this top folder can collide.
        let name = free_folder_name(
            &tx,
            &folder.owner_id,
            folder.parent_id.as_deref(),
            &folder.name,
            Some(id),
        )
        .await?;
        tx.execute(
            "UPDATE folder SET name = ?1 WHERE id = ?2",
            params![name, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &folder.owner_id).await?;
        tx.commit().await.map_err(backend)?;
        self.announce([
            Topic::Library(folder.owner_id.clone()),
            Topic::Trash(folder.owner_id),
        ]);
        Ok(())
    }

    async fn purge_file(&self, id: &str) -> Result<bool> {
        let owner = {
            let file = self.file(id).await?.ok_or(StoreError::NotFound)?;
            // A live file is never purged — trash it first. This answers
            // `false` rather than failing, because the purge's contract is
            // "take the trash", and a live file is not trash.
            if file.deleted_at.is_none() {
                return Ok(false);
            }
            file.owner_id.clone()
        };
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let deleted = delete_file_rows(&tx, &[id.to_string()], &TrashGuard::Trashed).await?;
        if deleted.is_empty() {
            // A restore landed between the live-check and this
            // transaction: the file is live again, and a live file is not
            // trash — the purge answers so truthfully.
            return Ok(false);
        }
        refresh_usage(&tx, &owner).await?;
        tx.commit().await.map_err(backend)?;
        // After the delete: the bytes may only follow a delete that
        // committed, or a crash in between would leave a row whose file is
        // gone. Best-effort — a file that survives is orphaned bytes the
        // boot sweep collects.
        let _ = self
            .blobs
            .delete(&[
                &format!("files/{id}"),
                &format!("thumbs/{id}"),
                &format!("previews/{id}"),
            ])
            .await;
        self.announce([Topic::Library(owner.clone()), Topic::Trash(owner)]);
        Ok(true)
    }

    async fn purge_folder(&self, id: &str) -> Result<u64> {
        let folder = self.folder(id).await?.ok_or(StoreError::NotFound)?;
        // A live folder is never purged — trash it first.
        if folder.deleted_at.is_none() {
            return Err(StoreError::NotFound);
        }
        let owner = folder.owner_id.clone();
        let conn = self.conn.lock().await;
        // The trashed subtree: the folder itself and every trashed folder
        // under it. A live row under trash cannot exist — nothing is created
        // or moved under a trashed folder, and restores cascade — so the
        // trashed set is the whole subtree; the foreign key would say so
        // loudly if that ever stopped being true.
        let mut folder_rows = Vec::new();
        let mut rows = conn
            .query(
                "WITH RECURSIVE sub(id) AS ( \
                   SELECT ?1 \
                   UNION ALL \
                   SELECT f.id FROM folder f JOIN sub s ON f.parent_id = s.id \
                 ) \
                 SELECT id, parent_id FROM folder                  WHERE owner_id = ?2 AND id IN (SELECT id FROM sub) AND deleted_at IS NOT NULL",
                params![id, owner.as_str()],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            folder_rows.push((text(&row, 0)?, opt_text(&row, 1)?));
        }
        drop(rows);
        let mut file_ids = Vec::new();
        let mut rows = conn
            .query(
                "WITH RECURSIVE sub(id) AS ( \
                   SELECT ?1 \
                   UNION ALL \
                   SELECT f.id FROM folder f JOIN sub s ON f.parent_id = s.id \
                 ) \
                 SELECT id FROM file                  WHERE owner_id = ?2 AND folder_id IN (SELECT id FROM sub)                  AND deleted_at IS NOT NULL",
                params![id, owner.as_str()],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            file_ids.push(text(&row, 0)?);
        }
        drop(rows);
        drop(conn);
        let folder_ids = order_deepest_first(&folder_rows);
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let deleted_files = delete_file_rows(&tx, &file_ids, &TrashGuard::Trashed).await?;
        let deleted_folders = delete_folder_rows(&tx, &folder_ids, &TrashGuard::Trashed).await?;
        refresh_usage(&tx, &owner).await?;
        tx.commit().await.map_err(backend)?;
        // After the delete: bytes may only follow a delete that committed,
        // and only for rows that went — a restore that landed in the read
        // window keeps its bytes.
        for file_id in &deleted_files {
            let _ = self
                .blobs
                .delete(&[
                    &format!("files/{file_id}"),
                    &format!("thumbs/{file_id}"),
                    &format!("previews/{file_id}"),
                ])
                .await;
        }
        let purged = (deleted_files.len() + deleted_folders.len()) as u64;
        self.announce([Topic::Library(owner.clone()), Topic::Trash(owner)]);
        Ok(purged)
    }

    async fn purge_expired(&self, before: OffsetDateTime) -> Result<u64> {
        let before = stamp(before)?;
        let conn = self.conn.lock().await;
        let (file_ids, folder_rows) = select_expired(&conn, before.as_str()).await?;
        drop(conn);
        if file_ids.is_empty() && folder_rows.is_empty() {
            return Ok(0);
        }
        let (deleted_files, deleted_folders) = commit_trash_purge(
            self,
            file_ids,
            folder_rows,
            &TrashGuard::Expired {
                before: before.as_str(),
            },
        )
        .await?;
        Ok((deleted_files.len() + deleted_folders.len()) as u64)
    }

    async fn empty_trash(&self, owner_id: &str) -> Result<u64> {
        let conn = self.conn.lock().await;
        let (file_ids, folder_rows) = select_trash(&conn, owner_id).await?;
        drop(conn);
        if file_ids.is_empty() && folder_rows.is_empty() {
            return Ok(0);
        }
        let (deleted_files, deleted_folders) = commit_trash_purge(
            self,
            file_ids
                .into_iter()
                .map(|id| (id, owner_id.to_string()))
                .collect::<Vec<_>>(),
            folder_rows
                .into_iter()
                .map(|(id, parent)| (id, owner_id.to_string(), parent))
                .collect::<Vec<_>>(),
            &TrashGuard::Trashed,
        )
        .await?;
        Ok((deleted_files.len() + deleted_folders.len()) as u64)
    }

    async fn create_share_link(
        &self,
        created_by: &str,
        kind: ShareKind,
        target_id: &str,
        can_download: bool,
        expires_at: Option<OffsetDateTime>,
        password_hash: Option<String>,
    ) -> Result<CreatedLink> {
        // The sharer must own a live target: a stranger's, a missing, or a
        // trashed target refuses before any token exists to leak.
        match target_owner(&self.conn, kind, target_id).await? {
            Some(owner) if owner == created_by => {}
            Some(_) => return Err(StoreError::CrossOwner),
            None => return Err(StoreError::NotFound),
        }
        let token = new_token();
        let token_hash = hash_share_token(&token);
        let id = Ulid::new().to_string();
        let now = now_text()?;
        let expires = expires_at.map(stamp).transpose()?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO share_link (id, token_hash, kind, target_id, created_by, \
             can_download, created_at, expires_at, revoked_at, password_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
            params![
                id.clone(),
                token_hash,
                kind.as_str(),
                target_id,
                created_by,
                if can_download { 1 } else { 0 },
                now,
                expires,
                password_hash,
            ],
        )
        .await
        .map_err(backend)?;
        let mut rows = conn
            .query(
                &format!("SELECT {LINK_COLUMNS} FROM share_link WHERE id = ?1"),
                params![id],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let link = link_from(&row)?;
        drop(rows);
        drop(conn);
        self.announce([Topic::Shares(created_by.to_string())]);
        Ok(CreatedLink { link, token })
    }

    async fn revoke_share_link(&self, id: &str) -> Result<()> {
        let created_by = {
            let row = self
                .one_row(
                    &format!("SELECT {LINK_COLUMNS} FROM share_link WHERE id = ?1"),
                    params![id],
                )
                .await?;
            match row {
                Some(row) => link_from(&row)?.created_by.clone(),
                None => return Err(StoreError::NotFound),
            }
        };
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE share_link SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
            params![now_text()?, id],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Shares(created_by)]);
        Ok(())
    }

    async fn share_links(&self, owner_id: &str) -> Result<Vec<ShareLink>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM share_link \
                     WHERE created_by = ?1 ORDER BY created_at DESC, id"
                ),
                params![owner_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(link_from(&row)?);
        }
        Ok(out)
    }

    async fn resolve_share_link(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ShareLink>> {
        let sql = format!("SELECT {LINK_COLUMNS} FROM share_link WHERE token_hash = ?1");
        match self.one_row(&sql, params![token_hash]).await? {
            Some(row) => {
                let link = link_from(&row)?;
                // A dead link and a wrong token both answer `None`: telling a
                // stranger which tokens exist is the leak the distinction
                // would be.
                if link.is_live(now) {
                    Ok(Some(link))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    async fn shared_target_ids(&self, owner_id: &str) -> Result<Vec<(ShareKind, String)>> {
        let now = OffsetDateTime::now_utc();
        let mut found: std::collections::HashSet<(ShareKind, String)> =
            std::collections::HashSet::new();
        let conn = self.conn.lock().await;
        // Link liveness is decided here rather than in SQL, the way
        // resolve_share_link decides it: one definition of live, the row's
        // own timestamps.
        let mut rows = conn
            .query(
                &format!("SELECT {LINK_COLUMNS} FROM share_link WHERE created_by = ?1"),
                params![owner_id],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            let link = link_from(&row)?;
            if link.is_live(now) {
                found.insert((link.kind, link.target_id));
            }
        }
        drop(rows);
        // Grants name no owner — the target's own row does — so the ids are
        // matched against the person's files and folders directly.
        let mut rows = conn
            .query(
                "SELECT DISTINCT kind, target_id FROM share_user \
                 WHERE (kind = 'file' AND target_id IN (SELECT id FROM file WHERE owner_id = ?1)) \
                    OR (kind = 'folder' AND target_id IN (SELECT id FROM folder WHERE owner_id = ?1))",
                params![owner_id],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            found.insert((ShareKind::parse(&text(&row, 0)?)?, text(&row, 1)?));
        }
        drop(rows);
        drop(conn);
        Ok(found.into_iter().collect())
    }

    async fn add_share_user(
        &self,
        caller_id: &str,
        kind: ShareKind,
        target_id: &str,
        user_id: &str,
        can_download: bool,
    ) -> Result<()> {
        // The caller must own a live target — the same rule as
        // create_share_link: a stranger's, a missing, or a trashed target
        // refuses before any grant names it. The grantee must exist, or the
        // row would point at nobody.
        let owner = match target_owner(&self.conn, kind, target_id).await? {
            Some(owner) if owner == caller_id => owner,
            Some(_) => return Err(StoreError::CrossOwner),
            None => return Err(StoreError::NotFound),
        };
        if self.user(user_id).await?.is_none() {
            return Err(StoreError::NotFound);
        }
        if owner == user_id {
            // Sharing with the owner is a no-op with a success answer: the
            // owner already sees everything.
            return Ok(());
        }
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO share_user (kind, target_id, user_id, can_download, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (kind, target_id, user_id) \
             DO UPDATE SET can_download = excluded.can_download",
            params![
                kind.as_str(),
                target_id,
                user_id,
                if can_download { 1 } else { 0 },
                now_text()?
            ],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Shares(owner), Topic::Shares(user_id.to_string())]);
        Ok(())
    }

    async fn remove_share_user(
        &self,
        kind: ShareKind,
        target_id: &str,
        user_id: &str,
    ) -> Result<()> {
        // The owner is read before the delete: after it there may be nothing
        // left to announce to. Trash does not stop an unshare — the grant is
        // gone either way, and the owner is still listening.
        let owner = row_owner(&self.conn, kind, target_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM share_user WHERE kind = ?1 AND target_id = ?2 AND user_id = ?3",
            params![kind.as_str(), target_id, user_id],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Shares(owner), Topic::Shares(user_id.to_string())]);
        Ok(())
    }

    async fn shares_for_target(
        &self,
        caller_id: &str,
        kind: ShareKind,
        target_id: &str,
    ) -> Result<Vec<ShareUser>> {
        // The caller must own a live target — the same rule as the sibling
        // share writes: a stranger's target is cross-owner, a missing or
        // trashed one not-found.
        match target_owner(&self.conn, kind, target_id).await? {
            Some(owner) if owner == caller_id => {}
            Some(_) => return Err(StoreError::CrossOwner),
            None => return Err(StoreError::NotFound),
        }
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT kind, target_id, user_id, can_download, created_at FROM share_user \
                 WHERE kind = ?1 AND target_id = ?2 ORDER BY created_at DESC, user_id",
                params![kind.as_str(), target_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(ShareUser {
                kind: ShareKind::parse(&text(&row, 0)?)?,
                target_id: text(&row, 1)?,
                user_id: text(&row, 2)?,
                can_download: row.get::<i64>(3).map_err(backend)? != 0,
                created_at: parse_stamp(&text(&row, 4)?)?,
            });
        }
        Ok(out)
    }

    async fn shares_for_user(&self, user_id: &str) -> Result<Vec<SharedItem>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT kind, target_id, can_download, created_at FROM share_user \
                 WHERE user_id = ?1 ORDER BY created_at DESC",
                params![user_id],
            )
            .await
            .map_err(backend)?;
        let mut grants = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            grants.push((
                ShareKind::parse(&text(&row, 0)?)?,
                text(&row, 1)?,
                row.get::<i64>(2).map_err(backend)? != 0,
                parse_stamp(&text(&row, 3)?)?,
            ));
        }
        drop(rows);
        // Targets that went missing or were trashed since the grant fall out
        // silently: the grant row stays (the owner may restore the target),
        // but the listing shows only what opens today.
        let mut out = Vec::new();
        for (kind, target_id, can_download, created_at) in grants {
            let (name, mime, owner_id) = match kind {
                ShareKind::File => {
                    let mut rows = conn
                        .query(
                            "SELECT name, mime, owner_id FROM file \
                             WHERE id = ?1 AND deleted_at IS NULL",
                            params![target_id.as_str()],
                        )
                        .await
                        .map_err(backend)?;
                    match rows.next().await.map_err(backend)? {
                        Some(row) => (text(&row, 0)?, Some(text(&row, 1)?), text(&row, 2)?),
                        None => continue,
                    }
                }
                ShareKind::Folder => {
                    let mut rows = conn
                        .query(
                            "SELECT name, owner_id FROM folder \
                             WHERE id = ?1 AND deleted_at IS NULL",
                            params![target_id.as_str()],
                        )
                        .await
                        .map_err(backend)?;
                    match rows.next().await.map_err(backend)? {
                        Some(row) => (text(&row, 0)?, None, text(&row, 1)?),
                        None => continue,
                    }
                }
            };
            if owner_id == user_id {
                continue;
            }
            out.push(SharedItem {
                kind,
                target_id,
                name,
                mime,
                owner_id,
                can_download,
                created_at,
            });
        }
        Ok(out)
    }

    async fn can_see(&self, kind: ShareKind, target_id: &str, user_id: &str) -> Result<bool> {
        // Ownership survives the trash: the owner sees the row trashed or
        // not. Anyone else needs a live grant onto the live target or a live
        // folder above it — and a missing target is simply not seen, never
        // distinguished.
        match row_owner(&self.conn, kind, target_id).await? {
            Some(owner) if owner == user_id => Ok(true),
            Some(_) => {
                if target_owner(&self.conn, kind, target_id).await?.is_none() {
                    return Ok(false);
                }
                let conn = self.conn.lock().await;
                Ok(effective_grant(&conn, kind, target_id, user_id)
                    .await?
                    .is_some())
            }
            None => Ok(false),
        }
    }

    async fn can_download(&self, kind: ShareKind, target_id: &str, user_id: &str) -> Result<bool> {
        // The owner always downloads. Anyone else downloads only through a
        // grant whose `can_download` is set — on the target or above it. A
        // view-only grant opens the page but not the bytes.
        match row_owner(&self.conn, kind, target_id).await? {
            Some(owner) if owner == user_id => Ok(true),
            Some(_) => {
                if target_owner(&self.conn, kind, target_id).await?.is_none() {
                    return Ok(false);
                }
                let conn = self.conn.lock().await;
                Ok(effective_grant(&conn, kind, target_id, user_id)
                    .await?
                    .is_some_and(|granted| granted))
            }
            None => Ok(false),
        }
    }
    async fn create_upload_session(
        &self,
        owner_id: &str,
        folder_id: Option<&str>,
        name: &str,
        size_bytes: u64,
    ) -> Result<UploadSession> {
        let name = label_of(name);
        // The quota is checked before anything exists on disk: refusing now
        // leaves nothing to clean up, and the finish rechecks anyway for
        // whatever filled up while the chunks were arriving.
        let conn = self.conn.lock().await;
        check_live_parent_on(&conn, owner_id, folder_id).await?;
        let (quota, used) = check_quota(&conn, owner_id).await?;
        fit_quota(quota, used, size_bytes)?;
        let id = Ulid::new().to_string();
        let now = OffsetDateTime::now_utc();
        let created = stamp(now)?;
        let expires = stamp(now + time::Duration::hours(UPLOAD_TTL_HOURS as i64))?;
        conn.execute(
            "INSERT INTO upload_session (id, owner_id, folder_id, name, size_bytes, \
             chunk_size, received_bytes, state, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'active', ?7, ?8)",
            params![
                id.clone(),
                owner_id,
                folder_id,
                name,
                size_bytes as i64,
                CHUNK_SIZE as i64,
                created,
                expires
            ],
        )
        .await
        .map_err(backend)?;
        let mut rows = conn
            .query(
                &format!("SELECT {SESSION_COLUMNS} FROM upload_session WHERE id = ?1"),
                params![id.as_str()],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let session = session_from(&row)?;
        drop(rows);
        drop(conn);
        std::fs::create_dir_all(session_dir(&self.storage, &id))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(session)
    }

    async fn upload_session(&self, id: &str) -> Result<Option<UploadSession>> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM upload_session WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => Ok(Some(session_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn record_chunk(&self, id: &str, index: u64, bytes: &[u8]) -> Result<UploadSession> {
        let session = self.upload_session(id).await?.ok_or(StoreError::NotFound)?;
        if session.state != UploadState::Active || session.expires_at <= OffsetDateTime::now_utc() {
            return Err(StoreError::UploadExpired);
        }
        let count = chunk_count(session.size_bytes);
        // Every refusal here is about shape, never about content: the bytes
        // are opaque until the finish sniffs them.
        if index >= count {
            return Err(StoreError::BadChunk);
        }
        if bytes.len() as u64 != expected_chunk_len(session.size_bytes, index) {
            return Err(StoreError::BadChunk);
        }
        let dir = session_dir(&self.storage, &id);
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Backend(e.to_string()))?;
        write_file_atomic(&chunk_path(&self.storage, &id, index), bytes)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let received = staged_received(&self.storage, &id);
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE upload_session SET received_bytes = ?1 WHERE id = ?2",
            params![received as i64, id],
        )
        .await
        .map_err(backend)?;
        let mut rows = conn
            .query(
                &format!("SELECT {SESSION_COLUMNS} FROM upload_session WHERE id = ?1"),
                params![id],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let session = session_from(&row)?;
        drop(rows);
        drop(conn);
        Ok(session)
    }

    async fn uploaded_chunks(&self, id: &str) -> Result<Vec<u64>> {
        self.upload_session(id).await?.ok_or(StoreError::NotFound)?;
        let mut indexes = staged_indexes(&self.storage, id);
        indexes.sort_unstable();
        Ok(indexes)
    }

    async fn finish_upload(&self, id: &str) -> Result<File> {
        let session = self.upload_session(id).await?.ok_or(StoreError::NotFound)?;
        if session.state != UploadState::Active || session.expires_at <= OffsetDateTime::now_utc() {
            return Err(StoreError::UploadExpired);
        }
        // Every chunk present, every chunk exact: the finish assembles what
        // the session promised, byte for byte, or refuses with the chunks
        // still staged for another attempt.
        let count = chunk_count(session.size_bytes);
        for index in 0..count {
            let len = std::fs::metadata(chunk_path(&self.storage, &id, index))
                .map(|m| m.len())
                .unwrap_or(u64::MAX);
            if len != expected_chunk_len(session.size_bytes, index) {
                return Err(StoreError::BadChunk);
            }
        }
        // Assemble through a temp name in the files directory: a crash
        // mid-assemble leaves a temp the sweep deletes, never a half file
        // wearing a real name.
        let file_id = Ulid::new().to_string();
        // Staged in the files directory under a temp name: the sniffer and
        // the thumbnailer need a real local file, and the adopt that places
        // the assembled bytes is a same-tree rename away.
        let tmp = file_path(&self.storage, &file_id).with_extension(format!("{}.tmp", Ulid::new()));
        {
            use std::io::Write as _;
            let mut out =
                std::fs::File::create(&tmp).map_err(|e| StoreError::Backend(e.to_string()))?;
            let mut total = 0u64;
            for index in 0..count {
                let mut chunk = std::fs::File::open(chunk_path(&self.storage, &id, index))
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                total += std::io::copy(&mut chunk, &mut out)
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
            }
            out.flush()
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            drop(out);
            if total != session.size_bytes {
                let _ = std::fs::remove_file(&tmp);
                return Err(StoreError::BadChunk);
            }
        }
        // The mime comes from the assembled bytes, never from anything the
        // uploader said along the way.
        let mut head = vec![0u8; 1024 * 1024];
        let head_len = read_up_to(&tmp, &mut head).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            StoreError::Backend(e.to_string())
        })?;
        head.truncate(head_len.min(session.size_bytes as usize));
        let mime = sniff::sniff(&head).to_string();
        let thumb_bytes = if !thumbs::thumbnailed(&mime) || session.size_bytes > THUMB_SOURCE_CAP {
            None
        } else if thumbs::is_video_mime(&mime) {
            // The assembled file is already on disk: frame it in place
            // rather than reading it whole into memory first.
            thumbs::thumbnail_for_video_file(&tmp)
        } else {
            match std::fs::read(&tmp) {
                Ok(full) => thumbs::thumbnail_for_bytes(&full),
                Err(_) => None,
            }
        };
        let thumb_state = if !thumbs::thumbnailed(&mime) {
            ThumbState::None
        } else if session.size_bytes > THUMB_SOURCE_CAP {
            ThumbState::Failed
        } else if thumb_bytes.is_some() {
            ThumbState::Ready
        } else if thumbs::is_video_mime(&mime) && !thumbs::ffmpeg_available() {
            ThumbState::None
        } else {
            ThumbState::Failed
        };
        // IMMEDIATE: the quota recheck, the name check and the row writes
        // are one write set — the library may have filled while the chunks
        // were arriving. Every check runs before the assembled bytes take
        // their real name: a refusal leaves files/ untouched, with the
        // chunks still staged for another attempt.
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let vetted: Result<String> = async {
            // The session was vetted outside this transaction; the window
            // since then belongs to abort, expiry and a second finish. The
            // row itself is the last word under the write lock.
            finishable_session(&tx, id).await?;
            check_live_parent(&tx, &session.owner_id, session.folder_id.as_deref()).await?;
            let (quota, used) = check_quota(&tx, &session.owner_id).await?;
            fit_quota(quota, used, session.size_bytes)?;
            // A live sibling wearing the session's name is no refusal: the
            // finished file takes the first free `stem (2).ext` postfix.
            free_file_name(
                &tx,
                &session.owner_id,
                session.folder_id.as_deref(),
                &session.name,
                None,
            )
            .await
        }
        .await;
        let name = match vetted {
            Ok(name) => name,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        if let Err(e) = self
            .blobs
            .adopt(&format!("files/{file_id}"), &tmp, session.size_bytes)
            .await
        {
            // A placement that failed leaves the assembled file staged:
            // the next attempt starts clean rather than sweeping around a
            // half-placed name.
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Backend(e.to_string()));
        }
        let mut thumb_placed = false;
        if let Some(thumb) = thumb_bytes.as_ref() {
            if self
                .blobs
                .put(&format!("thumbs/{file_id}"), thumb)
                .await
                .is_ok()
            {
                thumb_placed = true;
            } else {
                let _ = self.blobs.delete(&[&format!("thumbs/{file_id}")]).await;
            }
        }
        let stored_thumb = if thumb_state == ThumbState::Ready && thumb_placed {
            ThumbState::Ready
        } else if thumb_state == ThumbState::Ready {
            ThumbState::Failed
        } else {
            thumb_state
        };
        let now = now_text()?;
        let written = tx
            .execute(
                "INSERT INTO file (id, owner_id, folder_id, name, mime, size_bytes, \
                 thumb_state, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, NULL)",
                params![
                    file_id.clone(),
                    session.owner_id.as_str(),
                    session.folder_id.as_deref(),
                    name.as_str(),
                    mime,
                    session.size_bytes as i64,
                    stored_thumb.as_str(),
                    now
                ],
            )
            .await;
        if let Err(e) = written {
            // The row was never born: the assembled bytes must not outlive
            // it, but the chunks stay staged — the attempt, not the upload,
            // is what failed.
            let _ = self
                .blobs
                .delete(&[
                    &format!("files/{file_id}"),
                    &format!("thumbs/{file_id}"),
                    &format!("previews/{file_id}"),
                ])
                .await;
            return Err(backend(e));
        }
        tx.execute(
            "UPDATE upload_session SET state = 'done', received_bytes = ?1 WHERE id = ?2",
            params![session.size_bytes as i64, id],
        )
        .await
        .map_err(backend)?;
        refresh_usage(&tx, &session.owner_id).await?;
        let mut rows = tx
            .query(
                &format!("SELECT {FILE_COLUMNS} FROM file WHERE id = ?1"),
                params![file_id],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;
        let file = file_from(&row)?;
        drop(rows);
        tx.commit().await.map_err(backend)?;
        // After the commit: the chunks served their purpose.
        let _ = std::fs::remove_dir_all(session_dir(&self.storage, &id));
        self.announce([Topic::Library(session.owner_id)]);
        Ok(file)
    }

    async fn abort_upload(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE upload_session SET state = 'aborted' WHERE id = ?1 AND state = 'active'",
            params![id],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        let _ = std::fs::remove_dir_all(session_dir(&self.storage, &id));
        Ok(())
    }

    async fn prune_expired_uploads(&self, now: OffsetDateTime) -> Result<u64> {
        let now = stamp(now)?;
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT id FROM upload_session WHERE state = 'active' AND expires_at <= ?1",
                params![now],
            )
            .await
            .map_err(backend)?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            ids.push(text(&row, 0)?);
        }
        drop(rows);
        for id in &ids {
            conn.execute(
                "UPDATE upload_session SET state = 'aborted' WHERE id = ?1",
                params![id.as_str()],
            )
            .await
            .map_err(backend)?;
        }
        drop(conn);
        for id in &ids {
            let _ = std::fs::remove_dir_all(session_dir(&self.storage, &id));
        }
        Ok(ids.len() as u64)
    }

    async fn search(&self, owner_id: &str, query: &str, limit: u32) -> Result<Listing> {
        // An empty query matches everything under LIKE `%%`; the route keeps
        // that from ever arriving, and the store answers it with nothing
        // rather than the whole library.
        if query.trim().is_empty() {
            return Ok(Listing {
                folders: Vec::new(),
                files: Vec::new(),
            });
        }
        let pattern = like_pattern(query.trim());
        let limit = limit.clamp(1, 100) as i64;
        let conn = self.conn.lock().await;
        let mut folders = Vec::new();
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {FOLDER_COLUMNS} FROM folder \
                     WHERE owner_id = ?1 AND deleted_at IS NULL AND name LIKE ?2 ESCAPE '\\' \
                     ORDER BY name, id LIMIT ?3"
                ),
                params![owner_id, pattern.as_str(), limit],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            folders.push(folder_from(&row)?);
        }
        let mut files = Vec::new();
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {FILE_COLUMNS} FROM file \
                     WHERE owner_id = ?1 AND deleted_at IS NULL AND name LIKE ?2 ESCAPE '\\' \
                     ORDER BY name, id LIMIT ?3"
                ),
                params![owner_id, pattern, limit],
            )
            .await
            .map_err(backend)?;
        while let Some(row) = rows.next().await.map_err(backend)? {
            files.push(file_from(&row)?);
        }
        Ok(Listing { folders, files })
    }
} // impl Store for TursoStore

/// One folder row for a write to vet: locks nothing itself, runs on the
/// caller's connection — plain or transactional.
async fn folder_row(conn: &Connection, id: &str) -> Result<Option<Folder>> {
    let mut rows = conn
        .query(
            &format!("SELECT {FOLDER_COLUMNS} FROM folder WHERE id = ?1"),
            params![id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => Ok(Some(folder_from(&row)?)),
        None => Ok(None),
    }
}

/// One file row for a write to vet, same shape as [`folder_row`].
async fn file_row(conn: &Connection, id: &str) -> Result<Option<File>> {
    let mut rows = conn
        .query(
            &format!("SELECT {FILE_COLUMNS} FROM file WHERE id = ?1"),
            params![id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => Ok(Some(file_from(&row)?)),
        None => Ok(None),
    }
}

/// The parent of one folder, for the cycle and ancestor walks.
async fn parent_of(conn: &Connection, id: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query("SELECT parent_id FROM folder WHERE id = ?1", params![id])
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => Ok(opt_text(&row, 0)?),
        None => Ok(None),
    }
}

/// Vets a destination folder: missing is [`StoreError::NotFound`], another
/// owner's is [`StoreError::CrossOwner`], trashed is `NotFound` — trash is
/// invisible to writes that are not restore, purge or empty.
async fn check_live_parent(
    conn: &Connection,
    owner_id: &str,
    parent_id: Option<&str>,
) -> Result<()> {
    let Some(parent_id) = parent_id else {
        return Ok(());
    };
    match folder_row(conn, parent_id).await? {
        None => Err(StoreError::NotFound),
        Some(folder) if folder.owner_id != owner_id => Err(StoreError::CrossOwner),
        Some(folder) if folder.deleted_at.is_some() => Err(StoreError::NotFound),
        Some(_) => Ok(()),
    }
}

/// [`check_live_parent`] on the shared connection, for the single-statement
/// writes that take no transaction of their own.
async fn check_live_parent_on(
    conn: &tokio::sync::MutexGuard<'_, Connection>,
    owner_id: &str,
    parent_id: Option<&str>,
) -> Result<()> {
    check_live_parent(conn, owner_id, parent_id).await
}

/// Whether a live sibling file already wears `name`. Trashed rows do not
/// count — deleting a name frees it — and `exclude` skips the row being
/// renamed or moved. Folders and files keep separate namespaces: a folder
/// and a file may share a name in one directory.
async fn file_name_taken(
    conn: &Connection,
    owner_id: &str,
    folder_id: Option<&str>,
    name: &str,
    exclude: Option<&str>,
) -> Result<bool> {
    let exclude = exclude.unwrap_or("");
    let mut rows = match folder_id {
        Some(folder) => conn
            .query(
                "SELECT 1 FROM file WHERE owner_id = ?1 AND folder_id = ?2 \
                 AND name = ?3 AND deleted_at IS NULL AND id <> ?4",
                params![owner_id, folder, name, exclude],
            )
            .await
            .map_err(backend)?,
        None => conn
            .query(
                "SELECT 1 FROM file WHERE owner_id = ?1 AND folder_id IS NULL \
                 AND name = ?2 AND deleted_at IS NULL AND id <> ?3",
                params![owner_id, name, exclude],
            )
            .await
            .map_err(backend)?,
    };
    Ok(rows.next().await.map_err(backend)?.is_some())
}

/// Whether a live sibling folder already wears `name` — the folder half of
/// [`file_name_taken`], over `parent_id` (NULL is the root) instead of
/// `folder_id`. Same rules: trash does not count, `exclude` skips the row
/// being renamed, moved or restored.
async fn folder_name_taken(
    conn: &Connection,
    owner_id: &str,
    parent_id: Option<&str>,
    name: &str,
    exclude: Option<&str>,
) -> Result<bool> {
    let exclude = exclude.unwrap_or("");
    let mut rows = match parent_id {
        Some(parent) => conn
            .query(
                "SELECT 1 FROM folder WHERE owner_id = ?1 AND parent_id = ?2 \
                 AND name = ?3 AND deleted_at IS NULL AND id <> ?4",
                params![owner_id, parent, name, exclude],
            )
            .await
            .map_err(backend)?,
        None => conn
            .query(
                "SELECT 1 FROM folder WHERE owner_id = ?1 AND parent_id IS NULL \
                 AND name = ?2 AND deleted_at IS NULL AND id <> ?3",
                params![owner_id, name, exclude],
            )
            .await
            .map_err(backend)?,
    };
    Ok(rows.next().await.map_err(backend)?.is_some())
}

/// The name a new or renamed file actually takes: `want` when no live
/// sibling wears it, else the first free `stem (2).ext` postfix — `(3)`,
/// `(4)`, … when `(2)` is taken too. Runs inside the caller's write
/// transaction, so the check and the write are one write set.
async fn free_file_name(
    conn: &Connection,
    owner_id: &str,
    folder_id: Option<&str>,
    want: &str,
    exclude: Option<&str>,
) -> Result<String> {
    if !file_name_taken(conn, owner_id, folder_id, want, exclude).await? {
        return Ok(want.to_string());
    }
    let mut n = 2u32;
    loop {
        let candidate = postfixed_file_name(want, n);
        if !file_name_taken(conn, owner_id, folder_id, &candidate, exclude).await? {
            return Ok(candidate);
        }
        // Unreachable in practice — four billion live siblings — but the
        // loop must be total, so exhaustion is a backend error, never a
        // name refusal.
        n = n
            .checked_add(1)
            .ok_or_else(|| StoreError::Backend("too many siblings".into()))?;
    }
}

/// The folder half of [`free_file_name`]: `want` when free, else the first
/// free `want (2)` postfix.
async fn free_folder_name(
    conn: &Connection,
    owner_id: &str,
    parent_id: Option<&str>,
    want: &str,
    exclude: Option<&str>,
) -> Result<String> {
    if !folder_name_taken(conn, owner_id, parent_id, want, exclude).await? {
        return Ok(want.to_string());
    }
    let mut n = 2u32;
    loop {
        let candidate = postfixed_folder_name(want, n);
        if !folder_name_taken(conn, owner_id, parent_id, &candidate, exclude).await? {
            return Ok(candidate);
        }
        // Unreachable in practice — see `free_file_name` — but total.
        n = n
            .checked_add(1)
            .ok_or_else(|| StoreError::Backend("too many siblings".into()))?;
    }
}

/// Refuses with [`StoreError::AncestorTrashed`] while any folder at or above
/// `folder_id` wears trash. The walk starts at the folder itself, so callers
/// pass the parent chain's head and get the whole ancestry vetted.
async fn ancestor_trashed(conn: &Connection, folder_id: &str) -> Result<()> {
    let mut cursor = Some(folder_id.to_string());
    while let Some(current) = cursor {
        match folder_row(conn, &current).await? {
            None => return Ok(()),
            Some(folder) if folder.deleted_at.is_some() => {
                return Err(StoreError::AncestorTrashed);
            }
            Some(folder) => cursor = folder.parent_id,
        }
    }
    Ok(())
}

/// The download flag of the grant that opens `target_id` for `user_id`,
/// if any: the most permissive grant on the target or any live folder
/// above it wins. A folder grant covers everything under it, so seeing a
/// file means asking about its whole ancestry — and a download grant
/// anywhere on that chain opens the bytes, even under a view-only grant
/// on the target itself.
///
/// The walk is cycle-safe — ancestry cannot circle (a move into its own
/// descendant is refused), and the visited set makes that a fact rather than
/// a trust. Only live folders pass the grant down: a grant on trash opens
/// nothing below it.
async fn effective_grant(
    conn: &Connection,
    kind: ShareKind,
    target_id: &str,
    user_id: &str,
) -> Result<Option<bool>> {
    let mut chain = vec![(kind, target_id.to_string())];
    let mut cursor: Option<String> = match kind {
        ShareKind::Folder => Some(target_id.to_string()),
        ShareKind::File => file_row(conn, target_id)
            .await?
            .and_then(|file| file.folder_id),
    };
    let mut visited = std::collections::HashSet::new();
    while let Some(current) = cursor {
        if !visited.insert(current.clone()) {
            break;
        }
        match folder_row(conn, &current).await? {
            Some(folder) if folder.deleted_at.is_none() => {
                chain.push((ShareKind::Folder, current));
                cursor = folder.parent_id;
            }
            _ => break,
        }
    }
    // Most permissive wins across the whole chain: every grant is read and
    // any download flag opens the bytes. Visibility only needs one grant
    // anywhere, so callers keep reading `is_some` for `can_see` and the flag
    // for `can_download`.
    let mut seen = false;
    let mut downloadable = false;
    for (kind, id) in chain {
        let mut rows = conn
            .query(
                "SELECT can_download FROM share_user \
                 WHERE kind = ?1 AND target_id = ?2 AND user_id = ?3",
                params![kind.as_str(), id, user_id],
            )
            .await
            .map_err(backend)?;
        if let Some(row) = rows.next().await.map_err(backend)? {
            seen = true;
            if row.get::<i64>(0).map_err(backend)? != 0 {
                downloadable = true;
            }
        }
    }
    Ok(seen.then_some(downloadable))
}

/// The owner of a share target's row, trashed or not — the ownership half
/// of [`Store::can_see`]. `None` only when there is no such row at all.
async fn row_owner(
    conn: &tokio::sync::Mutex<Connection>,
    kind: ShareKind,
    target_id: &str,
) -> Result<Option<String>> {
    let conn = conn.lock().await;
    let mut rows = match kind {
        ShareKind::File => conn
            .query(
                "SELECT owner_id FROM file WHERE id = ?1",
                params![target_id],
            )
            .await
            .map_err(backend)?,
        ShareKind::Folder => conn
            .query(
                "SELECT owner_id FROM folder WHERE id = ?1",
                params![target_id],
            )
            .await
            .map_err(backend)?,
    };
    match rows.next().await.map_err(backend)? {
        Some(row) => Ok(Some(text(&row, 0)?)),
        None => Ok(None),
    }
}

/// The live owner of a share target, or `None` when the target is missing or
/// trashed. Grants and links are vetted against this: shares never name what
/// is not live.
async fn target_owner(
    conn: &tokio::sync::Mutex<Connection>,
    kind: ShareKind,
    target_id: &str,
) -> Result<Option<String>> {
    let conn = conn.lock().await;
    let mut rows = match kind {
        ShareKind::File => conn
            .query(
                "SELECT owner_id FROM file WHERE id = ?1 AND deleted_at IS NULL",
                params![target_id],
            )
            .await
            .map_err(backend)?,
        ShareKind::Folder => conn
            .query(
                "SELECT owner_id FROM folder WHERE id = ?1 AND deleted_at IS NULL",
                params![target_id],
            )
            .await
            .map_err(backend)?,
    };
    match rows.next().await.map_err(backend)? {
        Some(row) => Ok(Some(text(&row, 0)?)),
        None => Ok(None),
    }
}

/// The trash predicate a delete re-tests inside its own transaction. The
/// candidate lists of a purge are read before the delete transaction
/// opens; a restore committing in that window must keep its row, so the
/// guarded deletes re-check `deleted_at` under the write lock the
/// immediate transaction holds from begin to commit.
enum TrashGuard<'a> {
    /// The row must still be trashed (`purge_file`, `purge_folder`,
    /// `empty_trash`).
    Trashed,
    /// The row must still be trashed before the cutoff (`purge_expired`).
    Expired { before: &'a str },
}

/// Whether `id` still satisfies the guard, re-read inside the delete
/// transaction. The write lock an immediate transaction holds makes this
/// the last word: a restore either committed before it and shows up live
/// here, or waits behind the delete and cannot resurrect the row.
async fn passes_trash_guard(
    tx: &Transaction<'_>,
    table: &str,
    id: &str,
    guard: &TrashGuard<'_>,
) -> Result<bool> {
    let (sql, bind) = match guard {
        TrashGuard::Trashed => (
            format!("SELECT 1 FROM {table} WHERE id = ?1 AND deleted_at IS NOT NULL"),
            vec![id],
        ),
        TrashGuard::Expired { before } => (
            format!(
                "SELECT 1 FROM {table} WHERE id = ?1 \
                 AND deleted_at IS NOT NULL AND deleted_at < ?2"
            ),
            vec![id, *before],
        ),
    };
    let mut rows = tx.query(&sql, bind).await.map_err(backend)?;
    Ok(rows.next().await.map_err(backend)?.is_some())
}

/// The delete side of a trash purge. The candidates were read outside any
/// transaction; this opens the immediate transaction, re-validates every
/// candidate against `guard` under its write lock, takes the survivors'
/// rows and shares, recomputes each owner's usage, and commits. Only then
/// do the bytes go, and only for rows that actually went — a restore that
/// landed in the selection window keeps its row, shares and bytes. The
/// return carries what was really deleted.
async fn commit_trash_purge(
    store: &TursoStore,
    files: Vec<(String, String)>,
    folders: Vec<(String, String, Option<String>)>,
    guard: &TrashGuard<'_>,
) -> Result<(Vec<String>, Vec<String>)> {
    // Folders deepest first: a parent row cannot go while a child row
    // still names it.
    let folder_ids = order_deepest_first(
        &folders
            .iter()
            .map(|(id, _, parent)| (id.clone(), parent.clone()))
            .collect::<Vec<_>>(),
    );
    let mut owners: Vec<String> = files.iter().map(|(_, owner)| owner.clone()).collect();
    owners.extend(folders.iter().map(|(_, owner, _)| owner.clone()));
    owners.sort();
    owners.dedup();
    let file_ids = files.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    let mut conn = store.tx_conn().await?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let deleted_files = delete_file_rows(&tx, &file_ids, guard).await?;
    let deleted_folders = delete_folder_rows(&tx, &folder_ids, guard).await?;
    for owner in &owners {
        refresh_usage(&tx, owner).await?;
    }
    tx.commit().await.map_err(backend)?;
    // After the delete: bytes may only follow a delete that committed.
    // Best-effort — survivors are orphaned bytes the boot sweep collects.
    if !deleted_files.is_empty() {
        let mut keys = Vec::with_capacity(deleted_files.len() * 3);
        for id in &deleted_files {
            keys.push(format!("{FILES_DIR}/{id}"));
            keys.push(format!("{THUMBS_DIR}/{id}"));
            keys.push(format!("{PREVIEWS_DIR}/{id}"));
        }
        let keys = keys.iter().map(String::as_str).collect::<Vec<_>>();
        let _ = store.blobs.delete(&keys).await;
    }
    for owner in owners {
        store.announce([Topic::Library(owner.clone()), Topic::Trash(owner)]);
    }
    Ok((deleted_files, deleted_folders))
}

/// The trash candidates for `purge_expired`, read off the shared
/// connection: files as `(id, owner)`, folders as `(id, owner, parent)`.
/// Reading is deliberately outside the delete transaction — the two are
/// separate moments, and `commit_trash_purge` re-validates every candidate
/// under the write lock before taking it.
async fn select_expired(
    conn: &Connection,
    before: &str,
) -> Result<(Vec<(String, String)>, Vec<(String, String, Option<String>)>)> {
    let mut file_ids = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id, owner_id FROM file WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
            params![before],
        )
        .await
        .map_err(backend)?;
    while let Some(row) = rows.next().await.map_err(backend)? {
        file_ids.push((text(&row, 0)?, text(&row, 1)?));
    }
    let mut folder_rows = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id, owner_id, parent_id FROM folder \
             WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
            params![before],
        )
        .await
        .map_err(backend)?;
    while let Some(row) = rows.next().await.map_err(backend)? {
        folder_rows.push((text(&row, 0)?, text(&row, 1)?, opt_text(&row, 2)?));
    }
    Ok((file_ids, folder_rows))
}

/// The trash candidates for `empty_trash`: the owner's files as bare ids
/// and folders as `(id, parent_id)`, read off the shared connection with
/// the same outside-the-transaction caveat as `select_expired`.
async fn select_trash(
    conn: &Connection,
    owner_id: &str,
) -> Result<(Vec<String>, Vec<(String, Option<String>)>)> {
    let mut file_ids = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id FROM file WHERE owner_id = ?1 AND deleted_at IS NOT NULL",
            params![owner_id],
        )
        .await
        .map_err(backend)?;
    while let Some(row) = rows.next().await.map_err(backend)? {
        file_ids.push(text(&row, 0)?);
    }
    let mut folder_rows = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id, parent_id FROM folder WHERE owner_id = ?1 AND deleted_at IS NOT NULL",
            params![owner_id],
        )
        .await
        .map_err(backend)?;
    while let Some(row) = rows.next().await.map_err(backend)? {
        folder_rows.push((text(&row, 0)?, opt_text(&row, 1)?));
    }
    Ok((file_ids, folder_rows))
}

/// The last word on an upload session, re-read inside the finish
/// transaction: the row was vetted outside it, and the window since then
/// belongs to abort, expiry and a second finish. Every expiry wears the
/// same stamp format, so the text order is the time order.
async fn finishable_session(tx: &Transaction<'_>, id: &str) -> Result<()> {
    let mut rows = tx
        .query(
            "SELECT 1 FROM upload_session \
             WHERE id = ?1 AND state = 'active' AND expires_at > ?2",
            params![id, now_text()?],
        )
        .await
        .map_err(backend)?;
    let live = rows.next().await.map_err(backend)?.is_some();
    drop(rows);
    if !live {
        return Err(StoreError::UploadExpired);
    }
    Ok(())
}

/// Deletes file rows and every share naming them: links and grants onto a
/// purged file are dead, and the purge is what says so. Upload sessions that
/// named these files never exist — sessions name folders, not files. Rows
/// the guard finds alive again are skipped whole — row and shares both,
/// a live file keeps its grants — and the return carries the ids that
/// actually went.
async fn delete_file_rows(
    tx: &Transaction<'_>,
    ids: &[String],
    guard: &TrashGuard<'_>,
) -> Result<Vec<String>> {
    let mut deleted = Vec::with_capacity(ids.len());
    for id in ids {
        if !passes_trash_guard(tx, "file", id, guard).await? {
            continue;
        }
        tx.execute(
            "DELETE FROM share_link WHERE target_id = ?1 AND kind = 'file'",
            params![id.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "DELETE FROM share_user WHERE target_id = ?1 AND kind = 'file'",
            params![id.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute("DELETE FROM file WHERE id = ?1", params![id.as_str()])
            .await
            .map_err(backend)?;
        deleted.push(id.clone());
    }
    Ok(deleted)
}

/// Deletes folder rows deepest first, with every share naming them. Active
/// upload sessions staged into these folders keep their rows — the upload
/// may still finish into the root's listing — but lose the folder: a session
/// cannot finish into trash that has been purged. Rows the guard finds
/// alive again are skipped whole; the return carries the ids that went.
async fn delete_folder_rows(
    tx: &Transaction<'_>,
    ids: &[String],
    guard: &TrashGuard<'_>,
) -> Result<Vec<String>> {
    let mut deleted = Vec::with_capacity(ids.len());
    for id in ids {
        if !passes_trash_guard(tx, "folder", id, guard).await? {
            continue;
        }
        tx.execute(
            "DELETE FROM share_link WHERE target_id = ?1 AND kind = 'folder'",
            params![id.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "DELETE FROM share_user WHERE target_id = ?1 AND kind = 'folder'",
            params![id.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "UPDATE upload_session SET folder_id = NULL WHERE folder_id = ?1",
            params![id.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute("DELETE FROM folder WHERE id = ?1", params![id.as_str()])
            .await
            .map_err(backend)?;
        deleted.push(id.clone());
    }
    Ok(deleted)
}

/// Orders `(id, parent_id)` rows deepest first, so a purge deletes children
/// before the parents whose foreign keys still name them. A row whose parent
/// is outside the set counts depth only within it — the purge set of a trash
/// sweep always contains whole subtrees, so an outside parent is live and
/// never in the way.
fn order_deepest_first(rows: &[(String, Option<String>)]) -> Vec<String> {
    use std::collections::{HashMap, HashSet};
    let in_set: HashSet<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    let parents: HashMap<&str, &str> = rows
        .iter()
        .filter_map(|(id, parent)| parent.as_deref().map(|p| (id.as_str(), p)))
        .collect();
    let mut ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
    ids.sort_by_key(|id| std::cmp::Reverse(depth_in(id.as_str(), &parents, &in_set)));
    ids
}

/// How deep `id` sits inside `rows`: the ancestors it names that are
/// themselves in the set. A row whose parent is outside the set counts depth
/// only within it.
fn depth_in(
    id: &str,
    parents: &std::collections::HashMap<&str, &str>,
    in_set: &std::collections::HashSet<&str>,
) -> usize {
    let mut depth = 0;
    let mut cursor = id;
    while let Some(parent) = parents.get(cursor) {
        if !in_set.contains(parent) {
            break;
        }
        depth += 1;
        cursor = parent;
    }
    depth
}

/// Decides a file's thumbnail: none for bytes no thumbnail is attempted for,
/// an attempted webp for images and video with affordable bytes, `failed`
/// for everything else. Video without `ffmpeg` on `PATH` wears `none` — no
/// attempt was possible — while a failed extraction wears `failed` like an
/// undecodable image. The bytes are `None` when the caller did not read the
/// file back — too big to thumbnail reads as failed, never as a second full
/// read.
fn thumb_for(mime: &str, size: u64, bytes: Option<&[u8]>) -> (ThumbState, Option<Vec<u8>>) {
    if !thumbs::thumbnailed(mime) {
        return (ThumbState::None, None);
    }
    if size > THUMB_SOURCE_CAP {
        return (ThumbState::Failed, None);
    }
    if thumbs::is_video_mime(mime) {
        if !thumbs::ffmpeg_available() {
            return (ThumbState::None, None);
        }
        return match bytes.and_then(thumbs::thumbnail_for_video_bytes) {
            Some(thumb) => (ThumbState::Ready, Some(thumb)),
            None => (ThumbState::Failed, None),
        };
    }
    match bytes.and_then(thumbs::thumbnail_for_bytes) {
        Some(thumb) => (ThumbState::Ready, Some(thumb)),
        None => (ThumbState::Failed, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_labels_never_paths() {
        assert_eq!(label_of("../../etc/passwd"), "passwd");
        assert_eq!(label_of("C:\\Windows\\x.txt"), "x.txt");
        assert_eq!(label_of("  spaced  "), "spaced");
        assert_eq!(label_of("a\u{0}b"), "ab");
    }

    #[test]
    fn an_empty_name_becomes_untitled() {
        assert_eq!(label_of(""), "Untitled");
        assert_eq!(label_of("///"), "Untitled");
        assert_eq!(label_of("   "), "Untitled");
    }

    #[test]
    fn chunk_arithmetic() {
        assert_eq!(chunk_count(0), 0);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_SIZE), 1);
        assert_eq!(chunk_count(CHUNK_SIZE + 1), 2);
        assert_eq!(expected_chunk_len(10, 0), 10);
        assert_eq!(expected_chunk_len(CHUNK_SIZE * 2 + 7, 0), CHUNK_SIZE);
        assert_eq!(expected_chunk_len(CHUNK_SIZE * 2 + 7, 2), 7);
    }

    #[test]
    fn deepest_folders_go_first() {
        let rows = vec![
            ("root".to_string(), None),
            ("kid".to_string(), Some("root".to_string())),
            ("grand".to_string(), Some("kid".to_string())),
        ];
        let ordered = order_deepest_first(&rows);
        assert_eq!(ordered, vec!["grand", "kid", "root"]);
    }

    #[test]
    fn like_queries_escape_their_wildcards() {
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b\\c"), "%a\\_b\\\\c%");
    }

    #[test]
    fn postfixes_split_at_the_last_dot() {
        assert_eq!(postfixed_file_name("report.txt", 2), "report (2).txt");
        assert_eq!(
            postfixed_file_name("archive.tar.gz", 2),
            "archive.tar (2).gz"
        );
        assert_eq!(postfixed_file_name("name", 2), "name (2)");
        assert_eq!(postfixed_file_name("name", 3), "name (3)");
        // A leading dot is the whole stem, not an extension: dotfiles keep
        // their name whole and wear the postfix at the end.
        assert_eq!(postfixed_file_name(".gitignore", 2), ".gitignore (2)");
        assert_eq!(postfixed_folder_name("New folder", 2), "New folder (2)");
    }

    #[test]
    fn postfixes_fit_the_name_cap() {
        let long = "a".repeat(300);
        let file = postfixed_file_name(&format!("{long}.txt"), 2);
        assert_eq!(file.chars().count(), MAX_NAME_CHARS);
        assert!(file.ends_with(" (2).txt"));
        let folder = postfixed_folder_name(&long, 2);
        assert_eq!(folder.chars().count(), MAX_NAME_CHARS);
        assert!(folder.ends_with(" (2)"));
        // Multi-byte stems truncate on a character boundary, never mid-rune.
        let wide = "é".repeat(300);
        let cut = postfixed_folder_name(&wide, 2);
        assert_eq!(cut.chars().count(), MAX_NAME_CHARS);
        assert!(cut.ends_with(" (2)"));
        assert!(cut.starts_with("é"));
    }

    #[tokio::test]
    async fn a_restore_landing_between_select_and_delete_keeps_the_file() {
        use time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(dir.path().join("in.db").to_str().unwrap(), Some(&storage))
            .await
            .unwrap();
        let user = store.provision_user("sub-alice", "alice@example.com", "Alice", None, 0, 1024 * 1024 * 1024)
            .await
            .unwrap();

        // The purge_expired window: candidates read, then a restore lands,
        // then the delete transaction opens on the stale candidates.
        let file = store
            .insert_file(&user.id, None, "keep.txt", b"precious")
            .await
            .unwrap();
        store.delete_file(&file.id).await.unwrap();
        let before = stamp(OffsetDateTime::now_utc() + Duration::days(31)).unwrap();
        let conn = store.conn.lock().await;
        let (files, folders) = select_expired(&conn, &before).await.unwrap();
        drop(conn);
        assert_eq!(files, vec![(file.id.clone(), user.id.clone())]);
        assert!(folders.is_empty());
        store.restore_file(&file.id).await.unwrap();
        let (deleted_files, deleted_folders) = commit_trash_purge(
            &store,
            files,
            folders,
            &TrashGuard::Expired { before: &before },
        )
        .await
        .unwrap();
        assert!(deleted_files.is_empty());
        assert!(deleted_folders.is_empty());
        let live = store.file(&file.id).await.unwrap().unwrap();
        assert!(live.deleted_at.is_none());
        assert!(storage.join(FILES_DIR).join(&file.id).is_file());

        // The empty_trash window, same interleave.
        store.delete_file(&file.id).await.unwrap();
        let conn = store.conn.lock().await;
        let (files, folders) = select_trash(&conn, &user.id).await.unwrap();
        drop(conn);
        assert_eq!(files, vec![file.id.clone()]);
        assert!(folders.is_empty());
        store.restore_file(&file.id).await.unwrap();
        let (deleted_files, deleted_folders) = commit_trash_purge(
            &store,
            files
                .into_iter()
                .map(|id| (id, user.id.clone()))
                .collect::<Vec<_>>(),
            folders
                .into_iter()
                .map(|(id, parent)| (id, user.id.clone(), parent))
                .collect::<Vec<_>>(),
            &TrashGuard::Trashed,
        )
        .await
        .unwrap();
        assert!(deleted_files.is_empty());
        assert!(deleted_folders.is_empty());
        assert!(
            store
                .file(&file.id)
                .await
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );
        assert!(storage.join(FILES_DIR).join(&file.id).is_file());

        // The guards refuse only the resurrected: real trash still purges
        // through the public paths, rows then bytes.
        store.delete_file(&file.id).await.unwrap();
        assert_eq!(store.empty_trash(&user.id).await.unwrap(), 1);
        assert!(store.file(&file.id).await.unwrap().is_none());
        assert!(!storage.join(FILES_DIR).join(&file.id).is_file());

        let other = store
            .insert_file(&user.id, None, "also-doomed.txt", b"precious")
            .await
            .unwrap();
        store.delete_file(&other.id).await.unwrap();
        assert_eq!(
            store
                .purge_expired(OffsetDateTime::now_utc() + Duration::days(31))
                .await
                .unwrap(),
            1
        );
        assert!(store.file(&other.id).await.unwrap().is_none());
        assert!(!storage.join(FILES_DIR).join(&other.id).is_file());
    }

    #[tokio::test]
    async fn a_purge_file_refuses_what_a_restore_brought_back() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(dir.path().join("in.db").to_str().unwrap(), Some(&storage))
            .await
            .unwrap();
        let user = store.provision_user("sub-alice", "alice@example.com", "Alice", None, 0, 1024 * 1024 * 1024)
            .await
            .unwrap();
        let file = store
            .insert_file(&user.id, None, "back.txt", b"precious")
            .await
            .unwrap();
        store.delete_file(&file.id).await.unwrap();
        store.restore_file(&file.id).await.unwrap();
        // The restore won the window: the purge answers false for a live
        // file and leaves row and bytes alone.
        assert!(!store.purge_file(&file.id).await.unwrap());
        assert!(
            store
                .file(&file.id)
                .await
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );
        assert!(storage.join(FILES_DIR).join(&file.id).is_file());
        // Actual trash still purges, rows then bytes.
        store.delete_file(&file.id).await.unwrap();
        assert!(store.purge_file(&file.id).await.unwrap());
        assert!(store.file(&file.id).await.unwrap().is_none());
        assert!(!storage.join(FILES_DIR).join(&file.id).is_file());
    }

    #[tokio::test]
    async fn a_purge_folder_refuses_what_a_restore_brought_back() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(dir.path().join("in.db").to_str().unwrap(), Some(&storage))
            .await
            .unwrap();
        let user = store.provision_user("sub-alice", "alice@example.com", "Alice", None, 0, 1024 * 1024 * 1024)
            .await
            .unwrap();
        let root = store.create_folder(&user.id, None, "root").await.unwrap();
        let kid = store
            .create_folder(&user.id, Some(&root.id), "kid")
            .await
            .unwrap();
        let file = store
            .insert_file(&user.id, Some(&kid.id), "inside.txt", b"precious")
            .await
            .unwrap();
        store.delete_folder(&root.id).await.unwrap();
        store.restore_folder(&root.id).await.unwrap();
        // The restore won the window: a live folder is never purged, and
        // the refusal is the public face of that — nothing of the subtree
        // goes.
        assert!(matches!(
            store.purge_folder(&root.id).await,
            Err(StoreError::NotFound)
        ));
        assert!(
            store
                .folder(&root.id)
                .await
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );
        assert!(
            store
                .folder(&kid.id)
                .await
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );
        assert!(
            store
                .file(&file.id)
                .await
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );
        assert!(storage.join(FILES_DIR).join(&file.id).is_file());
        // Trashed again, the whole subtree goes, rows then bytes.
        store.delete_folder(&root.id).await.unwrap();
        assert_eq!(store.purge_folder(&root.id).await.unwrap(), 3);
        assert!(store.folder(&root.id).await.unwrap().is_none());
        assert!(store.folder(&kid.id).await.unwrap().is_none());
        assert!(store.file(&file.id).await.unwrap().is_none());
        assert!(!storage.join(FILES_DIR).join(&file.id).is_file());
    }

    #[tokio::test]
    async fn an_upload_finishes_once_and_never_after_an_abort() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(dir.path().join("in.db").to_str().unwrap(), Some(&storage))
            .await
            .unwrap();
        let user = store.provision_user("sub-alice", "alice@example.com", "Alice", None, 0, 1024 * 1024 * 1024)
            .await
            .unwrap();
        let session = store
            .create_upload_session(&user.id, None, "once.bin", 4)
            .await
            .unwrap();
        store.record_chunk(&session.id, 0, b"1234").await.unwrap();
        let file = store.finish_upload(&session.id).await.unwrap();
        assert_eq!(file.name, "once.bin");
        // A racing second finish already assembled its view while the
        // session was active — its chunks sit staged. The finished row
        // itself must refuse it.
        let chunk = chunk_path(&storage, &session.id, 0);
        std::fs::create_dir_all(chunk.parent().unwrap()).unwrap();
        std::fs::write(chunk, b"1234").unwrap();
        assert!(matches!(
            store.finish_upload(&session.id).await,
            Err(StoreError::UploadExpired)
        ));
        let files = store
            .list_children(&user.id, None)
            .await
            .unwrap()
            .files
            .len();
        assert_eq!(files, 1);
        // An aborted session never finishes: no file row, ever.
        let session = store
            .create_upload_session(&user.id, None, "gone.bin", 4)
            .await
            .unwrap();
        store.record_chunk(&session.id, 0, b"1234").await.unwrap();
        store.abort_upload(&session.id).await.unwrap();
        assert!(matches!(
            store.finish_upload(&session.id).await,
            Err(StoreError::UploadExpired)
        ));
        let files = store
            .list_children(&user.id, None)
            .await
            .unwrap()
            .files
            .len();
        assert_eq!(files, 1);
    }
}
