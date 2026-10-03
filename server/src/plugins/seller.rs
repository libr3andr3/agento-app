//! The seller (D14): agente sells itself. When the gateway says this agent
//! belongs to a seller account (`/v1/me → seller: true`, the founder's own
//! phone), the customer agent gets two tools: who is this number — a Pro or
//! Max customer, a trial, or a new lead — and, once an order for a plan is
//! paid (verified by the same payment notification path every business
//! uses), activate that plan on the customer's Yaya account. Every other
//! business never sees these tools: they are gated on `ToolCaps::seller`.

use anyhow::anyhow;
use serde_json::{json, Value};
use std::time::Duration;

use crate::harness::{meta_when, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Seller;

fn phone_of(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<String> {
    let raw = args["phone"].as_str().filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| ctx.peer.as_deref().map(crate::harness::canon_phone))
        .ok_or_else(|| anyhow!("missing 'phone'"))?;
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 8 {
        return Err(anyhow!("'{raw}' is not a phone with country code"));
    }
    Ok(digits)
}

async fn lookup_customer(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let phone = phone_of(ctx, args)?;
    let r = ctx.state.registry.get(&format!("/v1/seller/lookup?phone={phone}"), Duration::from_secs(15)).await
        .map_err(|e| anyhow!("gateway: {e}"))?;
    let plan = if r["found"].as_bool() == Some(true) { r["plan"].as_str().unwrap_or("free").to_string() } else { "lead".to_string() };
    crate::contacts::set_plan(&ctx.state.db, ctx.business_id, &phone, &plan).await;
    let mut out = r.clone();
    out["note"] = json!(match plan.as_str() {
        "lead" => "no Yaya account yet: a NEW LEAD. Sell the trial: they install agente, sign in with this number, and get 14 days of Pro free.",
        "trial" => "on the free trial: help them get value fast; when they are ready to pay, take the order for Plan Pro or Plan Max.",
        "pro" | "max" => "a paying customer: support them; offer Max (3 000 conversations, up to three phones) only if Pro is holding them back.",
        _ => "trial over, on the free tier (30 conversations a month): the natural moment to upgrade to Pro.",
    });
    Ok(out)
}

async fn activate_plan(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let phone = phone_of(ctx, args)?;
    let plan = args["plan"].as_str().map(|s| s.trim().to_lowercase()).unwrap_or_default();
    if !matches!(plan.as_str(), "pro" | "max") {
        return Ok(json!({"status": "rejected", "note": "plan must be pro or max"}));
    }
    let wanted = args["months"].as_i64().unwrap_or(1).clamp(1, 12);
    let orders: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
        "SELECT id, phone, items FROM orders WHERE business_id = $1 AND status = 'confirmed' AND paid = TRUE \
         AND created_at > $2 ORDER BY created_at DESC LIMIT 30",
    ).bind(ctx.business_id).bind(crate::db::ago(chrono::Duration::days(3))).fetch_all(&ctx.state.db).await?;
    // The order that pays for THIS plan: this customer's, a line that is the
    // plan itself ("Plan Pro", not any product containing "pro"), not used
    // for an activation before. Its quantity is the months paid for.
    let tail = &phone[phone.len().saturating_sub(9)..];
    let product = format!("plan {plan}");
    let mut paid: Option<(uuid::Uuid, i64)> = None;
    for (id, p, items) in &orders {
        if !crate::harness::canon_phone(p).ends_with(tail) {
            continue;
        }
        if crate::account::setting(&ctx.state.db, &format!("plan_activated:{id}")).await.is_some() {
            continue;
        }
        let items: Value = serde_json::from_str(items).unwrap_or(Value::Null);
        let qty = items.as_array().into_iter().flatten()
            .filter(|it| it["product"].as_str().is_some_and(|n| { let n = n.trim().to_lowercase(); n == product || n.starts_with(&format!("{product} ")) }))
            .map(|it| it["qty"].as_i64().unwrap_or(1).max(1))
            .sum::<i64>();
        if qty > 0 {
            paid = Some((*id, qty.min(12)));
            break;
        }
    }
    let Some((order_id, paid_months)) = paid else {
        return Ok(json!({"status": "rejected",
            "note": format!("no unused PAID order for Plan {} from +{phone} in the last days — register the order with create_order (product 'Plan {}') and verify the transfer with collect_payment first", plan, plan)}));
    };
    // What was paid is what is activated, whatever the conversation asked for.
    if wanted != paid_months {
        tracing::info!(%order_id, wanted, paid_months, "activation months follow the paid order");
    }
    let months = paid_months;
    let r = ctx.state.registry.post("/v1/seller/plan", &json!({"phone": phone, "plan": plan, "months": months}), Duration::from_secs(20)).await
        .map_err(|e| anyhow!("gateway: {e}"))?;
    if r["ok"].as_bool() == Some(true) {
        // One payment, one activation.
        let _ = crate::account::set_setting(&ctx.state.db, &format!("plan_activated:{order_id}"), &chrono::Utc::now().to_rfc3339()).await;
        crate::contacts::set_plan(&ctx.state.db, ctx.business_id, &phone, &plan).await;
        Ok(json!({"status": "activated", "plan": plan, "months": months, "expiresAt": r["expiresAt"],
            "note": "tell the customer the plan is active on their Yaya account: their app picks it up within a minute (or when they reopen it)"}))
    } else {
        Ok(json!({"status": "failed", "gateway": r,
            "note": "if the gateway found no Yaya account for this number, ask the customer to sign in to agente (Cuenta) with this same number, then activate again"}))
    }
}

