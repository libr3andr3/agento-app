-- Audit anchors (2026-08-30): the head of each agent's append-only audit
-- chain, countersigned with the gateway's clock. See src/audit.rs.
CREATE TABLE IF NOT EXISTS audit_anchors (
    agent       TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    head        TEXT NOT NULL,
    phone_ts    TEXT NOT NULL DEFAULT '',
    anchored_at TEXT NOT NULL,
    sig         TEXT NOT NULL,
    PRIMARY KEY (agent, seq)
);
