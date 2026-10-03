-- Yaya ID without passwords: an account is a name, an email and a phone;
-- proof is a one-time code delivered over WhatsApp and email. The backup
-- key is minted here per account and handed to signed-in devices/browsers.
ALTER TABLE accounts ADD COLUMN phone TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_phone ON accounts(phone) WHERE phone IS NOT NULL;
ALTER TABLE accounts ADD COLUMN backup_key TEXT;

CREATE TABLE IF NOT EXISTS account_otps (
    id          TEXT PRIMARY KEY,
    email       TEXT,
    phone       TEXT,
    name        TEXT,
    code_hash   TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    expires_at  TEXT NOT NULL,
    consumed_at TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_account_otps_target ON account_otps(email, phone, created_at DESC);
