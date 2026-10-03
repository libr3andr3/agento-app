-- "Help make agente better": shared turns are tagged by niche
-- (industry-COUNTRY) and curated into one skill per niche that every phone
-- in it mounts into its system prompt.
ALTER TABLE training_samples ADD COLUMN niche TEXT;
CREATE INDEX IF NOT EXISTS idx_training_niche ON training_samples(niche, created_at DESC);

CREATE TABLE IF NOT EXISTS niche_skills (
    niche      TEXT PRIMARY KEY,
    skill      TEXT NOT NULL,
    version    INTEGER NOT NULL DEFAULT 1,
    samples    INTEGER NOT NULL DEFAULT 0,          -- how many turns informed it
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
