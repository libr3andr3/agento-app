-- Owner devices (DECISIONS D3): the peers this core answers owner-scope
-- commands from. Trust lives here, on the device, not in the gateway roster;
-- a device gets in by proving a short-lived pairing code the owner read off
-- this core. Revocation is soft so the audit trail stays.
CREATE TABLE IF NOT EXISTS owner_devices (
  agent        TEXT PRIMARY KEY,                -- agent:<hex> of the paired device
  label        TEXT,                            -- "android app", "iPhone de Andre"
  added_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
  last_seen_at TEXT,
  revoked_at   TEXT
);
