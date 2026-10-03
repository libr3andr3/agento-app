-- yaya mesh: a post-quantum p2p VPN between agents (WireGuard with a
-- PreSharedKey derived from ML-KEM-768 over the E2E relay). This device's
-- keys are sealed like the identity; peers keep the derived PSK sealed too.
CREATE TABLE IF NOT EXISTS mesh_keys (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  keys        BLOB NOT NULL,                  -- sealed: 32 B wg secret + 64 B ML-KEM seed
  listen_port INTEGER NOT NULL,
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE TABLE IF NOT EXISTS mesh_peers (
  agent      TEXT PRIMARY KEY,
  name       TEXT,
  wg         TEXT,                            -- peer WireGuard public key (base64)
  psk        BLOB,                            -- sealed 32 B pre-shared key
  ip         TEXT,                            -- peer mesh address (10.77.x.y)
  endpoint   TEXT,                            -- the endpoint we dial (relay or direct)
  endpoints  TEXT,                            -- JSON: what the peer reported
  relay      TEXT,                            -- JSON: {host, myPort, peerPort}
  offer      TEXT,                            -- JSON: a pending offer (ct etc.) awaiting the owner
  status     TEXT NOT NULL,                   -- invited | offered | pending | linked | declined
  role       TEXT,                            -- initiator | responder
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE TABLE IF NOT EXISTS mesh_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
