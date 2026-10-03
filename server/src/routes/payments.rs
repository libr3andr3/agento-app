//! Money-in notifications: the phone forwards raw notifications, the agent
//! reads them, the ones that are payments settle the bill they belong to.

use super::*;

// ------------------------------------------------------- notification events

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NotificationReq {
    #[serde(flatten)]
    envelope: crate::sources::Envelope,
    /// Older shells sent `source` (label) beside the envelope.
    #[serde(default)]
    source: Option<String>,
}

/// One reading of one notification by the agent. JSON in, JSON out; the
/// model sees the whole envelope, not a sentence we composed from it.
async fn read_notification(state: &SharedState, business_id: Uuid, locale: &crate::locale::Locale, env: &crate::sources::Envelope) -> Option<crate::sources::Reading> {
    if crate::limits::charge(&state.db, business_id, crate::limits::Meter::Llm).await.is_err() {
        return None;
    }
    let prompt = format!(
        "You are the agent on a small business owner's phone. An Android notification arrived; \
         decide whether it announces MONEY RECEIVED by the phone's owner (a customer paid them), \
         from any wallet, bank or payment app in the world, in any language. The business's \
         currency is {cur}. Notification (raw): {env}\n\
         Answer ONLY a JSON object: {{\"money_in\": true|false, \"amount\": number|null, \
         \"currency\": ISO-4217|null, \"payer\": string|null, \"payer_phone\": digits|null, \
         \"wallet\": string|null, \"kind\": \"payment\"|\"promo\"|\"otp\"|\"other\"}}. \
         money_in is true ONLY for money that arrived to the owner (not sent, not a promo, not \
         a balance or a reminder). payer = the sender as printed, no boilerplate. wallet = the \
         brand a person would name (e.g. the app's name). Never invent; null when absent.",
        cur = locale.currency_phrase(),
        env = env.as_json(),
    );
    let resp = state.llm.chat(&[json!({"role": "user", "content": prompt})], None).await.ok()?;
    let content = resp["content"].as_str().or_else(|| resp["choices"][0]["message"]["content"].as_str())?;
    let v: Value = serde_json::from_str(&content[content.find('{')?..=content.rfind('}')?]).ok()?;
    Some(crate::sources::Reading {
        money_in: v["money_in"].as_bool().unwrap_or(false),
        amount: v["amount"].as_f64().filter(|a| *a > 0.0),
        currency: v["currency"].as_str().map(|c| c.to_ascii_uppercase()).filter(|c| c.len() == 3),
        payer: v["payer"].as_str().map(|p| p.trim().to_string()).filter(|p| p.len() >= 2),
        payer_phone: v["payer_phone"].as_str().map(|p| p.chars().filter(|c| c.is_ascii_digit()).collect::<String>()).filter(|p| p.len() >= 7),
        wallet: v["wallet"].as_str().map(|w| w.trim().to_string()).filter(|w| !w.is_empty()),
        kind: v["kind"].as_str().unwrap_or("other").to_string(),
    })
}

fn same_phone(a: &str, b: &str) -> bool {
    let d = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>();
    let (a, b) = (d(a), d(b));
    let n = a.len().min(b.len()).min(9);
    n >= 7 && a.ends_with(&b[b.len() - n..])
}

