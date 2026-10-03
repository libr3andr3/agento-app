-- Business plans are charged through yaya.cash (our own processor): the
-- request keeps the charge it opened there.
ALTER TABLE plan_requests ADD COLUMN payment_id TEXT;
ALTER TABLE plan_requests ADD COLUMN amount_minor INTEGER;
