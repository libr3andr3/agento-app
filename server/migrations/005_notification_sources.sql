-- What this phone has learned about the apps that notify it. No list of
-- wallets ships with the product: the first notification from any app is
-- read by the agent, the verdict is remembered here, and apps that never
-- carry money are muted for a while. Verdicts are shared with the network
-- so every other phone in the country starts with a prior.
CREATE TABLE IF NOT EXISTS notification_sources (
    package     TEXT PRIMARY KEY,
    label       TEXT NOT NULL DEFAULT '',
    wallet      TEXT,                                 -- the brand as the agent named it ("Yape")
    class       TEXT NOT NULL DEFAULT 'unknown',      -- money | not_money | unknown
    seen        INTEGER NOT NULL DEFAULT 0,
    money_seen  INTEGER NOT NULL DEFAULT 0,
    muted_until TEXT,
    last_seen   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    reported_at TEXT
);
