-- Learning loop v0.1: layered schemas + gap events + candidate patches.
-- Design rules: events append-only; every record names its owner and origin;
-- anything cross-client carries shapes, never client values.

-- businesses.schema_config is reinterpreted as the CLIENT PATCH layer
-- (values only). The live schema is core ⊕ bundle ⊕ patch ⊕ trial candidates.
ALTER TABLE businesses ADD COLUMN IF NOT EXISTS bundle TEXT NOT NULL DEFAULT 'generic@1';
ALTER TABLE businesses ADD COLUMN IF NOT EXISTS provenance JSONB NOT NULL DEFAULT '{}';

-- Append-only. Never UPDATEd: a gap's resolution is discovered by joining
-- candidates.origin_gap; corrections are new events.
CREATE TABLE IF NOT EXISTS gap_events (
    id            TEXT PRIMARY KEY,             -- gap_evt_*
    business_id   UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    bundle        TEXT NOT NULL,                -- pin at emission time
    session       TEXT NOT NULL,                -- peer@YYYYMMDD
    turn          INT  NOT NULL DEFAULT 0,      -- index into conversation_logs.messages
    kind          TEXT NOT NULL,                -- missing_field | missing_value | conflicting_value | unsupported_intent
    field_path    TEXT,                         -- proposed slot; curator may rename on promotion
    utterance_redacted TEXT NOT NULL,           -- privacy boundary: enforced at write time
    agent_fallback TEXT NOT NULL,               -- deferred_to_owner | answered_generic | escalated_human
    ts            TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_gaps_biz_time ON gap_events(business_id, ts);
CREATE INDEX IF NOT EXISTS idx_gaps_field ON gap_events(field_path);

-- The experiment record: status is its state machine (on_trial -> graduated |
-- unwound | expired); the evidence lives append-only in candidate_uses.
CREATE TABLE IF NOT EXISTS candidates (
    id            TEXT PRIMARY KEY,             -- cand_*
    origin_gap    TEXT NOT NULL REFERENCES gap_events(id),
    business_id   UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    field_path    TEXT NOT NULL,
    value         JSONB NOT NULL,
    source        JSONB NOT NULL,               -- {channel, owner_utterance, confidence}
    scope         TEXT NOT NULL DEFAULT 'exact_field',
    trial         JSONB NOT NULL,               -- {window_days, min_uses, graduate_if, unwind_if}
    status        TEXT NOT NULL DEFAULT 'on_trial',
    owner_corrections INT NOT NULL DEFAULT 0,
    decided_note  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    decided_at    TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_cands_biz ON candidates(business_id, status);
CREATE INDEX IF NOT EXISTS idx_cands_field ON candidates(field_path);

-- Append-only trial evidence, one row per (session, signal).
CREATE TABLE IF NOT EXISTS candidate_uses (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    candidate_id TEXT NOT NULL REFERENCES candidates(id) ON DELETE CASCADE,
    session      TEXT NOT NULL,
    outcome      TEXT NOT NULL,                 -- resolved | confused | abandoned
    ts           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_uses_cand ON candidate_uses(candidate_id);
