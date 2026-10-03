-- Self-service account deletion (yaya.tech/eliminar-cuenta). A deletion
-- leaves one tombstone row: when it happened and a hash of the proven phone —
-- enough to answer "was my account really deleted?" without keeping the number.
CREATE TABLE IF NOT EXISTS account_deletions (
    id         TEXT PRIMARY KEY,             -- the deleted account's uuid
    phone_hash TEXT NOT NULL,                -- sha256 of the normalized phone
    devices    INTEGER NOT NULL DEFAULT 0,   -- agents revoked along with it
    deleted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
