-- Coins (GNU Taler shape): blind-signed by the yaya exchange, verified
-- offline against pinned denomination keys, passed peer to peer over
-- Bluetooth, the mesh or the E2E relay. A coin we withdrew carries its
-- secret (sealed); a coin we received carries the payer's signed spend
-- statement instead, and becomes ours for good once refreshed.
CREATE TABLE IF NOT EXISTS coins (
  coin         TEXT PRIMARY KEY,                -- agent:<hex> of the coin key
  secret       BLOB,                            -- sealed 32 B seed (only for coins we withdrew/refreshed)
  denomination TEXT NOT NULL,
  value_minor  INTEGER NOT NULL,
  sig          TEXT NOT NULL,                   -- exchange signature (base64)
  randomizer   TEXT NOT NULL,                   -- RSABSSA message randomizer (base64)
  spend        TEXT,                            -- JSON envelope by the coin key (received coins)
  status       TEXT NOT NULL,                   -- fresh | received | spent | refreshed | deposited | bad
  peer         TEXT,                            -- who we paid / who paid us
  payment      TEXT,                            -- coin_payments.id
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
  updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_coins_status ON coins(status, value_minor);
CREATE TABLE IF NOT EXISTS coin_payments (
  id         TEXT PRIMARY KEY,                  -- nonce of the payment
  direction  TEXT NOT NULL,                     -- out | in
  peer       TEXT,
  amount     INTEGER NOT NULL,
  coins      INTEGER NOT NULL,
  via        TEXT,                              -- relay | mesh | bluetooth | offline
  note       TEXT,
  acked      INTEGER NOT NULL DEFAULT 0,
  payload    TEXT,                              -- the payment JSON (out: what we sent)
  at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE TABLE IF NOT EXISTS exchange_keys (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  exchange   TEXT NOT NULL,                     -- pinned signer id (TOFU)
  doc        TEXT NOT NULL,                     -- the signed keys document
  fetched_at TEXT NOT NULL
);
