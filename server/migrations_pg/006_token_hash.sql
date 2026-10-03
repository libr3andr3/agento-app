-- Device tokens at rest: store SHA-256, never the token itself.
--
-- A database dump was a set of working credentials — devices.token was the
-- literal bearer token. Postgres has sha256() built in, so the lookup stays an
-- indexed equality test and the server needs no hashing dependency. The tokens
-- are 40 CSPRNG characters, far beyond enumeration, so no salt or KDF applies.
--
-- main.rs re-runs every migration on each boot, so this has to be re-runnable:
-- the backfill is guarded on the old column still existing.

ALTER TABLE devices ADD COLUMN IF NOT EXISTS token_hash  TEXT;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS expires_at  TIMESTAMPTZ;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS revoked_at  TIMESTAMPTZ;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'devices' AND column_name = 'token'
    ) THEN
        UPDATE devices
           SET token_hash = encode(sha256(token::bytea), 'hex')
         WHERE token_hash IS NULL;
        ALTER TABLE devices DROP COLUMN token;
    END IF;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_token_hash ON devices(token_hash);
