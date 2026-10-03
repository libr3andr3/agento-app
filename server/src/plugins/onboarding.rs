//! Onboarding tools: pin the vertical bundle, persist the client patch.

use anyhow::anyhow;
use serde_json::{json, Value};

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};
use crate::learning;

pub struct Onboarding;

/// These tools rewrite the business's own configuration, so they belong to the
/// owner's interview and nobody else. `run_loop` already refuses to dispatch
/// them for a customer turn; this is the second, independent check, so neither
/// one alone is load-bearing. A customer reaching here means the first gate
/// regressed — say nothing useful about why.
fn owner_only(ctx: &ToolCtx<'_>) -> Option<Value> {
    ctx.peer.as_ref().map(|peer| {
        tracing::warn!(
            business = %ctx.business_id, %peer,
            "customer turn reached an onboarding tool — dispatch gate regressed"
        );
        json!({"error": "onboarding agent only"})
    })
}

/// Onboarding writes land in the CLIENT PATCH only — core and bundle are
/// never touched per-client. Values are merged so partial saves are safe.
async fn save_business_schema(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if let Some(denied) = owner_only(ctx) {
        return Ok(denied);
    }
    let mut schema = args
        .get("schema")
        .cloned()
        .ok_or_else(|| anyhow!("missing 'schema'"))?;
    // The model writes day keys in whatever language the interview ran in
    // ("lunes": …); converge stored patches on mon..sun at the chokepoint.
    if schema.get("businessHours").is_some() {
        schema["businessHours"] = crate::harness::canon_hours_keys(&schema["businessHours"]);
    }
    let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(ctx.business_id)
        .fetch_one(&ctx.state.db)
        .await?;
    let mut patch = row.0;
    learning::deep_merge(&mut patch, &schema);
    // Saving is incremental and does NOT complete onboarding — only the
    // explicit finish_onboarding call does. (A partial early save used to
    // flip the flag and the app celebrated "todo listo" mid-interview.)
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(&patch)
        .bind(ctx.business_id)
        .execute(&ctx.state.db)
        .await?;
    Ok(json!({"status": "saved", "patch": patch,
              "note": "values stored; onboarding continues until finish_onboarding"}))
}

/// Customer-facing tools the owner may switch off at runtime by talking to
/// their manager agent. A fixed allowlist rather than "any tool name": the
/// conversation must never be able to disable the owner's own controls.
const TOGGLABLE: &[&str] = &[
    "check_availability", "book_appointment", "handle_cancellation",
    "create_order", "quote_delivery", "collect_payment", "schedule_reminder",
    "report_gap",
];

