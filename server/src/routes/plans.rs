//! Plan screens (proxied to the gateway) and device location.

use super::*;

// ----------------------------------------------------------------- plans
//
// The app's plan screen reads; buying happens on the web with the Yaya
// account. Identity-authenticated at the gateway; here only the loopback
// app key matters (a device token, when present, must be valid).

pub(super) async fn plan_caller(state: &SharedState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    if bearer(headers).is_some() {
        auth(state, headers).await?;
    }
    Ok(())
}

pub(super) async fn plan_get(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    plan_caller(&state, &headers).await?;
    let fresh = crate::network::sync_plan(&state).await;
    let cached = state.plan_info.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut info = fresh.unwrap_or(cached);
    if info.is_null() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "yaya.tech unreachable"));
    }
    info["agent"] = json!(state.identity.id());
    // D14: what this phone counted this month, next to what the gateway allows.
    if bearer(&headers).is_some() {
        if let Ok(bid) = auth(&state, &headers).await {
            if let Ok(c) = crate::learning::compose(&state.db, &state.schemas_dir, bid).await {
                let tz = crate::harness::biz_tz(&c.values);
                if let Ok(u) = super::customer::plan_usage(&state, bid, tz, None).await {
                    info["conversationsUsed"] = json!(u.conv_used);
                    info["conversationsCap"] = json!(u.conv_cap);
                }
            }
        }
    }
    Ok(Json(info))
}

// ---------------------------------------------------------------- credits
//
// Prepaid credits (agente/docs/CREDITS.md): the gateway keeps the ledger,
// `outcomes.rs` reports charges and caches the summary.

pub(super) async fn credits_get(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    plan_caller(&state, &headers).await?;
    let mut v = crate::outcomes::summary(&state).await;
    if v.is_null() || !v.is_object() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "yaya.tech unreachable"));
    }
    v["agent"] = json!(state.identity.id());
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(super) struct TopupReq {
    #[serde(default)] tier: Option<String>,
    #[serde(default)] amount: Option<i64>,
    #[serde(default)] method: Option<String>,
}

/// `POST /api/topup/session` — a card checkout URL (Dodo) or, in Perú with
/// `method = yape`, the Yape/Plin reference. Money moves on the gateway.
pub(super) async fn topup_session(State(state): State<SharedState>, headers: HeaderMap, Json(req): Json<TopupReq>) -> ApiResult {
    plan_caller(&state, &headers).await?;
    let tier = req.tier.clone().or_else(|| match req.amount { Some(10) => Some("basic".into()), Some(25) => Some("plus".into()), Some(50) => Some("max".into()), _ => None })
        .unwrap_or_else(|| "plus".into());
    let method = req.method.as_deref().unwrap_or("card");
    crate::outcomes::topup(&state, &tier, req.amount, method).await.map(Json).map_err(account_err)
}

pub(super) async fn wallets_get(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(crate::outcomes::wallets(&state).await))
}

pub(super) async fn categories_get(State(state): State<SharedState>) -> ApiResult {
    let mut v = crate::outcomes::categories(&state).await;
    if v["prohibited"].as_array().map_or(true, |a| a.is_empty()) {
        v["prohibited"] = json!(crate::outcomes::DEFAULT_PROHIBITED.iter().map(|k| json!({"key": k})).collect::<Vec<_>>());
    }
    Ok(Json(v))
}

/// `POST /api/outcomes/{id}/reverse` — the owner cancelled/refunded a
/// charged outcome; the gateway returns the credit within 24 h.
pub(super) async fn outcome_reverse(State(state): State<SharedState>, headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>) -> ApiResult {
    auth(&state, &headers).await?;
    let id = Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "bad outcome id"))?;
    Ok(Json(crate::outcomes::reverse(&state, id).await))
}

// -------------------------------------------------------------- location
//
// Where this agent physically is. Business: goes public in the card's offer
// (label + coarse geo, 3 decimals ≈ 100 m) so "near me" works on the network.
// Client: stays in the private profile, sent with find_businesses only.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(super) struct LocationReq {
    #[serde(default)] label: Option<String>,
    #[serde(default)] city: Option<String>,
    #[serde(default)] district: Option<String>,
    #[serde(default)] region: Option<String>,
    #[serde(default)] country_code: Option<String>,
    #[serde(default)] lat: Option<f64>,
    #[serde(default)] lng: Option<f64>,
}

