//! The owner reads and verifies their agent's audit chain.

use super::*;

#[derive(Deserialize)]
pub(super) struct AuditQ {
    before: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/audit?before=&limit=` — newest first.
pub(super) async fn audit_list(State(state): State<SharedState>, headers: HeaderMap, Query(q): Query<AuditQ>) -> ApiResult {
    auth(&state, &headers).await?;
    let entries = crate::audit::list(&state, q.before, q.limit.unwrap_or(100)).await.map_err(internal)?;
    Ok(Json(json!({"entries": entries, "agent": state.identity.id()})))
}

/// `GET /api/audit/verify` — walks the whole chain.
pub(super) async fn audit_verify(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    auth(&state, &headers).await?;
    Ok(Json(crate::audit::verify(&state).await.map_err(internal)?))
}

/// `POST /api/audit/anchor` — ask the gateway to countersign the head now.
pub(super) async fn audit_anchor(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    auth(&state, &headers).await?;
    crate::audit::anchor(&state).await.map(Json).map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn owner_reads_verifies_and_anchors_the_chain() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        assert_eq!(api(&s, "GET", "/api/audit", None, None).await.0, 401);
        m.say("ok");
        api(&s, "POST", "/api/onboarding_message", Some(&t), Some(json!({"message": "hola"}))).await;
        let (st, v) = api(&s, "GET", "/api/audit?limit=5", Some(&t), None).await;
        assert_eq!(st, 200);
        assert_eq!(v["agent"], json!(s.identity.id()));
        assert!(!v["entries"].as_array().unwrap().is_empty(), "the owner turn is on the record");
        let (_, v) = api(&s, "GET", "/api/audit/verify", Some(&t), None).await;
        assert_eq!(v["ok"], true);
        // Gateway down → 503 with the reason.
        assert_eq!(api(&s, "POST", "/api/audit/anchor", Some(&t), None).await.0, 503);
        m.on("/v1/audit/anchor", json!({"anchoredAt": "2026-09-21T00:00:00Z", "sig": "s", "key": "k"}));
        let (st, v) = api(&s, "POST", "/api/audit/anchor", Some(&t), None).await;
        assert_eq!((st, v["status"].as_str()), (200, Some("anchored")));
    }
}
