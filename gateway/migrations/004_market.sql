-- The marketplace layer: who has dealt with whom, and what they said.

-- Every relay hop between two agents, aggregated. A review is only accepted
-- between agents that actually exchanged messages — the cheapest proof of a
-- real interaction the relay can give without reading a single byte.
CREATE TABLE IF NOT EXISTS interactions (
    a        TEXT NOT NULL,          -- lexically smaller agent id
    b        TEXT NOT NULL,
    first_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    last_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    n        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (a, b)
);

-- One standing review per (reviewer, reviewee): re-reviewing replaces it, so
-- reputation is one vote per counterparty, never a flood from one key.
-- Works in both directions: a client rates a business, a business rates a
-- customer (no-shows, rudeness, prompt payment) — "customer reputation".
CREATE TABLE IF NOT EXISTS reviews (
    reviewer   TEXT NOT NULL,
    reviewee   TEXT NOT NULL,
    stars      INTEGER NOT NULL CHECK (stars BETWEEN 1 AND 5),
    comment    TEXT NOT NULL DEFAULT '',
    tags       TEXT NOT NULL DEFAULT '[]',   -- e.g. ["no_show"], ["on_time","paid_fast"]
    sig        TEXT NOT NULL,                 -- reviewer's envelope, verbatim
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (reviewer, reviewee)
);
CREATE INDEX IF NOT EXISTS idx_reviews_reviewee ON reviews(reviewee, updated_at DESC);
