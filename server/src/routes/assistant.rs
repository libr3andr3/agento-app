//! The consumer app's personal assistant (client mode).

use super::*;

// -------------------------------------------------------- personal assistant
//
// The consumer app. No device token: the phone owner IS the tenant, the
// loopback app key is the whole authorization, and the "business" row is
// their profile (created at boot in client mode, lazily otherwise so the
// host binary can be poked from curl).

pub(super) async fn self_id(state: &SharedState) -> Result<Uuid, (StatusCode, Json<Value>)> {
    crate::plugins::assistant::ensure_self(
        &state.db,
        &std::env::var("COUNTRY").unwrap_or_else(|_| "PE".into()),
        std::env::var("LANGUAGE").ok().as_deref(),
    )
    .await
    .map_err(internal)
}

#[derive(Deserialize)]
pub(super) struct AssistantReq {
    message: String,
}

pub(super) async fn assistant_message(
    State(state): State<SharedState>,
    Json(req): Json<AssistantReq>,
) -> ApiResult {
    let message = req.message.trim();
    if message.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty message"));
    }
    if message.chars().count() > 4000 {
        return Err(err(StatusCode::BAD_REQUEST, "message too long"));
    }
    let id = self_id(&state).await?;
    let history = load_history(&state, id, "assistant", "self").await?;
    let (user_msg_id, turn) = append_message(&state, id, "assistant", "self", "user", message).await?;
    let outcome = agents::run_assistant_agent(&state, id, message, &history, turn, user_msg_id)
        .await
        .map_err(agent_err)?;
    append_message(&state, id, "assistant", "self", "assistant", &outcome.reply).await?;
    let (action, action_data) = outcome
        .action
        .map(|(a, d)| (json!(a), d))
        .unwrap_or((Value::Null, Value::Null));
    Ok(Json(json!({
        "agentResponse": outcome.reply,
        "action": action,
        "actionData": action_data,
    })))
}

/// The stored conversation, oldest first, for the chat screen to render on
/// open. `limit` bounds the tail (default 200).
pub(super) async fn assistant_history(
    State(state): State<SharedState>,
    Query(q): Query<HistoryQ>,
) -> ApiResult {
    let id = self_id(&state).await?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT role, content, created_at FROM ( \
            SELECT role, content, created_at, idx FROM messages \
            WHERE business_id = $1 AND agent_type = 'assistant' AND peer = 'self' \
            ORDER BY idx DESC LIMIT $2 \
         ) t ORDER BY idx",
    )
    .bind(id)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    let profile: (String,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    let profile: Value = serde_json::from_str(&profile.0).unwrap_or(Value::Null);
    Ok(Json(json!({
        "messages": rows.into_iter().map(|(role, content, at)| json!({
            "role": role, "content": content, "at": at,
        })).collect::<Vec<_>>(),
        "profile": profile["profile"],
    })))
}

#[derive(Deserialize)]
pub(super) struct HistoryQ {
    limit: Option<i64>,
}

#[derive(Deserialize, Default)]
pub(super) struct ResetReq {
    /// Also wipe what the assistant remembered about the person.
    #[serde(default)]
    forget_profile: bool,
}

/// "Borrar conversación": the transcript goes; the profile stays unless asked.
pub(super) async fn assistant_reset(
    State(state): State<SharedState>,
    body: Option<Json<ResetReq>>,
) -> ApiResult {
    let req = body.map(|b| b.0).unwrap_or_default();
    let id = self_id(&state).await?;
    sqlx::query("DELETE FROM messages WHERE business_id = $1 AND agent_type = 'assistant'")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    sqlx::query("DELETE FROM tool_events WHERE business_id = $1 AND agent_type = 'assistant'")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    if req.forget_profile {
        let (raw,): (String,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .map_err(internal)?;
        let mut patch: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
        patch.as_object_mut().map(|m| m.remove("profile"));
        sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
            .bind(patch.to_string())
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }
    Ok(Json(json!({"status": "ok"})))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn the_personal_assistant_chat() {
        let m = Mock::start().await;
        let s = testkit::state_with(testkit::Opts { client_mode: true, upstream: Some(m.base.clone()) }).await;
        assert_eq!(api(&s, "POST", "/api/assistant/message", None, Some(json!({"message": "  "}))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/assistant/message", None, Some(json!({"message": "x".repeat(4001)}))).await.0, 400);
        m.call_tool("remember", json!({"key": "city", "value": "Lima"}));
        m.say("Anotado, vives en Lima.");
        let (st, v) = api(&s, "POST", "/api/assistant/message", None, Some(json!({"message": "vivo en Lima"}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!((v["agentResponse"].clone(), v["action"].clone()), (json!("Anotado, vives en Lima."), json!("remember")));
        let (_, h) = api(&s, "GET", "/api/assistant/history?limit=1", None, None).await;
        assert_eq!(h["messages"].as_array().unwrap().len(), 1);
        assert_eq!(h["profile"], json!({"city": "Lima"}));
        api(&s, "POST", "/api/assistant/reset", None, None).await;
        let (_, h) = api(&s, "GET", "/api/assistant/history", None, None).await;
        assert_eq!((h["messages"].clone(), h["profile"].clone()), (json!([]), json!({"city": "Lima"})), "reset keeps the profile by default");
        api(&s, "POST", "/api/assistant/reset", None, Some(json!({"forget_profile": true}))).await;
        assert!(api(&s, "GET", "/api/assistant/history", None, None).await.1["profile"].is_null());
    }
}
