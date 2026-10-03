//! The CRM and the conversation log: who the business talks to, what was
//! said, and what the agent did about it.

use super::*;

#[derive(Deserialize)]
pub(super) struct ContactsQ {
    #[serde(default)]
    q: Option<String>,
}

pub(super) async fn contacts_list(State(state): State<SharedState>, headers: HeaderMap, Query(q): Query<ContactsQ>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(json!({"contacts": crate::contacts::list(&state.db, business_id, q.q.as_deref()).await.map_err(internal)?})))
}

pub(super) async fn contact_update(State(state): State<SharedState>, headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<Uuid>, Json(patch): Json<Value>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    match crate::contacts::update(&state.db, business_id, id, &patch).await.map_err(internal)? {
        Some(c) => Ok(Json(c)),
        None => Err(err(StatusCode::NOT_FOUND, "no such contact")),
    }
}

/// Human wording for what the agent did, from the tool it called.
fn action_label(tool: &str, lang: &str) -> String {
    let es = lang.starts_with("es");
    match tool {
        "check_availability" => if es { "Revisó la disponibilidad" } else { "Checked availability" },
        "book_appointment" => if es { "Reservó una cita" } else { "Booked an appointment" },
        "cancel_appointment" => if es { "Canceló una cita" } else { "Cancelled an appointment" },
        "collect_payment" => if es { "Verificó el pago" } else { "Checked for the payment" },
        "create_order" => if es { "Registró un pedido" } else { "Took an order" },
        "quote_delivery" => if es { "Cotizó el delivery" } else { "Quoted delivery" },
        "report_gap" => if es { "Te dejó una pregunta" } else { "Left you a question" },
        "schedule_reminder" => if es { "Programó un recordatorio" } else { "Scheduled a reminder" },
        "find_businesses" => if es { "Buscó negocios en la red" } else { "Searched the network" },
        "ask_business" => if es { "Habló con otro negocio" } else { "Talked to a business" },
        other => return other.replace('_', " "),
    }.to_string()
}

/// One short line from the call: the values a person would want to see.
fn action_summary(args: &Value, result: &Value) -> String {
    let mut bits: Vec<String> = Vec::new();
    for k in ["date", "time", "starts_at", "service", "customer_name", "amount", "items", "district", "question", "status", "reason"] {
        for v in [&args[k], &result[k]] {
            match v {
                Value::String(s) if !s.is_empty() => bits.push(s.chars().take(40).collect()),
                Value::Number(n) => bits.push(n.to_string()),
                Value::Array(a) if !a.is_empty() => bits.push(format!("{} item(s)", a.len())),
                _ => {}
            }
        }
    }
    bits.dedup();
    bits.into_iter().take(4).collect::<Vec<_>>().join(" · ")
}

/// `GET /api/conversations` — the live conversations, newest first.
pub(super) async fn conversations(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(conversations_json(&state, business_id).await?))
}

pub(crate) async fn conversations_json(state: &SharedState, business_id: Uuid) -> Result<Value, (StatusCode, Json<Value>)> {
    let rows: Vec<(String, String, i64, String, String)> = sqlx::query_as(
        "SELECT m.peer, MAX(m.created_at) AS last_at, COUNT(*) AS n, \
                (SELECT content FROM messages x WHERE x.business_id = m.business_id AND x.peer = m.peer AND x.agent_type = 'customer' ORDER BY created_at DESC LIMIT 1) AS last_text, \
                (SELECT role FROM messages x WHERE x.business_id = m.business_id AND x.peer = m.peer AND x.agent_type = 'customer' ORDER BY created_at DESC LIMIT 1) AS last_role \
         FROM messages m WHERE m.business_id = $1 AND m.agent_type = 'customer' GROUP BY m.peer ORDER BY last_at DESC LIMIT 100",
    ).bind(business_id).fetch_all(&state.db).await.map_err(internal)?;
    let mut out = Vec::new();
    for (peer, last_at, n, last_text, last_role) in rows {
        let contact = crate::contacts::by_peer(&state.db, business_id, &peer).await.unwrap_or(Value::Null);
        out.push(json!({
            "peer": peer, "lastAt": last_at, "messages": n, "lastText": last_text.chars().take(160).collect::<String>(), "lastRole": last_role,
            "contact": contact,
        }));
    }
    Ok(json!({"conversations": out}))
}

/// `GET /api/conversations/{peer}` — messages and agent actions in order.
pub(super) async fn conversation(State(state): State<SharedState>, headers: HeaderMap, axum::extract::Path(peer): axum::extract::Path<String>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(conversation_json(&state, business_id, &peer).await?))
}

