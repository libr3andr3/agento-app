-- Fleet-wide registry of apps that deliver "money arrived" notifications.
-- Every forwarded notification updates its row; once a package has proven
-- itself (settled real bills, or credits across several businesses) it is
-- promoted and pushed back to every APK in that country — a bank that
-- launches tomorrow is recognised without an app release.
CREATE TABLE IF NOT EXISTS payment_sources (
    package         TEXT PRIMARY KEY,
    label           TEXT NOT NULL DEFAULT '',
    countries       JSONB NOT NULL DEFAULT '{}',   -- {"PE": events, "CO": events}
    channel_names   JSONB NOT NULL DEFAULT '[]',   -- distinct channel names seen (max 8)
    installer       TEXT,
    events          INTEGER NOT NULL DEFAULT 0,    -- notifications forwarded
    credits         INTEGER NOT NULL DEFAULT 0,    -- parsed as incoming money
    settled         INTEGER NOT NULL DEFAULT 0,    -- actually closed a bill
    businesses      JSONB NOT NULL DEFAULT '[]',   -- distinct business ids (max 50)
    promoted        BOOLEAN NOT NULL DEFAULT FALSE,
    promoted_at     TIMESTAMPTZ,
    blocked         BOOLEAN NOT NULL DEFAULT FALSE, -- founder veto
    first_seen      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen       TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- The full notification envelope (channel, template, sub/big text, lines,
-- installer…) so parsing can be improved retroactively against real data.
ALTER TABLE payments ADD COLUMN IF NOT EXISTS meta JSONB;
ALTER TABLE payments ADD COLUMN IF NOT EXISTS payer_phone TEXT;
ALTER TABLE payments ADD COLUMN IF NOT EXISTS parsed_by TEXT;  -- rules | llm
