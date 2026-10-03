//! The marketplace layer of the registry: reputation in both directions and
//! a ranked match for "I need X near Y". Everything here is derived from
//! what agents signed themselves (cards, reviews) plus what the relay can
//! observe without reading mail (who talked to whom).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};
use yaya_wire::AgentId;

use crate::{agent_row, auth::require_linked, device_of, err, internal, verify_envelope, ApiResult, App, Auth, Shared};

// ---------------------------------------------------------- interactions

/// Records one relay hop between two agents (called from inbox_post).
pub async fn note_interaction(app: &App, x: &str, y: &str) {
    let (a, b) = if x <= y { (x, y) } else { (y, x) };
    let _ = sqlx::query(
        "INSERT INTO interactions (a, b, n) VALUES ($1, $2, 1) \
         ON CONFLICT (a, b) DO UPDATE SET n = n + 1, last_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(a)
    .bind(b)
    .execute(&app.db)
    .await;
}

async fn interacted(app: &App, x: &str, y: &str) -> Result<bool, (StatusCode, Json<Value>)> {
    let (a, b) = if x <= y { (x, y) } else { (y, x) };
    let row: Option<(i64,)> = sqlx::query_as("SELECT n FROM interactions WHERE a = $1 AND b = $2")
        .bind(a)
        .bind(b)
        .fetch_optional(&app.db)
        .await
        .map_err(internal)?;
    Ok(row.is_some_and(|r| r.0 > 0))
}

// --------------------------------------------------------------- reviews

const ALLOWED_TAGS: &[&str] = &[
    // about a business
    "on_time", "late", "as_described", "overcharged", "friendly", "rude", "would_return",
    // about a customer
    "no_show", "paid_fast", "paid_late", "cancelled_late", "polite", "clear",
];

/// `POST /v1/agents/{id}/reviews` — body is a signed envelope whose payload is
/// `{stars, comment?, tags?}`. Accepted only from an agent that has exchanged
/// relay messages with the reviewee; one standing review per pair.
pub async fn post_review(
    State(app): State<Shared>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
    Json(env): Json<Value>,
) -> ApiResult {
    let (bearer, _) = require_linked(&app, &auth).await?;
    let signer = verify_envelope(&bearer, &env)?;
    let reviewee = agent_row(&app, &id).await.map(|r| r.agent).unwrap_or_else(|_| id.clone());
    if !AgentId::looks_valid(&reviewee) {
        return Err(err(StatusCode::BAD_REQUEST, "bad reviewee id"));
    }
    if reviewee == signer {
        return Err(err(StatusCode::BAD_REQUEST, "an agent cannot review itself"));
    }
    let p = &env["payload"];
    let stars = p["stars"].as_i64().unwrap_or(0);
    if !(1..=5).contains(&stars) {
        return Err(err(StatusCode::BAD_REQUEST, "stars must be 1-5"));
    }
    if p["about"].as_str().is_some_and(|a| a != reviewee) {
        return Err(err(StatusCode::BAD_REQUEST, "payload.about must match the path"));
    }
    if !interacted(&app, &signer, &reviewee).await? {
        return Err(err(
            StatusCode::FORBIDDEN,
            "reviews are accepted only between agents that have exchanged messages",
        ));
    }
    let comment: String = p["comment"].as_str().unwrap_or("").chars().take(280).collect();
    let tags: Vec<&str> = p["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).filter(|t| ALLOWED_TAGS.contains(t)).take(6).collect())
        .unwrap_or_default();
    sqlx::query(
        "INSERT INTO reviews (reviewer, reviewee, stars, comment, tags, sig) VALUES ($1,$2,$3,$4,$5,$6) \
         ON CONFLICT (reviewer, reviewee) DO UPDATE SET stars = excluded.stars, comment = excluded.comment, \
           tags = excluded.tags, sig = excluded.sig, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(&signer)
    .bind(&reviewee)
    .bind(stars)
    .bind(&comment)
    .bind(json!(tags).to_string())
    .bind(env.to_string())
    .execute(&app.db)
    .await
    .map_err(internal)?;
    tracing::info!(reviewer = %signer, reviewee = %reviewee, stars, "review recorded");
    Ok(Json(json!({"ok": true, "about": reviewee, "stars": stars, "reputation": reputation_of(&app, &reviewee).await?}))
        .into_response())
}

/// Reputation summary for any agent id — a business or a customer. Public:
/// a business deciding whether to hold a slot for a stranger needs it as
/// much as a customer choosing a barber does.
pub async fn reputation_of(app: &App, agent: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let rows: Vec<(String, i64, String, String, String)> = sqlx::query_as(
        "SELECT reviewer, stars, comment, tags, updated_at FROM reviews WHERE reviewee = $1 \
         ORDER BY updated_at DESC LIMIT 200",
    )
    .bind(agent)
    .fetch_all(&app.db)
    .await
    .map_err(internal)?;
    let n = rows.len() as i64;
    let avg = if n > 0 { rows.iter().map(|r| r.1 as f64).sum::<f64>() / n as f64 } else { 0.0 };
    let mut hist = [0i64; 5];
    let mut tag_counts: std::collections::BTreeMap<String, i64> = Default::default();
    for r in &rows {
        hist[(r.1.clamp(1, 5) - 1) as usize] += 1;
        if let Ok(Value::Array(ts)) = serde_json::from_str::<Value>(&r.3) {
            for t in ts.iter().filter_map(|t| t.as_str()) {
                *tag_counts.entry(t.to_string()).or_default() += 1;
            }
        }
    }
    let (ints, first_seen): (i64, Option<String>) = sqlx::query_as(
        "SELECT COALESCE(SUM(n), 0), MIN(first_at) FROM interactions WHERE a = $1 OR b = $1",
    )
    .bind(agent)
    .fetch_one(&app.db)
    .await
    .map_err(internal)?;
    let counterparties: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM interactions WHERE a = $1 OR b = $1")
            .bind(agent)
            .fetch_one(&app.db)
            .await
            .map_err(internal)?;
    let device = device_of(app, agent).await;
    // Reviewer verification rides along: a 5★ from a hardware-attested phone
    // is worth more than one from a key minted a minute ago.
    let mut recent = Vec::new();
    for r in rows.iter().take(8) {
        let d = device_of(app, &r.0).await;
        recent.push(json!({
            "stars": r.1, "comment": r.2,
            "tags": serde_json::from_str::<Value>(&r.3).unwrap_or(json!([])),
            "at": r.4,
            "reviewerVerified": d["verified"].as_bool() == Some(true),
        }));
    }
    Ok(json!({
        "agent": agent,
        "reviews": n,
        "rating": if n > 0 { json!((avg * 100.0).round() / 100.0) } else { Value::Null },
        "histogram": {"1": hist[0], "2": hist[1], "3": hist[2], "4": hist[3], "5": hist[4]},
        "tags": tag_counts,
        "interactions": ints,
        "counterparties": counterparties.0,
        "memberSince": first_seen,
        "hardwareVerified": device["verified"].as_bool() == Some(true),
        "recent": recent,
    }))
}

pub async fn reputation(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let agent = agent_row(&app, &id).await.map(|r| r.agent).unwrap_or_else(|_| id.clone());
    if !AgentId::looks_valid(&agent) {
        return Err(err(StatusCode::BAD_REQUEST, "bad agent id"));
    }
    Ok(Json(reputation_of(&app, &agent).await?).into_response())
}

// ----------------------------------------------------------------- match

#[derive(serde::Deserialize, Default)]
pub struct MatchQ {
    #[serde(default, alias = "query")] pub q: Option<String>,
    #[serde(default)] pub country: Option<String>,
    #[serde(default)] pub city: Option<String>,
    #[serde(default)] pub industry: Option<String>,
    #[serde(default)] pub limit: Option<usize>,
    #[serde(default)] pub lat: Option<f64>,
    #[serde(default)] pub lng: Option<f64>,
}

/// Great-circle distance in km.
pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(fold_accents)
        .collect()
}

fn fold_accents(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

/// Loose stem so "barbería" matches "barbero"/"barber": compare the first
/// five folded letters when both tokens are long enough.
fn stem_eq(a: &str, b: &str) -> bool {
    // Characters, not bytes: a byte slice panics inside a multi-byte letter
    // (Cyrillic, CJK), and one such card would take down every search.
    fn stem(s: &str) -> Option<&str> {
        s.char_indices().nth(5).map(|(i, _)| &s[..i]).or_else(|| (s.chars().count() == 5).then_some(s))
    }
    a == b || matches!((stem(a), stem(b)), (Some(x), Some(y)) if x == y)
}

/// What the offer text says, flattened for matching: name, industry,
/// description, service and product names, city/district, delivery zones.
fn haystack(payload: &Value) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for k in ["name", "industry", "description"] {
        if let Some(s) = payload[k].as_str() { parts.push(s.to_string()); }
    }
    let offer = &payload["offer"];
    for k in ["location", "city", "district", "address", "summary"] {
        if let Some(s) = offer[k].as_str() { parts.push(s.to_string()); }
    }
    for k in ["services", "products"] {
        if let Some(m) = offer[k].as_object() { parts.extend(m.keys().cloned()); }
    }
    if let Some(z) = offer["delivery"]["zones"].as_object() { parts.extend(z.keys().cloned()); }
    if let Some(b) = payload["bundle"].as_str() { parts.push(b.split('@').next().unwrap_or("").to_string()); }
    parts.iter().flat_map(|p| tokens(p)).collect()
}

pub struct Scored {
    pub score: f64,
    pub why: Vec<String>,
    pub distance_km: Option<f64>,
}

/// Deterministic ranking. Text relevance dominates; reputation, measured
/// boot and a fresh, non-empty slot list break ties and push live,
/// trustworthy businesses up. No model in the loop — the client agent does
/// the language work and hands us a clean query.
pub fn score(q: &MatchQ, payload: &Value, rating: Option<f64>, n_reviews: i64, verified: bool, now: chrono::DateTime<chrono::Utc>) -> Scored {
    let mut s = 0.0;
    let mut why = Vec::new();
    let hay = haystack(payload);
    let qtok = tokens(q.q.as_deref().unwrap_or(""));
    if !qtok.is_empty() {
        let hits = qtok.iter().filter(|t| hay.iter().any(|h| stem_eq(h, t))).count();
        if hits == 0 && q.industry.is_none() {
            return Scored { score: -1.0, why, distance_km: None };
        }
        s += hits as f64 * 10.0;
        if hits > 0 { why.push(format!("matches {hits}/{} terms", qtok.len())); }
    }
    if let Some(ind) = q.industry.as_deref() {
        let it = tokens(ind);
        let pi = tokens(payload["industry"].as_str().unwrap_or(""));
        if it.iter().any(|t| pi.iter().any(|p| stem_eq(p, t))) {
            s += 12.0;
            why.push("industry".into());
        } else if qtok.is_empty() {
            return Scored { score: -1.0, why, distance_km: None };
        }
    }
    if let Some(city) = q.city.as_deref() {
        let ct = tokens(city);
        let offer = &payload["offer"];
        let place: Vec<String> = ["location", "city", "district", "address"]
            .iter()
            .filter_map(|k| offer[*k].as_str())
            .flat_map(tokens)
            .chain(offer["delivery"]["zones"].as_object().map(|z| z.keys().flat_map(|k| tokens(k)).collect::<Vec<_>>()).unwrap_or_default())
            .collect();
        if ct.iter().any(|t| place.iter().any(|p| stem_eq(p, t))) {
            s += 8.0;
            why.push("near you".into());
        } else if !place.is_empty() {
            s -= 4.0;
        }
    }
    if let Some(r) = rating {
        // 1 review at 5★ is weaker evidence than 20 at 4.6 — shrink toward 3.5.
        let k = 3.0;
        let bayes = (r * n_reviews as f64 + 3.5 * k) / (n_reviews as f64 + k);
        s += (bayes - 3.5) * 3.0;
        why.push(format!("{r:.1}★ ({n_reviews})"));
    }
    if verified {
        s += 2.0;
        why.push("hardware-attested".into());
    }
    let offer = &payload["offer"];
    let fresh = offer["slotsComputedAt"].as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)).num_hours() < 24);
    let n_slots = offer["nextSlots"].as_array().map_or(0, |a| a.len());
    if fresh && n_slots > 0 {
        s += 3.0;
        why.push(format!("{n_slots} open slots"));
    }
    // Real distance beats name-matching a district: within 3 km is a walk,
    // beyond 30 km is another town.
    let mut distance_km = None;
    if let (Some(la), Some(lo), Some(bla), Some(blo)) = (q.lat, q.lng, offer["geo"]["lat"].as_f64(), offer["geo"]["lng"].as_f64()) {
        let d = haversine_km((la, lo), (bla, blo));
        distance_km = Some((d * 10.0).round() / 10.0);
        s += if d <= 3.0 { 9.0 } else if d <= 10.0 { 5.0 } else if d <= 30.0 { 1.0 } else { -6.0 };
        why.push(format!("{:.1} km", d));
    }
    if payload["onboarded"].as_bool() != Some(true) {
        s -= 20.0;
    }
    Scored { score: s, why, distance_km }
}

