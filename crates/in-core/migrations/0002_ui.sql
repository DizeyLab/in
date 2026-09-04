-- The person's UI density: 'ledger' or 'instrument', read by `root_layout`
-- into `data-ui`. The DEFAULT is the backfill: SQLite fills every existing
-- row with it as the column is added. Databases already carrying the column
-- never see this file applied piecemeal — they reach the declared shape
-- through `in reconcile`, whose copy map backfills the same value.
ALTER TABLE user ADD COLUMN ui TEXT NOT NULL DEFAULT 'ledger';
