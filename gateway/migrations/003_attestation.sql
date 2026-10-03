-- Hardware attestation verdict per agent: what the chip said about the phone
-- this agent runs on, as verified by the registry on publish. Public.
CREATE TABLE IF NOT EXISTS device_verdicts (
    agent      TEXT PRIMARY KEY,
    verified   INTEGER NOT NULL DEFAULT 0,
    verdict    TEXT NOT NULL,
    checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
