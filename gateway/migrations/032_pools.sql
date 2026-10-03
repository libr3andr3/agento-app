-- Pools: group buying on the network (docs/POOLS.md). A pool is one seller,
-- one or more items with volume price tiers, a minimum and a target in kg,
-- a deadline. Members join with kg per item; their money sits in escrow
-- (credit_ledger, kind pool-escrow) until a majority confirms delivery.
CREATE TABLE IF NOT EXISTS pools (
    id          TEXT PRIMARY KEY,
    seller      TEXT NOT NULL,            -- agent id that delivers
    seller_acct TEXT,                     -- its account (paid on release)
    organizer   TEXT NOT NULL,            -- agent id that opened it
    title       TEXT NOT NULL,
    items       TEXT NOT NULL,            -- JSON {item: {"tiers": [{"kg": 0, "unitMinor": 285}, ...]}}
    min_kg      REAL NOT NULL,
    target_kg   REAL NOT NULL,
    delivery_minor INTEGER NOT NULL DEFAULT 0,  -- delivery fee for the whole order, split by kg
    deadline    TEXT NOT NULL,
    city        TEXT,
    country     TEXT,
    note        TEXT,
    status      TEXT NOT NULL DEFAULT 'open',   -- open | shipped | delivered | failed | cancelled
    kg          REAL NOT NULL DEFAULT 0,
    fee_minor   INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    closed_at   TEXT
);
CREATE INDEX IF NOT EXISTS pools_open ON pools (status, deadline);
CREATE TABLE IF NOT EXISTS pool_members (
    pool        TEXT NOT NULL REFERENCES pools(id),
    agent       TEXT NOT NULL,
    account     TEXT NOT NULL,
    items       TEXT NOT NULL,            -- JSON {item: kg}
    kg          REAL NOT NULL,
    escrow_minor INTEGER NOT NULL,
    due_minor   INTEGER,                  -- final amount at close
    confirmed   INTEGER,                  -- NULL | 1 delivered | 0 not delivered
    joined_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (pool, agent)
);
CREATE TABLE IF NOT EXISTS pool_intents (
    agent       TEXT NOT NULL,
    item        TEXT NOT NULL,
    kg          REAL NOT NULL,
    city        TEXT,
    country     TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS pool_intents_item ON pool_intents (item, created_at);
