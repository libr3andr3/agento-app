-- Agent0 console: the web app talks to the phone through the relay as a
-- linked device of its own; devices carry a kind and a role; presence is
-- the last inbox poll; every new account starts on a 7-day trial and
-- credits are bought directly ("recarga") and spent per call past the caps.
ALTER TABLE account_agents ADD COLUMN kind TEXT NOT NULL DEFAULT 'phone';   -- phone | web | node
ALTER TABLE account_agents ADD COLUMN role TEXT;                            -- owner-chosen: recepcionista | ventas | soporte | gerente | …
ALTER TABLE agents_seen ADD COLUMN last_poll TEXT;                          -- last GET /v1/inbox — "online" when recent

-- Accounts that predate the trial model start their 7 days now, so nobody
-- is cut off by the deploy; accounts that already bought a plan keep it.
INSERT INTO plans (agent, plan, cap, note, expires_at, source)
SELECT 'acct:' || id, 'trial', 4000, 'trial (migration 014)',
       strftime('%Y-%m-%dT%H:%M:%fZ','now','+7 days'), 'trial'
FROM accounts
WHERE 'acct:' || id NOT IN (SELECT agent FROM plans);
