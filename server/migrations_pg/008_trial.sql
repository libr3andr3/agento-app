-- Beta launch: every business gets a free trial window (default 7 days).
-- Existing rows are grandfathered onto a fresh window from migration time,
-- which is exactly what the friends-and-family launch wants.
ALTER TABLE businesses
    ADD COLUMN IF NOT EXISTS trial_ends_at TIMESTAMPTZ NOT NULL
        DEFAULT (now() + interval '7 days');
