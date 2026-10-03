-- The market: packs bought on the yaya network and mounted on this phone.
-- A pack is a bundle-shaped doc (fields = niche questions, defaults, a
-- skill text, tools) the composer merges after the vertical bundle; the
-- gateway is the source of truth (GET /v1/purchases), this is the cache.
CREATE TABLE IF NOT EXISTS mounted_bundles (
  listing    TEXT PRIMARY KEY,               -- gateway listing id
  title      TEXT NOT NULL,
  kind       TEXT NOT NULL,                  -- bundle
  doc        TEXT NOT NULL,                  -- JSON: {fields, defaults, skill, tools}
  mounted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
