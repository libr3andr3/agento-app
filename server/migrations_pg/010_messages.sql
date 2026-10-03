-- Conversations as append-only rows, replacing the conversation_logs JSONB
-- blob. The blob was a read-modify-write race (two concurrent turns from one
-- peer = one turn silently lost) and its array indices were the only anchor
-- gap events had to the raw question — indices that shifted every time the
-- 60-message window was trimmed. Rows fix the race (INSERT, no rewrite),
-- and gap events now anchor to a message id that never moves.
--
-- This is also the transcript corpus the self-improvement loop needs: full
-- turns plus every tool call, durable, replayable.

CREATE TABLE messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    agent_type  TEXT NOT NULL,              -- onboarding | customer
    peer        TEXT NOT NULL,
    idx         INT  NOT NULL,              -- position within the conversation
    role        TEXT NOT NULL,              -- user | assistant
    content     TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (business_id, agent_type, peer, idx)
);
CREATE INDEX idx_messages_convo ON messages (business_id, agent_type, peer, idx DESC);

-- Every dispatched tool call, with its result and latency. Args and results
-- stay in the tenant's own rows — same residency as the conversation itself.
CREATE TABLE tool_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    agent_type  TEXT NOT NULL,
    peer        TEXT,
    session     TEXT NOT NULL,
    tool        TEXT NOT NULL,
    args        JSONB NOT NULL,
    result      JSONB NOT NULL,
    latency_ms  INT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_tool_events_biz ON tool_events (business_id, created_at);

-- Gap events anchor to the exact user message that exposed the gap. The old
-- `turn` column stays for historical rows but is no longer written to.
ALTER TABLE gap_events ADD COLUMN message_id UUID REFERENCES messages(id) ON DELETE SET NULL;

DROP TABLE conversation_logs;
