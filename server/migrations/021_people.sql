-- One customer, however many apps they write from.
--
-- Until now a customer WAS their conversation: `contacts.peer` is
-- `<package>:<display name>`, so the same woman asking about a delivery on
-- WhatsApp and then on Instagram is two rows, two histories, and an agent that
-- greets her as a stranger the second time. `contacts` could only ever merge
-- two rows when both carried a phone number, and Instagram and Messenger never
-- give one.
--
-- `people` is the human. `person_links` is every way we have seen them reach
-- us. The conversation keying is untouched: `peer` stays exactly what it was,
-- so message history, the audit chain and D14's `conversation_months` billing
-- keep working and nobody is re-billed for a customer they already paid for.
CREATE TABLE IF NOT EXISTS people (
    id          BLOB PRIMARY KEY,
    business_id BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    name        TEXT,
    phone       TEXT,                                  -- digits, country code first
    email       TEXT,
    notes       TEXT,
    first_seen  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_seen   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE INDEX IF NOT EXISTS idx_people_phone ON people(business_id, phone);
CREATE INDEX IF NOT EXISTS idx_people_email ON people(business_id, email);
CREATE INDEX IF NOT EXISTS idx_people_seen  ON people(business_id, last_seen DESC);

-- (channel, handle) is the identity the phone reports: a channel groups the
-- apps that share an address space (`com.whatsapp` and `com.whatsapp.w4b` are
-- both `whatsapp`), and a handle is the customer's phone digits where the app
-- gives them and their normalised display name where it does not. The pair is
-- unique per business: seeing it twice IS the same person, by definition.
CREATE TABLE IF NOT EXISTS person_links (
    business_id  BLOB NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    channel      TEXT NOT NULL,
    handle       TEXT NOT NULL,
    person_id    BLOB NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    display_name TEXT,
    -- The legacy conversation key this link last spoke on, so a person can be
    -- walked back to their history without changing how history is stored.
    peer         TEXT,
    -- 1 when `handle` is phone digits, i.e. safe to match across channels.
    is_phone     INTEGER NOT NULL DEFAULT 0,
    first_seen   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_seen    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    PRIMARY KEY (business_id, channel, handle)
);
CREATE INDEX IF NOT EXISTS idx_links_person ON person_links(person_id);
CREATE INDEX IF NOT EXISTS idx_links_peer   ON person_links(business_id, peer);

-- Which human this conversation belongs to. Nullable: rows written before this
-- migration keep working untouched and are adopted the next time that customer
-- sends a message.
ALTER TABLE contacts ADD COLUMN person_id BLOB;
CREATE INDEX IF NOT EXISTS idx_contacts_person ON contacts(business_id, person_id);