pub(super) async fn set_location(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<LocationReq>,
) -> ApiResult {
    let (id, is_business) = if bearer(&headers).is_some() {
        (auth(&state, &headers).await?, true)
    } else if state.client_mode {
        (self_id(&state).await?, false)
    } else {
        return Err(err(StatusCode::UNAUTHORIZED, "missing bearer token"));
    };
    if let (Some(la), Some(lo)) = (req.lat, req.lng) {
        if !(-90.0..=90.0).contains(&la) || !(-180.0..=180.0).contains(&lo) {
            return Err(err(StatusCode::BAD_REQUEST, "bad coordinates"));
        }
    }
    let clean = |o: &Option<String>| o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(80).collect::<String>());
    let label = clean(&req.label).or_else(|| match (clean(&req.district), clean(&req.city)) {
        (Some(d), Some(c)) if d != c => Some(format!("{d}, {c}")),
        (Some(d), None) => Some(d),
        (_, Some(c)) => Some(c),
        _ => None,
    });
    let (raw,): (String,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(id).fetch_one(&state.db).await.map_err(internal)?;
    let mut patch: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    if !patch.is_object() { patch = json!({}); }
    if let (Some(la), Some(lo)) = (req.lat, req.lng) {
        let r = |x: f64| (x * 1000.0).round() / 1000.0;
        patch["geo"] = json!({"lat": r(la), "lng": r(lo), "at": chrono::Utc::now().to_rfc3339()});
    }
    if let Some(l) = &label {
        // The owner's own wording wins over a reverse-geocode.
        if is_business {
            if patch["location"].as_str().map_or(true, |s| s.trim().is_empty()) {
                patch["location"] = json!(l);
            }
        } else {
            patch["location"] = json!(l);
            if !patch["profile"].is_object() { patch["profile"] = json!({}); }
            if let Some(c) = clean(&req.city) { patch["profile"]["city"] = json!(c); }
            if let Some(d) = clean(&req.district) { patch["profile"]["district"] = json!(d); }
        }
    }
    if let Some(r) = clean(&req.region) { patch["region"] = json!(r); }
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(patch.to_string()).bind(id).execute(&state.db).await.map_err(internal)?;
    if is_business {
        let bg = state.clone();
        tokio::spawn(async move { crate::network::publish_if_stale(&bg, std::time::Duration::from_secs(60)).await; });
    }
    Ok(Json(json!({"status": "ok", "location": patch["location"], "geo": patch["geo"]})))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn plan_and_credits_proxy_the_gateway() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        assert_eq!(api(&s, "GET", "/api/plan", None, None).await.0, 503, "never heard from the gateway");
        let (t, _) = testkit::onboard(&s, &m).await;
        m.on("/v1/me", json!({"plan": "pro", "caps": {}}));
        let (st, v) = api(&s, "GET", "/api/plan", Some(&t), None).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["agent"], json!(s.identity.id()));
        assert!(v["conversationsCap"].is_number());
        assert_eq!(api(&s, "GET", "/api/plan", Some("bogus"), None).await.0, 401, "a token, when sent, must be valid");
        m.on("/v1/credits", json!({"balance": 4}));
        assert_eq!(api(&s, "GET", "/api/credits", Some(&t), None).await.1["balance"], 4);
    }

    #[tokio::test]
    async fn topup_tier_from_amount() {
        let m = Mock::start().await;
        m.on("/v1/topup/session", json!({"url": "u"})).on("/v1/topup/yape", json!({"ref": "r"}));
        let s = testkit::state_on(&m).await;
        api(&s, "POST", "/api/topup/session", None, Some(json!({"amount": 25}))).await;
        api(&s, "POST", "/api/topup/session", None, Some(json!({"amount": 7}))).await;
        api(&s, "POST", "/api/topup/session", None, Some(json!({"tier": "max", "method": "yape"}))).await;
        let tiers: Vec<_> = m.seen().iter().map(|x| x.body["tier"].clone()).collect();
        assert_eq!(tiers, vec![json!("plus"), json!("plus"), json!("max")]);
        assert_eq!(m.seen_path("/v1/topup/yape").len(), 1);
    }

    #[tokio::test]
    async fn catalogs_and_reversal() {
        let s = testkit::state().await;
        let (_, v) = api(&s, "GET", "/api/categories", None, None).await;
        assert_eq!(v["prohibited"].as_array().unwrap().len(), crate::outcomes::DEFAULT_PROHIBITED.len());
        assert_eq!(api(&s, "GET", "/api/wallets", None, None).await.1, json!({}));
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        assert_eq!(api(&s, "POST", "/api/outcomes/nope/reverse", Some(&t), None).await.0, 400);
        assert_eq!(api(&s, "POST", &format!("/api/outcomes/{}/reverse", uuid::Uuid::new_v4()), Some(&t), None).await.1["reversed"], false);
    }

    #[tokio::test]
    async fn location_is_coarse_and_the_owners_words_win() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        assert_eq!(api(&s, "POST", "/api/location", None, Some(json!({"city": "Lima"}))).await.0, 401);
        assert_eq!(api(&s, "POST", "/api/location", Some(&t), Some(json!({"lat": 91.0, "lng": 0.0}))).await.0, 400);
        let (st, v) = api(&s, "POST", "/api/location", Some(&t), Some(json!({"district": "Miraflores", "city": "Lima", "lat": -12.1219876, "lng": -77.0298765, "region": "Lima"}))).await;
        assert_eq!(st, 200);
        assert_eq!(v["location"], "Miraflores, Lima");
        assert_eq!(v["geo"]["lat"], -12.122, "rounded to ~100 m");
        // A second reverse-geocode does not replace what is there.
        api(&s, "POST", "/api/location", Some(&t), Some(json!({"city": "Callao"}))).await;
        let (cfg,): (serde_json::Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_eq!((cfg["location"].clone(), cfg["region"].clone()), (json!("Miraflores, Lima"), json!("Lima")));
    }
}
