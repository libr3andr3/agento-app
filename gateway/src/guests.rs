//! Running without an account. A guest declares itself (signed) and says
//! whether its redacted conversations may train the agents; from then on
//! the metered surface treats it like a free account with no network
//! standing. Samples are accepted only from agents that said yes.

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{agent_of, client_ip, err, internal, ApiResult, App, Auth, Shared};

pub async fn is_guest(app: &App, agent: &str) -> Result<bool, (StatusCode, Json<Value>)> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT guest FROM agents_seen WHERE agent = $1").bind(agent).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(row.map_or(false, |r| r.0 == 1))
}

#[derive(serde::Deserialize)]
pub struct GuestReq { #[serde(default = "yes")] share: bool }
fn yes() -> bool { true }

/// `POST /v1/agents/guest` — "continue without an account".
pub async fn declare(State(app): State<Shared>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, Extension(auth): Extension<Auth>, headers: HeaderMap, Json(req): Json<GuestReq>) -> ApiResult {
    let agent = agent_of(&app, &auth).await?;
    let ip = client_ip(&headers, peer);
    sqlx::query(
        "INSERT INTO agents_seen (agent, ip, calls, guest, share) VALUES ($1, $2, 0, 1, $3) \
         ON CONFLICT (agent) DO UPDATE SET guest = 1, share = excluded.share, last_seen = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(&agent).bind(&ip).bind(req.share as i64).execute(&app.db).await.map_err(internal)?;
    tracing::info!(%agent, share = req.share, "guest declared");
    // A fresh device gets the welcome trial too: without a plan or an account
    // to draw credits from, the metered surface would 429 the very onboarding
    // that makes the agent useful (never overwrites an existing plan row).
    crate::plans::start_trial(&app, &agent).await;
    let plan = crate::plans::effective_for(&app, &agent).await?.plan;
    Ok(Json(json!({"ok": true, "guest": true, "share": req.share, "plan": plan})).into_response())
}

/// `POST /v1/agents/share` — the training switch, for guests and accounts alike.
pub async fn share(State(app): State<Shared>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, Extension(auth): Extension<Auth>, headers: HeaderMap, Json(req): Json<GuestReq>) -> ApiResult {
    let agent = agent_of(&app, &auth).await?;
    // Upsert: an agent the metered surface has not seen yet (a freshly
    // linked phone) must still get the switch it asked for.
    sqlx::query(
        "INSERT INTO agents_seen (agent, ip, calls, share) VALUES ($1, $2, 0, $3) \
         ON CONFLICT (agent) DO UPDATE SET share = excluded.share",
    ).bind(&agent).bind(client_ip(&headers, peer)).bind(req.share as i64).execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true, "share": req.share})).into_response())
}

#[derive(serde::Deserialize)]
pub struct SampleReq { sample: Value, #[serde(default)] niche: Option<String>, #[serde(default)] country: Option<String>, #[serde(default)] industry: Option<String>, #[serde(default)] language: Option<String> }

/// `POST /v1/training` — one redacted turn. Only from agents that opted in,
/// bounded per agent per day, bounded in size.
pub async fn sample(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<SampleReq>) -> ApiResult {
    let agent = agent_of(&app, &auth).await?;
    let row: Option<(i64,)> = sqlx::query_as("SELECT share FROM agents_seen WHERE agent = $1").bind(&agent).fetch_optional(&app.db).await.map_err(internal)?;
    if row.map_or(true, |r| r.0 == 0) {
        return Err(err(StatusCode::FORBIDDEN, "this agent has not opted in to sharing"));
    }
    let body = req.sample.to_string();
    if body.len() > 8 * 1024 {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "sample too large"));
    }
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM training_samples WHERE agent = $1 AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day')")
        .bind(&agent).fetch_one(&app.db).await.map_err(internal)?;
    if n >= app.training_per_agent_per_day {
        return Ok(Json(json!({"ok": false, "reason": "daily cap"})).into_response());
    }
    let niche = req.niche.as_deref().map(|n| n.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(48).collect::<String>()).filter(|n| !n.is_empty());
    sqlx::query("INSERT INTO training_samples (id, agent, country, industry, language, sample, niche) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&agent).bind(req.country.as_deref().map(|c| c.to_ascii_uppercase())).bind(req.industry).bind(req.language).bind(body).bind(&niche)
        .execute(&app.db).await.map_err(internal)?;
    // Data for service: the shared turn earns its message back.
    let reward = crate::wallet::reward_data(&app, &agent).await;
    Ok(Json(json!({"ok": true, "niche": niche, "rewardMinor": reward})).into_response())
}

