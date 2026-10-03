-- The registry's own signing key (lean index records are registry-signed).
CREATE TABLE IF NOT EXISTS registry_key (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    secret_key BLOB NOT NULL,
    public_key BLOB NOT NULL
);
-- Human handle per agent (urn:agent:yaya:<slug>), unique, chosen from the
-- card's name; collisions get a numeric suffix.
ALTER TABLE agents ADD COLUMN handle TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_handle ON agents(handle);
-- E2E mailbox: ciphertext only. The relay never holds a key.
CREATE TABLE IF NOT EXISTS mailbox (
    id           TEXT PRIMARY KEY,
    to_agent     TEXT NOT NULL,
    from_agent   TEXT NOT NULL,
    body         TEXT NOT NULL,          -- sealed box JSON (see core e2e.rs)
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    delivered_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_mailbox_to ON mailbox(to_agent, delivered_at, created_at);
