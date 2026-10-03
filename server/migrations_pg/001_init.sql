CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS businesses (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name          TEXT NOT NULL,
    industry      TEXT NOT NULL,
    owner_phone   TEXT NOT NULL,
    schema_config JSONB NOT NULL DEFAULT '{}',
    onboarded     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS devices (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    token       TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL DEFAULT 'phone',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS appointments (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id   UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    customer_name TEXT NOT NULL,
    phone         TEXT NOT NULL,
    specialist    TEXT,
    starts_at     TIMESTAMPTZ NOT NULL,
    status        TEXT NOT NULL DEFAULT 'confirmed', -- confirmed | pending_payment | cancelled
    paid          BOOLEAN NOT NULL DEFAULT FALSE,
    price         NUMERIC,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_appointments_biz_time ON appointments(business_id, starts_at);

CREATE TABLE IF NOT EXISTS conversation_logs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    agent_type  TEXT NOT NULL,          -- onboarding | customer
    peer        TEXT NOT NULL,          -- phone number / social handle
    messages    JSONB NOT NULL DEFAULT '[]',
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (business_id, agent_type, peer)
);

CREATE TABLE IF NOT EXISTS payments (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id  UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    source       TEXT NOT NULL,           -- yape | plin-<bank> | app package
    payer        TEXT,
    amount       NUMERIC,
    raw_text     TEXT NOT NULL,
    appointment_id UUID REFERENCES appointments(id),
    received_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_payments_biz_time ON payments(business_id, received_at);
