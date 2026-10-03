-- agente-core on-device schema (SQLite). One business per database: the
-- phone IS the server. Types: UUIDs are 16-byte BLOBs generated in Rust;
-- timestamps are RFC 3339 UTC text in exactly the shape sqlx writes
-- ("2026-08-22T12:00:00.123+00:00"), so lexical comparison == chronological
-- comparison between stored defaults and Rust-bound parameters; money is REAL;
-- JSON is TEXT.


CREATE TABLE IF NOT EXISTS businesses (
    id            BLOB PRIMARY KEY,
    name          TEXT NOT NULL,
    industry      TEXT NOT NULL,
    owner_phone   TEXT NOT NULL,
    schema_config TEXT NOT NULL DEFAULT '{}',
    onboarded     INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    bundle        TEXT NOT NULL DEFAULT 'generic@1',
    provenance    TEXT NOT NULL DEFAULT '{}',
    trial_ends_at TEXT NOT NULL DEFAULT '2999-01-01T00:00:00+00:00',
    country       TEXT NOT NULL DEFAULT 'PE',
    plan          TEXT NOT NULL DEFAULT 'free',
    msg_cap       INTEGER,
    customer_cap  INTEGER
);

CREATE TABLE IF NOT EXISTS devices (
    id          BLOB PRIMARY KEY,
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    name        TEXT NOT NULL DEFAULT 'phone',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    token_hash  TEXT,
    expires_at  TEXT,
    revoked_at  TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_token_hash ON devices(token_hash);

CREATE TABLE IF NOT EXISTS appointments (
    id             BLOB PRIMARY KEY,
    business_id    BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    customer_name  TEXT NOT NULL,
    phone          TEXT NOT NULL,
    specialist     TEXT,
    starts_at      TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'confirmed',
    paid           INTEGER NOT NULL DEFAULT 0,
    price          REAL,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    customer_email TEXT,
    remind_minutes INTEGER,
    service        TEXT,
    duration_mins  INTEGER,
    reminder_sends INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_appointments_biz_time ON appointments(business_id, starts_at);

CREATE TABLE IF NOT EXISTS payments (
    id             BLOB PRIMARY KEY,
    business_id    BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    source         TEXT NOT NULL,
    payer          TEXT,
    amount         REAL,
    raw_text       TEXT NOT NULL,
    appointment_id BLOB REFERENCES appointments(id),
    received_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    order_id       BLOB,
    currency       TEXT,
    source_package TEXT,
    meta           TEXT,
    payer_phone    TEXT,
    parsed_by      TEXT
);
CREATE INDEX IF NOT EXISTS idx_payments_biz_time ON payments(business_id, received_at);

CREATE TABLE IF NOT EXISTS gap_events (
    id                 TEXT PRIMARY KEY,
    business_id        BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    bundle             TEXT NOT NULL,
    session            TEXT NOT NULL,
    turn               INTEGER NOT NULL DEFAULT 0,
    kind               TEXT NOT NULL,
    field_path         TEXT,
    utterance_redacted TEXT NOT NULL,
    agent_fallback     TEXT NOT NULL,
    ts                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    message_id         BLOB REFERENCES messages(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_gaps_biz_time ON gap_events(business_id, ts);
CREATE INDEX IF NOT EXISTS idx_gaps_field ON gap_events(field_path);

CREATE TABLE IF NOT EXISTS candidates (
    id                TEXT PRIMARY KEY,
    origin_gap        TEXT NOT NULL REFERENCES gap_events(id),
    business_id       BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    field_path        TEXT NOT NULL,
    value             TEXT NOT NULL,
    source            TEXT NOT NULL,
    scope             TEXT NOT NULL DEFAULT 'exact_field',
    trial             TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'on_trial',
    owner_corrections INTEGER NOT NULL DEFAULT 0,
    decided_note      TEXT,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    decided_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_cands_biz ON candidates(business_id, status);
CREATE INDEX IF NOT EXISTS idx_cands_field ON candidates(field_path);

CREATE TABLE IF NOT EXISTS candidate_uses (
    id           BLOB PRIMARY KEY,
    candidate_id TEXT NOT NULL REFERENCES candidates(id) ON DELETE CASCADE,
    session      TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    ts           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_uses_cand ON candidate_uses(candidate_id);

CREATE TABLE IF NOT EXISTS usage_counters (
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    day         TEXT NOT NULL,
    kind        TEXT NOT NULL,
    n           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (business_id, day, kind)
);

CREATE TABLE IF NOT EXISTS phone_verifications (
    id               BLOB PRIMARY KEY,
    phone            TEXT NOT NULL,
    code_hash        TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    expires_at       TEXT NOT NULL,
    verified_at      TEXT,
    proof_hash       TEXT,
    proof_expires_at TEXT,
    consumed_at      TEXT,
    wa_message_id    TEXT,
    delivery_status  TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_phone_verifications_phone ON phone_verifications(phone, created_at DESC);

CREATE TABLE IF NOT EXISTS orders (
    id            BLOB PRIMARY KEY,
    business_id   BLOB NOT NULL REFERENCES businesses(id),
    customer_name TEXT NOT NULL,
    phone         TEXT NOT NULL,
    items         TEXT NOT NULL,
    total         REAL,
    status        TEXT NOT NULL DEFAULT 'pending_payment',
    paid          INTEGER NOT NULL DEFAULT 0,
    notes         TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_orders_business ON orders(business_id, created_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id          BLOB PRIMARY KEY,
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    agent_type  TEXT NOT NULL,
    peer        TEXT NOT NULL,
    idx         INTEGER NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    UNIQUE (business_id, agent_type, peer, idx)
);
CREATE INDEX IF NOT EXISTS idx_messages_convo ON messages(business_id, agent_type, peer, idx DESC);

CREATE TABLE IF NOT EXISTS tool_events (
    id          BLOB PRIMARY KEY,
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    agent_type  TEXT NOT NULL,
    peer        TEXT,
    session     TEXT NOT NULL,
    tool        TEXT NOT NULL,
    args        TEXT NOT NULL,
    result      TEXT NOT NULL,
    latency_ms  INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_tool_events_biz ON tool_events(business_id, created_at);

-- Money apps this phone has learned about. Fleet-wide promotion moves to the
-- network later; locally, "settled a bill here" is the trust signal.
CREATE TABLE IF NOT EXISTS payment_sources (
    package       TEXT PRIMARY KEY,
    label         TEXT NOT NULL DEFAULT '',
    countries     TEXT NOT NULL DEFAULT '{}',
    channel_names TEXT NOT NULL DEFAULT '[]',
    installer     TEXT,
    events        INTEGER NOT NULL DEFAULT 0,
    credits       INTEGER NOT NULL DEFAULT 0,
    settled       INTEGER NOT NULL DEFAULT 0,
    businesses    TEXT NOT NULL DEFAULT '[]',
    promoted      INTEGER NOT NULL DEFAULT 0,
    promoted_at   TEXT,
    blocked       INTEGER NOT NULL DEFAULT 0,
    first_seen    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_seen     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);

-- Agent identity (NANDA-style): one Ed25519 keypair per installation. The
-- public key is the agent's id on the network; the secret never leaves the
-- device.
CREATE TABLE IF NOT EXISTS agent_identity (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    public_key  BLOB NOT NULL,
    secret_key  BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
