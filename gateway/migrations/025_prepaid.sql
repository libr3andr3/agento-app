-- Prepaid credits (closed loop, USD cents) — the revenue model since app
-- 1.22.0. See agente/docs/CREDITS.md. Separate from `credit_ledger` (the
-- PEN network/wallet ledger): this one is charged per CONFIRMED OUTCOME
-- (a booking or a sale), never per call, and may run negative down to a
-- grace floor.

-- Country from the WhatsApp dialling code at account creation; immutable.
ALTER TABLE accounts ADD COLUMN country TEXT;
ALTER TABLE accounts ADD COLUMN category TEXT;
ALTER TABLE accounts ADD COLUMN terms_version TEXT;
ALTER TABLE accounts ADD COLUMN terms_accepted_at TEXT;

-- Per-country switches: whether deposits are collected before a booking
-- counts as confirmed, the tax rate baked into the (tax-inclusive) prices,
-- and the currency the checkout displays. Rows are seeded below and edited
-- by ops; the app never carries this list.
CREATE TABLE IF NOT EXISTS country_config (
    iso              TEXT PRIMARY KEY,
    deposits_enabled INTEGER NOT NULL DEFAULT 0,
    tax_rate         REAL NOT NULL DEFAULT 0,      -- percent, e.g. 18 = 18 %
    display_currency TEXT NOT NULL DEFAULT 'USD',
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
INSERT OR IGNORE INTO country_config (iso, deposits_enabled, tax_rate, display_currency) VALUES
    ('PE', 1, 18, 'PEN'),
    ('MX', 0, 16, 'MXN'),
    ('CO', 0, 19, 'COP'),
    ('CL', 0, 19, 'CLP'),
    ('AR', 0, 21, 'ARS'),
    ('BR', 0, 17, 'BRL'),
    ('EC', 0, 15, 'USD'),
    ('PA', 0, 7,  'USD');

-- The ledger, in three buckets that are kept apart and consumed in order:
--   free  — welcome grant + promos; expires 60 days after the grant
--   bonus — the extra a top-up tier gives; never expires, never refunded
--   paid  — what was actually paid; never expires, refundable to the
--           original method only
-- A positive row is a LOT (kind grant | promo | bonus | topup). A negative
-- row names the lot it drew from (`lot`): debit (a confirmed outcome),
-- reversal (+, gives a debit back to its lot), refund (paid, −), void
-- (bonus, −, when its top-up was refunded), expire (free, −), forfeit
-- (dormancy). A debit that outruns every lot writes one lot-less paid row:
-- the overdraft that makes the balance negative down to the grace floor.
-- The balance is SUM(amount_cents); per bucket, SUM filtered by bucket.
CREATE TABLE IF NOT EXISTS prepaid_ledger (
    id            TEXT PRIMARY KEY,
    account       TEXT NOT NULL,
    kind          TEXT NOT NULL,
    bucket        TEXT NOT NULL,                  -- free | bonus | paid
    amount_cents  INTEGER NOT NULL,
    lot           TEXT,                           -- the positive row this one consumed / returned
    expires_at    TEXT,                           -- free lots only
    outcome_id    TEXT,
    business      TEXT,
    client_hash   TEXT,
    is_new_client INTEGER,
    tax_rate      REAL,                           -- debits from the paid bucket only
    net_cents     INTEGER,
    method        TEXT,                           -- card | yape
    external_id   TEXT,                           -- Dodo payment / refund id, yaya.cash ref
    note          TEXT,
    meta          TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_prepaid_account ON prepaid_ledger(account, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_prepaid_lot ON prepaid_ledger(lot) WHERE lot IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_prepaid_outcome ON prepaid_ledger(account, outcome_id) WHERE outcome_id IS NOT NULL;
-- Idempotency for money in/out from providers: one lot of each kind per payment.
CREATE UNIQUE INDEX IF NOT EXISTS idx_prepaid_external ON prepaid_ledger(kind, external_id) WHERE external_id IS NOT NULL;

-- Who is already a known client of which business (per business, not global).
CREATE TABLE IF NOT EXISTS prepaid_clients (
    account     TEXT NOT NULL,
    business    TEXT NOT NULL,
    client_hash TEXT NOT NULL,
    first_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (account, business, client_hash)
);

-- Charged outcomes and money per calendar month (EXPIRY_TZ): volume pricing
-- kicks in after 100, the hard cap stops charging at $199. Counted on the
-- charged amount whatever bucket paid it.
CREATE TABLE IF NOT EXISTS prepaid_months (
    account       TEXT NOT NULL,
    month         TEXT NOT NULL,                  -- YYYY-MM
    outcomes      INTEGER NOT NULL DEFAULT 0,
    charged_cents INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account, month)
);

-- One welcome grant per WhatsApp number, ever.
CREATE TABLE IF NOT EXISTS prepaid_welcome (
    phone      TEXT PRIMARY KEY,
    account    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- The "modo manual" notice: one per episode (cleared when the balance recovers).
CREATE TABLE IF NOT EXISTS prepaid_handoffs (
    account     TEXT PRIMARY KEY,
    notified_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- Dormancy: 24 months without a ledger movement → warning; 30 days later,
-- still nothing → every bucket is forfeited (stated in the terms).
CREATE TABLE IF NOT EXISTS prepaid_dormancy (
    account      TEXT PRIMARY KEY,
    warned_at    TEXT NOT NULL,
    forfeited_at TEXT
);

-- Yape recargas of prepaid credits carry their tier here (basic|plus|max).
ALTER TABLE plan_requests ADD COLUMN product TEXT;
