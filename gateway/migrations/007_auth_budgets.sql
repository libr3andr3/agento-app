-- Proof-of-possession auth, revocation and finer budgets.

-- Media (Whisper/TTS/vision) is metered separately from chat: it costs more
-- upstream and a free tier should not be able to spend 600 renders a day.
ALTER TABLE usage ADD COLUMN media INTEGER NOT NULL DEFAULT 0;

-- Per-address ceiling across all agents: the farming brake after the
-- new-agents-per-IP one.
CREATE TABLE IF NOT EXISTS ip_usage (
    ip  TEXT NOT NULL,
    day TEXT NOT NULL,
    n   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (ip, day)
);

-- Relay volume per (sender, recipient, day): a fresh key cannot flood one
-- business's inbox even inside its own allowance.
CREATE TABLE IF NOT EXISTS relay_daily (
    from_agent TEXT NOT NULL,
    to_agent   TEXT NOT NULL,
    day        TEXT NOT NULL,
    n          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (from_agent, to_agent, day)
);

-- An agent can retire its key (lost phone) and name a successor; the handle
-- follows the successor when it first publishes.
ALTER TABLE agents ADD COLUMN revoked_at TEXT;
ALTER TABLE agents ADD COLUMN successor TEXT;
CREATE INDEX IF NOT EXISTS idx_agents_successor ON agents(successor);
