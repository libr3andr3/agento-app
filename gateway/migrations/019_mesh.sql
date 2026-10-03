-- yaya mesh rendezvous: every agent that joins gets one address in
-- 10.77.0.0/16 and publishes its WireGuard + ML-KEM-768 keys and the
-- endpoints it can be reached at; links between two agents may borrow a
-- port pair on the public relay so NATs are no obstacle.
CREATE TABLE IF NOT EXISTS mesh_agents (
    agent       TEXT PRIMARY KEY,
    account     TEXT,
    ip          TEXT NOT NULL UNIQUE,
    wg          TEXT NOT NULL,                 -- WireGuard public key (base64)
    kem         TEXT NOT NULL,                 -- ML-KEM-768 encapsulation key (base64)
    listen_port INTEGER,
    endpoints   TEXT NOT NULL DEFAULT '[]',    -- JSON list of "ip:port" the agent reported
    hostname    TEXT,
    a2a_port    INTEGER,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS mesh_links (
    id         TEXT PRIMARY KEY,
    a          TEXT NOT NULL,                  -- initiator agent
    b          TEXT NOT NULL,                  -- responder agent
    relay_host TEXT,
    port_a     INTEGER,
    port_b     INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at TEXT,
    UNIQUE (a, b)
);
