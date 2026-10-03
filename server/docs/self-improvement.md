# Self-improvement track: from learning loop v0.1 to a harness that learns

Status: **design** (approved scope: design only; implementation milestones listed at the end)

## Where this comes from

The SIA paper ("Self-Improving AI with Harness & Weight Updates", arXiv:2605.27276)
frames agent improvement as two levers driven by one Feedback-Agent: **harness
updates** (prompts, tool dispatch, parsing, retry logic — what makes the model
agentic) and **weight updates** (what builds domain intuition). Its Feedback-Agent
consumes *complete trajectories* — every prompt, response, tool call, and result —
plus performance metrics, and emits the next generation of the agent.

agente's learning loop v0.1 already implements the harness-update *shape* for one
layer: business **knowledge**. Gap → candidate → trial (`graduate_if`/`unwind_if`)
→ patch, with provenance, plus human-curated cross-client promotion into versioned
bundles. What it does not yet do:

1. Nothing observes the agent from outside — gaps are self-reported by the acting
   agent (`report_gap`), so failures the agent doesn't notice are never recorded.
2. Outcome signals (bookings paid, cancelled, ghosted) exist in the DB but feed
   nothing except candidate trials.
3. The harness itself — prompt sections, tool descriptions — is Rust string
   constants: unversioned, un-trialable, unwound only by a deploy.
4. There is no eval gate. Notably the SIA paper has none either (it names
   "coupled co-evolutionary Goodhart" as an open limitation); we add one on
   purpose before any automated harness mutation.

The substrate now exists: the `messages` table (full turns, append-only) and
`tool_events` (every dispatched call, args, result, latency) added in migration
010 are exactly the trajectory record SIA's Feedback-Agent reads.

## The four stages, in dependency order

### 1. Outcome metrics (`metrics_daily`)

A nightly job (tokio interval task, or a cron hitting an admin route) computing,
per business per local day, from tables that already exist:

- turns, distinct customer sessions
- bookings/orders created → reached `pending_payment` → `paid` → cancelled
- **ghost rate**: sessions whose last message is an unanswered assistant question
  and the customer never returns within 48h
- gap events emitted; gaps answered by the owner; median time-to-answer
- blocked out-of-scope tool calls (the probing signal `run_loop` already logs)

Stored as one row per (business_id, day, metric, value). Cheap, pure SQL over
`messages` / `tool_events` / `appointments` / `orders` / `gap_events`. This is
the objective function every later stage scores against — build it first and
let it accumulate history before anything consumes it.

### 2. Offline feedback pass (the Feedback-Agent, scoped)

A daily batch job, **not** in the request path: a cheap model reads yesterday's
transcripts (`messages` + `tool_events` + outcomes for the session) and emits
findings **into the existing `gap_events` table via `learning::emit_gap`** —
same redaction, same dashboard, same curator report. The actor stops grading
itself; the reviewer sees what the actor missed:

- confidently-wrong answers (reply contradicts composed `values`)
- lost bookings (customer proposed a time, no `book_appointment` call followed)
- policy drift (promises outside mounted capability, tone failures)

Constraints: hard daily cap per business (it is LLM spend), findings carry
`source: feedback_pass` in the gap row so curator reports can separate
self-reported from observed gaps. No write access to anything but `gap_events`.
This stage needs no new mechanism — it is a new *producer* for a consumer
(owner dashboard + curator) that already works.

### 3. Prompt sections as candidates (`prompt_candidates`)

Generalize the candidate machinery from schema facts to prompt text. One table
mirroring `candidates`: id, business_id, section text, origin (gap id or
feedback-pass finding), `trial` JSON with the same `graduate_if`/`unwind_if`
shape — except evidence is **stage-1 metric deltas** over the trial window
(booking conversion, ghost rate), not substring matches.

Injection point already exists: the learning plugin's `prompt/customer`
waterfall hook fetches this business's active prompt candidates and appends
them as sections. **No kernel runtime mutation needed** — the kernel stays
immutable; the *data* the hook reads changes. Unwinding = the row flips status
and the section simply stops composing, the same guarantee schema trials have.

Cross-client promotion stays human: a graduated section that recurs across a
vertical goes to the curator, into the bundle (a `prompt_sections:` key next to
`tools:`), version-bumped, CHANGELOG'd — the exact paper trail bundles already
have.

### 4. Eval gate (our addition over the paper)

Before stage 3 is allowed to *automatically* trial anything, `run_loop` must be
testable without a live provider:

- Extract an `LlmClient` trait from `llm::Llm` (one method: `chat`); `AppState`
  holds `Arc<dyn LlmClient>`. Production impl unchanged; test impl is a script
  of canned responses/tool calls.
- Replay suite: scenario files (persona turns + scripted model behavior +
  expected tool calls/DB effects) exercising booking, payment verification,
  cancellation, gap reporting, tool gating. Seed scenarios come from real
  transcripts in `messages`, redacted.
- Gate rule: **no automated graduation of a prompt candidate without a green
  eval run** on the current build. The curator can override; the machine
  cannot. This is the anti-Goodhart backstop SIA lacks: the eval set is fixed
  independently of what the optimizing loop can see.

This also, finally, puts the most intricate control flow in the codebase
(`run_loop`, the settlement paths end-to-end) under test.

## Explicit non-goal: weight updates

SIA's second lever (LoRA + RL on an open model) is wrong for this system today:
we run API models, per-vertical data volumes are tiny, and the failure modes
above are all harness-shaped. The economic analog we adopt instead: distill
graduated knowledge and exemplar transcripts into per-vertical **playbooks**
(curator-approved bundle content, stage 3's promotion path). Revisit only if we
ever self-host a model.

## Milestones

| # | Deliverable | Depends on |
|---|---|---|
| M1 | `metrics_daily` job + admin report route | migration 010 (done) |
| M2 | feedback pass v0: lost-booking + contradiction detectors, capped, `source: feedback_pass` | M1 |
| M3 | `LlmClient` trait + scripted fake + 6-8 replay scenarios in CI | — (parallel) |
| M4 | `prompt_candidates` + waterfall injection, manual trials only | M1, M3 |
| M5 | automated trial start from feedback-pass findings, gated on M3 evals | M2, M3, M4 |
