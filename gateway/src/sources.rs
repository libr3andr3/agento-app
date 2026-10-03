//! Organic wallet support. Phones report what their agent concluded about
//! the apps that notify them; owners' payout screens report the wallet
//! names they use. Out of that the network answers two questions for any
//! country: which packages bring money (priors a new phone forwards
//! eagerly) and what wallets are called there (suggestions in the form).

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{auth::require_linked, err, internal, ApiResult, Auth, Shared};

fn country_of(s: Option<&str>) -> String {
    s.unwrap_or("").trim().to_ascii_uppercase().chars().filter(|c| c.is_ascii_alphabetic()).take(2).collect()
}

#[derive(serde::Deserialize)]
pub struct VoteReq { package: String, #[serde(default)] label: String, #[serde(default)] wallet: Option<String>, class: String, #[serde(default)] country: Option<String> }

/// `POST /v1/sources` — one phone's verdict about one app (upserted).
pub async fn vote(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<VoteReq>) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    let country = country_of(req.country.as_deref());
    if country.len() != 2 || req.package.trim().is_empty() || !matches!(req.class.as_str(), "money" | "not_money") {
        return Err(err(StatusCode::BAD_REQUEST, "package, class (money|not_money) and country are required"));
    }
    let wallet = req.wallet.as_deref().map(str::trim).filter(|w| !w.is_empty()).map(|w| w.chars().take(40).collect::<String>());
    sqlx::query(
        "INSERT INTO source_votes (package, agent, country, label, wallet, class) VALUES ($1,$2,$3,$4,$5,$6) \
         ON CONFLICT (package, agent) DO UPDATE SET country = excluded.country, label = excluded.label, \
           wallet = COALESCE(excluded.wallet, source_votes.wallet), class = excluded.class, n = source_votes.n + 1, \
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(req.package.trim().chars().take(200).collect::<String>()).bind(&agent).bind(&country).bind(req.label.chars().take(80).collect::<String>()).bind(&wallet).bind(&req.class)
    .execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(serde::Deserialize)]
pub struct CountryQ { #[serde(default)] country: Option<String> }

/// `GET /v1/sources?country=PE` — packages that bring money in a country:
/// two independent phones agreed, or one saw it repeatedly.
pub async fn priors(State(app): State<Shared>, Query(q): Query<CountryQ>) -> ApiResult {
    let country = country_of(q.country.as_deref());
    let rows: Vec<(String, String, Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT package, MAX(label), MAX(wallet), COUNT(*) AS agents, SUM(n) AS events FROM source_votes \
         WHERE country = $1 AND class = 'money' GROUP BY package HAVING agents >= 2 OR events >= 4 \
         ORDER BY agents DESC, events DESC LIMIT 100",
    ).bind(&country).fetch_all(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"country": country, "sources": rows.into_iter().map(|(p, l, w, a, e)| json!({"package": p, "label": l, "wallet": w, "agents": a, "events": e})).collect::<Vec<_>>()})).into_response())
}

#[derive(serde::Deserialize)]
pub struct RailsReq { #[serde(default)] country: Option<String>, names: Vec<String> }

/// `POST /v1/rails` — the wallet names an owner typed on their payout screen.
pub async fn rails_vote(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<RailsReq>) -> ApiResult {
    require_linked(&app, &auth).await?;
    let country = country_of(req.country.as_deref());
    if country.len() != 2 {
        return Err(err(StatusCode::BAD_REQUEST, "country is required"));
    }
    for name in req.names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).take(5) {
        let name: String = name.chars().take(40).collect();
        sqlx::query(
            "INSERT INTO rails_seen (country, key, name) VALUES ($1, $2, $3) \
             ON CONFLICT (country, key) DO UPDATE SET n = rails_seen.n + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
        ).bind(&country).bind(name.to_lowercase()).bind(&name).execute(&app.db).await.map_err(internal)?;
    }
    Ok(Json(json!({"ok": true})).into_response())
}

