-- Per-agent daily LLM usage (UTC day).
CREATE TABLE IF NOT EXISTS usage (
    agent TEXT NOT NULL,
    day   TEXT NOT NULL,
    n     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent, day)
);
-- First-seen bookkeeping: who minted how many agents from where.
CREATE TABLE IF NOT EXISTS agents_seen (
    agent      TEXT PRIMARY KEY,
    ip         TEXT,
    first_seen TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    last_seen  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    calls      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_seen_ip ON agents_seen(ip, first_seen);
-- Plan overrides set by sales; absent = free tier.
CREATE TABLE IF NOT EXISTS plans (
    agent TEXT PRIMARY KEY,
    plan  TEXT NOT NULL,
    cap   INTEGER,                -- NULL = tier default, 0 = unlimited
    note  TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
-- The registry: one signed card per agent (NANDA-style AgentFacts).
CREATE TABLE IF NOT EXISTS agents (
    agent      TEXT PRIMARY KEY,
    did        TEXT,
    card       TEXT NOT NULL,      -- the signed envelope, verbatim
    name       TEXT,
    industry   TEXT,
    country    TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_agents_country ON agents(country, industry);