/// `GET|POST /v1/match` — ranked businesses for a need. Each hit carries the
/// whole offer (services, prices, hours, next free slots, rails, deposit,
/// delivery) and the reputation summary: enough for a client agent to
/// choose, quote and book without a second round-trip.
pub async fn match_agents(State(app): State<Shared>, Extension(auth): Extension<Auth>, Query(qs): Query<MatchQ>, body: Option<Json<MatchQ>>) -> ApiResult {
    // Searching is what a customer's agent does; the result is a lead the
    // business pays for, so the searcher must be a real identity.
    let (client, _) = require_linked(&app, &auth).await?;
    let q = body.map(|b| b.0).unwrap_or(qs);
    let rows: Vec<(String, Option<String>, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT a.agent, a.handle, a.card, a.updated_at, v.verified FROM agents a \
         LEFT JOIN device_verdicts v ON v.agent = a.agent \
         WHERE a.name IS NOT NULL AND a.revoked_at IS NULL AND ($1 IS NULL OR lower(a.country) = lower($1))",
    )
    .bind(q.country.as_deref())
    .fetch_all(&app.db)
    .await
    .map_err(internal)?;
    let now = chrono::Utc::now();
    let mut hits: Vec<(f64, Value)> = Vec::new();
    for (agent, handle, card, updated_at, verified) in rows {
        let payload = serde_json::from_str::<Value>(&card).map(|v| v["payload"].clone()).unwrap_or(Value::Null);
        let rep: (i64, Option<f64>) = sqlx::query_as("SELECT COUNT(*), AVG(stars) FROM reviews WHERE reviewee = $1")
            .bind(&agent)
            .fetch_one(&app.db)
            .await
            .map_err(internal)?;
        let sc = score(&q, &payload, rep.1, rep.0, verified == Some(1), now);
        if sc.score < 0.0 || !crate::credits::listable(&app, &agent).await? {
            continue;
        }
        hits.push((sc.score, json!({
            "agent": agent,
            "handle": handle,
            "agent_name": handle.as_ref().map(|h| format!("urn:agent:yaya:{h}")),
            "name": payload["name"], "industry": payload["industry"], "country": payload["country"],
            "description": payload["description"],
            "offer": payload["offer"],
            "askPrice": crate::wallet::ask_price_of_payload(&payload),
            "reputation": {"rating": rep.1.map(|r| (r * 100.0).round() / 100.0), "reviews": rep.0},
            "hardwareVerified": verified == Some(1),
            "distanceKm": sc.distance_km,
            "updatedAt": updated_at,
            "score": (sc.score * 10.0).round() / 10.0,
            "why": sc.why,
            "facts_url": format!("{}/v1/agents/{agent}/facts", app.public_url),
            "reputation_url": format!("{}/v1/agents/{agent}/reputation", app.public_url),
            "inbox_url": format!("{}/v1/agents/{agent}/inbox", app.public_url),
        })));
    }
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let limit = q.limit.unwrap_or(10).clamp(1, 50);
    let matches: Vec<Value> = hits.into_iter().take(limit).map(|h| h.1).collect();
    let shown: Vec<String> = matches.iter().filter_map(|m| m["agent"].as_str().map(String::from)).collect();
    crate::credits::note_match(&app, &client, &shown).await;
    Ok(Json(json!({"query": q.q, "matches": matches})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(name: &str, industry: &str, city: &str, slots: usize) -> Value {
        json!({
            "name": name, "industry": industry, "onboarded": true,
            "description": format!("{name} — {industry}"),
            "offer": {"city": city, "services": {"corte clásico": 25, "barba": 15},
                      "nextSlots": (0..slots).map(|i| format!("2026-08-23T1{i}:00")).collect::<Vec<_>>(),
                      "slotsComputedAt": chrono::Utc::now().to_rfc3339()}
        })
    }

    #[test]
    fn text_match_survives_accents_and_inflection() {
        let q = MatchQ { q: Some("barberia en miraflores".into()), city: Some("Miraflores".into()), ..Default::default() };
        let hit = score(&q, &card("Peluquería Tito", "barbería", "Miraflores", 3), Some(4.8), 12, true, chrono::Utc::now());
        let miss = score(&q, &card("Vet Amigo", "veterinaria", "Callao", 3), Some(5.0), 2, false, chrono::Utc::now());
        assert!(hit.score > 20.0, "{:?}", hit.why);
        assert!(miss.score < 0.0);
    }

    #[test]
    fn distance_ranks_the_closer_shop() {
        let mut near = card("Near", "barbería", "Lima", 2);
        near["offer"]["geo"] = json!({"lat": -12.121, "lng": -77.030});
        let mut far = card("Far", "barbería", "Lima", 2);
        far["offer"]["geo"] = json!({"lat": -12.050, "lng": -77.030});
        let q = MatchQ { q: Some("barbería".into()), lat: Some(-12.120), lng: Some(-77.031), ..Default::default() };
        let a = score(&q, &near, None, 0, false, chrono::Utc::now());
        let b = score(&q, &far, None, 0, false, chrono::Utc::now());
        assert!(a.score > b.score);
        assert!(a.distance_km.unwrap() < 1.0 && b.distance_km.unwrap() > 5.0);
    }

    #[test]
    fn reputation_and_slots_break_ties() {
        let q = MatchQ { q: Some("barbería".into()), ..Default::default() };
        let a = score(&q, &card("A", "barbería", "Lima", 4), Some(4.9), 30, true, chrono::Utc::now());
        let b = score(&q, &card("B", "barbería", "Lima", 0), None, 0, false, chrono::Utc::now());
        assert!(a.score > b.score);
    }

    #[test]
    fn one_glowing_review_does_not_outrank_a_track_record() {
        let q = MatchQ { q: Some("barbería".into()), ..Default::default() };
        let newbie = score(&q, &card("N", "barbería", "Lima", 2), Some(5.0), 1, false, chrono::Utc::now());
        let veteran = score(&q, &card("V", "barbería", "Lima", 2), Some(4.6), 40, false, chrono::Utc::now());
        assert!(veteran.score > newbie.score);
    }

    #[test]
    fn non_latin_cards_and_queries_never_crash_matching() {
        let ru = json!({"name": "Ресторан Москва", "industry": "ресторан", "onboarded": true, "offer": {"city": "Лима"}});
        let zh = json!({"name": "東京タワー寿司", "industry": "寿司", "onboarded": true});
        for q in ["ресторан", "寿司屋さん", "barbería", "ресторанчик рядом"] {
            let q = MatchQ { q: Some(q.into()), ..Default::default() };
            score(&q, &ru, None, 0, false, chrono::Utc::now());
            score(&q, &zh, None, 0, false, chrono::Utc::now());
        }
        assert!(stem_eq("ресторан", "ресторанчик"), "stems compare characters, not bytes");
        assert!(!stem_eq("ресторан", "реставрация"));
        assert!(stem_eq("barberia", "barbero"));
    }

    #[tokio::test]
    async fn reviews_need_a_conversation_and_stay_one_per_pair() {
        use crate::testkit::{self, account_with_agent, anon, as_agent, Keypair};
        let app = testkit::app().await;
        let (biz, client) = (Keypair::generate(), Keypair::generate());
        account_with_agent(&app, "biz", "51900000001", &biz).await;
        account_with_agent(&app, "cli", "51900000002", &client).await;
        let bid = biz.id().to_string();
        let path = format!("/v1/agents/{bid}/reviews");
        let review = |stars: i64, tags: Value| client.envelope(json!({"stars": stars, "comment": "x".repeat(400), "tags": tags}));
        assert_eq!(as_agent(&app, &client, "POST", &path, Some(review(5, json!([])))).await.0, 403, "no conversation, no review");
        note_interaction(&app, &client.id().to_string(), &bid).await;
        assert_eq!(as_agent(&app, &client, "POST", &path, Some(review(6, json!([])))).await.0, 400);
        assert_eq!(as_agent(&app, &client, "POST", &format!("/v1/agents/{}/reviews", client.id()), Some(review(5, json!([])))).await.0, 400, "not yourself");
        let about = client.envelope(json!({"stars": 5, "about": "agent:someone-else"}));
        assert_eq!(as_agent(&app, &client, "POST", &path, Some(about)).await.0, 400);
        let (st, v) = as_agent(&app, &client, "POST", &path, Some(review(2, json!(["late", "hacker", "rude"])))).await;
        assert_eq!(st, 200, "{v}");
        let (_, v) = as_agent(&app, &client, "POST", &path, Some(review(4, json!(["on_time"])))).await;
        assert_eq!((v["reputation"]["reviews"].clone(), v["reputation"]["rating"].clone()), (json!(1), json!(4.0)), "a second review replaces the first");
        let (_, rep) = anon(&app, "GET", &format!("/v1/agents/{bid}/reputation"), None).await;
        assert_eq!(rep["tags"], json!({"on_time": 1}));
        assert_eq!(rep["recent"][0]["comment"].as_str().unwrap().chars().count(), 280);
        assert_eq!(rep["counterparties"], 1);
        assert_eq!(anon(&app, "GET", "/v1/agents/bad/reputation", None).await.0, 400);
    }

    #[tokio::test]
    async fn match_ranks_live_businesses_for_a_signed_client() {
        use crate::testkit::{self, account_with_agent, as_agent, Keypair};
        let app = testkit::app().await;
        let client = Keypair::generate();
        account_with_agent(&app, "cli", "51900000009", &client).await;
        for (i, (name, ind, city)) in [("Barbería Tito", "barbería", "Miraflores"), ("Vet Amigo", "veterinaria", "Callao"), ("Ресторан", "ресторан", "Лима")].iter().enumerate() {
            let kp = Keypair::generate();
            account_with_agent(&app, &format!("b{i}"), &format!("5190000000{i}"), &kp).await;
            crate::credits::add(&app, &format!("b{i}"), 10_000, "topup", None, None).await.unwrap();
            sqlx::query("INSERT INTO agents (agent, card, name, country) VALUES ($1,$2,$3,'PE')")
                .bind(kp.id().to_string()).bind(json!({"payload": card(name, ind, city, 2)}).to_string()).bind(*name)
                .execute(&app.db).await.unwrap();
        }
        let (st, v) = as_agent(&app, &client, "GET", "/v1/match?q=barberia&city=miraflores&country=pe", None).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["matches"].as_array().unwrap().len(), 1);
        assert_eq!(v["matches"][0]["name"], "Barbería Tito");
        let (st, _) = as_agent(&app, &client, "POST", "/v1/match", Some(json!({"q": "ресторан"}))).await;
        assert_eq!(st, 200, "a Cyrillic query is answered, not dropped");
        assert_eq!(crate::testkit::anon(&app, "GET", "/v1/match?q=x", None).await.0, 401);
    }
}