/// `GET /v1/rails?country=PE` — wallet names people use there, most common
/// first, each with the packages the network has seen deliver its money.
pub async fn rails(State(app): State<Shared>, Query(q): Query<CountryQ>) -> ApiResult {
    let country = country_of(q.country.as_deref());
    let typed: Vec<(String, i64)> = sqlx::query_as("SELECT name, n FROM rails_seen WHERE country = $1 ORDER BY n DESC LIMIT 20")
        .bind(&country).fetch_all(&app.db).await.map_err(internal)?;
    let seen: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT wallet, package, SUM(n) FROM source_votes WHERE country = $1 AND class = 'money' AND wallet IS NOT NULL GROUP BY wallet, package",
    ).bind(&country).fetch_all(&app.db).await.map_err(internal)?;
    let mut out: Vec<Value> = Vec::new();
    let mut push = |name: &str, n: i64, source: &str| {
        let packages: Vec<&str> = seen.iter().filter(|(w, _, _)| w.eq_ignore_ascii_case(name)).map(|(_, p, _)| p.as_str()).collect();
        if !out.iter().any(|o| o["name"].as_str().map_or(false, |x| x.eq_ignore_ascii_case(name))) {
            out.push(json!({"name": name, "n": n, "source": source, "packages": packages}));
        }
    };
    for (name, n) in &typed { push(name, *n, "owners"); }
    let mut by_wallet: std::collections::BTreeMap<String, i64> = Default::default();
    for (w, _, n) in &seen { *by_wallet.entry(w.clone()).or_default() += n; }
    let mut walls: Vec<(String, i64)> = by_wallet.into_iter().collect();
    walls.sort_by(|a, b| b.1.cmp(&a.1));
    for (w, n) in walls { push(&w, n, "notifications"); }
    Ok(Json(json!({"country": country, "rails": out})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_agent, Keypair};

    async fn phones(app: &Shared, n: usize) -> Vec<Keypair> {
        let mut out = Vec::new();
        for i in 0..n {
            let kp = Keypair::generate();
            account_with_agent(app, &format!("a{i}"), &format!("5190000000{i}"), &kp).await;
            out.push(kp);
        }
        out
    }

    #[tokio::test]
    async fn money_sources_need_two_phones_or_repeated_sightings() {
        let app = testkit::app().await;
        let p = phones(&app, 3).await;
        let yape = json!({"package": "com.bcp.innovacxion.yapeapp", "label": "Yape", "wallet": "Yape", "class": "money", "country": "pe"});
        for bad in [json!({"package": "x", "class": "maybe", "country": "PE"}), json!({"package": " ", "class": "money", "country": "PE"}), json!({"package": "x", "class": "money", "country": "P"})] {
            assert_eq!(as_agent(&app, &p[0], "POST", "/v1/sources", Some(bad)).await.0, 400);
        }
        assert_eq!(as_agent(&app, &Keypair::generate(), "POST", "/v1/sources", Some(yape.clone())).await.0, 401);
        as_agent(&app, &p[0], "POST", "/v1/sources", Some(yape.clone())).await;
        assert_eq!(anon(&app, "GET", "/v1/sources?country=PE", None).await.1["sources"], json!([]), "one phone once is not a prior");
        as_agent(&app, &p[1], "POST", "/v1/sources", Some(yape.clone())).await;
        let (_, v) = anon(&app, "GET", "/v1/sources?country=pe", None).await;
        assert_eq!((v["sources"][0]["agents"].clone(), v["sources"][0]["wallet"].clone()), (json!(2), json!("Yape")));
        assert_eq!(anon(&app, "GET", "/v1/sources?country=CL", None).await.1["sources"], json!([]), "per country");
        let mut not = yape.clone();
        not["class"] = json!("not_money");
        not["package"] = json!("com.whatsapp");
        for _ in 0..5 { as_agent(&app, &p[2], "POST", "/v1/sources", Some(not.clone())).await; }
        assert_eq!(anon(&app, "GET", "/v1/sources?country=PE", None).await.1["sources"].as_array().unwrap().len(), 1, "not_money never becomes a prior");
    }

    #[tokio::test]
    async fn rails_merge_what_owners_typed_with_what_notifications_showed() {
        let app = testkit::app().await;
        let p = phones(&app, 2).await;
        assert_eq!(as_agent(&app, &p[0], "POST", "/v1/rails", Some(json!({"names": ["Yape"]}))).await.0, 400);
        as_agent(&app, &p[0], "POST", "/v1/rails", Some(json!({"country": "PE", "names": ["Yape", " ", "Plin", "a", "b", "c", "d"]}))).await;
        as_agent(&app, &p[1], "POST", "/v1/rails", Some(json!({"country": "PE", "names": ["yape"]}))).await;
        as_agent(&app, &p[0], "POST", "/v1/sources", Some(json!({"package": "com.bcp.yape", "wallet": "YAPE", "class": "money", "country": "PE"}))).await;
        as_agent(&app, &p[0], "POST", "/v1/sources", Some(json!({"package": "com.bbva.tunki", "wallet": "Tunki", "class": "money", "country": "PE"}))).await;
        let (_, v) = anon(&app, "GET", "/v1/rails?country=PE", None).await;
        let rails = v["rails"].as_array().unwrap();
        assert_eq!((rails[0]["name"].clone(), rails[0]["n"].clone(), rails[0]["packages"].clone()), (json!("Yape"), json!(2), json!(["com.bcp.yape"])), "{v}");
        let names: Vec<&str> = rails.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"Tunki") && !names.contains(&"YAPE"), "case-insensitive merge: {names:?}");
        assert_eq!(names.iter().filter(|n| ["a", "b", "c", "d"].contains(n)).count(), 3, "at most five non-blank names per vote");
        assert_eq!(country_of(Some(" pe1x")), "PE");
    }
}

