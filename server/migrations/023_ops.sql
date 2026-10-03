-- The operator's own systems (src/ops.rs): an outbox of signed webhook
-- events, and on each agro participant the public id their website names
-- the profile by, the link we sent, and when.
ALTER TABLE agro_participants ADD COLUMN pid TEXT;
ALTER TABLE agro_participants ADD COLUMN profile_url TEXT;
ALTER TABLE agro_participants ADD COLUMN link_sent_at TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_agro_pid ON agro_participants(pid);

CREATE TABLE IF NOT EXISTS ops_events (
    id          TEXT PRIMARY KEY,                      -- evt_…, also the x-agente-delivery header
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,                         -- participant.completed, ping, …
    body        TEXT NOT NULL,                         -- the exact JSON posted (signed as sent)
    status      TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_error  TEXT,
    response    TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    delivered_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_ops_due ON ops_events(status, next_at);
CREATE INDEX IF NOT EXISTS idx_ops_recent ON ops_events(business_id, created_at DESC);
