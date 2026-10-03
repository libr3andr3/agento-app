-- The agro marketplace (bundle `agro`): the people a coordinator business
-- connects over WhatsApp. One row per conversation peer, whatever role they
-- say they play: productor (sells what they grow), comprador (buys for a
-- company or business), transportista (moves the cargo). `profile` is the
-- role's interview so far (nombre, ubicacion, productos, …); `status` turns
-- 'complete' once every required field of the role is in.
CREATE TABLE IF NOT EXISTS agro_participants (
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    peer        TEXT NOT NULL,
    phone       TEXT,                                  -- digits, when the peer is a phone
    role        TEXT NOT NULL CHECK (role IN ('productor', 'comprador', 'transportista')),
    name        TEXT,
    profile     TEXT NOT NULL DEFAULT '{}',
    status      TEXT NOT NULL DEFAULT 'onboarding' CHECK (status IN ('onboarding', 'complete')),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    completed_at TEXT,
    PRIMARY KEY (business_id, peer)
);
CREATE INDEX IF NOT EXISTS idx_agro_role ON agro_participants(business_id, role, status);
