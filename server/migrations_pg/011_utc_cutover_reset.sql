-- UTC cutover reset (2026-08-19, approved by the owner — sole tester).
--
-- Rows written before migration 010 carry Lima-wall-clock timestamps stamped
-- as UTC; under the real-UTC code every one of them is 5 hours wrong, and
-- there is no way to tell a pre-cutover row from a post-cutover one. Rather
-- than shift data we cannot fully trust, the operational tables start clean.
-- Runs exactly once (sqlx migrator); a fresh database truncates nothing.

TRUNCATE businesses CASCADE;   -- takes devices, appointments, orders, payments,
                               -- gap_events, candidates, messages, tool_events,
                               -- usage_counters, conversation state with it
TRUNCATE phone_verifications;  -- no FK to businesses; also pre-cutover
