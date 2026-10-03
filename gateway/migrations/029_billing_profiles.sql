-- B2B: a business's fiscal identity belongs to the business, not to one
-- checkout. Asked once — "¿tienes RUC? te hacemos factura" — and every charge
-- after it issues a deductible factura without asking again. This is also the
-- record a year-end reporting product reads: who bought, under what RUC.
--
-- Absent is a real answer. No profile means a boleta, which is what a consumer
-- gets and what a business may still take under S/700.
CREATE TABLE IF NOT EXISTS billing_profiles (
    account    TEXT PRIMARY KEY,
    doc_type   TEXT NOT NULL,              -- DNI | RUC
    doc        TEXT NOT NULL,
    name       TEXT NOT NULL,              -- razón social, as SUNAT spells it
    address    TEXT,                       -- dirección fiscal
    email      TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
