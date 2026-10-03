-- Paid plans become first-class: where they came from and when they lapse.
ALTER TABLE plans ADD COLUMN expires_at TEXT;
ALTER TABLE plans ADD COLUMN source TEXT NOT NULL DEFAULT 'admin';   -- admin | play | yape
ALTER TABLE plans ADD COLUMN product TEXT;                           -- Play product id / payment ref

-- Business-side purchases: the owner pays by Yape/Plin quoting a reference;
-- the payment is confirmed (manually or by a receipt feed) via /admin/plan.
CREATE TABLE IF NOT EXISTS plan_requests (
    ref        TEXT PRIMARY KEY,
    agent      TEXT NOT NULL,
    plan       TEXT NOT NULL,
    amount     REAL NOT NULL,
    currency   TEXT NOT NULL,
    months     INTEGER NOT NULL DEFAULT 1,
    status     TEXT NOT NULL DEFAULT 'pending',   -- pending | paid | cancelled
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    paid_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_plan_requests_agent ON plan_requests(agent, created_at DESC);
