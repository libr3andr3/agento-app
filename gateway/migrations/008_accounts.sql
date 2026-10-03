-- Yaya ID: a person (or business owner) with an email and a password owns
-- one or more agents. Plans attach to the account (subject 'acct:<id>' in
-- `plans` / `plan_requests`); every metered call requires a linked agent.
CREATE TABLE IF NOT EXISTS accounts (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    name          TEXT,
    password_hash TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- Bearer sessions (`ysess_…`), hash-only at rest. `device` sessions are
-- minted by a phone at login; `web` ones by the web app.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    account    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    label      TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at TEXT NOT NULL,
    last_seen  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_sessions_account ON sessions(account);

CREATE TABLE IF NOT EXISTS account_agents (
    agent     TEXT PRIMARY KEY,
    account   TEXT NOT NULL,
    label     TEXT,
    linked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_account_agents_account ON account_agents(account);

-- End-to-end encrypted snapshots of a phone's core database. The gateway
-- stores ciphertext + the client's own metadata; the key never leaves the
-- owner (derived from the account password on the phone and in the browser).
CREATE TABLE IF NOT EXISTS backups (
    id         TEXT PRIMARY KEY,
    account    TEXT NOT NULL,
    agent      TEXT NOT NULL,
    size       INTEGER NOT NULL,
    meta       TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_backups_account ON backups(account, created_at DESC);
