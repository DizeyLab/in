//! Reconcile a live Turso database with the declared whole schema.
//!
//! The live file is never altered in place. Instead a new file is built from
//! `migrations/0001_init.sql`, the data is copied with an explicit per-table
//! column map, the copy is verified, and only then the files are swapped. The
//! original file is kept as a timestamped backup and is never deleted.
//!
//! Binary is the one thing that needs no extraction here: In never kept
//! bytes in its tables — files, thumbnails and chunks live under the storage
//! directory from the first schema, keyed by row id — so the copy moves rows
//! only, and the tree beside the database is untouched throughout.

use super::schema::{declared_fingerprint, diff_report, fingerprint, schema_sql};
use super::{Result, StoreError};

use turso::{Builder, Connection};
pub struct ReconcileOptions {
    /// Print the diff and plan, then stop without touching anything.
    pub dry_run: bool,
    /// Skip the interactive confirmation.
    pub yes: bool,
    /// Boot path: do not prompt, log what happened and proceed.
    pub auto: bool,
}

/// Rebuilds the database at `path` to match the declared schema.
///
/// - Empty or already-current databases are a no-op.
/// - A differing database is backed up, rebuilt, verified and swapped into
///   place.
/// - `storage` is accepted for the call shape the boot path shares with
///   databases that once kept binary in their tables; In never did, so the
///   tree beside the database is left exactly as it was.
/// - A rebuild that fails verification deletes the `.rebuilt` file and leaves
///   the original untouched.
pub async fn reconcile(
    path: &str,
    storage: Option<&std::path::Path>,
    opts: ReconcileOptions,
) -> Result<()> {
    let _ = storage;
    if path == ":memory:" {
        return Err(StoreError::Backend(
            "reconcile is not meaningful on an in-memory database".into(),
        ));
    }
    let db_path = std::path::Path::new(path);
    if !db_path.exists() {
        return Err(StoreError::Backend(format!("database not found: {}", path)));
    }

    // Open the live database read-only to inspect its fingerprint.
    let old_db = Builder::new_local(path)
        .build()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let old_conn = old_db
        .connect()
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    let old_fp = fingerprint(&old_conn).await?;
    let new_fp = declared_fingerprint().await?;

    if old_fp == new_fp {
        if !opts.auto {
            println!("database already matches the declared schema");
        }
        return Ok(());
    }

    let diff = diff_report(&old_fp, &new_fp);

    if opts.dry_run {
        println!("schema difference:\n{}", diff);
        println!("dry run: would rebuild {}", path);
        return Ok(());
    }

    if opts.auto {
        eprintln!("database schema differs from declared schema; rebuilding automatically");
        eprintln!("difference:\n{}", diff);
    } else {
        println!("schema difference:\n{}", diff);
        if !opts.yes {
            print!("rebuild {}? [y/N] ", path);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut answer = String::new();
            match std::io::stdin().read_line(&mut answer) {
                Ok(_) if answer.trim().eq_ignore_ascii_case("y") => {}
                _ => {
                    println!("rebuild cancelled");
                    return Ok(());
                }
            }
        }
    }

    let rebuilt_path = format!("{}.rebuilt", path);
    cleanup_rebuilt(&rebuilt_path);

    let result = rebuild(path, &rebuilt_path, &old_conn).await;

    // Close the read-only connection before any file moves; this also
    // releases the WAL locks on the original file.
    drop(old_conn);
    drop(old_db);

    if let Err(e) = result {
        cleanup_rebuilt(&rebuilt_path);
        return Err(e);
    }

    let backup_path = backup_name(path)?;
    std::fs::rename(path, &backup_path).map_err(|e| {
        StoreError::Backend(format!(
            "failed to move original database to backup {}: {}",
            backup_path, e
        ))
    })?;
    // SQLite WAL/SHM siblings belong to the main file; the backup must be a
    // complete database on its own, and the rebuilt file starts with none.
    rename_sibling(path, &backup_path, "-wal");
    rename_sibling(path, &backup_path, "-shm");
    if let Err(e) = std::fs::rename(&rebuilt_path, path) {
        // Best-effort undo so the live file is not gone. If this also fails,
        // the caller has both the backup and the rebuilt file to recover from.
        let _ = std::fs::rename(&backup_path, path);
        let _ = rename_sibling(&backup_path, path, "-wal");
        let _ = rename_sibling(&backup_path, path, "-shm");
        return Err(StoreError::Backend(format!(
            "failed to move rebuilt database into place {}: {}",
            rebuilt_path, e
        )));
    }
    // The rebuilt file was checkpointed before the swap, so its own sidecars
    // hold nothing — but a `.rebuilt-wal` left lying about would be adopted by
    // the NEXT rebuild's fresh `.rebuilt` file, which is how a stale WAL
    // corrupts a database that was otherwise fine.
    let _ = std::fs::remove_file(format!("{}-wal", rebuilt_path));
    let _ = std::fs::remove_file(format!("{}-shm", rebuilt_path));

    if opts.auto {
        eprintln!(
            "database rebuilt and verified; original backed up to {}",
            backup_path
        );
        eprintln!("rebuilt database now at {}", path);
    } else {
        println!("database rebuilt and verified");
        println!("original backed up to {}", backup_path);
        println!("rebuilt database now at {}", path);
    }

    Ok(())
}

