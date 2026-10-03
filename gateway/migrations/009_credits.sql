-- Credits: half of every paid plan comes back as credits, spent when a new
-- customer reaches the business through the agentic network. Minor units
-- (céntimos); the balance is the sum of the ledger.
CREATE TABLE IF NOT EXISTS credit_ledger (
    id         TEXT PRIMARY KEY,
    account    TEXT NOT NULL,
    delta      INTEGER NOT NULL,              -- + grant, − spend
    currency   TEXT NOT NULL DEFAULT 'PEN',
    kind       TEXT NOT NULL,                 -- starter | grant | lead | adjust
    ref        TEXT,                          -- plan reference / client agent
    note       TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_ledger_account ON credit_ledger(account, created_at DESC);

-- Attribution: the last time a client's search returned a business.
CREATE TABLE IF NOT EXISTS match_hits (
    client_agent   TEXT NOT NULL,
    business_agent TEXT NOT NULL,
    at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (client_agent, business_agent)
);

-- One lead per (business, client) per 30 days.
CREATE TABLE IF NOT EXISTS leads (
    business_agent TEXT NOT NULL,
    client_agent   TEXT NOT NULL,
    charged        INTEGER NOT NULL,
    at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (business_agent, client_agent)
);
