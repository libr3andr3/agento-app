//! What the owner sees: agenda, earnings, gaps, appointments.

use super::*;

// --------------------------------------------------------------- dashboard

pub(super) async fn dashboard(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    crate::backup::backup_if_due(state.clone());
    // Keep the network card fresh (throttled inside; never blocks).
    let bg = state.clone();
    tokio::spawn(async move { crate::network::publish(&bg, false).await; });
    Ok(Json(dashboard_json(&state, business_id).await?))
}

/// The dashboard payload: what the app's home screen and the web console
/// both render. Every date bucket is a business-LOCAL day.
pub(crate) async fn dashboard_json(state: &SharedState, business_id: Uuid) -> Result<Value, (StatusCode, Json<Value>)> {
    // Compose settles expired trials on the way in (decide_pending) and gives
    // us the business's timezone: every date bucket below is a LOCAL day.
    let composed = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map_err(internal)?;
    let tz = crate::harness::biz_tz(&composed.values);
    let tz_name = tz.name();
    let biz: (String, bool) =
        sqlx::query_as("SELECT name, onboarded FROM businesses WHERE id = $1")
            .bind(business_id)
            .fetch_one(&state.db)
            .await
            .map_err(internal)?;
    let usage = plan_usage(state, business_id, tz, None).await?;

    // Local day / ISO week / month as UTC ranges, computed here (SQLite has
    // no time zones; the business's own clock decides what "today" is).
    let [(d0, d1), (w0, w1), (m0, m1)] = crate::harness::local_ranges(tz);
    let earnings: (Option<f64>, Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT \
           SUM(CASE WHEN starts_at >= $2 AND starts_at < $3 THEN price END), \
           SUM(CASE WHEN starts_at >= $4 AND starts_at < $5 THEN price END), \
           SUM(CASE WHEN starts_at >= $6 AND starts_at < $7 THEN price END) \
         FROM appointments \
         WHERE business_id = $1 AND paid AND status <> 'cancelled'",
    )
    .bind(business_id)
    .bind(d0).bind(d1).bind(w0).bind(w1).bind(m0).bind(m1)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    // Product money counts the day it was paid for (created), not scheduled —
    // orders have no future slot to hang earnings on.
    let order_earnings: (Option<f64>, Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT \
           SUM(CASE WHEN created_at >= $2 AND created_at < $3 THEN total END), \
           SUM(CASE WHEN created_at >= $4 AND created_at < $5 THEN total END), \
           SUM(CASE WHEN created_at >= $6 AND created_at < $7 THEN total END) \
         FROM orders \
         WHERE business_id = $1 AND paid AND status <> 'cancelled'",
    )
    .bind(business_id)
    .bind(d0).bind(d1).bind(w0).bind(w1).bind(m0).bind(m1)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    let recent_orders: Vec<(Uuid, String, Value, Option<f64>, String, bool, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            "SELECT id, customer_name, items, total, status, paid, created_at \
             FROM orders \
             WHERE business_id = $1 AND created_at > $2 \
               AND status <> 'cancelled' \
               AND (status <> 'done' OR done_at >= $3) \
             ORDER BY created_at DESC LIMIT 50",
        )
        .bind(business_id)
        .bind(crate::db::ago(chrono::Duration::days(14)))
        .bind(d0)
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

    let rows: Vec<(Uuid, String, String, Option<String>, chrono::DateTime<chrono::Utc>, String, bool, Option<f64>, Option<String>, Option<i32>, Option<String>, Option<i32>)> =
        sqlx::query_as(
            "SELECT id, customer_name, phone, specialist, starts_at, status, paid, price, \
                    customer_email, remind_minutes, service, duration_mins \
             FROM appointments \
             WHERE business_id = $1 AND status <> 'cancelled' \
               AND starts_at >= $2 AND starts_at < $3 \
             ORDER BY starts_at LIMIT 200",
        )
        .bind(business_id)
        .bind(d0)
        .bind(d0 + chrono::Duration::days(14))
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

    // Open gaps = questions customers asked that the schema couldn't answer
    // and the owner hasn't resolved yet. Raw wording comes straight off the
    // anchored message row (the gap event itself stores only shapes); rows
    // from before the anchor existed fall back to the redacted utterance.
    let gaps: Vec<(String, String, Option<String>, String, String, chrono::DateTime<chrono::Utc>, Option<String>)> =
        sqlx::query_as(
            "SELECT g.id, g.kind, g.field_path, g.utterance_redacted, g.session, g.ts, m.content \
             FROM gap_events g \
             LEFT JOIN candidates c ON c.origin_gap = g.id \
             LEFT JOIN messages m ON m.id = g.message_id \
             WHERE g.business_id = $1 AND c.id IS NULL \
               AND g.ts > $2 \
             ORDER BY g.ts DESC LIMIT 10",
        )
        .bind(business_id)
        .bind(crate::db::ago(chrono::Duration::days(14)))
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    let open_gaps: Vec<Value> = gaps
        .into_iter()
        .map(|(id, kind, field_path, redacted, session, ts, raw)| {
            json!({
                "id": id,
                "kind": kind,
                "fieldPath": field_path,
                "question": raw.unwrap_or(redacted),
                "customer": session.split('@').next().unwrap_or(""),
                "ts": ts.to_rfc3339(),
            })
        })
        .collect();

    let locale = crate::locale::Locale::from_values(&composed.values);
    // D15: the UI spec and what its blocks need beyond the queue — hours and
    // slot length for the week grid, the catalog and its photos.
    let media = crate::media::list(&state.db, business_id).await.unwrap_or_default();
    let v = &composed.values;
    Ok(json!({
        "businessName": biz.0,
        "onboarded": biz.1,
        "ui": composed.doc["_ui"],
        "uiDesigned": composed.doc["_uiDesigned"],
        "businessKind": v["businessKind"],
        "businessHours": v["businessHours"],
        "slotDuration": v["slotDuration"],
        "products": v["products"],
        "pricing": v["pricing"],
        "media": media,
        "locale": {
            "country": locale.country, "language": locale.language,
            "currency": locale.currency, "currencySymbol": locale.symbol,
            "timezone": tz_name
        },
        "plan": {
            "name": usage.plan,
            "messagesUsed": usage.msgs_used, "messagesCap": usage.msgs_cap,
            "customersUsed": usage.customers_used, "customersCap": usage.customers_cap,
            "conversationsUsed": usage.conv_used, "conversationsCap": usage.conv_cap,
            // A full month of conversations (or the legacy daily messages
            // cap) stops NEW customers; existing chats this month go on.
            "limitReached": (usage.msgs_cap > 0 && usage.msgs_used >= usage.msgs_cap) || (usage.conv_cap > 0 && usage.conv_used >= usage.conv_cap),
            "customersFull": usage.customers_cap > 0 && usage.customers_used >= usage.customers_cap,
            "salesPhone": sales_phone(state).await,
        },
        "credits": crate::outcomes::dashboard_block(state).await,
        "openGaps": open_gaps,
        "earnings": {
            "today": earnings.0.unwrap_or(0.0) + order_earnings.0.unwrap_or(0.0),
            "week": earnings.1.unwrap_or(0.0) + order_earnings.1.unwrap_or(0.0),
            "month": earnings.2.unwrap_or(0.0) + order_earnings.2.unwrap_or(0.0)
        },
        "orders": recent_orders.into_iter().map(|o| json!({
            "id": o.0, "customer": o.1, "items": o.2, "total": o.3,
            "status": o.4, "paid": o.5,
            "date": crate::harness::fmt_local(o.6, tz, "%Y-%m-%d"),
            "time": crate::harness::fmt_local(o.6, tz, "%H:%M")
        })).collect::<Vec<_>>(),
        "appointments": rows.into_iter().map(|r| json!({
            "id": r.0, "customer": r.1, "phone": r.2, "specialist": r.3,
            "date": crate::harness::fmt_local(r.4, tz, "%Y-%m-%d"),
            "time": crate::harness::fmt_local(r.4, tz, "%H:%M"),
            "status": r.5, "paid": r.6, "price": r.7,
            "reminderEmail": r.8, "remindMinutes": r.9,
            "service": r.10, "durationMins": r.11
        })).collect::<Vec<_>>()
    }))
}