impl Plugin<Agente> for Seller {
    fn name(&self) -> &'static str {
        "seller"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta_when(Scope::Customer, true, "seller"),
            json!({"type": "function", "function": {
                "name": "lookup_customer",
                "description": "Who is this number for agente: a paying customer (pro/max), on trial, on the free tier, or a NEW LEAD with no account. Call it at the start of a conversation and before selling.",
                "parameters": {"type": "object", "properties": {
                    "phone": {"type": "string", "description": "phone with country code; defaults to the customer in this chat"}
                }}
            }}),
            tool_fn(|c, a| Box::pin(lookup_customer(c, a))),
        )?;
        k.tool(
            meta_when(Scope::Customer, true, "seller"),
            json!({"type": "function", "function": {
                "name": "activate_plan",
                "description": "Activate Plan Pro or Plan Max on the customer's Yaya account AFTER their order for it is paid and verified (create_order → collect_payment). Refuses without a paid order.",
                "parameters": {"type": "object", "properties": {
                    "phone": {"type": "string", "description": "customer phone with country code; defaults to this chat"},
                    "plan": {"type": "string", "enum": ["pro", "max"]},
                    "months": {"type": "integer", "minimum": 1, "maximum": 12, "description": "1, or 12 for a year"}
                }, "required": ["plan"]}
            }}),
            tool_fn(|c, a| Box::pin(activate_plan(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, tool_ctx, Mock};

    async fn paid_order(s: &crate::AppState, b: uuid::Uuid, phone: &str, product: &str, qty: i64) {
        sqlx::query("INSERT INTO orders (id, business_id, customer_name, phone, items, total, status, paid) VALUES ($1,$2,'Ana',$3,$4,100,'confirmed',1)")
            .bind(uuid::Uuid::new_v4()).bind(b).bind(phone).bind(json!([{"product": product, "qty": qty, "unitPrice": 100}]).to_string())
            .execute(&s.db).await.unwrap();
    }

    async fn setup() -> (Mock, crate::SharedState, uuid::Uuid) {
        let m = Mock::start().await;
        m.on("/v1/seller/plan", json!({"ok": true, "expiresAt": "2026-10-21"}));
        m.on("/v1/seller/lookup", json!({"found": true, "plan": "trial"}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        (m, s, b)
    }

    #[tokio::test]
    async fn a_paid_plan_order_activates_that_plan_once_for_what_was_paid() {
        let (m, s, b) = setup().await;
        paid_order(&s, b, "com.whatsapp:+51 977 000 111", "Plan Pro", 1).await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 977 000 111")).await;
        let r = activate_plan(&c, &json!({"plan": "pro", "months": 12})).await.unwrap();
        assert_eq!(r["status"], "activated", "{r}");
        assert_eq!(r["months"], 1, "one month paid is one month activated");
        assert_eq!(m.seen_path("/v1/seller/plan")[0].body["months"], 1);
        let again = activate_plan(&c, &json!({"plan": "pro"})).await.unwrap();
        assert_eq!(again["status"], "rejected", "the same payment does not activate twice: {again}");
        assert_eq!(m.seen_path("/v1/seller/plan").len(), 1);
    }

    #[tokio::test]
    async fn a_year_is_twelve_months_paid() {
        let (m, s, b) = setup().await;
        paid_order(&s, b, "51977000111", "Plan Max", 12).await;
        let c = tool_ctx(&s, b, Some("51977000111")).await;
        assert_eq!(activate_plan(&c, &json!({"plan": "max", "months": 12})).await.unwrap()["months"], 12);
        assert_eq!(m.seen_path("/v1/seller/plan")[0].body["plan"], "max");
    }

    #[tokio::test]
    async fn other_products_never_activate_a_plan() {
        let (m, s, b) = setup().await;
        paid_order(&s, b, "51977000111", "Polo profesional", 1).await;
        paid_order(&s, b, "51977000111", "Maxi gorra", 1).await;
        let c = tool_ctx(&s, b, Some("51977000111")).await;
        assert_eq!(activate_plan(&c, &json!({"plan": "pro"})).await.unwrap()["status"], "rejected");
        assert_eq!(activate_plan(&c, &json!({"plan": "max"})).await.unwrap()["status"], "rejected");
        assert!(m.seen_path("/v1/seller/plan").is_empty());
        // Paying for Pro does not buy Max, and another number cannot ride on this order.
        paid_order(&s, b, "51977000111", "Plan Pro", 1).await;
        assert_eq!(activate_plan(&c, &json!({"plan": "max"})).await.unwrap()["status"], "rejected");
        assert_eq!(activate_plan(&c, &json!({"plan": "pro", "phone": "+51 988 000 222"})).await.unwrap()["status"], "rejected");
        assert_eq!(activate_plan(&c, &json!({"plan": "gold"})).await.unwrap()["status"], "rejected");
    }

    #[tokio::test]
    async fn lookup_labels_and_remembers_the_plan() {
        let (m, s, b) = setup().await;
        crate::contacts::touch(&s.db, b, "com.whatsapp:+51 977 000 111").await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 977 000 111")).await;
        let r = lookup_customer(&c, &json!({})).await.unwrap();
        assert!(r["note"].as_str().unwrap().contains("free trial"));
        assert!(m.seen_path("/v1/seller/lookup")[0].path.ends_with("phone=51977000111"));
        assert_eq!(crate::contacts::by_peer(&s.db, b, "com.whatsapp:+51 977 000 111").await.unwrap()["plan"], "trial");
        m.set("/v1/seller/lookup", json!({"found": false}));
        assert!(lookup_customer(&c, &json!({})).await.unwrap()["note"].as_str().unwrap().contains("NEW LEAD"));
        assert!(lookup_customer(&c, &json!({"phone": "123"})).await.is_err());
        let off = testkit::state().await;
        let b2 = testkit::business(&off.db).await;
        assert!(lookup_customer(&tool_ctx(&off, b2, Some("51977000111")).await, &json!({})).await.unwrap_err().to_string().contains("gateway"));
    }
}
