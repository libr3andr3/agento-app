//! The learning loop mounted as a plugin — the showcase of why the kernel
//! exists: this one plugin contributes a tool (report_gap), prompt sections
//! for both agents, and the turn-after trial bookkeeping. Unload it and
//! agente reverts to a plain receptionist with zero residue.

use serde_json::{json, Value};

use crate::harness::{hook_fn, meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};
use crate::learning::{self, Trial};

pub struct Learning;

/// The customer agent's duty when the schema can't answer: log the gap FIRST,
/// then fall back. Emitted even if the owner never replies — the gap itself
/// is the curator's signal.
async fn report_gap(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if ctx.peer.is_none() {
        return Ok(json!({"error": "report_gap is for the customer agent only"}));
    }
    let kind = args["kind"].as_str().unwrap_or("missing_field");
    let field_path = args["field_path"].as_str().filter(|s| !s.is_empty());
    let utterance = args["customer_question_redacted"]
        .as_str()
        .or_else(|| args["utterance"].as_str())
        .unwrap_or("");
    let fallback = args["fallback"].as_str().unwrap_or("deferred_to_owner");
    let id = learning::emit_gap(
        &ctx.state.db,
        ctx.business_id,
        &ctx.bundle_pin,
        &ctx.session,
        ctx.turn,
        kind,
        field_path,
        utterance,
        fallback,
        ctx.message_id,
    )
    .await?;
    // Same question again while a candidate for this field is mounted:
    // the trial value confused the customer — that's evidence too.
    learning::record_confusion(&ctx.state.db, &ctx.trials, &ctx.session, field_path).await?;
    tracing::info!(gap = %id, kind, field = ?field_path, "gap event");
    // Broadcast for future subscribers (owner push notification, analytics).
    ctx.state
        .kernel
        .emit(&ctx.hook(), "gap/emitted", json!({
            "gapId": id, "kind": kind, "fieldPath": field_path,
        }))
        .await;
    Ok(json!({
        "gap_id": id,
        "status": "recorded",
        "note": "the owner will see this question in their dashboard; tell the \
                 customer you'll check and get back to them shortly"
    }))
}

const CUSTOMER_PROMPT_SECTION: &str = "GAP DISCIPLINE — this is how the product learns: \
the moment the customer asks something `values` cannot answer (no slot, empty slot, they \
contradict a value, or they want something no tool does), call report_gap FIRST with a \
redacted version of their question (<ZONE>/<MONEY>/<DATE>/<PERSON>/<SERVICE> placeholders), \
then reply with your fallback: normally tell them warmly you'll confirm with the owner and \
get back to them shortly (deferred_to_owner). Never guess a price, zone, or policy that \
isn't in `values` — a logged gap beats a wrong answer. If the customer asks to speak \
with a human, the owner, or a real person, call report_gap with fallback \
escalated_human and tell them the owner has been notified and will reply right here \
in this chat (that is true: the owner's phone gets an alert). Values listed under `_trials` are \
recent owner answers on trial: use them confidently, never mention that they are \
provisional. When `values` DOES answer the question, answer directly and confidently — \
never say you need to check or consult.";

const ONBOARDING_PROMPT_SECTION: &str = "Also tell the owner, when you close the \
interview, that whenever a customer asks something new, the question will appear in \
their dashboard for a one-tap answer that the agent remembers forever.";

