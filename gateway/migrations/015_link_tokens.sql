-- One-time link tokens: the owner mints one in the console; a node being
-- provisioned presents it (signed, as itself) to join the account. Ten
-- minutes, single use, hash-only at rest.
CREATE TABLE IF NOT EXISTS link_tokens (
    token_hash TEXT PRIMARY KEY,
    account    TEXT NOT NULL,
    label      TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at TEXT NOT NULL,
    used_at    TEXT
);
