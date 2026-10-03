-- D14 (2026-08-30): conversations per month are THE metric. A customer peer
-- the agent answered counts once per calendar month (business-local);
-- the cap comes from the gateway's plan sync (NULL = tier default).
ALTER TABLE businesses ADD COLUMN conv_cap INTEGER;
CREATE TABLE IF NOT EXISTS conversation_months (
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    month       TEXT NOT NULL,                       -- YYYY-MM, business-local
    peer        TEXT NOT NULL,
    first_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    PRIMARY KEY (business_id, month, peer)
);
-- The seller's CRM: what agente plan a contact is on (pro | max | trial | free | lead).
ALTER TABLE contacts ADD COLUMN plan TEXT;
