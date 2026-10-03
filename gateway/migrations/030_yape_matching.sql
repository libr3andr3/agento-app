-- Yape top-ups matched by unique céntimos (docs: gateway/src/yape.rs).

-- The price, and the céntimo offset from it (0 = the exact price, then
-- +1, -1, +2, -2 …) on requests the gateway opens on its own (no payment
-- processor in between); and when the request stops holding its amount.
ALTER TABLE plan_requests ADD COLUMN base_minor INTEGER;
ALTER TABLE plan_requests ADD COLUMN yape_tag INTEGER;
ALTER TABLE plan_requests ADD COLUMN expires_at TEXT;
-- What actually arrived, when a transfer paid the request.
ALTER TABLE plan_requests ADD COLUMN paid_minor INTEGER;
-- One pending request per amount: the amount alone names the payer, and a
-- paid / cancelled / expired request gives its amount back at once.
CREATE UNIQUE INDEX IF NOT EXISTS idx_plan_requests_open_tagged
    ON plan_requests(amount_minor) WHERE status = 'pending' AND yape_tag IS NOT NULL;

-- Every Yape notification the house phone forwards, exactly once.
CREATE TABLE IF NOT EXISTS yape_inbox (
    id           TEXT PRIMARY KEY,        -- sha256(package | posted ms | title | text): reposts collapse
    package      TEXT NOT NULL,
    posted_at    TEXT NOT NULL,           -- the notification's own time (when the money moved)
    received_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    title        TEXT,
    text         TEXT NOT NULL,
    payer        TEXT,
    amount_minor INTEGER,
    status       TEXT NOT NULL,           -- matched | unmatched | ignored | resolved | dismissed
    reason       TEXT,                    -- why it is not matched (see yape.rs)
    ref          TEXT,                    -- the plan_requests.ref it paid
    match_kind   TEXT,                    -- exact | rounded | manual
    suggestions  TEXT,                    -- JSON: refs an operator may mean
    resolved_at  TEXT,
    note         TEXT
);
CREATE INDEX IF NOT EXISTS idx_yape_inbox_status ON yape_inbox(status, received_at DESC);
