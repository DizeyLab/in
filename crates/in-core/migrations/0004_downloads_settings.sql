-- Per-file download counts and instance-level key/value settings.
--
-- `file.download_count` counts user-facing byte serves: the download route
-- and the public-link bytes bump it through `record_download`, and nothing
-- internal — thumbnails, sniffing, the boot sweep — ever touches it. The
-- DEFAULT is the backfill: SQLite fills every existing row with 0 as the
-- column is added. Databases already carrying the column never see this file
-- applied piecemeal — they reach the declared shape through `in reconcile`,
-- whose copy map backfills the same value.
--
-- `setting` is the instance's key/value drawer, one row per key, read and
-- written through `get_setting`/`set_setting`. A fresh table, so an old
-- database simply gains it empty on rebuild.
ALTER TABLE file ADD COLUMN download_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE setting (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
