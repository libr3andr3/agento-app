-- Prepaid credits on the phone (agente/docs/CREDITS.md). The gateway keeps
-- the ledger; the core only remembers which outcomes it reported (so a
-- retry is idempotent and a cancellation can name its charge) and what the
-- gateway last said about the balance (`settings.credits_summary`).
ALTER TABLE businesses ADD COLUMN category TEXT;
ALTER TABLE businesses ADD COLUMN terms_version TEXT;
ALTER TABLE businesses ADD COLUMN terms_accepted_at TEXT;

CREATE TABLE IF NOT EXISTS outcomes (
    id            TEXT PRIMARY KEY,               -- appointment / order id (text uuid)
    business_id   BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,                  -- booking | sale
    client_hash   TEXT NOT NULL,
    customer      TEXT,
    synced        INTEGER NOT NULL DEFAULT 0,     -- 1 once the gateway acknowledged the charge
    charged_cents INTEGER,
    is_new_client INTEGER,
    reversed      INTEGER NOT NULL DEFAULT 0,     -- 1 = reversal requested; 2 = acknowledged
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    synced_at     TEXT
);
CREATE INDEX IF NOT EXISTS idx_outcomes_pending ON outcomes(synced, reversed);