/// Deletes one value from the client patch by dot path ("products.gorra",
/// "delivery.freeAt"). save_business_schema only merges — without this, a
/// discontinued product would haunt the catalog forever.
async fn remove_field(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if let Some(denied) = owner_only(ctx) {
        return Ok(denied);
    }
    let path = args["path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| anyhow!("missing 'path'"))?;
    let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(ctx.business_id)
        .fetch_one(&ctx.state.db)
        .await?;
    let mut patch = row.0;
    let parts: Vec<&str> = path.split('.').collect();
    let (parents, leaf) = parts.split_at(parts.len() - 1);
    let mut cur = &mut patch;
    for p in parents {
        match cur.get_mut(*p) {
            Some(next) => cur = next,
            None => return Ok(json!({"status": "not_found", "path": path})),
        }
    }
    let removed = cur
        .as_object_mut()
        .and_then(|m| m.remove(leaf[0]))
        .is_some();
    if !removed {
        return Ok(json!({"status": "not_found", "path": path}));
    }
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(&patch)
        .bind(ctx.business_id)
        .execute(&ctx.state.db)
        .await?;
    Ok(json!({"status": "removed", "path": path}))
}

/// Runtime capability toggle: writes `disabledTools` in the client patch;
/// the customer agent's tool list is filtered against it on every turn, so
/// "ya no quiero reservas" takes effect in the very next conversation and
/// flipping it back is just another sentence.
async fn set_capability(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if let Some(denied) = owner_only(ctx) {
        return Ok(denied);
    }
    let tool = args["tool"].as_str().ok_or_else(|| anyhow!("missing 'tool'"))?;
    let enabled = args["enabled"]
        .as_bool()
        .ok_or_else(|| anyhow!("missing 'enabled' (bool)"))?;
    if !TOGGLABLE.contains(&tool) {
        return Ok(json!({"status": "unknown_tool", "togglable": TOGGLABLE}));
    }
    let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(ctx.business_id)
        .fetch_one(&ctx.state.db)
        .await?;
    let mut patch = row.0;
    let mut off: Vec<String> = patch["disabledTools"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if enabled {
        off.retain(|t| t != tool);
    } else if !off.iter().any(|t| t == tool) {
        off.push(tool.to_string());
    }
    patch["disabledTools"] = json!(off);
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(&patch)
        .bind(ctx.business_id)
        .execute(&ctx.state.db)
        .await?;
    Ok(json!({"status": "ok", "tool": tool, "enabled": enabled, "disabledTools": off}))
}

/// The single, deliberate end of the interview. The APK's "¡Todo listo!"
/// moment keys on this action — nothing else may trigger it.
async fn finish_onboarding(ctx: &ToolCtx<'_>, _args: &Value) -> anyhow::Result<Value> {
    if let Some(denied) = owner_only(ctx) {
        return Ok(denied);
    }
    // D15: an app must exist when the celebration screen opens. If the
    // agent never called design_ui, the vertical's template becomes the
    // design — the owner can rename tabs later by talking to the agent.
    let composed = learning::compose(&ctx.state.db, &ctx.state.schemas_dir, ctx.business_id).await?;
    let mut note = "the business agent is now live".to_string();
    if composed.doc["_uiDesigned"].as_bool() != Some(true) {
        let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
            .bind(ctx.business_id)
            .fetch_one(&ctx.state.db)
            .await?;
        let mut patch = row.0;
        patch["ui"] = composed.doc["_uiTemplate"].clone();
        sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
            .bind(&patch)
            .bind(ctx.business_id)
            .execute(&ctx.state.db)
            .await?;
        note.push_str("; the app uses the vertical's default tabs (design_ui was not called)");
    }
    sqlx::query("UPDATE businesses SET onboarded = TRUE WHERE id = $1")
        .bind(ctx.business_id)
        .execute(&ctx.state.db)
        .await?;
    Ok(json!({"status": "onboarded", "note": note}))
}

/// Pins the business to a vertical bundle at its current version.
async fn set_bundle(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if let Some(denied) = owner_only(ctx) {
        return Ok(denied);
    }
    let want = args["vertical"].as_str().unwrap_or("generic");
    let (pin, bundle) = learning::pin_bundle(&ctx.state.db, &ctx.state.schemas_dir, ctx.business_id, want).await?;
    let vertical = pin.split('@').next().unwrap_or("generic").to_string();

    // Tell the onboarding agent what this vertical has learned to ask.
    let must_ask: Vec<Value> = bundle["fields"]
        .as_object()
        .map(|fs| {
            fs.iter()
                .filter(|(_, f)| f["ask_in_onboarding"].as_bool() == Some(true))
                .map(|(k, f)| json!({"field": k, "question_es": f["question_es"]}))
                .collect()
        })
        .unwrap_or_default();
    // D15: the vertical's operating knowledge and the UI template the
    // agent will personalize with design_ui at the end of the interview.
    let skill = std::fs::read_to_string(learning::bundle_dir(&ctx.state.schemas_dir, &vertical).join("skill.md"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let kind = ctx.values["businessKind"].as_str().unwrap_or("both");
    Ok(json!({
        "status": "pinned", "bundle": pin, "askInOnboarding": must_ask,
        "skill": skill,
        "uiTemplate": crate::ui::template(&bundle["ui"], kind),
        "note": "read `skill` before the next question: it says what to ask first and how each answer steers the next one. `uiTemplate` is what design_ui starts from.",
    }))
}

/// The owner's app, designed by the agent (D15): tabs, their names in the
/// owner's words, a one-line intro per tab for the walkthrough. Validated
/// against the block catalog — whatever survives is what the phone draws.
async fn design_ui(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if let Some(denied) = owner_only(ctx) {
        return Ok(denied);
    }
    // Fresh compose: businessKind may have been saved earlier this turn.
    let composed = learning::compose(&ctx.state.db, &ctx.state.schemas_dir, ctx.business_id).await?;
    let kind = composed.values["businessKind"].as_str().unwrap_or("both").to_string();
    let spec = if args.get("tabs").is_some() { args.clone() } else { args["ui"].clone() };
    let Some(ui) = crate::ui::normalize(&spec, &kind) else {
        return Ok(json!({
            "status": "invalid",
            "note": "no valid tab survived — every tab needs a label and at least one block from the catalog that fits this business",
            "template": composed.doc["_uiTemplate"],
            "blocks": crate::ui::BLOCKS,
        }));
    };
    let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(ctx.business_id)
        .fetch_one(&ctx.state.db)
        .await?;
    let mut patch = row.0;
    patch["ui"] = ui.clone();
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(&patch)
        .bind(ctx.business_id)
        .execute(&ctx.state.db)
        .await?;
    Ok(json!({"status": "ok", "ui": ui, "note": "the app redraws with these tabs on its next refresh"}))
}

impl Plugin<Agente> for Onboarding {
    fn name(&self) -> &'static str {
        "onboarding"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Onboarding, true),
            json!({"type": "function", "function": {
                "name": "set_bundle",
                "description": "Pin the business to its vertical bundle as soon as you know the type of business. Returns the questions this vertical has learned to ask in onboarding — you must ask ALL of them.",
                "parameters": {
                    "type": "object",
                    "properties": {"vertical": {"type": "string", "description": "one of the available verticals; use 'generic' if none fits"}},
                    "required": ["vertical"]
                }
            }}),
            tool_fn(|c, a| Box::pin(set_bundle(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, true),
            json!({"type": "function", "function": {
                "name": "save_business_schema",
                "description": "Persist inferred values into the client patch (merged; safe to call more than once)",
                "parameters": {
                    "type": "object",
                    "properties": {"schema": {
                        "type": "object",
                        "description": "Value map keyed by schema field paths: businessKind (services|products|both), staffCount, staffSpecialties, businessHours {mon..sun: \"9-17\"}, slotDuration, walkInsAllowed, cancellationNoticeMins, paymentMethod (upfront|atVisit|transfer), bookingDeposit (number in the business currency: fixed amount paid by instant transfer just to reserve, null if they pay full price or at visit), pricing (map service->price), products (map product->price), delivery ({zones: map area->fee, default: fee for the rest of the coverage city, coverage: string (the city), freeAt: [short keywords for free meeting points]}), requireEmail (bool: bookings demand the customer's email — virtual meetings, email confirmations), maxAdvanceBookingDays, timezone (IANA zone — set ONLY if the business is outside its country's default zone), language (BCP-47 like es/en/hi — only if the owner wants a different language than the country default), currency (ISO 4217 — only if different from the country default), plus any bundle fields"
                    }},
                    "required": ["schema"]
                }
            }}),
            tool_fn(|c, a| Box::pin(save_business_schema(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, true),
            json!({"type": "function", "function": {
                "name": "remove_field",
                "description": "Delete one value from the business config by dot path — e.g. a discontinued product (products.gorra), a dropped delivery zone (delivery.zones.comas). save_business_schema only merges; this is the only way to remove.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }}),
            tool_fn(|c, a| Box::pin(remove_field(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, true),
            json!({"type": "function", "function": {
                "name": "set_capability",
                "description": "Turn a customer-facing capability on or off at runtime. Togglable: check_availability, book_appointment, handle_cancellation, create_order, quote_delivery, collect_payment, schedule_reminder, report_gap. Takes effect in the customer's very next conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string"},
                        "enabled": {"type": "boolean"}
                    },
                    "required": ["tool", "enabled"]
                }
            }}),
            tool_fn(|c, a| Box::pin(set_capability(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, true),
            json!({"type": "function", "function": {
                "name": "design_ui",
                "description": format!("Design the owner's app: up to {} tabs, each with a label in the OWNER'S OWN WORDS and language (≤16 chars), an icon, a one-line `intro` the walkthrough will show, and its blocks in order. Start from the uiTemplate set_bundle returned; keep the blocks that matter for how THIS business runs, drop the rest, order by what the owner does most. Blocks: {} Call it once at the end of the interview (before finish_onboarding) and again whenever the owner asks to change what they see first.", crate::ui::MAX_TABS, crate::ui::catalog_help()),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tabs": {"type": "array", "items": {"type": "object", "properties": {
                            "id": {"type": "string"},
                            "label": {"type": "string"},
                            "icon": {"type": "string", "enum": crate::ui::ICONS},
                            "intro": {"type": "string"},
                            "blocks": {"type": "array", "items": {"type": "string", "enum": crate::ui::BLOCKS}}
                        }, "required": ["label", "blocks"]}},
                        "home": {"type": "string", "description": "id of the tab that opens first"}
                    },
                    "required": ["tabs"]
                }
            }}),
            tool_fn(|c, a| Box::pin(design_ui(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, true),
            json!({"type": "function", "function": {
                "name": "finish_onboarding",
                "description": "Declare the interview COMPLETE and activate the business agent. Call ONLY when every core and bundle question is answered, the final schema is saved and design_ui was called. This triggers the app's completion screen.",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }}),
            tool_fn(|c, a| Box::pin(finish_onboarding(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, tool_ctx};

    async fn setup() -> (crate::SharedState, uuid::Uuid) {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        (s, b)
    }

    async fn patch(s: &crate::AppState, b: uuid::Uuid) -> Value {
        sqlx::query_scalar("SELECT schema_config FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap()
    }

    #[tokio::test]
    async fn every_owner_tool_refuses_a_customer() {
        let (s, b) = setup().await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 1")).await;
        let args = json!({"schema": {"pricing": {"corte": 1}}, "path": "x", "tool": "book_appointment", "enabled": false, "vertical": "generic", "tabs": []});
        assert_eq!(save_business_schema(&c, &args).await.unwrap()["error"], "onboarding agent only");
        assert_eq!(remove_field(&c, &args).await.unwrap()["error"], "onboarding agent only");
        assert_eq!(set_capability(&c, &args).await.unwrap()["error"], "onboarding agent only");
        assert_eq!(finish_onboarding(&c, &args).await.unwrap()["error"], "onboarding agent only");
        assert_eq!(set_bundle(&c, &args).await.unwrap()["error"], "onboarding agent only");
        assert_eq!(design_ui(&c, &args).await.unwrap()["error"], "onboarding agent only");
        assert_eq!(patch(&s, b).await, json!({}));
    }

    #[tokio::test]
    async fn save_merges_and_canonicalises_hours_remove_deletes() {
        let (s, b) = setup().await;
        let c = tool_ctx(&s, b, None).await;
        save_business_schema(&c, &json!({"schema": {"pricing": {"corte": 25}, "businessHours": {"lunes a viernes": "9-18"}}})).await.unwrap();
        save_business_schema(&c, &json!({"schema": {"pricing": {"barba": 15}}})).await.unwrap();
        let p = patch(&s, b).await;
        assert_eq!(p["pricing"], json!({"corte": 25, "barba": 15}), "merged, not replaced");
        assert_eq!(p["businessHours"]["wed"], "9-18");
        assert!(save_business_schema(&c, &json!({})).await.is_err());
        assert_eq!(remove_field(&c, &json!({"path": "pricing.barba"})).await.unwrap()["status"], "removed");
        assert_eq!(remove_field(&c, &json!({"path": "pricing.barba"})).await.unwrap()["status"], "not_found");
        assert_eq!(remove_field(&c, &json!({"path": "nope.deeper"})).await.unwrap()["status"], "not_found");
        assert!(remove_field(&c, &json!({"path": " "})).await.is_err());
        assert_eq!(patch(&s, b).await["pricing"], json!({"corte": 25}));
    }

    #[tokio::test]
    async fn capabilities_toggle_within_the_list() {
        let (s, b) = setup().await;
        let c = tool_ctx(&s, b, None).await;
        let r = set_capability(&c, &json!({"tool": "book_appointment", "enabled": false})).await.unwrap();
        assert_eq!(r["disabledTools"], json!(["book_appointment"]));
        set_capability(&c, &json!({"tool": "book_appointment", "enabled": false})).await.unwrap();
        assert_eq!(patch(&s, b).await["disabledTools"], json!(["book_appointment"]), "no duplicates");
        assert_eq!(set_capability(&c, &json!({"tool": "book_appointment", "enabled": true})).await.unwrap()["disabledTools"], json!([]));
        assert_eq!(set_capability(&c, &json!({"tool": "set_capability", "enabled": false})).await.unwrap()["status"], "unknown_tool");
        assert!(set_capability(&c, &json!({"tool": "book_appointment"})).await.is_err());
    }

    #[tokio::test]
    async fn bundles_ui_and_finishing() {
        let (s, b) = setup().await;
        let c = tool_ctx(&s, b, None).await;
        let r = set_bundle(&c, &json!({"vertical": "../../etc"})).await.unwrap();
        assert!(r["bundle"].as_str().unwrap().starts_with("generic@"), "unknown verticals fall back to generic");
        assert!(r["uiTemplate"]["tabs"].is_array());
        let (pin,): (String,) = sqlx::query_as("SELECT bundle FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!(pin, r["bundle"].as_str().unwrap());
        assert_eq!(design_ui(&c, &json!({"tabs": [{"label": "X", "blocks": ["magic"]}]})).await.unwrap()["status"], "invalid");
        let r = design_ui(&c, &json!({"ui": {"tabs": [{"label": "Caja", "blocks": ["earnings"]}]}})).await.unwrap();
        assert_eq!(r["ui"]["tabs"][0]["label"], "Caja");
        let r = finish_onboarding(&c, &json!({})).await.unwrap();
        assert_eq!(r["status"], "onboarded");
        let (on,): (bool,) = sqlx::query_as("SELECT onboarded FROM businesses").fetch_one(&s.db).await.unwrap();
        assert!(on);
        // Finishing without a designed UI mounts the vertical's default tabs.
        let (s2, b2) = setup().await;
        let c2 = tool_ctx(&s2, b2, None).await;
        assert!(finish_onboarding(&c2, &json!({})).await.unwrap()["note"].as_str().unwrap().contains("default tabs"));
        assert!(patch(&s2, b2).await["ui"]["tabs"].is_array());
    }
}
