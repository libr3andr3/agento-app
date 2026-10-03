-- Hardware attestation chain of this installation's device key (Android
-- Keystore, attestation challenge = sha256("agente-attest:v1:" + agent id)).
-- Public by nature: it is what we publish so the network can verify that
-- this agent runs on a real, locked, verified-boot phone in the genuine app.
CREATE TABLE IF NOT EXISTS device_attestation (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    agent      TEXT NOT NULL,
    chain      TEXT NOT NULL,          -- JSON array of base64 DER certs, leaf first
    level      TEXT,                   -- strongbox | tee | software (what the shell reported)
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