/// `GET /v1/skills/{niche}` — the curated skill for a niche, or an empty
/// one. Public: it is prompt text, nothing personal.
pub async fn skill(State(app): State<Shared>, axum::extract::Path(niche): axum::extract::Path<String>) -> ApiResult {
    let row: Option<(String, i64, i64, String)> = sqlx::query_as("SELECT skill, version, samples, updated_at FROM niche_skills WHERE niche = $1")
        .bind(niche.trim()).fetch_optional(&app.db).await.map_err(internal)?;
    let mut resp = match row {
        Some((skill, version, samples, at)) => Json(json!({"niche": niche, "skill": skill, "version": version, "samples": samples, "updatedAt": at})),
        None => Json(json!({"niche": niche, "skill": "", "version": 0, "samples": 0})),
    }.into_response();
    resp.headers_mut().insert("cache-control", "public, max-age=600".parse().unwrap());
    Ok(resp)
}

#[derive(serde::Deserialize)]
pub struct SkillReq { niche: String, skill: String, #[serde(default)] samples: Option<i64> }

/// `POST /admin/skills` — the curator publishes (or bumps) a niche skill.
pub async fn set_skill(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<SkillReq>) -> ApiResult {
    let k = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !yaya_wire::secret::ct_eq(k, &app.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    if req.skill.len() > 12_000 {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "skill too long"));
    }
    sqlx::query(
        "INSERT INTO niche_skills (niche, skill, version, samples) VALUES ($1, $2, 1, $3) \
         ON CONFLICT (niche) DO UPDATE SET skill = excluded.skill, version = niche_skills.version + 1, samples = excluded.samples, \
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(req.niche.trim()).bind(req.skill.trim()).bind(req.samples.unwrap_or(0)).execute(&app.db).await.map_err(internal)?;
    let (v,): (i64,) = sqlx::query_as("SELECT version FROM niche_skills WHERE niche = $1").bind(req.niche.trim()).fetch_one(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true, "niche": req.niche.trim(), "version": v})).into_response())
}