impl Plugin<Agente> for Learning {
    fn name(&self) -> &'static str {
        "learning"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["llm"] // answer_gap extraction goes through the mounted adapter
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, true),
            json!({"type": "function", "function": {
                "name": "report_gap",
                "description": "MUST be called the moment the schema cannot answer the customer — before replying. Redact all specifics in customer_question_redacted using typed placeholders: <ZONE>, <MONEY>, <DATE>, <TIME>, <PERSON>, <SERVICE>.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["missing_field", "missing_value", "conflicting_value", "unsupported_intent"],
                                 "description": "missing_field: schema has no slot for this; missing_value: slot exists but this business never filled it; conflicting_value: customer contradicts the schema; unsupported_intent: they want something no tool does"},
                        "field_path": {"type": "string", "description": "dot-separated camelCase slot this maps to, e.g. deliveryZones.surco, bookingDeposit; omit for unsupported_intent"},
                        "customer_question_redacted": {"type": "string", "description": "the customer's question with entities replaced by <ZONE>/<MONEY>/<DATE>/<PERSON>/<SERVICE>"},
                        "fallback": {"type": "string", "enum": ["deferred_to_owner", "answered_generic", "escalated_human"]}
                    },
                    "required": ["kind", "customer_question_redacted", "fallback"]
                }
            }}),
            tool_fn(|c, a| Box::pin(report_gap(c, a))),
        )?;

        // Prompt sections ride the waterfall; remove the plugin, they vanish.
        k.on(
            "prompt/customer",
            hook_fn(|_rt, mut payload| {
                Box::pin(async move {
                    payload["sections"]
                        .as_array_mut()
                        .map(|a| a.push(json!(CUSTOMER_PROMPT_SECTION)));
                    Ok(payload)
                })
            }),
        );
        k.on(
            "prompt/onboarding",
            hook_fn(|_rt, mut payload| {
                Box::pin(async move {
                    payload["sections"]
                        .as_array_mut()
                        .map(|a| a.push(json!(ONBOARDING_PROMPT_SECTION)));
                    Ok(payload)
                })
            }),
        );

        // Trial evidence + adjudication after every customer turn.
        k.on(
            "turn/customer/after",
            hook_fn(|rt, payload| {
                Box::pin(async move {
                    let trials: Vec<Trial> = payload["trials"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| {
                                    Some(Trial {
                                        id: t["id"].as_str()?.to_string(),
                                        field_path: t["fieldPath"].as_str()?.to_string(),
                                        value: t["value"].clone(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let session = payload["session"].as_str().unwrap_or("");
                    learning::record_trial_uses(
                        &rt.state.db,
                        &trials,
                        session,
                        payload["message"].as_str().unwrap_or(""),
                        payload["reply"].as_str().unwrap_or(""),
                    )
                    .await?;
                    learning::decide_pending(&rt.state.db, rt.business_id).await?;
                    Ok(payload)
                })
            }),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::HookRt;
    use crate::testkit::{self, tool_ctx};

    #[tokio::test]
    async fn gaps_are_recorded_redacted_and_count_against_trials() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let owner = tool_ctx(&s, b, None).await;
        assert!(report_gap(&owner, &json!({})).await.unwrap()["error"].is_string(), "customer agent only");
        let c = tool_ctx(&s, b, Some("p1")).await;
        let r = report_gap(&c, &json!({"kind": "missing_value", "field_path": "pricing.tinte", "customer_question_redacted": "¿tinte? llámame al 987654321", "fallback": "escalated_human"})).await.unwrap();
        assert_eq!(r["status"], "recorded");
        let (kind, fp, utt, fb): (String, Option<String>, String, String) = sqlx::query_as("SELECT kind, field_path, utterance_redacted, agent_fallback FROM gap_events").fetch_one(&s.db).await.unwrap();
        assert_eq!((kind.as_str(), fp.as_deref(), fb.as_str()), ("missing_value", Some("pricing.tinte"), "escalated_human"));
        assert!(!utt.contains("987654321"));
        // An empty field path is no field path; defaults fill the rest.
        report_gap(&c, &json!({"field_path": "", "utterance": "x"})).await.unwrap();
        let (fp, fb): (Option<String>, String) = sqlx::query_as("SELECT field_path, agent_fallback FROM gap_events WHERE utterance_redacted = 'x'").fetch_one(&s.db).await.unwrap();
        assert_eq!((fp, fb.as_str()), (None, "deferred_to_owner"));
    }

    #[tokio::test]
    async fn prompt_hooks_add_their_sections() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let rt = HookRt { state: &s, business_id: b };
        let customer = s.kernel.waterfall(&rt, "prompt/customer", json!({"sections": [], "tools": ["book_appointment"], "locked": ["schedule_reminder"], "firstContact": true, "lang": "es", "peer": "p"})).await;
        let text = customer["sections"].to_string();
        for needle in ["GAP DISCIPLINE", "CAPABILITY HONESTY", "book_appointment", "UNLOCK once", "TRANSPARENCY", "Hola, soy el asistente con IA de Tito"] {
            assert!(text.contains(needle), "{needle}");
        }
        let agent_peer = s.kernel.waterfall(&rt, "prompt/customer", json!({"sections": [], "firstContact": true, "peer": "agent:x"})).await;
        assert!(!agent_peer["sections"].to_string().contains("FIRST CONTACT"), "agents are not disclosed to");
        let onboarding = s.kernel.waterfall(&rt, "prompt/onboarding", json!({"sections": [], "tools": []})).await;
        assert!(onboarding["sections"].to_string().contains("dashboard for a one-tap answer"));
        assert!(!onboarding["sections"].to_string().contains("UNLOCK"), "no locked tools, no unlock line");
    }
}
