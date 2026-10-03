-- Per-business daily spend counters, and a bound on reminder email.
--
-- Registration throttling is in-memory (see limits.rs); these counters are in
-- the database because a spend ceiling must survive a restart.

CREATE TABLE IF NOT EXISTS usage_counters (
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    day         DATE NOT NULL,              -- Lima wall-clock day
    kind        TEXT NOT NULL,              -- llm | stt | tts
    n           BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (business_id, day, kind)
);
CREATE INDEX IF NOT EXISTS idx_usage_day ON usage_counters(day);

-- How many calendar invites have gone out for an appointment. Without a cap,
-- a conversation can drive unlimited outbound mail from the business's own
-- SMTP identity.
ALTER TABLE appointments ADD COLUMN IF NOT EXISTS reminder_sends INT NOT NULL DEFAULT 0;