// ----------------------------------------------------------- learning loop

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AnswerGapReq {
    gap_id: String,
    answer: String,
}

/// The owner answers a customer question the agent couldn't. Each extracted
/// fact mounts immediately as a candidate on trial — usable in the very next
/// conversation, unwound cleanly if it misbehaves.
pub(super) async fn answer_gap(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<AnswerGapReq>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    if req.answer.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty answer"));
    }
    let created =
        crate::learning::answer_gap(&state.db, &state.llm, business_id, &req.gap_id, &req.answer)
            .await
            .map_err(internal)?;
    Ok(Json(json!({
        "status": "mounted",
        "candidates": created,
        "note": "tu agente ya lo sabe — a prueba los próximos días"
    })))
}


// ------------------------------------------------------------------- extras

pub(super) async fn list_appointments(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let composed = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map_err(internal)?;
    let tz = crate::harness::biz_tz(&composed.values);
    let rows: Vec<(Uuid, String, String, Option<String>, chrono::DateTime<chrono::Utc>, String, bool)> =
        sqlx::query_as(
            "SELECT id, customer_name, phone, specialist, starts_at, status, paid \
             FROM appointments WHERE business_id = $1 AND starts_at > $2 \
             ORDER BY starts_at LIMIT 100",
        )
        .bind(business_id)
        .bind(crate::db::ago(chrono::Duration::days(1)))
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|r| json!({
            "id": r.0, "customer": r.1, "phone": r.2, "specialist": r.3,
            "startsAt": crate::harness::fmt_local(r.4, tz, "%Y-%m-%dT%H:%M"),
            "status": r.5, "paid": r.6
        }))
        .collect::<Vec<_>>())))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn the_owner_home_screen() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        testkit::appointment(&s.db, b, "Ana", "confirmed", true, Some(30.0), 1).await;
        testkit::appointment(&s.db, b, "Luz", "cancelled", true, Some(99.0), 1).await;
        testkit::appointment(&s.db, b, "Rosa", "pending_payment", false, Some(20.0), 1).await;
        testkit::order(&s.db, b, "Carlos", "confirmed", true, Some(15.0)).await;
        testkit::order(&s.db, b, "Old", "cancelled", true, Some(500.0)).await;
        let (st, v) = api(&s, "GET", "/api/dashboard", Some(&t), None).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["businessName"], "Barbería Tito");
        assert_eq!(v["onboarded"], false);
        assert_eq!(v["locale"]["currency"], "PEN");
        assert_eq!(v["plan"]["name"], "free");
        assert_eq!(v["plan"]["salesPhone"], "51999000111");
        assert_eq!(v["earnings"]["month"], 45.0, "paid, not cancelled: 30 + 15");
        let customers: Vec<&str> = v["appointments"].as_array().unwrap().iter().map(|a| a["customer"].as_str().unwrap()).collect();
        assert_eq!(customers.len(), 2);
        assert!(!customers.contains(&"Luz"));
        assert_eq!(v["orders"].as_array().unwrap().len(), 1);
        assert!(v["ui"]["tabs"].is_array());
        assert_eq!(v["openGaps"], json!([]));
        assert_eq!(api(&s, "GET", "/api/dashboard", None, None).await.0, 401);
    }

    #[tokio::test]
    async fn upcoming_appointments_list() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        testkit::appointment(&s.db, b, "Pasado", "confirmed", false, None, -48).await;
        testkit::appointment(&s.db, b, "Luego", "confirmed", false, None, 3).await;
        testkit::appointment(&s.db, b, "Antes", "confirmed", false, None, 1).await;
        let (_, v) = api(&s, "GET", "/api/appointments", Some(&t), None).await;
        let names: Vec<&str> = v.as_array().unwrap().iter().map(|a| a["customer"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Antes", "Luego"]);
    }

    #[tokio::test]
    async fn answering_a_gap_needs_words() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        assert_eq!(api(&s, "POST", "/api/answer_gap", Some(&t), Some(json!({"gapId": "g", "answer": "  "}))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/answer_gap", None, Some(json!({"gapId": "g", "answer": "x"}))).await.0, 401);
    }
}