/// Does the actual rebuild: creates `.rebuilt`, copies the data, and verifies
/// the result. On success the rebuilt file is complete and checkpointed; on
/// failure the caller deletes it.
async fn rebuild(path: &str, rebuilt_path: &str, old_conn: &Connection) -> Result<()> {
    // The copy runs as `INSERT ... SELECT FROM old.<table>`, which needs
    // ATTACH — still gated in this engine, and switched on only here, for the
    // one connection that does the rebuild. The verification afterwards is
    // what protects the data, not the copy's mechanism.
    let new_db = Builder::new_local(rebuilt_path)
        .experimental_attach(true)
        .build()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let new_conn = new_db
        .connect()
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    new_conn
        .execute_batch(&schema_sql())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    copy_data(old_conn, &new_conn, path).await?;
    verify(old_conn, &new_conn).await?;

    // A checkpoint PRAGMA answers with a row, so it is a query: `execute`
    // treats a row as a failure ("unexpected row during execution").
    let mut checkpoint = new_conn
        .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await
        .map_err(|e| StoreError::Backend(format!("checkpoint: {e}")))?;
    while checkpoint
        .next()
        .await
        .map_err(|e| StoreError::Backend(format!("checkpoint: {e}")))?
        .is_some()
    {}

    Ok(())
}

