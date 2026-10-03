//! Payment verification tool (any bank/wallet notification the phone forwards). Bundle-gated.

use anyhow::anyhow;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Payments;

/// Between looks for a payment the customer says they just made.
const RETRY_WAIT: std::time::Duration = if cfg!(test) { std::time::Duration::from_millis(5) } else { std::time::Duration::from_secs(3) };

/// What the payment being verified is for. Orders joined later and the tool
/// was appointments-only for a while: for a product business it answered
/// "no pending appointment", which the model read as "no order registered"
/// and it created a duplicate — one customer, two identical pending orders.
enum Bill {
    Appointment(Uuid),
    Order(Uuid),
}

/// Verifies payment against Yape/Plin notifications forwarded by the phone
/// (payments table). Confirms the appointment or product order only if a
/// matching, unlinked payment arrived in the last 2 hours.
/// D14: what a paid order hands over on the spot. `values.digital` maps a
/// product name to its delivery (a link, a file, access instructions); any
/// ordered item found there is delivered by the agent's next reply.
async fn digital_deliveries(ctx: &ToolCtx<'_>, order_id: Uuid) -> Vec<Value> {
    let Some(map) = ctx.values["digital"].as_object().filter(|m| !m.is_empty()) else { return vec![] };
    let items: Option<(String,)> = sqlx::query_as("SELECT items FROM orders WHERE id = $1").bind(order_id).fetch_optional(&ctx.state.db).await.ok().flatten();
    let items: Value = items.and_then(|(s,)| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for it in items.as_array().into_iter().flatten() {
        let Some(name) = it["product"].as_str() else { continue };
        let key = name.trim().to_lowercase();
        if let Some((k, v)) = map.iter().find(|(k, _)| k.trim().to_lowercase() == key || key.contains(&k.trim().to_lowercase())) {
            out.push(json!({"product": k, "deliver": v}));
        }
    }
    out
}

async fn collect_payment(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    // Identity from the conversation, canonicalized — the model retyping the
    // number differently must never make us lose the appointment (seen in prod).
    let peer = ctx
        .peer
        .as_deref()
        .or_else(|| args["phone"].as_str())
        .ok_or_else(|| anyhow!("missing 'phone'"))?;
    let me = crate::harness::canon_phone(peer);

    let appts: Vec<(Uuid, Option<f64>, String, String, bool, String, Option<String>)> =
        sqlx::query_as(
            "SELECT id, price, phone, status, paid, customer_name, service \
             FROM appointments \
             WHERE business_id = $1 AND status <> 'cancelled' \
             ORDER BY created_at DESC LIMIT 30",
        )
        .bind(ctx.business_id)
        .fetch_all(&ctx.state.db)
        .await?;
    let orders: Vec<(Uuid, Option<f64>, String, String, bool, String)> = sqlx::query_as(
        "SELECT id, total, phone, status, paid, customer_name \
         FROM orders \
         WHERE business_id = $1 AND status <> 'cancelled' \
         ORDER BY created_at DESC LIMIT 30",
    )
    .bind(ctx.business_id)
    .fetch_all(&ctx.state.db)
    .await?;

    // The nameless-payment rule counts every open bill the business has,
    // whichever table it lives in.
    let business_pending = appts.iter().filter(|r| r.3 == "pending_payment").count()
        + orders.iter().filter(|r| r.3 == "pending_payment").count();

    let my_appts: Vec<_> = appts
        .into_iter()
        .filter(|r| crate::harness::canon_phone(&r.2) == me)
        .collect();
    let my_orders: Vec<_> = orders
        .into_iter()
        .filter(|r| crate::harness::canon_phone(&r.2) == me)
        .collect();

    // This customer's newest open bill: appointment first (deposits hold a
    // slot someone else could take), else product order.
    // A booking made without a deposit is `confirmed` but unpaid — when the
    // customer says "ya te yapeé" that IS the bill to verify; answering
    // "nothing pending" sent the model into re-book / re-collect loops (2026-09-04).
    let open = |status: &str, paid: bool| status == "pending_payment" || (status == "confirmed" && !paid);
    let pending_appt = my_appts.iter().find(|r| r.3 == "pending_payment")
        .or_else(|| my_appts.iter().find(|r| open(&r.3, r.4)));
    let pending_order = my_orders.iter().find(|r| r.3 == "pending_payment")
        .or_else(|| my_orders.iter().find(|r| open(&r.3, r.4)));

    let (bill, expected, customer_name) = if let Some(a) = pending_appt {
        // What must have arrived is derived from the business's own
        // configuration and the stored appointment — never from the tool
        // arguments. The customer is on the other end of this conversation,
        // so any number they can steer the model into passing is a number
        // they chose to owe.
        let Some(expected) =
            crate::harness::owed_for(&ctx.values, a.6.as_deref(), a.1)
        else {
            tracing::warn!(
                business = %ctx.business_id, appointment = %a.0, service = ?a.6,
                "no price or deposit configured — cannot verify payment"
            );
            return Ok(json!({
                "status": "amount_unknown",
                "note": "this service has no price or deposit configured, so the payment \
                         cannot be verified — tell the customer the owner will confirm"
            }));
        };
        (Bill::Appointment(a.0), expected, a.5.clone())
    } else if let Some(o) = pending_order {
        let Some(total) = o.1.filter(|t| *t > 0.0) else {
            return Ok(json!({
                "status": "amount_unknown",
                "note": "this order has no total recorded — the owner must confirm it"
            }));
        };
        (Bill::Order(o.0), total, o.5.clone())
    } else {
        // The payment notification may have arrived first and auto-confirmed.
        let already_paid = my_appts.iter().any(|r| r.3 == "confirmed" && r.4)
            || my_orders.iter().any(|r| r.3 == "confirmed" && r.4);
        return Ok(if already_paid {
            let mut deliver = Vec::new();
            for o in my_orders.iter().filter(|r| r.3 == "confirmed" && r.4).take(3) {
                deliver.extend(digital_deliveries(ctx, o.0).await);
            }
            json!({"status": "paid", "note": "payment already received and matched", "deliver": deliver})
        } else {
            json!({"status": "nothing_pending",
                   "note": "this customer has no pending appointment or order — if they \
                            just agreed to one, REGISTER it first (book_appointment / \
                            create_order), then verify the payment"})
        });
    };

    // Customers say "ya pagué" seconds before the bank notification reaches
    // us (the phone forwards it on its own thread, banks add delay). Wait a
    // little rather than telling a paying customer their money didn't arrive.
    let mut payment: Option<(Uuid, Option<f64>, Option<String>)> = None;
    let mut named_mismatch = false;
    for attempt in 0..5 {
        // A payment whose amount we couldn't parse is not evidence of anything,
        // and `expected` is never NULL now, so neither side can wildcard.
        let candidates: Vec<(Uuid, Option<f64>, Option<String>)> = sqlx::query_as(
            "SELECT id, amount, payer FROM payments \
             WHERE business_id = $1 AND appointment_id IS NULL AND order_id IS NULL \
               AND received_at > $3 \
               AND amount IS NOT NULL AND amount >= $2 \
             ORDER BY received_at DESC LIMIT 20",
        )
        .bind(ctx.business_id)
        .bind(expected)
        .bind(crate::db::ago(chrono::Duration::hours(2)))
        .fetch_all(&ctx.state.db)
        .await?;

        // One rule set for the whole codebase — see settlement.rs.
        let pick = crate::settlement::pick_payment_for_bill(
            &candidates,
            &customer_name,
            business_pending,
        );
        named_mismatch = pick.named_mismatch;
        payment = pick
            .payment
            .and_then(|id| candidates.iter().find(|c| c.0 == id).cloned());
        if payment.is_some() {
            break;
        }
        // The payment may have raced us and auto-confirmed the bill.
        let already: Option<(bool,)> = match &bill {
            Bill::Appointment(id) => {
                sqlx::query_as("SELECT paid FROM appointments WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&ctx.state.db)
                    .await?
            }
            Bill::Order(id) => sqlx::query_as("SELECT paid FROM orders WHERE id = $1")
                .bind(id)
                .fetch_optional(&ctx.state.db)
                .await?,
        };
        if already.map(|r| r.0) == Some(true) {
            match &bill {
                Bill::Appointment(id) => crate::outcomes::confirm(ctx.state, ctx.business_id, "booking", *id, &me, &customer_name).await,
                Bill::Order(id) => crate::outcomes::confirm(ctx.state, ctx.business_id, "sale", *id, &me, &customer_name).await,
            }
            let deliver = match &bill { Bill::Order(id) => digital_deliveries(ctx, *id).await, _ => vec![] };
            return Ok(json!({"status": "paid",
                             "note": "payment already received and matched", "deliver": deliver}));
        }
        if attempt < 4 {
            tokio::time::sleep(RETRY_WAIT).await;
        }
    }
    if payment.is_none() && named_mismatch {
        return Ok(json!({
            "status": "no_payment_received",
            "note": "a transfer arrived but under a DIFFERENT name — ask the customer to \
                     confirm the name on the account they paid from"
        }));
    }

    match payment {
        Some((pay_id, pay_amount, payer)) => {
            // Claim the payment first, and only while it is still unlinked:
            // the webhook may have given it to another bill meanwhile, and
            // one transfer must never settle two.
            let (col, table, kind, id) = match &bill {
                Bill::Appointment(id) => ("appointment_id", "appointments", "booking", *id),
                Bill::Order(id) => ("order_id", "orders", "sale", *id),
            };
            let claimed = sqlx::query(&format!(
                "UPDATE payments SET {col} = $1 WHERE id = $2 AND appointment_id IS NULL AND order_id IS NULL"
            ))
            .bind(id)
            .bind(pay_id)
            .execute(&ctx.state.db)
            .await?
            .rows_affected();
            if claimed == 0 {
                return Ok(json!({
                    "status": "no_payment_received",
                    "note": "no matching payment notification yet — tell the customer \
                             it hasn't arrived and to check the number/amount"
                }));
            }
            sqlx::query(&format!("UPDATE {table} SET paid = TRUE, status = 'confirmed' WHERE id = $1"))
                .bind(id)
                .execute(&ctx.state.db)
                .await?;
            // A bill the agent verified is an outcome like one the webhook settled.
            crate::outcomes::confirm(ctx.state, ctx.business_id, kind, id, &me, &customer_name).await;
            let deliver = match &bill { Bill::Order(id) => digital_deliveries(ctx, *id).await, _ => vec![] };
            Ok(json!({"status": "paid", "amount": pay_amount, "payer": payer,
                      "verified_via": "bank/wallet notification", "deliver": deliver}))
        }
        None => Ok(json!({
            "status": "no_payment_received",
            "note": "no matching payment notification yet — tell the customer \
                     it hasn't arrived and to check the number/amount"
        })),
    }
}

impl Plugin<Agente> for Payments {
    fn name(&self) -> &'static str {
        "payments"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, false),
            // No `amount` parameter on purpose: the sum owed is read from the
            // appointment/order and the business's deposit/pricing config.
            // Letting the model pass one let a customer set their own price.
            json!({"type": "function", "function": {
                "name": "collect_payment",
                "description": "Check whether the customer's transfer (local instant-payment apps) for their pending appointment OR product order has actually arrived, and confirm it if so. The amount owed is determined by the business's own pricing — you cannot set it.",
                "parameters": {
                    "type": "object",
                    "properties": {"phone": {"type": "string"}},
                    "required": ["phone"]
                }
            }}),
            tool_fn(|c, a| Box::pin(collect_payment(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, set_values, tool_ctx};

    async fn pay(s: &crate::AppState, b: Uuid, payer: &str, amount: f64) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO payments (id, business_id, source, payer, amount, raw_text) VALUES ($1,$2,'Yape',$3,$4,'x')")
            .bind(id).bind(b).bind(payer).bind(amount).execute(&s.db).await.unwrap();
        id
    }

    async fn appt(s: &crate::AppState, b: Uuid, name: &str, phone: &str, status: &str, price: Option<f64>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO appointments (id, business_id, customer_name, phone, starts_at, status, price, service) VALUES ($1,$2,$3,$4,$5,$6,$7,'corte')")
            .bind(id).bind(b).bind(name).bind(phone).bind(crate::db::hence(chrono::Duration::days(1))).bind(status).bind(price).execute(&s.db).await.unwrap();
        id
    }

    async fn outcomes(s: &crate::AppState) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM outcomes").fetch_one(&s.db).await.unwrap()
    }

    #[tokio::test]
    async fn a_verified_payment_confirms_and_is_billed() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let a = appt(&s, b, "Ana Rojas", "com.whatsapp:+51 1", "pending_payment", Some(25.0)).await;
        pay(&s, b, "Ana Rojas", 25.0).await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 1")).await;
        let r = collect_payment(&c, &json!({})).await.unwrap();
        assert_eq!((r["status"].clone(), r["amount"].clone()), (json!("paid"), json!(25.0)));
        let (st, paid): (String, bool) = sqlx::query_as("SELECT status, paid FROM appointments WHERE id = $1").bind(a).fetch_one(&s.db).await.unwrap();
        assert_eq!((st.as_str(), paid), ("confirmed", true));
        assert_eq!(outcomes(&s).await, 1, "a booking confirmed by the agent is an outcome like any other");
        // Asking again: already paid, no second charge.
        assert_eq!(collect_payment(&c, &json!({})).await.unwrap()["status"], "paid");
        assert_eq!(outcomes(&s).await, 1);
    }

    #[tokio::test]
    async fn a_payment_already_linked_elsewhere_is_not_linked_again() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let other = appt(&s, b, "Ana Rojas", "com.whatsapp:+51 9", "confirmed", Some(25.0)).await;
        let a = appt(&s, b, "Ana Rojas", "com.whatsapp:+51 1", "pending_payment", Some(25.0)).await;
        let p = pay(&s, b, "Ana Rojas", 25.0).await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 1")).await;
        // The webhook links it to the other booking between our look and our write.
        let s2 = s.clone();
        let linker = tokio::spawn(async move {
            sqlx::query("UPDATE payments SET appointment_id = $1 WHERE id = $2").bind(other).bind(p).execute(&s2.db).await.unwrap();
        });
        linker.await.unwrap();
        let r = collect_payment(&c, &json!({})).await.unwrap();
        assert_eq!(r["status"], "no_payment_received");
        let (linked_to,): (Option<Uuid>,) = sqlx::query_as("SELECT appointment_id FROM payments WHERE id = $1").bind(p).fetch_one(&s.db).await.unwrap();
        assert_eq!(linked_to, Some(other));
        let (paid,): (bool,) = sqlx::query_as("SELECT paid FROM appointments WHERE id = $1").bind(a).fetch_one(&s.db).await.unwrap();
        assert!(!paid);
    }

    #[tokio::test]
    async fn paths_without_a_verifiable_bill() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let c = tool_ctx(&s, b, Some("p1")).await;
        assert_eq!(collect_payment(&c, &json!({})).await.unwrap()["status"], "nothing_pending");
        appt(&s, b, "Ana", "p1", "pending_payment", None).await;
        assert_eq!(collect_payment(&c, &json!({})).await.unwrap()["status"], "amount_unknown");
        let owner = tool_ctx(&s, b, None).await;
        assert!(collect_payment(&owner, &json!({})).await.is_err());
        // An order with no total cannot be verified either.
        let b2 = testkit::business(&s.db).await;
        testkit::order(&s.db, b2, "Carlos", "pending_payment", false, None).await;
        sqlx::query("UPDATE orders SET phone = 'p2' WHERE business_id = $1").bind(b2).execute(&s.db).await.unwrap();
        assert_eq!(collect_payment(&tool_ctx(&s, b2, Some("p2")).await, &json!({})).await.unwrap()["status"], "amount_unknown");
    }

    #[tokio::test]
    async fn wrong_name_and_digital_delivery() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        set_values(&s.db, b, json!({"digital": {"Curso Excel": "https://drive/curso"}})).await;
        let o = testkit::order(&s.db, b, "Ana Rojas", "pending_payment", false, Some(30.0)).await;
        sqlx::query("UPDATE orders SET phone = 'p1', items = '[{\"product\":\"curso excel\",\"qty\":1}]' WHERE id = $1").bind(o).execute(&s.db).await.unwrap();
        pay(&s, b, "Carlos Mendoza", 30.0).await;
        let c = tool_ctx(&s, b, Some("p1")).await;
        let r = collect_payment(&c, &json!({})).await.unwrap();
        assert_eq!(r["status"], "no_payment_received");
        assert!(r["note"].as_str().unwrap().contains("DIFFERENT name"));
        pay(&s, b, "Ana Rojas", 30.0).await;
        let r = collect_payment(&c, &json!({})).await.unwrap();
        assert_eq!(r["status"], "paid");
        assert_eq!(r["deliver"], json!([{"product": "Curso Excel", "deliver": "https://drive/curso"}]));
        assert_eq!(outcomes(&s).await, 1);
    }
}
