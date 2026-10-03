-- The yaya exchange (GNU Taler shape): blind-signed coins per denomination.
-- The exchange never sees a coin at withdrawal (blinded) and cannot link a
-- deposit to a withdrawal; it only keeps the list of spent coins. Coins are
-- liabilities: `credit_ledger` account 'coins' holds what is in circulation.
CREATE TABLE IF NOT EXISTS denominations (
    id          TEXT PRIMARY KEY,               -- e.g. d100-2026-08
    value_minor INTEGER NOT NULL,
    sk_pem      TEXT NOT NULL,
    pk_pem      TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at  TEXT NOT NULL,
    retired     INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS spent_coins (
    coin        TEXT PRIMARY KEY,               -- coin id (agent:<hex> of the coin key)
    denomination TEXT NOT NULL,
    kind        TEXT NOT NULL,                  -- deposit | refresh
    account     TEXT,                           -- who redeemed (deposit) — never who withdrew
    spent_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS withdrawals (
    id          TEXT PRIMARY KEY,
    account     TEXT NOT NULL,
    amount      INTEGER NOT NULL,
    coins       INTEGER NOT NULL,
    at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_withdrawals_account ON withdrawals(account, at DESC);