/// `POST /api/payment_event` — a raw notification from the phone. Muted apps
/// are answered without reading; the rest are read by the agent, remembered,
/// reported to the network, and — when money arrived — settled.
pub(super) async fn payment_event(State(state): State<SharedState>, headers: HeaderMap, Json(req): Json<NotificationReq>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let mut env = req.envelope;
    if env.app_label.is_empty() {
        env.app_label = req.source.unwrap_or_default();
    }
    if env.package.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "package is required"));
    }
    if let Some(until) = crate::sources::muted_until(&state.db, &env.package).await {
        return Ok(Json(json!({"handled": false, "mute": true, "untilMs": until.timestamp_millis()})));
    }
    // Android reposts notifications and HttpURLConnection may resend a POST:
    // the same notification is the same payment. Same app, same text, same
    // post time (or, from shells that send none, within ten minutes).
    if let Some(prior) = already_recorded(&state, business_id, &env).await.map_err(internal)? {
        tracing::info!(package = %env.package, payment = %prior.0, "payment notification repeated; not recorded again");
        return Ok(Json(json!({
            "handled": true, "mute": false, "duplicate": true, "paymentId": prior.0, "amount": prior.1, "payer": prior.2,
            "route": "duplicate", "matchedAppointment": Value::Null, "matchedOrder": Value::Null,
        })));
    }
    let values = crate::learning::compose(&state.db, &state.schemas_dir, business_id).await.map_err(internal)?.values;
    let locale = crate::locale::Locale::from_values(&values);
    let Some(reading) = read_notification(&state, business_id, &locale, &env).await else {
        // Could not read it (no LLM, allowance spent): say nothing, try again next time.
        return Ok(Json(json!({"handled": false, "mute": false})));
    };
    let mute = crate::sources::remember(&state.db, &env, &reading).await.map_err(internal)?;
    crate::sources::report(&state, &env, &reading, &locale.country).await;
    if !reading.money_in {
        tracing::info!(package = %env.package, kind = %reading.kind, "notification read: not money");
        return Ok(Json(json!({"handled": false, "mute": mute.is_some(), "untilMs": mute.map(|m| m.timestamp_millis()), "kind": reading.kind})));
    }

    // A payment can only settle a bill in the business's currency.
    let wrong_currency = matches!((&reading.currency, locale.currency.as_str()), (Some(c), biz) if !biz.is_empty() && c != biz);
    let settleable = !wrong_currency && reading.amount.is_some();
    let full = env.full_text();
    let pay: (Uuid,) = sqlx::query_as(
        "INSERT INTO payments (id, business_id, source, payer, amount, raw_text, currency, source_package, meta, payer_phone, parsed_by) \
         VALUES ($11,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id",
    )
    .bind(business_id).bind(reading.wallet.clone().unwrap_or_else(|| env.app_label.clone())).bind(&reading.payer).bind(reading.amount)
    .bind(&full).bind(&reading.currency).bind(&env.package).bind(env.as_json()).bind(&reading.payer_phone).bind("agent")
    .bind(Uuid::new_v4()).fetch_one(&state.db).await.map_err(internal)?;

    crate::contacts::learn(&state.db, business_id, None, reading.payer_phone.as_deref(), reading.payer.as_deref(), None).await;
    let mut matched: Option<(Uuid, String)> = None;
    let mut matched_order: Option<(Uuid, String)> = None;
    let mut route = "none";
    if settleable {
        let amount = reading.amount;
        let pendings: Vec<(Uuid, String, Option<f64>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT id, customer_name, price, service, phone FROM appointments WHERE business_id = $1 AND status = 'pending_payment' ORDER BY created_at DESC LIMIT 30",
        ).bind(business_id).fetch_all(&state.db).await.map_err(internal)?;
        let pending_orders: Vec<(Uuid, String, Option<f64>, Option<String>)> = sqlx::query_as(
            "SELECT id, customer_name, total, phone FROM orders WHERE business_id = $1 AND status = 'pending_payment' ORDER BY created_at DESC LIMIT 30",
        ).bind(business_id).fetch_all(&state.db).await.map_err(internal)?;
        let total_open = pendings.len() + pending_orders.len();
        let appt_bills: Vec<crate::settlement::Bill<Uuid>> = pendings.iter().map(|p| (p.0, p.1.clone(), crate::harness::owed_for(&values, p.3.as_deref(), p.2))).collect();
        let order_bills: Vec<crate::settlement::Bill<Uuid>> = pending_orders.iter().map(|o| (o.0, o.1.clone(), o.2.filter(|t| *t > 0.0))).collect();

        // 1. The payer's phone, when the wallet printed one: numbers have no spelling variants.
        let mut chosen_appt: Option<Uuid> = None;
        let mut chosen_order: Option<Uuid> = None;
        if let Some(ph) = reading.payer_phone.as_deref() {
            let covers = |owed: Option<f64>| matches!((amount, owed), (Some(a), Some(o)) if a >= o);
            chosen_appt = appt_bills.iter().zip(pendings.iter()).find(|(b, p)| p.4.as_deref().is_some_and(|x| same_phone(x, ph)) && covers(b.2)).map(|(b, _)| b.0);
            if chosen_appt.is_none() {
                chosen_order = order_bills.iter().zip(pending_orders.iter()).find(|(b, o)| o.3.as_deref().is_some_and(|x| same_phone(x, ph)) && covers(b.2)).map(|(b, _)| b.0);
            }
            if chosen_appt.is_some() || chosen_order.is_some() { route = "phone"; }
        }
        // 2. The payer's name (settlement.rs rules).
        if chosen_appt.is_none() && chosen_order.is_none() {
            chosen_appt = crate::settlement::pick_bill_for_payment(reading.payer.as_deref(), amount, &appt_bills, total_open);
            if chosen_appt.is_none() {
                chosen_order = crate::settlement::pick_bill_for_payment(reading.payer.as_deref(), amount, &order_bills, total_open);
            }
            if chosen_appt.is_some() || chosen_order.is_some() {
                route = if reading.payer.is_some() { "name" } else { "single_open_bill" };
            }
        }
        if let Some(appt_id) = chosen_appt {
            matched = sqlx::query_as("UPDATE appointments SET paid = TRUE, status = 'confirmed' WHERE id = $1 RETURNING id, customer_name")
                .bind(appt_id).fetch_optional(&state.db).await.map_err(internal)?;
            if let Some((id, name)) = &matched {
                sqlx::query("UPDATE payments SET appointment_id = $1 WHERE id = $2").bind(appt_id).bind(pay.0).execute(&state.db).await.map_err(internal)?;
                let phone = pendings.iter().find(|p| p.0 == appt_id).and_then(|p| p.4.clone()).unwrap_or_default();
                crate::outcomes::confirm(&state, business_id, "booking", *id, &phone, name).await;
            }
        } else if let Some(order_id) = chosen_order {
            matched_order = sqlx::query_as("UPDATE orders SET paid = TRUE, status = 'confirmed' WHERE id = $1 RETURNING id, customer_name")
                .bind(order_id).fetch_optional(&state.db).await.map_err(internal)?;
            if let Some((id, name)) = &matched_order {
                sqlx::query("UPDATE payments SET order_id = $1 WHERE id = $2").bind(order_id).bind(pay.0).execute(&state.db).await.map_err(internal)?;
                let phone = pending_orders.iter().find(|o| o.0 == order_id).and_then(|o| o.3.clone()).unwrap_or_default();
                crate::outcomes::confirm(&state, business_id, "sale", *id, &phone, name).await;
            }
        }
    } else {
        tracing::info!(%business_id, currency = ?reading.currency, biz = %locale.currency, package = %env.package, "payment recorded but not settleable");
    }
    tracing::info!(%business_id, package = %env.package, wallet = ?reading.wallet, amount = ?reading.amount, route, settled = matched.is_some() || matched_order.is_some(), "payment read");
    crate::backup::backup_if_due(state.clone());
    crate::audit::record(&state, Some(business_id), crate::audit::kind::PAYMENT, "system", &pay.0.to_string(), json!({
        "package": env.package, "wallet": reading.wallet, "amount": reading.amount, "currency": reading.currency,
        "route": route, "appointment": matched.as_ref().map(|m| m.0), "order": matched_order.as_ref().map(|m| m.0),
    })).await;
    Ok(Json(json!({
        "handled": true, "mute": false, "paymentId": pay.0, "wallet": reading.wallet, "amount": reading.amount,
        "currency": reading.currency, "payer": reading.payer, "route": route,
        "matchedAppointment": matched.map(|(id, c)| json!({"id": id, "customer": c})),
        "matchedOrder": matched_order.map(|(id, c)| json!({"id": id, "customer": c})),
    })))
}

