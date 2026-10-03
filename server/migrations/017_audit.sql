-- Audit substrate (2026-08-30): append-only, hash-chained, signed by the
-- agent's identity, anchored to the gateway's clock. Triggers make the
-- table write-once at the database level; the chain makes it tamper-evident
-- even to whoever holds the file. Never exported in backups (backup::PRIVATE).
CREATE TABLE IF NOT EXISTS audit_log (
    seq         INTEGER PRIMARY KEY,
    ts          TEXT NOT NULL,          -- RFC 3339 UTC ms, monotonic
    kind        TEXT NOT NULL,          -- see audit::kind
    actor       TEXT NOT NULL,          -- agent | customer:<peer> | owner | owner:<device> | system
    subject     TEXT NOT NULL DEFAULT '',
    business_id BLOB,
    payload     TEXT NOT NULL,          -- compact JSON, redacted per kind
    prev_hash   TEXT NOT NULL,
    hash        TEXT NOT NULL UNIQUE,
    sig         TEXT NOT NULL           -- Ed25519 over hash, hex
);
CREATE INDEX IF NOT EXISTS idx_audit_kind ON audit_log(kind, seq);
CREATE TRIGGER IF NOT EXISTS audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;

-- The gateway's countersignatures: "at <anchored_at> by my clock, this
-- agent's chain head was <head_hash> at seq <seq>".
CREATE TABLE IF NOT EXISTS audit_anchors (
    seq         INTEGER PRIMARY KEY,
    head_hash   TEXT NOT NULL,
    anchored_at TEXT NOT NULL,
    anchor_sig  TEXT NOT NULL,
    anchor_key  TEXT NOT NULL,
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