/// `GET /admin/training?niche=` — recent samples for the curator.
pub async fn samples(State(app): State<Shared>, headers: HeaderMap, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> ApiResult {
    let k = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !yaya_wire::secret::ct_eq(k, &app.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    let niche = q.get("niche").cloned();
    let limit: i64 = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(200).clamp(1, 2000);
    let rows: Vec<(String, Option<String>, Option<String>, String, String)> = sqlx::query_as(
        "SELECT agent, niche, language, sample, created_at FROM training_samples WHERE ($1 IS NULL OR niche = $1) ORDER BY created_at DESC LIMIT $2",
    ).bind(&niche).bind(limit).fetch_all(&app.db).await.map_err(internal)?;
    let niches: Vec<(Option<String>, i64)> = sqlx::query_as("SELECT niche, count(*) FROM training_samples GROUP BY niche ORDER BY 2 DESC").fetch_all(&app.db).await.map_err(internal)?;
    Ok(Json(json!({
        "niches": niches.into_iter().map(|(n, c)| json!({"niche": n, "samples": c})).collect::<Vec<_>>(),
        "samples": rows.into_iter().map(|(a, n, l, s, at)| json!({"agent": a.chars().take(22).collect::<String>(), "niche": n, "language": l, "sample": serde_json::from_str::<Value>(&s).unwrap_or(Value::Null), "at": at})).collect::<Vec<_>>(),
    })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_admin, as_agent, Keypair};

    #[tokio::test]
    async fn a_guest_gets_the_trial_and_its_choice_about_training() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        let id = kp.id().to_string();
        assert!(!is_guest(&app, &id).await.unwrap());
        assert_eq!(anon(&app, "POST", "/v1/agents/guest", Some(json!({}))).await.0, 401);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/agents/guest", Some(json!({"share": false}))).await;
        assert_eq!((st, v["plan"].clone(), v["share"].clone()), (200, json!("trial"), json!(false)), "{v}");
        assert!(is_guest(&app, &id).await.unwrap());
        let s = json!({"sample": {"q": "hola", "a": "buenas"}, "niche": "Barbería PE!", "country": "pe"});
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/training", Some(s.clone())).await.0, 403, "no opt-in, no samples");
        as_agent(&app, &kp, "POST", "/v1/agents/share", Some(json!({"share": true}))).await;
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/training", Some(s)).await;
        assert_eq!((st, v["niche"].clone(), v["rewardMinor"].clone()), (200, json!("BarberaPE"), Value::Null), "guests earn nothing: {v}");
        let big = json!({"sample": "x".repeat(9000)});
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/training", Some(big)).await.0, 413);
    }

    #[tokio::test]
    async fn opting_in_works_before_the_agent_was_ever_seen() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/agents/share", Some(json!({"share": true}))).await;
        assert_eq!((st, v["share"].clone()), (200, json!(true)));
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/training", Some(json!({"sample": "hola"}))).await.0, 200, "the switch it saw flip is the one that counts");
        assert!(!is_guest(&app, &kp.id().to_string()).await.unwrap(), "sharing does not make it a guest");
    }

    #[tokio::test]
    async fn linked_samples_earn_and_the_daily_cap_holds() {
        let app = testkit::app_with(|a| crate::App { training_per_agent_per_day: 2, ..a }).await;
        let kp = Keypair::generate();
        account_with_agent(&app, "a", "51900000001", &kp).await;
        as_agent(&app, &kp, "POST", "/v1/agents/share", Some(json!({"share": true}))).await;
        let (_, v) = as_agent(&app, &kp, "POST", "/v1/training", Some(json!({"sample": "uno"}))).await;
        assert_eq!(v["rewardMinor"], crate::wallet::data_reward());
        as_agent(&app, &kp, "POST", "/v1/training", Some(json!({"sample": "dos"}))).await;
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/training", Some(json!({"sample": "tres"}))).await;
        assert_eq!((st, v["ok"].clone(), v["reason"].clone()), (200, json!(false), json!("daily cap")));
    }

    #[tokio::test]
    async fn the_curator_publishes_niche_skills() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        as_agent(&app, &kp, "POST", "/v1/agents/guest", Some(json!({"share": true}))).await;
        as_agent(&app, &kp, "POST", "/v1/training", Some(json!({"sample": {"t": 1}, "niche": "barberia-pe"}))).await;
        assert_eq!(anon(&app, "POST", "/admin/skills", Some(json!({"niche": "barberia-pe", "skill": "x"}))).await.0, 401);
        assert_eq!(anon(&app, "GET", "/admin/training", None).await.0, 401);
        assert_eq!(as_admin(&app, "POST", "/admin/skills", Some(json!({"niche": "barberia-pe", "skill": "x".repeat(12_001)}))).await.0, 413);
        assert_eq!(as_admin(&app, "POST", "/admin/skills", Some(json!({"niche": " barberia-pe ", "skill": " corta "}))).await.1["version"], 1);
        assert_eq!(as_admin(&app, "POST", "/admin/skills", Some(json!({"niche": "barberia-pe", "skill": "corta bien", "samples": 1}))).await.1["version"], 2);
        let (_, sk) = anon(&app, "GET", "/v1/skills/barberia-pe", None).await;
        assert_eq!((sk["skill"].clone(), sk["version"].clone()), (json!("corta bien"), json!(2)));
        assert_eq!(anon(&app, "GET", "/v1/skills/none", None).await.1["version"], 0);
        let (_, t) = as_admin(&app, "GET", "/admin/training?niche=barberia-pe&limit=5", None).await;
        assert_eq!(t["samples"][0]["sample"], json!({"t": 1}));
        assert_eq!(t["samples"][0]["agent"].as_str().unwrap().chars().count(), 22, "agent ids are truncated for the curator");
    }
}