/// The payment an identical notification already produced, if any.
async fn already_recorded(state: &SharedState, business_id: Uuid, env: &crate::sources::Envelope) -> Result<Option<(Uuid, Option<f64>, Option<String>)>, sqlx::Error> {
    let full = env.full_text();
    match env.post_time {
        Some(t) => sqlx::query_as(
            "SELECT id, amount, payer FROM payments WHERE business_id = $1 AND source_package = $2 AND raw_text = $3 \
             AND json_extract(meta, '$.postTime') = $4 LIMIT 1",
        ).bind(business_id).bind(&env.package).bind(&full).bind(t).fetch_optional(&state.db).await,
        None => sqlx::query_as(
            "SELECT id, amount, payer FROM payments WHERE business_id = $1 AND source_package = $2 AND raw_text = $3 \
             AND json_extract(meta, '$.postTime') IS NULL AND received_at > $4 LIMIT 1",
        ).bind(business_id).bind(&env.package).bind(&full).bind(crate::db::ago(chrono::Duration::minutes(10))).fetch_optional(&state.db).await,
    }
}

/// `GET /api/payment_sources` — the network's priors for this country plus
/// what this phone learned itself. The phone forwards priors eagerly.
pub(super) async fn payment_sources(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let (country,): (String,) = sqlx::query_as("SELECT country FROM businesses WHERE id = $1").bind(business_id).fetch_one(&state.db).await.map_err(internal)?;
    let priors = crate::sources::priors(&state, &country).await;
    Ok(Json(json!({"country": country, "sources": priors, "local": crate::sources::local(&state.db).await})))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::{json, Value};
    use uuid::Uuid;

    fn yape(post_time: i64, text: &str) -> Value {
        json!({"package": "com.bcp.innovacxion.yapeapp", "appLabel": "Yape", "title": "Yape", "text": text, "postTime": post_time})
    }

    fn reading(amount: f64, payer: &str, phone: Option<&str>) -> String {
        json!({"money_in": true, "amount": amount, "currency": "PEN", "payer": payer, "payer_phone": phone, "wallet": "Yape", "kind": "payment"}).to_string()
    }

    async fn appointment(s: &crate::AppState, b: Uuid, name: &str, phone: &str, price: f64) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO appointments (id, business_id, customer_name, phone, starts_at, status, price) VALUES ($1,$2,$3,$4,$5,'pending_payment',$6)")
            .bind(id).bind(b).bind(name).bind(phone).bind(crate::db::hence(chrono::Duration::days(1))).bind(price).execute(&s.db).await.unwrap();
        id
    }

    async fn order(s: &crate::AppState, b: Uuid, name: &str, total: f64) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO orders (id, business_id, customer_name, phone, items, total, status) VALUES ($1,$2,$3,'+51 988 000 000','[]',$4,'pending_payment')")
            .bind(id).bind(b).bind(name).bind(total).execute(&s.db).await.unwrap();
        id
    }

    async fn status_of(s: &crate::AppState, table: &str, id: Uuid) -> (String, bool) {
        sqlx::query_as(&format!("SELECT status, paid FROM {table} WHERE id = $1")).bind(id).fetch_one(&s.db).await.unwrap()
    }

    async fn setup() -> (Mock, crate::SharedState, String, Uuid) {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        m.on("/v1/outcomes/confirm", json!({"charged": 1}));
        m.on("/v1/sources", json!({}));
        (m, s, t, b)
    }

    #[tokio::test]
    async fn a_named_payment_settles_the_matching_booking() {
        let (m, s, t, b) = setup().await;
        let ana = appointment(&s, b, "Ana Rojas", "+51 977 111 222", 50.0).await;
        let rosa = appointment(&s, b, "Rosa Quispe", "+51 977 333 444", 50.0).await;
        m.say(&reading(50.0, "Ana Rojas", None));
        let (st, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(yape(1, "Ana Rojas te yapeó S/ 50"))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!((v["route"].clone(), v["matchedAppointment"]["customer"].clone()), (json!("name"), json!("Ana Rojas")));
        assert_eq!(status_of(&s, "appointments", ana).await, ("confirmed".into(), true));
        assert_eq!(status_of(&s, "appointments", rosa).await, ("pending_payment".into(), false));
        // Charged once as an outcome.
        assert_eq!(m.seen_path("/v1/outcomes/confirm").len(), 1);
        let audited: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE kind = 'payment'").fetch_one(&s.db).await.unwrap();
        assert_eq!(audited, 1);
    }

    #[tokio::test]
    async fn a_reposted_notification_is_one_payment_not_two() {
        // Android reposts notifications and HttpURLConnection may resend a
        // POST: the same Yape must never settle a second booking.
        let (m, s, t, b) = setup().await;
        let first = appointment(&s, b, "Ana Rojas", "+51 977 111 222", 50.0).await;
        let second = appointment(&s, b, "Ana Rojas", "+51 977 111 222", 50.0).await;
        m.say(&reading(50.0, "Ana Rojas", Some("977111222")));
        let n1 = yape(1_726_000_000_000, "Ana Rojas te yapeó S/ 50");
        let (_, v1) = api(&s, "POST", "/api/payment_event", Some(&t), Some(n1.clone())).await;
        let (st, v2) = api(&s, "POST", "/api/payment_event", Some(&t), Some(n1)).await;
        assert_eq!(st, 200);
        assert_eq!(v2["paymentId"], v1["paymentId"], "the repost is the same payment");
        let payments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&s.db).await.unwrap();
        assert_eq!(payments, 1);
        let settled = [status_of(&s, "appointments", first).await.1, status_of(&s, "appointments", second).await.1];
        assert_eq!(settled.iter().filter(|p| **p).count(), 1, "one S/ 50 pays one booking");
        // A genuinely new payment (new postTime) still counts.
        let (_, v3) = api(&s, "POST", "/api/payment_event", Some(&t), Some(yape(1_726_000_060_000, "Ana Rojas te yapeó S/ 50"))).await;
        assert_ne!(v3["paymentId"], v1["paymentId"]);
    }

    #[tokio::test]
    async fn the_payers_phone_beats_the_name() {
        let (m, s, t, b) = setup().await;
        let ana = appointment(&s, b, "Ana", "+51 977 111 222", 40.0).await;
        m.say(&reading(40.0, "A. R.", Some("51977111222")));
        let (_, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(yape(2, "x"))).await;
        assert_eq!(v["route"], "phone");
        assert_eq!(status_of(&s, "appointments", ana).await.1, true);
    }

    #[tokio::test]
    async fn orders_settle_when_no_booking_matches() {
        let (m, s, t, b) = setup().await;
        let o = order(&s, b, "Carlos Mendoza", 30.0).await;
        m.say(&reading(35.0, "Carlos Mendoza", None));
        let (_, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(yape(3, "x"))).await;
        assert_eq!(v["matchedOrder"]["customer"], "Carlos Mendoza");
        assert_eq!(status_of(&s, "orders", o).await, ("confirmed".into(), true));
        let (oid,): (Option<Uuid>,) = sqlx::query_as("SELECT order_id FROM payments").fetch_one(&s.db).await.unwrap();
        assert_eq!(oid, Some(o));
    }

    #[tokio::test]
    async fn short_foreign_or_amountless_payments_never_settle() {
        let (m, s, t, b) = setup().await;
        let ana = appointment(&s, b, "Ana Rojas", "1", 50.0).await;
        for (i, r) in [
            reading(20.0, "Ana Rojas", None),
            json!({"money_in": true, "amount": 50, "currency": "USD", "payer": "Ana Rojas"}).to_string(),
            json!({"money_in": true, "amount": null, "payer": "Ana Rojas"}).to_string(),
        ].into_iter().enumerate() {
            m.say(&r);
            let (_, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(yape(10 + i as i64, &format!("n{i}")))).await;
            assert_eq!(v["handled"], true);
            assert_eq!(v["route"], "none", "{r}");
        }
        assert_eq!(status_of(&s, "appointments", ana).await.1, false);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 3, "recorded, just not settled");
    }

    #[tokio::test]
    async fn promos_mute_the_app_and_unreadable_ones_wait() {
        let (m, s, t, _) = setup().await;
        m.say(&json!({"money_in": false, "kind": "promo"}).to_string());
        let promo = json!({"package": "com.shop.promo", "title": "50% off", "text": "hoy"});
        let (_, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(promo.clone())).await;
        assert_eq!((v["handled"].clone(), v["mute"].clone(), v["kind"].clone()), (json!(false), json!(true), json!("promo")));
        assert!(v["untilMs"].as_i64().unwrap() > chrono::Utc::now().timestamp_millis());
        // Muted: answered without asking the model.
        let before = m.seen_path("/chat/completions").len();
        let (_, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(promo)).await;
        assert_eq!(v["mute"], true);
        assert_eq!(m.seen_path("/chat/completions").len(), before);
        // The model answers prose: nothing is recorded, nothing muted.
        m.say("no sé");
        let (_, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(json!({"package": "com.x", "text": "?"}))).await;
        assert_eq!(v, json!({"handled": false, "mute": false}));
    }

    #[tokio::test]
    async fn validation_and_auth() {
        let (_, s, t, _) = setup().await;
        assert_eq!(api(&s, "POST", "/api/payment_event", None, Some(yape(1, "x"))).await.0, 401);
        let (st, v) = api(&s, "POST", "/api/payment_event", Some(&t), Some(json!({"text": "x"}))).await;
        assert_eq!((st, v["error"].as_str()), (400, Some("package is required")));
    }

    #[tokio::test]
    async fn without_a_post_time_repeats_within_ten_minutes_are_one() {
        let (m, s, t, _) = setup().await;
        m.say(&reading(10.0, "Luz", None));
        let n = json!({"package": "com.bcp.innovacxion.yapeapp", "source": "Yape", "text": "Luz te yapeó S/ 10"});
        let (_, a) = api(&s, "POST", "/api/payment_event", Some(&t), Some(n.clone())).await;
        let (_, b) = api(&s, "POST", "/api/payment_event", Some(&t), Some(n)).await;
        assert_eq!((b["duplicate"].clone(), b["paymentId"].clone()), (json!(true), a["paymentId"].clone()));
        // `source` filled in the label on the stored row.
        let (src,): (String,) = sqlx::query_as("SELECT source FROM payments").fetch_one(&s.db).await.unwrap();
        assert_eq!(src, "Yape");
    }

    #[tokio::test]
    async fn payment_sources_merge_network_priors_and_local() {
        let (m, s, t, _) = setup().await;
        m.set("/v1/sources", json!({"sources": [{"package": "com.bcp.innovacxion.yapeapp"}]}));
        let (st, v) = api(&s, "GET", "/api/payment_sources", Some(&t), None).await;
        assert_eq!(st, 200);
        assert_eq!(v["country"], "PE");
        assert_eq!(v["sources"].as_array().unwrap().len(), 1);
        assert!(v["local"].is_array());
    }

    #[test]
    fn same_phone_ignores_formatting_and_country_code() {
        assert!(super::same_phone("+51 977-111-222", "977111222"));
        assert!(!super::same_phone("977111222", "977111223"));
        assert!(!super::same_phone("123456", "123456"));
    }
}
