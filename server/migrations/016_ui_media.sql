-- D15 (2026-08-30): the owner's work queue and the catalog with photos.
-- `done` is the owner's swipe: the row leaves the queue, money already
-- counted stays counted.
ALTER TABLE orders ADD COLUMN done_at TEXT;
ALTER TABLE appointments ADD COLUMN done_at TEXT;
-- Catalog photos live on the phone like everything else. Bytes are the
-- app's resized JPEG (long edge ~1280px); a private link is minted from
-- them on demand and forgotten by the gateway minutes later.
CREATE TABLE IF NOT EXISTS media (
    id          BLOB PRIMARY KEY,
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    product     TEXT,
    caption     TEXT,
    mime        TEXT NOT NULL DEFAULT 'image/jpeg',
    size        INTEGER NOT NULL,
    bytes       BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_media_biz ON media(business_id, created_at DESC);
