-- Landing-page trial leads: the download page collects an email before it
-- hands out the APK link. One row per email (stored lowercased); a repeat
-- submit bumps the counter instead of duplicating the address.
CREATE TABLE leads (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email        TEXT NOT NULL UNIQUE,
    source       TEXT NOT NULL DEFAULT 'landing',
    ip           TEXT,
    user_agent   TEXT,
    submissions  INTEGER NOT NULL DEFAULT 1,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
