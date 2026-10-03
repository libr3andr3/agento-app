-- Web billing: card checkouts through Dodo Payments (world) and Izipay /
-- Micuentaweb (Perú, monthly subscription on the card), and the boleta
-- electrónica NubeFact issues for every Peruvian charge.
CREATE TABLE IF NOT EXISTS checkouts (
    id            TEXT PRIMARY KEY,               -- our order id (also sent to the provider)
    account       TEXT NOT NULL,
    provider      TEXT NOT NULL,                  -- dodo | izipay
    plan          TEXT NOT NULL,
    months        INTEGER NOT NULL DEFAULT 1,
    amount_minor  INTEGER NOT NULL,
    currency      TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'open',   -- open | paid | failed | cancelled
    external_id   TEXT,                           -- session / subscription / transaction id
    customer_doc  TEXT,                           -- DNI for the boleta (PE)
    customer_name TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    paid_at       TEXT
);
CREATE INDEX IF NOT EXISTS idx_checkouts_account ON checkouts(account, created_at DESC);

-- Provider events we have acted on: idempotency for webhooks/IPNs.
CREATE TABLE IF NOT EXISTS billing_events (
    id         TEXT PRIMARY KEY,                  -- provider event / transaction id
    provider   TEXT NOT NULL,
    checkout   TEXT,
    kind       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS invoices (
    id          TEXT PRIMARY KEY,
    account     TEXT NOT NULL,
    checkout    TEXT,
    provider    TEXT NOT NULL DEFAULT 'nubefact',
    serie       TEXT NOT NULL,
    numero      INTEGER NOT NULL,
    total_minor INTEGER NOT NULL,
    currency    TEXT NOT NULL DEFAULT 'PEN',
    status      TEXT NOT NULL,                    -- issued | accepted | rejected | error
    pdf_url     TEXT,
    xml_url     TEXT,
    response    TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_invoices_account ON invoices(account, created_at DESC);
CREATE TABLE IF NOT EXISTS invoice_counters (
    serie TEXT PRIMARY KEY,
    last  INTEGER NOT NULL DEFAULT 0
);
