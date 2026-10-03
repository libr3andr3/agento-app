-- Use it or lose it, for plan-included credits only.
--
-- A positive ledger row is a *lot*. Lots granted by a plan (`kind = grant`)
-- carry `expires_at` = the first instant of the next calendar month in the
-- expiry timezone (America/Lima by default): unspent remainder is written off
-- with a `kind = expire` row. Bought balance (topup), bonuses and earnings
-- never expire — the law on both sides of the border (Ley 29571 arts. 47/49;
-- Cal. Civ. Code 1749.5/1749.45; 12 CFR 1005.20) protects purchased value and
-- allows promotional/subscription-included value to expire if disclosed.
--
-- Every negative row written from now on names the lot it consumed (`lot`),
-- expiring lots first, so what expires is exactly what was not used.
ALTER TABLE credit_ledger ADD COLUMN expires_at TEXT;
ALTER TABLE credit_ledger ADD COLUMN lot TEXT;
CREATE INDEX IF NOT EXISTS idx_ledger_expiry ON credit_ledger(account, expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ledger_lot ON credit_ledger(lot) WHERE lot IS NOT NULL;
