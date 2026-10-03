-- The network ledger: credits grow into a wallet. Every value movement
-- between accounts is a pair of `credit_ledger` rows (payer −, payee +,
-- platform fee +) plus a receipt in the table that explains it. Amounts are
-- céntimos of PEN — everything on the network is priced in soles.

-- Paid consultations: what the sender paid for a relayed message, so the
-- receiving agent knows the question was paid for (`paid` on /v1/inbox).
ALTER TABLE mailbox ADD COLUMN paid_minor INTEGER;

CREATE TABLE IF NOT EXISTS asks (
    id         TEXT PRIMARY KEY,                  -- the mailbox id of the paid message
    from_agent TEXT NOT NULL,
    to_agent   TEXT NOT NULL,
    payer      TEXT NOT NULL,                     -- account
    payee      TEXT NOT NULL,                     -- account
    price      INTEGER NOT NULL,
    fee        INTEGER NOT NULL DEFAULT 0,
    at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_asks_to ON asks(to_agent, at DESC);

-- Things for sale: a file (a PDF handbook), a bundle (niche onboarding
-- questions + a skill the buyer's agent mounts), or a note (text).
CREATE TABLE IF NOT EXISTS listings (
    id           TEXT PRIMARY KEY,
    account      TEXT NOT NULL,                   -- seller (the platform sells as 'yaya')
    agent        TEXT,                            -- seller's agent, when created from one
    kind         TEXT NOT NULL,                   -- file | bundle | note
    title        TEXT NOT NULL,
    description  TEXT,
    niche        TEXT,                            -- industry-COUNTRY, like niche skills
    price_minor  INTEGER NOT NULL DEFAULT 0,
    currency     TEXT NOT NULL DEFAULT 'PEN',
    content      TEXT,                            -- bundle/note body (JSON or text)
    file_name    TEXT,
    file_size    INTEGER,
    file_sha256  TEXT,
    content_type TEXT,
    status       TEXT NOT NULL DEFAULT 'draft',   -- draft | active | hidden
    sales        INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_listings_status ON listings(status, niche, kind, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_listings_account ON listings(account, updated_at DESC);

CREATE TABLE IF NOT EXISTS purchases (
    id       TEXT PRIMARY KEY,
    listing  TEXT NOT NULL,
    buyer    TEXT NOT NULL,                       -- account
    seller   TEXT NOT NULL,                       -- account
    price    INTEGER NOT NULL,
    fee      INTEGER NOT NULL DEFAULT 0,
    at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE (listing, buyer)
);
CREATE INDEX IF NOT EXISTS idx_purchases_buyer ON purchases(buyer, at DESC);

-- Account-to-account transfers ("yaya coins between users"). The table and
-- the endpoint exist today; the switch is TRANSFERS_ENABLED.
CREATE TABLE IF NOT EXISTS transfers (
    id           TEXT PRIMARY KEY,
    from_account TEXT NOT NULL,
    to_account   TEXT NOT NULL,
    amount       INTEGER NOT NULL,
    note         TEXT,
    at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_transfers_from ON transfers(from_account, at DESC);
