-- Country chosen at registration: the root of the business's locale
-- (language, currency, timezone default, payment rails). Existing rows
-- predate the picker and were all Peruvian.
ALTER TABLE businesses ADD COLUMN IF NOT EXISTS country TEXT NOT NULL DEFAULT 'PE';
-- Payments now record the currency the bank notification named (ISO code,
-- NULL when the text carried only an unqualified number).
ALTER TABLE payments ADD COLUMN IF NOT EXISTS currency TEXT;
-- Which app posted the notification (package), for the founder's view of
-- what rails are actually being used per country.
ALTER TABLE payments ADD COLUMN IF NOT EXISTS source_package TEXT;
