-- The Yaya ID this installation is signed in as. One row; secrets are
-- sealed under the same device key as the identity when it is present.
CREATE TABLE IF NOT EXISTS account (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    account_id     TEXT NOT NULL,
    email          TEXT NOT NULL,
    name           TEXT,
    session        BLOB NOT NULL,   -- device session bearer (sealed)
    backup_key     BLOB,            -- PBKDF2 of the password (sealed)
    plan           TEXT,
    linked_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_backup_at TEXT
);
