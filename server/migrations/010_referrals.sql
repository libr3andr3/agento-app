-- Comunidad: referrals between businesses on the network. One row per
-- referral, on both phones: the sender keeps the reply it waited for, the
-- receiver keeps who was referred (name + phone, only ever shared with the
-- customer's consent) so the owner can reach them.
CREATE TABLE IF NOT EXISTS referrals (
  id             TEXT PRIMARY KEY,
  direction      TEXT NOT NULL,                 -- 'out' | 'in'
  peer           TEXT NOT NULL,                 -- the other business's agent id
  peer_name      TEXT,
  service        TEXT NOT NULL,
  wanted_at      TEXT,                          -- as the customer said it
  customer_name  TEXT,
  customer_phone TEXT,
  status         TEXT NOT NULL DEFAULT 'sent',  -- sent|replied|no_reply|received|declined
  reply          TEXT,
  reply_action   TEXT,
  reply_data     TEXT,
  session        TEXT,                          -- originating customer session (out)
  created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
  updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS referrals_dir_status ON referrals(direction, status, created_at);
