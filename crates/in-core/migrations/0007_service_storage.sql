-- Service storage: the machine account a family service drives, and the key
-- that opens it.
--
-- A service account is a `user` row no person signs into (`oidc_sub`
-- `service:<key>` — a shape a real im subject can never wear); its key lives
-- here, hashed, one per service. `file.external_id` is the service's own
-- handle for an upload: two pushes of the same id are one row, which the
-- partial unique index below makes a fact rather than a habit. Human files
-- leave the column NULL and never enter that index.
ALTER TABLE file ADD COLUMN external_id TEXT;
CREATE UNIQUE INDEX file_external_by_owner ON file(owner_id, external_id)
    WHERE external_id IS NOT NULL;
CREATE TABLE service_key (
    token_hash TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    service    TEXT NOT NULL,
    user_id    TEXT NOT NULL REFERENCES user(id),
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX service_key_service ON service_key(service);