/// A per-table column map. Every destination column must appear exactly once;
/// `copy_data` validates this against the declared schema before inserting.
struct TableMap {
    name: &'static str,
    columns: Vec<(&'static str, String)>,
}

/// Builds the explicit column map. Every table maps every column to its old
/// self: In's first schema is the declared one, so a database old enough to
/// be reconciled carries the same columns, and the map is the receipt that
/// says nothing was dropped, renamed or defaulted on the way across.
fn build_maps() -> Vec<TableMap> {
    vec![
        TableMap {
            name: "user",
            columns: old_cols(&[
                "id",
                "oidc_sub",
                "email",
                "display_name",
                "admin",
                "disabled",
                "quota_bytes",
                "used_bytes",
                "created_at",
                "last_seen_at",
            ]),
        },
        TableMap {
            name: "folder",
            columns: old_cols(&[
                "id",
                "owner_id",
                "parent_id",
                "name",
                "created_at",
                "deleted_at",
            ]),
        },
        TableMap {
            name: "file",
            columns: old_cols(&[
                "id",
                "owner_id",
                "folder_id",
                "name",
                "mime",
                "size_bytes",
                "thumb_state",
                "created_at",
                "updated_at",
                "deleted_at",
            ]),
        },
        TableMap {
            name: "share_link",
            columns: old_cols(&[
                "id",
                "token_hash",
                "kind",
                "target_id",
                "created_by",
                "can_download",
                "created_at",
                "expires_at",
                "revoked_at",
            ]),
        },
        TableMap {
            name: "share_user",
            columns: old_cols(&["kind", "target_id", "user_id", "can_download", "created_at"]),
        },
        TableMap {
            name: "upload_session",
            columns: old_cols(&[
                "id",
                "owner_id",
                "folder_id",
                "name",
                "size_bytes",
                "chunk_size",
                "received_bytes",
                "state",
                "created_at",
                "expires_at",
            ]),
        },
    ]
}

fn old_cols(cols: &[&'static str]) -> Vec<(&'static str, String)> {
    cols.iter().map(|c| (*c, format!("old.{}", c))).collect()
}

/// Validates that every destination column is covered and that no map
/// references a column that does not exist. A missing column is a startup
/// error, never a silent NULL.
async fn validate_maps(conn: &Connection, maps: &[TableMap]) -> Result<()> {
    use std::collections::HashSet;

    for map in maps {
        let sql = format!("PRAGMA table_info({})", map.name);
        let mut rows = conn
            .query(&sql, ())
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let mut actual = HashSet::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            let name: String = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
            actual.insert(name);
        }

        for (col, _) in &map.columns {
            if !actual.contains(*col) {
                return Err(StoreError::Backend(format!(
                    "reconcile map for {} references missing column {}",
                    map.name, col
                )));
            }
        }
        for col in actual {
            if !map.columns.iter().any(|(c, _)| c == &col) {
                return Err(StoreError::Backend(format!(
                    "reconcile map for {} is missing column {} (would become NULL)",
                    map.name, col
                )));
            }
        }
    }

    // Every table the declared schema creates must have a map: a declared
    // table without one is a table the rebuild would create empty while the
    // copy ran. Engine bookkeeping (`sqlite_*`) is not user data. A table the
    // old database carries but the declared schema does not is dropped on
    // purpose; it is the declared side that may never go unmapped.
    let mut rows = conn
        .query("SELECT name FROM sqlite_master WHERE type = 'table'", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut declared = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let name: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        if !name.starts_with("sqlite_") {
            declared.push(name);
        }
    }
    for table in declared {
        if !maps.iter().any(|m| m.name == table) {
            return Err(StoreError::Backend(format!(
                "reconcile has no map for declared table {table} (its rows would be dropped)"
            )));
        }
    }
    Ok(())
}

/// Copies every mapped table from the attached old database into the new
/// main schema, in foreign-key-safe order: owners before folders, folders
/// before the files and sessions that name them, everything before the
/// shares that name all of it.
async fn copy_data(_old_conn: &Connection, new_conn: &Connection, path: &str) -> Result<()> {
    let escaped = path.replace('\'', "''");
    new_conn
        .execute(&format!("ATTACH DATABASE '{}' AS old", escaped), ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    let maps = build_maps();
    validate_maps(new_conn, &maps).await?;

    // Foreign keys stay off during the copy and are checked by `verify`
    // instead: folder rows copy in rowid order, which is not a topological
    // order for the self-referencing parent link, and a child copied ahead of
    // its parent is a good row in a bad moment rather than a bad row.
    new_conn
        .execute("PRAGMA foreign_keys = OFF", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    for map in maps {
        let cols = map
            .columns
            .iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(", ");
        // `old` is the attached SCHEMA, not a table, so `old.id` in a select
        // list reads as "table old, column id". The source table is aliased
        // and the maps' `old.` prefix rewritten onto that alias.
        let exprs = map
            .columns
            .iter()
            .map(|(_, e)| e.replace("old.", "src."))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO main.{} ({}) SELECT {} FROM old.{} AS src",
            map.name, cols, exprs, map.name
        );
        new_conn.execute(&sql, ()).await.map_err(|e| {
            StoreError::Backend(format!("{} while copying {}: {}", e, map.name, sql))
        })?;
    }
    new_conn
        .execute("PRAGMA foreign_keys = ON", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    new_conn
        .execute("DETACH DATABASE old", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    Ok(())
}

/// Verifies the rebuilt database before the swap: foreign keys, integrity,
/// and row counts table by table. Anything lost or invented fails here
/// instead of shipping.
async fn verify(old_conn: &Connection, new_conn: &Connection) -> Result<()> {
    let mut rows = new_conn
        .query("PRAGMA foreign_key_check", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut fk_errors = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let table: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        let rowid: i64 = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
        let parent: String = row.get(2).map_err(|e| StoreError::Backend(e.to_string()))?;
        let fkid: i64 = row.get(3).map_err(|e| StoreError::Backend(e.to_string()))?;
        fk_errors.push(format!(
            "{} rowid {} references {} (foreign key {})",
            table, rowid, parent, fkid
        ));
    }
    if !fk_errors.is_empty() {
        return Err(StoreError::Backend(format!(
            "foreign key check failed:\n{}",
            fk_errors.join("\n")
        )));
    }

    let mut rows = new_conn
        .query("PRAGMA integrity_check", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut integrity = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let msg: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        integrity.push(msg);
    }
    if integrity.len() != 1 || integrity[0] != "ok" {
        return Err(StoreError::Backend(format!(
            "integrity check failed: {:?}",
            integrity
        )));
    }

    let tables = [
        "user",
        "folder",
        "file",
        "share_link",
        "share_user",
        "upload_session",
    ];
    for table in tables {
        let old_count = count_rows(old_conn, table).await?;
        let new_count = count_rows(new_conn, table).await?;
        if old_count != new_count {
            return Err(StoreError::Backend(format!(
                "row count mismatch for {}: old {} new {}",
                table, old_count, new_count
            )));
        }
    }

    Ok(())
}

async fn count_rows(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {}", table);
    let mut rows = conn
        .query(&sql, ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    else {
        return Err(StoreError::Backend(format!(
            "could not count rows in {}",
            table
        )));
    };
    row.get::<i64>(0)
        .map_err(|e| StoreError::Backend(e.to_string()))
}

fn cleanup_rebuilt(rebuilt_path: &str) {
    let _ = std::fs::remove_file(rebuilt_path);
    let _ = std::fs::remove_file(format!("{}-wal", rebuilt_path));
    let _ = std::fs::remove_file(format!("{}-shm", rebuilt_path));
}

/// Move a SQLite sidecar file (`-wal` or `-shm`) if it exists. A missing
/// sibling is fine: WAL mode may not have created one, and the backup is
/// still consistent without it.
fn rename_sibling(from: &str, to: &str, suffix: &str) {
    let from_path = format!("{}{}", from, suffix);
    let to_path = format!("{}{}", to, suffix);
    if std::path::Path::new(&from_path).exists() {
        let _ = std::fs::rename(&from_path, &to_path);
    }
}

fn backup_name(path: &str) -> Result<String> {
    let format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour][minute][second]Z",
    )
    .map_err(|e| StoreError::Backend(format!("invalid time format: {}", e)))?;
    let stamp = time::OffsetDateTime::now_utc()
        .format(&format)
        .map_err(|e| StoreError::Backend(format!("time format failed: {}", e)))?;

    let candidate = format!("{}.backup-{}", path, stamp);
    if !std::path::Path::new(&candidate).exists() {
        return Ok(candidate);
    }
    let mut n = 1;
    loop {
        let candidate = format!("{}.backup-{}-{}", path, stamp, n);
        if !std::path::Path::new(&candidate).exists() {
            return Ok(candidate);
        }
        n += 1;
        if n > 1000 {
            return Err(StoreError::Backend(
                "could not find an unused backup name".into(),
            ));
        }
    }
}

/// A probe the copy plan can ask: whether the old database already carries
/// `column` on `table`. Unused while the declared schema is the first one —
/// every database old enough to be reconciled predates every column it
/// would probe — and kept because the second migration will need exactly
/// this question on its first day.
#[allow(dead_code)]
async fn old_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut rows = conn
        .query(&format!("PRAGMA table_info({table})"), ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let name: String = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}
