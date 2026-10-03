-- Client edition: what the personal assistant did on the network.

-- One thread per business the person has talked to through the relay.
CREATE TABLE IF NOT EXISTS network_threads (
    agent       TEXT PRIMARY KEY,            -- business agent id
    handle      TEXT,
    name        TEXT,
    last_text   TEXT NOT NULL DEFAULT '',
    unread      TEXT,                        -- a reply that arrived after the tool stopped waiting
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);

-- Bookings and orders the business agents confirmed back to us.
CREATE TABLE IF NOT EXISTS client_bookings (
    id           BLOB PRIMARY KEY,
    agent        TEXT NOT NULL,              -- business agent id
    name         TEXT,                       -- business name
    kind         TEXT NOT NULL,              -- appointment | order
    remote_id    TEXT,                       -- the business's appointment/order id
    starts_at    TEXT,                       -- business-local "YYYY-MM-DDTHH:MM" as quoted
    service      TEXT,
    price        REAL,
    currency     TEXT,
    status       TEXT NOT NULL,
    reviewed     INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_client_bookings_agent ON client_bookings(agent, created_at DESC);
