-- In's schema, whole, in one file.
--
-- One plain SQL file, applied at boot to an empty database: Turso has no
-- migration runner of its own, and keeping the schema as SQL is what makes
-- the store trait swappable.
-- Timestamps are RFC 3339 text in UTC. Ids are ULIDs as text — sortable by
-- creation.
--
-- This file is the first migration, not the whole schema any more. Everything
-- after it is a numbered `0002_*.sql` beside it, and an empty database is
-- built by applying them in order. A live database is brought up by
-- `in reconcile`, not by re-running the files, so a change to a table lands
-- in three places — the newest migration, this file's table, and that tool's
-- copy map.

-- An account. There is no password column on purpose: sign-in is the OIDC
-- provider's business, and this database never sees a secret worth stealing.
-- `oidc_sub` is the provider's stable subject for the person; the row is
-- created on first sight of a sub (JIT provisioning) and refreshed on every
-- sight after. The first row ever provisioned is the admin.
--
-- `quota_bytes` is the person's ceiling, `used_bytes` the running total of
-- every live AND trashed byte they hold — trash counts, because a file that
-- can be restored is a file that still costs. `used_bytes` is recomputed
-- from the rows after every mutation, never blindly incremented, so a crash
-- between two writes cannot drift it from the truth.
CREATE TABLE user (
    id                TEXT PRIMARY KEY,
    oidc_sub          TEXT NOT NULL,
    email             TEXT NOT NULL,
    display_name      TEXT NOT NULL,
    admin             INTEGER NOT NULL DEFAULT 0,
    disabled          INTEGER NOT NULL DEFAULT 0,
    quota_bytes       INTEGER NOT NULL,
    used_bytes        INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL,
    last_seen_at      TEXT
);
CREATE UNIQUE INDEX user_oidc_sub ON user(oidc_sub);

-- A folder. `parent_id` NULL is the person's root: everything an account
-- holds hangs, directly or not, under nothing. Names are unique among live
-- siblings — two folders called the same thing in one place is one folder
-- spelled twice — and trashed rows do not count, so deleting a name frees it.
--
-- Trash is `deleted_at`: a folder trash stamps the folder, every descendant
-- folder and every file under them with the same timestamp, in one
-- transaction. The bytes stay on disk until a purge takes the rows.
CREATE TABLE folder (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT NOT NULL REFERENCES user(id),
    parent_id   TEXT REFERENCES folder(id),
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    deleted_at  TEXT
);
CREATE INDEX folder_by_parent ON folder(owner_id, parent_id);

-- A file. The bytes are deliberately not on this type: they sit at
-- `<storage>/files/<id>`, named by the row's own id, and a thumbnail — when
-- the bytes are an image — at `<storage>/thumbs/<id>`. The directory is state
-- the same way the database file is — back the two up together, and boot
-- deletes any file no row names. And a name is still exactly where an
-- uploaded file name must never end up: `../../etc` is a valid file name and
-- a terrible path, so `name` remains a label printed on a row; the file a row
-- names is found by its id alone, never resolved from anything the row was
-- given.
--
-- `mime` is what the server decided the bytes are, never what the upload
-- claimed. `thumb_state` is `none` for bytes no thumbnail is attempted for,
-- `pending` while one is being made, and `ready` or `failed` once it is.
CREATE TABLE file (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT NOT NULL REFERENCES user(id),
    folder_id   TEXT REFERENCES folder(id),
    name        TEXT NOT NULL,
    mime        TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    thumb_state TEXT NOT NULL DEFAULT 'none',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT
);
CREATE INDEX file_by_folder ON file(owner_id, folder_id);

-- A public share link. Only the hash is stored: the plaintext token is shown
-- once when the link is created, and anyone who holds it reads through it.
-- `kind` names which table `target_id` points at — `file` or `folder` — and
-- `expires_at` / `revoked_at` end the link without deleting the row, so a
-- dead link stays distinguishable from a wrong token.
CREATE TABLE share_link (
    id            TEXT PRIMARY KEY,
    token_hash    TEXT NOT NULL,
    kind          TEXT NOT NULL,
    target_id     TEXT NOT NULL,
    created_by    TEXT NOT NULL REFERENCES user(id),
    can_download  INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL,
    expires_at    TEXT,
    revoked_at    TEXT
);
CREATE UNIQUE INDEX share_link_token ON share_link(token_hash);

-- A per-person share: `user_id` may see one file or folder of somebody
-- else's. `kind` names which table `target_id` points at, as on `share_link`.
-- The row is the whole of the permission — no role, no hierarchy — so
-- removing it unshares completely.
CREATE TABLE share_user (
    kind          TEXT NOT NULL,
    target_id     TEXT NOT NULL,
    user_id       TEXT NOT NULL REFERENCES user(id),
    can_download  INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (kind, target_id, user_id)
);

-- A chunked upload on its way in. Chunks land at
-- `<storage>/uploads/<id>/<n>`, staged until the finish assembles them in
-- order, sniffs what they are, and moves the result to its file row. A
-- session whose `expires_at` passes is aborted and its chunks deleted — by
-- the boot sweep, and by anyone who asks — so an abandoned upload cannot pin
-- disk forever. `state` is `active`, `done` or `aborted`: a finished or
-- aborted session keeps its row as the record of what happened.
CREATE TABLE upload_session (
    id              TEXT PRIMARY KEY,
    owner_id        TEXT NOT NULL REFERENCES user(id),
    folder_id       TEXT REFERENCES folder(id),
    name            TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    chunk_size      INTEGER NOT NULL,
    received_bytes  INTEGER NOT NULL DEFAULT 0,
    state           TEXT NOT NULL DEFAULT 'active',
    created_at      TEXT NOT NULL,
    expires_at      TEXT NOT NULL
);
