//! Cobros: where this business's customers send money. Typed by the owner
//! (registration step 4, Settings → Cobros), stored in the client patch as
//! `payout`, quoted verbatim by the customer agent. Wallet names the
//! network has seen in the country are offered as suggestions — that is
//! the only "support" a wallet ever needs.

use super::*;

#[derive(Deserialize)]
pub(super) struct RailsQ {
    #[serde(default)]
    country: Option<String>,
}

/// `GET /api/rails?country=PE` — suggestions: what other owners in the
/// country typed, wallets whose notifications delivered money there, and
/// the locale's own hint. Names only; nothing is validated against them.
pub(super) async fn rails_catalog(State(state): State<SharedState>, Query(q): Query<RailsQ>) -> ApiResult {
    let country = q.country.as_deref().unwrap_or("PE").to_ascii_uppercase();
    let learned = state.registry.get(&format!("/v1/rails?country={country}"), std::time::Duration::from_secs(15)).await
        .ok().and_then(|v| v["rails"].as_array().cloned()).unwrap_or_default();
    let mut names: Vec<Value> = learned;
    let profile = crate::locale::profile(&country);
    for hint in profile.rails.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !names.iter().any(|n| n["name"].as_str().map_or(false, |x| x.eq_ignore_ascii_case(hint))) {
            names.push(json!({"name": hint, "source": "locale", "packages": []}));
        }
    }
    Ok(Json(json!({"country": country, "rails": names})))
}

async fn payout_of(state: &SharedState, business_id: Uuid) -> Result<Value, (StatusCode, Json<Value>)> {
    let (raw, country): (String, String) = sqlx::query_as("SELECT schema_config, country FROM businesses WHERE id = $1")
        .bind(business_id).fetch_one(&state.db).await.map_err(internal)?;
    let patch: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    let mut p = patch["payout"].clone();
    if !p.is_object() {
        p = json!({"holder": Value::Null, "cashOnly": false, "destinations": []});
    }
    p["country"] = json!(country);
    p["summary"] = json!(crate::payout::describe(&p));
    Ok(p)
}

pub(super) async fn payout_get(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    Ok(Json(payout_of(&state, business_id).await?))
}

/// `POST /api/payout` — replaces the payout block. 422 names the row that
/// is half-filled so the form can point at it.
pub(super) async fn payout_set(State(state): State<SharedState>, headers: HeaderMap, Json(input): Json<Value>) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let payout = crate::payout::normalise(&input)
        .map_err(|(i, m)| (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error": m, "index": i}))))?;
    let (raw, country): (String, String) = sqlx::query_as("SELECT schema_config, country FROM businesses WHERE id = $1").bind(business_id).fetch_one(&state.db).await.map_err(internal)?;
    let mut patch: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    if !patch.is_object() { patch = json!({}); }
    let cash_only = payout["cashOnly"].as_bool().unwrap_or(false);
    patch["payout"] = payout.clone();
    // A business that takes transfers pays by transfer unless the owner
    // said otherwise in the interview.
    if !cash_only && patch["paymentMethod"].is_null() {
        patch["paymentMethod"] = json!("transfer");
    }
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2").bind(patch.to_string()).bind(business_id).execute(&state.db).await.map_err(internal)?;
    tracing::info!(%business_id, cash_only, destinations = payout["destinations"].as_array().map_or(0, |d| d.len()), "payout set");
    // The network learns the wallet names people use here (best effort).
    let wallets = crate::payout::wallets(&payout);
    if !wallets.is_empty() {
        let bg = state.clone();
        tokio::spawn(async move {
            let _ = bg.registry.post("/v1/rails", &json!({"country": country, "names": wallets}), std::time::Duration::from_secs(15)).await;
            crate::network::publish_if_stale(&bg, std::time::Duration::from_secs(60)).await;
        });
    }
    Ok(Json(payout_of(&state, business_id).await?))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn payout_round_trip_and_validation() {
        let m = Mock::start().await;
        m.on("/v1/rails", json!({}));
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        let (st, v) = api(&s, "GET", "/api/payout", Some(&t), None).await;
        assert_eq!((st, v["destinations"].clone(), v["country"].clone(), v["summary"].clone()), (200, json!([]), json!("PE"), json!(null)));
        let (st, v) = api(&s, "POST", "/api/payout", Some(&t), Some(json!({"holder": "Tito", "destinations": [{"wallet": "Yape", "handle": "999 000 111"}, {"wallet": "Plin"}]}))).await;
        assert_eq!((st, v["index"].clone()), (422, json!(1)));
        let (st, v) = api(&s, "POST", "/api/payout", Some(&t), Some(json!({"holder": "Tito", "destinations": [{"wallet": "Yape", "handle": "999 000 111"}]}))).await;
        assert_eq!(st, 200);
        assert_eq!(v["summary"], "Yape 999 000 111 (a nombre de Tito)");
        let (cfg,): (serde_json::Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_eq!(cfg["paymentMethod"], "transfer");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(m.seen_path("/v1/rails")[0].body["names"], json!(["Yape"]), "the network learns the wallet name");
        // An owner who said cash keeps their word.
        api(&s, "POST", "/api/payout", Some(&t), Some(json!({"cashOnly": true}))).await;
        assert_eq!(api(&s, "GET", "/api/payout", Some(&t), None).await.1["summary"], "cash only");
        assert_eq!(api(&s, "GET", "/api/payout", None, None).await.0, 401);
    }

    #[tokio::test]
    async fn rails_merge_network_and_locale_without_duplicates() {
        let m = Mock::start().await;
        m.on("/v1/rails", json!({"rails": [{"name": "yape", "source": "network"}, {"name": "Tunki", "source": "network"}]}));
        let s = testkit::state_on(&m).await;
        let (st, v) = api(&s, "GET", "/api/rails?country=pe", None, None).await;
        assert_eq!(st, 200);
        assert_eq!(v["country"], "PE");
        let names: Vec<&str> = v["rails"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["yape", "Tunki", "Plin"]);
        // Offline: the locale's own hint.
        let off = testkit::state().await;
        let v = api(&off, "GET", "/api/rails?country=BR", None, None).await.1;
        assert_eq!(v["rails"][0]["name"], "Pix");
    }
}
