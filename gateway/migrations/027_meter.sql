-- D19: the meter is CONSUMPTION, not outcome.
--
-- Until now the prepaid balance was charged when the phone reported a
-- confirmed booking or sale ($1 known client / $2 new). From here it is
-- charged for the compute the account actually consumes: prompt and
-- completion tokens, seconds of speech recognised, characters spoken,
-- images looked at. Outcome pricing is not deleted — `METER_OUTCOMES=1`
-- brings it back — it is simply off, so phones already in the field can
-- keep calling `/v1/outcomes/confirm` and get a `charged: 0` answer
-- instead of an error.
--
-- The unit in the ledger does not change: it stays one minor unit of the
-- closed loop (a USD cent today). What a unit is *worth* lives entirely in
-- the METER_* rates, so re-pegging it later is a config change and never a
-- migration over money that has already moved.

-- A single chat turn costs a fraction of a cent, and rounding every turn up
-- to a whole cent would overcharge by orders of magnitude while rounding
-- down would charge nothing at all. So consumption accrues here in
-- micro-USD (millionths of a dollar; 10 000 = one cent) and only crosses
-- into `prepaid_ledger` when it has accumulated at least one whole cent.
-- The remainder stays owed and is carried to the next call.
CREATE TABLE IF NOT EXISTS prepaid_meter (
    account     TEXT PRIMARY KEY,
    micros      INTEGER NOT NULL DEFAULT 0,   -- sub-cent remainder, always 0..9999
    micros_life INTEGER NOT NULL DEFAULT 0,   -- everything ever metered, for reconciliation
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- What each debit was made of, so a statement line can say "1 240 tokens"
-- and not just "1 cent". One row per flushed cent-crossing, joined to the
-- ledger rows it produced by `ledger_id`.
CREATE TABLE IF NOT EXISTS prepaid_meter_log (
    id          TEXT PRIMARY KEY,
    account     TEXT NOT NULL,
    agent       TEXT,
    kind        TEXT NOT NULL,                -- chat | voice_in | voice_out | vision
    micros      INTEGER NOT NULL,             -- what this call metered, before the carry
    cents       INTEGER NOT NULL,             -- what it flushed to the ledger (may be 0)
    detail      TEXT,                         -- {promptTokens, completionTokens, seconds, chars, images}
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_meter_log_account ON prepaid_meter_log(account, created_at DESC);
