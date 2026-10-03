-- The CRM, on the phone. One row per person the business deals with: the
-- owner (from their Yaya account) and every customer the agent talks to.
-- Names, phones and emails are learned from conversations, bookings, orders
-- and payments; the owner can edit them. Backed up with everything else.
CREATE TABLE IF NOT EXISTS contacts (
    id          BLOB PRIMARY KEY,
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL DEFAULT 'customer',      -- owner | customer
    peer        TEXT,                                  -- conversation id (chat app:number, agent:…)
    source      TEXT,                                  -- whatsapp | instagram | network | owner | payment | …
    phone       TEXT,                                  -- digits, country code first
    email       TEXT,
    name        TEXT,
    agent_id    TEXT,                                  -- network peers
    notes       TEXT,
    tags        TEXT NOT NULL DEFAULT '[]',
    messages    INTEGER NOT NULL DEFAULT 0,
    first_seen  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_seen   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_contacts_peer ON contacts(business_id, peer) WHERE peer IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_contacts_phone ON contacts(business_id, phone);
CREATE INDEX IF NOT EXISTS idx_contacts_seen ON contacts(business_id, last_seen DESC);
