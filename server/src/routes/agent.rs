//! This installation on the network: card, publish, attestation, kernel.

use super::*;

pub(super) fn require_admin(state: &SharedState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let admin = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct_eq(admin, &state.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    Ok(())
}

/// This installation's identity and signed card — what the network sees.
pub(super) async fn agent_card(State(state): State<SharedState>) -> ApiResult {
    let env = crate::network::signed_card(&state).await;
    Ok(Json(json!({
        "agent": state.identity.id(),
        "did": state.identity.did(),
        "registry": crate::network::registry_url(),
        "card": env,
    })))
}

/// Force a (re)publish — the APK calls this when onboarding finishes so the
/// business shows up under its real name right away.
pub(super) async fn agent_publish(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    auth(&state, &headers).await?;
    let result = crate::network::publish(&state, true).await;
    Ok(Json(json!({"published": result.is_some(), "registry": result})))
}

#[derive(Deserialize)]
pub(super) struct DeviceReq {
    /// base64 DER certificates, leaf first, from the Android Keystore.
    chain: Vec<String>,
    #[serde(default)]
    level: Option<String>,
}

/// The Kotlin shell hands over the device key's attestation chain (minted
/// with this agent's challenge). Stored, embedded in the card, republished.
pub(super) async fn agent_device(
    State(state): State<SharedState>,
    Json(req): Json<DeviceReq>,
) -> ApiResult {
    // No business needs to exist yet; only this device's shell may say what
    // its hardware attests (the route is device-only, see `require_local`).
    if req.chain.len() < 2 || req.chain.len() > 8 || req.chain.iter().any(|c| c.len() > 16_000) {
        return Err(err(StatusCode::BAD_REQUEST, "chain must be 2–8 base64 certificates"));
    }
    sqlx::query(
        "INSERT INTO device_attestation (id, agent, chain, level) VALUES (1, $1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET agent = excluded.agent, chain = excluded.chain, \
           level = excluded.level, created_at = $4",
    )
    .bind(state.identity.id())
    .bind(json!(req.chain).to_string())
    .bind(&req.level)
    .bind(crate::db::now())
    .execute(&state.db)
    .await
    .map_err(internal)?;
    tracing::info!(certs = req.chain.len(), level = ?req.level, "device attestation stored");
    let result = crate::network::publish(&state, true).await;
    Ok(Json(json!({"stored": true, "registry": result})))
}

/// What's mounted right now: plugins, their tools/hooks/services, event map.
pub(super) async fn kernel_inspect(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    require_admin(&state, &headers)?;
    Ok(Json(state.kernel.inspect()))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock, ADMIN_KEY, APP_KEY};
    use serde_json::json;

    #[tokio::test]
    async fn card_publish_and_kernel() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        let (st, v) = api(&s, "GET", "/api/agent", None, None).await;
        assert_eq!(st, 200);
        assert_eq!(v["agent"], json!(s.identity.id()));
        assert!(v["did"].as_str().unwrap().starts_with("did:key:"));
        assert_eq!(yaya_wire::envelope::verify(&v["card"]).unwrap().to_string(), s.identity.id(), "the card is signed by this agent");
        assert_eq!(api(&s, "POST", "/api/agent/publish", None, None).await.0, 401);
        let (st, _) = api(&s, "POST", "/api/agent/publish", Some(&t), None).await;
        assert_eq!(st, 200);
        assert_eq!(api(&s, "GET", "/api/kernel", None, None).await.0, 401, "kernel needs the admin key");
        let (st, k) = testkit::call(&s, "GET", "/api/kernel", &[("x-app-key", APP_KEY), ("x-admin-key", ADMIN_KEY)], None).await;
        assert_eq!(st, 200);
        assert!(k.is_object());
    }

    #[tokio::test]
    async fn device_attestation_is_stored_only_from_this_device() {
        let s = testkit::state().await;
        let chain = json!({"chain": ["AAAA", "BBBB"], "level": "tee"});
        let remote: std::net::SocketAddr = ([198, 51, 100, 2], 1).into();
        assert_eq!(testkit::call_from(&s, remote, "POST", "/api/agent/device", &[("x-app-key", APP_KEY)], Some(chain.clone())).await.0, 403);
        assert_eq!(api(&s, "POST", "/api/agent/device", None, Some(json!({"chain": ["A"]}))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/agent/device", None, Some(json!({"chain": vec!["A"; 9]}))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/agent/device", None, Some(json!({"chain": ["A", "x".repeat(16_001)]}))).await.0, 400);
        let (st, v) = api(&s, "POST", "/api/agent/device", None, Some(chain)).await;
        assert_eq!((st, v["stored"].clone()), (200, json!(true)));
        let (agent, level): (String, Option<String>) = sqlx::query_as("SELECT agent, level FROM device_attestation").fetch_one(&s.db).await.unwrap();
        assert_eq!((agent, level.as_deref()), (s.identity.id(), Some("tee")));
    }
}
