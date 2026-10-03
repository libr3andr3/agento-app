-- Comunidad: referrals between businesses, reported (signed) by the receiver.
-- `charged` is the lead price moved from the receiver's account to the
-- sender's; 0 when unpaid (unlinked, same account, low balance, or a second
-- referral for the same pair within 30 days).
CREATE TABLE IF NOT EXISTS referrals (
    id         TEXT PRIMARY KEY,
    from_agent TEXT NOT NULL,
    to_agent   TEXT NOT NULL,
    status     TEXT NOT NULL,
    charged    INTEGER NOT NULL DEFAULT 0,
    at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS referrals_pair_at ON referrals(from_agent, to_agent, at);
