-- The person's theme and language: 'light'/'dark' and 'en'/'tr', read by
-- `root_layout` into `data-theme` and `<html lang>`. The DEFAULTs are the
-- backfill: SQLite fills every existing row with them as each column is
-- added. Databases already carrying the columns never see this file applied
-- piecemeal — they reach the declared shape through `in reconcile`, whose
-- copy map backfills the same values.
ALTER TABLE user ADD COLUMN theme TEXT NOT NULL DEFAULT 'light';
ALTER TABLE user ADD COLUMN language TEXT NOT NULL DEFAULT 'en';
