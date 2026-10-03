-- A plan bought by card in Perú is a subscription on that card (Izipay).
-- The first period is the checkout itself; this row is everything after it:
-- the card token Izipay kept, the subscription it charges every period, and
-- when the next charge is due — the plan stays on until a few days past
-- that date, so a renewal that lands in Izipay's night batch never leaves
-- the agent silent for an evening.
--
-- One row per checkout. `pending` is a claim taken before calling Izipay,
-- so the IPN and the browser return racing each other create one
-- subscription, not two; `error` is retried by the next IPN delivery.
CREATE TABLE IF NOT EXISTS card_subscriptions (
    checkout       TEXT PRIMARY KEY,                 -- the checkout whose payment started it
    account        TEXT NOT NULL,
    provider       TEXT NOT NULL,                    -- izipay
    plan           TEXT NOT NULL,
    months         INTEGER NOT NULL,                 -- 1 = monthly, 12 = yearly
    amount_minor   INTEGER NOT NULL,
    currency       TEXT NOT NULL,
    token          TEXT NOT NULL,                    -- the card on file (paymentMethodToken)
    external_id    TEXT,                             -- the provider's subscription id
    rrule          TEXT,
    status         TEXT NOT NULL DEFAULT 'pending',  -- pending | active | error | cancelled
    next_charge_on TEXT,                             -- YYYY-MM-DD (UTC) of the next installment
    last_error     TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    cancelled_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_card_subscriptions_account ON card_subscriptions(account, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_card_subscriptions_external ON card_subscriptions(provider, external_id) WHERE external_id IS NOT NULL;
