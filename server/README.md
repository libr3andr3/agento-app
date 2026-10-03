# agente-server

Multi-tenant appointment-agent backend. Rust + Tokio + axum, PostgreSQL (JSONB
business schemas), DeepSeek (OpenAI-compatible API) with a hand-rolled
tool-calling loop. The Android APK forwards incoming WhatsApp/Instagram
messages here and sends back whatever `agentResponse` says.

## Run

```bash
# Postgres (user-owned instance, port 5433):
/usr/lib/postgresql/17/bin/pg_ctl -D ~/agente-server/pgdata -o "-p 5433" start
cargo run   # reads .env — DATABASE_URL, LLM_API_KEY, LLM_MODEL, ADMIN_KEY, BIND_ADDR (:8118)
```

Migrations in `migrations/001_init.sql` run automatically at startup.

## API

All bodies JSON. Device auth = `Authorization: Bearer <deviceToken>`.

| Endpoint | Auth | Purpose |
|---|---|---|
| `POST /api/onboard_business` `{businessName, industry, ownerPhone}` | `X-Admin-Key` | Create tenant, issue device token, start onboarding chat |
| `POST /api/onboarding_message` `{message}` | Bearer | Owner ↔ onboarding agent; agent saves JSONB schema when complete |
| `POST /api/execute_action` `{phoneNumber, message, conversationHistory?}` | Bearer | Customer message in → `{agentResponse, action, actionData}` |
| `GET /api/appointments` | Bearer | Upcoming bookings |
| `GET /health` | — | Liveness |

## Agents

- **Onboarding agent** — interviews the owner conversationally (staff,
  specialties, slot model, hours, walk-ins, cancellation policy, payment,
  pricing, booking window, blackouts), then calls `save_business_schema`.
- **Customer agent** — per-tenant system prompt built from the JSONB schema.
  Tools: `get_business_schema`, `check_availability` (real free slots =
  businessHours × slotDuration − booked), `book_appointment` (conflict check;
  `pending_payment` when paymentMethod is upfront/yape), `collect_payment`
  (**mock Yape** — see `tools.rs`), `handle_cancellation` (enforces
  `cancellationNoticeMins`), `send_confirmation`.

Conversations persist per (business, agent_type, peer) in
`conversation_logs`; the last 60 messages are replayed as context.

## Known simplifications (v0)

- All times treated as UTC-naive; add per-business timezone before launch.
- Yape payment is mocked — `collect_payment` trusts the customer's word.
  Real integration must verify against the Yape/bank API before confirming.
- One device token per business, issued at onboarding; no token rotation yet.
- `maxAdvanceBookingDays`/`blackoutDates` are in the schema and prompt but not
  hard-enforced in `check_availability` yet.

## Next

Wire the APK: settings screen for server URL + device token; forward parsed
notifications to `/execute_action` and send `agentResponse` via RemoteInput;
onboarding chat screen hitting `/onboarding_message`.
