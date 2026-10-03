-- Outbound campaigns for a node (the core with a WhatsApp channel through
-- wa-node): who to write to, with what goal, how fast. Replies land in the
-- ordinary `messages` conversation of the peer (`wa:<digits>`).
CREATE TABLE IF NOT EXISTS campaigns (
    id          TEXT PRIMARY KEY,
    business_id TEXT NOT NULL,
    name        TEXT NOT NULL,
    goal        TEXT NOT NULL,            -- what the agent is trying to achieve, in its own words
    opener      TEXT NOT NULL,            -- first message; {name} is replaced
    status      TEXT NOT NULL DEFAULT 'active',   -- active | paused | done
    daily_cap   INTEGER NOT NULL DEFAULT 80,
    pace_secs   INTEGER NOT NULL DEFAULT 90,      -- minimum seconds between two openers
    quiet_from  INTEGER NOT NULL DEFAULT 20,      -- local hour: no openers from here…
    quiet_to    INTEGER NOT NULL DEFAULT 9,       -- …until here
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE TABLE IF NOT EXISTS campaign_contacts (
    campaign_id TEXT NOT NULL,
    phone       TEXT NOT NULL,            -- E.164 digits
    name        TEXT,
    status      TEXT NOT NULL DEFAULT 'pending',  -- pending | sent | replied | opted_out | failed | done
    sent_at     TEXT,
    replied_at  TEXT,
    note        TEXT,
    PRIMARY KEY (campaign_id, phone)
);
CREATE INDEX IF NOT EXISTS idx_campaign_contacts_status ON campaign_contacts(campaign_id, status);
-- One phone is never written to by two campaigns at once, and never again
-- after opting out: the opt-out list is global to the node.
CREATE TABLE IF NOT EXISTS opt_outs (
    phone TEXT PRIMARY KEY,
    at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    why   TEXT
);
