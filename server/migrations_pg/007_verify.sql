-- Owner phone verification via WhatsApp OTP.
--
-- A 6-digit code is only 10^6 possibilities, so the hash is bookkeeping, not
-- protection; the attempts counter and the 10-minute expiry are what make
-- guessing unprofitable. The proof token minted on success IS high-entropy
-- (40 chars from a CSPRNG) and follows the device-token rule: only its
-- SHA-256 is stored.

CREATE TABLE IF NOT EXISTS phone_verifications (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Canonical E.164 digits, no '+' (see whatsapp::normalize_phone).
    phone TEXT NOT NULL,
    code_hash TEXT NOT NULL,
    attempts INT NOT NULL DEFAULT 0,
    expires_at TIMESTAMPTZ NOT NULL,
    verified_at TIMESTAMPTZ,
    -- One-time registration proof, consumed by /api/onboard_business.
    proof_hash TEXT,
    proof_expires_at TIMESTAMPTZ,
    consumed_at TIMESTAMPTZ,
    -- Meta's message id + latest delivery status from the webhook, so a
    -- "code never arrived" report can be answered from our side.
    wa_message_id TEXT,
    delivery_status TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_phone_verifications_phone
    ON phone_verifications (phone, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_phone_verifications_wa_msg
    ON phone_verifications (wa_message_id) WHERE wa_message_id IS NOT NULL;
