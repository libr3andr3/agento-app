-- The network learns which apps bring money, and what people call their
-- wallets, from what phones report. No list ships with the product.
CREATE TABLE IF NOT EXISTS source_votes (
    package    TEXT NOT NULL,
    agent      TEXT NOT NULL,
    country    TEXT NOT NULL,
    label      TEXT NOT NULL DEFAULT '',
    wallet     TEXT,
    class      TEXT NOT NULL,                 -- money | not_money
    n          INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (package, agent)
);
CREATE INDEX IF NOT EXISTS idx_source_votes_country ON source_votes(country, package);

CREATE TABLE IF NOT EXISTS rails_seen (
    country    TEXT NOT NULL,
    key        TEXT NOT NULL,                 -- lowercased name
    name       TEXT NOT NULL,                 -- as first typed
    n          INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (country, key)
);
