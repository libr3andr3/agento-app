//! The owner's "Sitio web e integración" screen: their domain, the profile
//! link template, the webhook into their own systems (crate::ops).

use super::*;

/// The config as the owner's device sees it: secret included, plus a
/// preview of the link a participant would get.
fn view(c: &crate::ops::Config) -> Value {
    let mut v = c.to_json(true);
    v["example"] = json!(c.profile_url.as_deref().map(crate::ops::example));
    v
}

pub(super) async fn ops_get(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(view(&crate::ops::config(&state.db, business_id).await)))
}

pub(super) async fn ops_set(State(state): State<SharedState>, headers: HeaderMap, Json(patch): Json<Value>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    match crate::ops::configure(&state.db, business_id, &patch).await {
        Ok(c) => Ok(Json(view(&c))),
        Err(e) => Err(err(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string())),
    }
}

pub(super) async fn ops_rotate(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(json!({"secret": crate::ops::rotate_secret(&state.db, business_id).await.map_err(internal)?})))
}

pub(super) async fn ops_test(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    crate::ops::ping(&state, business_id).await.map(Json).map_err(|e| err(StatusCode::CONFLICT, &e.to_string()))
}

pub(super) async fn ops_events(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(json!({"events": crate::ops::recent(&state.db, business_id, 50).await})))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn the_owner_sets_up_their_site_from_the_app() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (token, _) = testkit::onboard(&s, &m).await;
        let t = Some(token.as_str());
        for (method, path) in [("GET", "/api/ops"), ("POST", "/api/ops"), ("POST", "/api/ops/secret"), ("POST", "/api/ops/test"), ("GET", "/api/ops/events")] {
            assert_eq!(api(&s, method, path, None, Some(json!({}))).await.0, 401, "{path} is the owner's");
        }
        let (st, v) = api(&s, "GET", "/api/ops", t, None).await;
        assert_eq!(st, 200);
        assert_eq!((v["domain"].is_null(), v["sendProfileLink"].as_bool()), (true, Some(true)));

        let (st, v) = api(&s, "POST", "/api/ops", t, Some(json!({"domain": "no es dominio"}))).await;
        assert_eq!(st, 422);
        assert!(v["error"].as_str().unwrap().contains("domain"), "{v}");

        let (st, v) = api(&s, "POST", "/api/ops", t, Some(json!({"domain": "example.com", "webhookUrl": format!("{}/hook", m.base)}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["profileUrl"], "https://example.com/perfil/{id}");
        let secret = v["secret"].as_str().unwrap().to_string();
        assert!(secret.starts_with("whsec_"), "the owner's device shows the secret to paste into their server");
        assert_eq!(v["example"], "https://example.com/perfil/p7k2m9x4qa", "a preview of what participants get");

        let (_, v) = api(&s, "POST", "/api/ops/secret", t, None).await;
        assert!(v["secret"].as_str().is_some_and(|x| x != secret));

        m.on("/hook", json!({"ok": true}));
        let (st, v) = api(&s, "POST", "/api/ops/test", t, None).await;
        assert_eq!((st, v["delivered"].as_bool()), (200, Some(true)), "{v}");
        let seen = m.seen_path("/hook").pop().unwrap();
        assert_eq!(seen.body["type"], "ping");

        let (_, v) = api(&s, "GET", "/api/ops/events", t, None).await;
        assert_eq!(v["events"][0]["type"], "ping");
        assert_eq!(v["events"][0]["status"], "delivered");

        let (st, v) = api(&s, "POST", "/api/ops/test", t, Some(json!({}))).await;
        assert_eq!(st, 200, "{v}");
        let (_, v) = api(&s, "POST", "/api/ops", t, Some(json!({"webhookUrl": ""}))).await;
        assert!(v["webhookUrl"].is_null());
        let (st, v) = api(&s, "POST", "/api/ops/test", t, None).await;
        assert_eq!(st, 409, "nothing to test: {v}");
    }
}
