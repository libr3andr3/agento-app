-- Free-for-everyone launch: the 7-day trial is retired in favour of a daily
-- cap per business. `plan` names the tier; the *_cap columns let sales raise
-- limits for one business without a code change (NULL = tier default,
-- 0 = unlimited).
ALTER TABLE businesses ADD COLUMN IF NOT EXISTS plan TEXT NOT NULL DEFAULT 'free';
ALTER TABLE businesses ADD COLUMN IF NOT EXISTS msg_cap INTEGER;
ALTER TABLE businesses ADD COLUMN IF NOT EXISTS customer_cap INTEGER;
-- Existing rows: the trial column stays for history but no longer gates.
UPDATE businesses SET trial_ends_at = now() + interval '100 years';
