-- Guests: agents that run without a Yaya account. They get the receptionist
-- on their own channels and nothing of the network (no listing, no
-- reputation, no backups). In exchange their conversations, redacted on the
-- phone, may train the agents — a switch they control.
ALTER TABLE agents_seen ADD COLUMN guest INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents_seen ADD COLUMN share INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS training_samples (
    id         TEXT PRIMARY KEY,
    agent      TEXT NOT NULL,
    country    TEXT,
    industry   TEXT,
    language   TEXT,
    sample     TEXT NOT NULL,                 -- redacted turn: {customer, reply, tools, …}
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_training_agent_day ON training_samples(agent, created_at);