pub(crate) async fn conversation_json(state: &SharedState, business_id: Uuid, peer: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let peer = peer.to_string();
    let values = crate::learning::compose(&state.db, &state.schemas_dir, business_id).await.map_err(internal)?.values;
    let lang = crate::locale::Locale::from_values(&values).language;
    let msgs: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT id, role, content, created_at FROM messages WHERE business_id = $1 AND peer = $2 AND agent_type = 'customer' ORDER BY created_at ASC LIMIT 400",
    ).bind(business_id).bind(&peer).fetch_all(&state.db).await.map_err(internal)?;
    let tools: Vec<(String, String, String, i32, String)> = sqlx::query_as(
        "SELECT tool, args, result, latency_ms, created_at FROM tool_events WHERE business_id = $1 AND peer = $2 ORDER BY created_at ASC LIMIT 400",
    ).bind(business_id).bind(&peer).fetch_all(&state.db).await.map_err(internal)?;
    let mut items: Vec<(String, Value)> = msgs.into_iter().map(|(id, role, content, at)| (at.clone(), json!({"kind": "message", "id": id, "role": role, "text": content, "at": at}))).collect();
    for (tool, args, result, latency, at) in tools {
        let a: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
        let r: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
        let ok = r["error"].is_null() && r["status"].as_str().map_or(true, |s| !s.contains("error") && !s.contains("fail"));
        items.push((at.clone(), json!({"kind": "action", "tool": tool, "label": action_label(&tool, &lang), "summary": action_summary(&a, &r), "ok": ok, "latencyMs": latency, "at": at})));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let contact = crate::contacts::by_peer(&state.db, business_id, &peer).await.unwrap_or(Value::Null);
    Ok(json!({"peer": peer, "contact": contact, "items": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, api, Mock};

    #[test]
    fn labels_and_summaries() {
        assert_eq!(action_label("book_appointment", "es"), "Reservó una cita");
        assert_eq!(action_label("book_appointment", "en"), "Booked an appointment");
        assert_eq!(action_label("design_ui", "es"), "design ui");
        let s = action_summary(&json!({"date": "2026-09-22", "time": "10:00", "service": "corte", "items": [1, 2]}), &json!({"status": "booked", "amount": 25}));
        assert_eq!(s, "2026-09-22 · 10:00 · corte · 25");
        assert_eq!(action_summary(&json!({}), &json!({})), "");
        assert_eq!(action_summary(&json!({"question": "q".repeat(100)}), &Value::Null).len(), 40);
    }

    #[tokio::test]
    async fn contacts_conversations_and_timeline() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"}));
        m.say("hola Ana");
        crate::routes::customer_turn(&s, b, "com.whatsapp:+51 977 000 111", "hola").await.unwrap();
        sqlx::query("INSERT INTO tool_events (id, business_id, agent_type, peer, session, tool, args, result, latency_ms) VALUES ($1,$2,'customer',$3,'s','check_availability','{\"date\":\"mañana\"}','{\"status\":\"error: closed\"}',12)")
            .bind(Uuid::new_v4()).bind(b).bind("com.whatsapp:+51 977 000 111").execute(&s.db).await.unwrap();
        let (_, v) = api(&s, "GET", "/api/conversations", Some(&t), None).await;
        let c = &v["conversations"][0];
        assert_eq!((c["peer"].clone(), c["messages"].clone(), c["lastRole"].clone()), (json!("com.whatsapp:+51 977 000 111"), json!(2), json!("assistant")));
        assert!(c["lastText"].as_str().unwrap().ends_with("hola Ana"));
        assert_eq!(c["contact"]["phone"], "51977000111");
        let (_, v) = api(&s, "GET", "/api/conversations/com.whatsapp:+51%20977%20000%20111", Some(&t), None).await;
        let kinds: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "message").count(), 2);
        let action = v["items"].as_array().unwrap().iter().find(|i| i["kind"] == "action").unwrap();
        assert_eq!((action["label"].clone(), action["ok"].clone()), (json!("Revisó la disponibilidad"), json!(false)));
        // Contacts: list, search, edit.
        let (_, v) = api(&s, "GET", "/api/contacts", Some(&t), None).await;
        assert_eq!(v["contacts"].as_array().unwrap().len(), 2, "owner + Ana");
        let id = v["contacts"][1]["id"].as_str().unwrap().to_string();
        let (st, v) = api(&s, "POST", &format!("/api/contacts/{id}"), Some(&t), Some(json!({"name": "Ana Rojas"}))).await;
        assert_eq!((st, v["name"].clone()), (200, json!("Ana Rojas")));
        assert_eq!(api(&s, "GET", "/api/contacts?q=rojas", Some(&t), None).await.1["contacts"].as_array().unwrap().len(), 1);
        assert_eq!(api(&s, "POST", &format!("/api/contacts/{}", Uuid::new_v4()), Some(&t), Some(json!({"name": "x"}))).await.0, 404);
        assert_eq!(api(&s, "GET", "/api/contacts", None, None).await.0, 401);
    }
}

#[derive(Deserialize)]
pub(super) struct AgroQ {
    #[serde(default)]
    role: Option<String>,
}

/// The agro bundle's directory: who registered over WhatsApp, by role.
pub(super) async fn agro_participants(State(state): State<SharedState>, headers: HeaderMap, Query(q): Query<AgroQ>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let role = match q.role.as_deref().filter(|r| !r.is_empty()) {
        None => None,
        Some(r) => Some(crate::agro::Role::from_key(r).ok_or_else(|| err(StatusCode::BAD_REQUEST, "role must be productor, comprador or transportista"))?),
    };
    Ok(Json(crate::agro::directory(&state.db, business_id, role).await.map_err(internal)?))
}
