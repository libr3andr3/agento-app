//! yaya mesh rendezvous — free by default: we are where agents meet.
//!
//! Agents register their WireGuard + ML-KEM-768 keys and the endpoints they
//! see themselves at (`POST /v1/mesh/register`); we hand each one an
//! address in `10.77.0.0/16` and answer lookups (`GET /v1/mesh/agents/{id}`).
//! When two agents link, the initiator may ask for a relay pair
//! (`POST /v1/mesh/links`): two UDP ports on the public relay that forward
//! to each other, so two phones behind carrier NAT still meet. Keys are
//! exchanged and the tunnel's post-quantum PreSharedKey is derived on the
//! phones over the E2E relay — this service sees public keys and ports only.

use axum::{extract::{Path, State}, http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::{json, Value};

use crate::{accounts, auth::require_linked, err, internal, ApiResult, App, Auth, Shared};

pub const SUBNET: &str = "10.77.0.0/16";

pub struct Relay {
    http: reqwest::Client,
    url: String,
    key: String,
    /// The address agents dial (the relay's public IP or name).
    pub host: String,
}

impl Relay {
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let url = std::env::var("MESH_RELAY_URL").ok().filter(|s| !s.trim().is_empty())?;
        let host = std::env::var("MESH_RELAY_HOST").ok().filter(|s| !s.trim().is_empty())?;
        Some(Self { http, url: url.trim_end_matches('/').to_string(), key: std::env::var("MESH_RELAY_KEY").unwrap_or_default(), host })
    }
    #[cfg(test)]
    pub fn for_test(url: &str, host: &str) -> Self {
        Self { http: reqwest::Client::new(), url: url.trim_end_matches('/').to_string(), key: "rk".into(), host: host.into() }
    }

    /// Two forwarding ports, `ttl` seconds of idleness allowed.
    pub async fn pair(&self, ttl_secs: u64) -> anyhow::Result<(u16, u16, String)> {
        let r = self.http.post(format!("{}/pairs", self.url)).header("x-relay-key", &self.key).json(&json!({"ttlSecs": ttl_secs})).send().await?;
        let v: Value = r.json().await?;
        let a = v["a"].as_u64().ok_or_else(|| anyhow::anyhow!("relay: {v}"))? as u16;
        let b = v["b"].as_u64().ok_or_else(|| anyhow::anyhow!("relay: {v}"))? as u16;
        Ok((a, b, v["expiresAt"].as_str().unwrap_or("").to_string()))
    }
}

fn reflect_addr() -> Option<String> {
    std::env::var("MESH_REFLECT").ok().filter(|s| !s.trim().is_empty())
}

pub fn info(app: &App) -> Value {
    json!({
        "subnet": SUBNET,
        "reflect": reflect_addr(),
        "relayHost": app.mesh_relay.as_ref().map(|r| r.host.clone()),
        "relay": app.mesh_relay.is_some(),
        "kem": "ML-KEM-768", "tunnel": "wireguard+psk", "a2a": "http://<mesh ip>:7770/a2a",
        "free": true,
    })
}

/// `GET /v1/mesh/info` — public; with a bearer, our own record too.
pub async fn info_route(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let mut v = info(&app);
    if let Ok(agent) = crate::agent_of(&app, &auth).await {
        v["me"] = record(&app, &agent).await?.unwrap_or(Value::Null);
    }
    Ok(Json(v).into_response())
}

/// The next free address: 10.77.x.y with x.y from 1 to 65534 (no .0/.255
/// hosts). Runs inside the registering write transaction, so two agents
/// joining at once cannot both pick the same one.
async fn allocate_ip(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<String, (StatusCode, Json<Value>)> {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_agents").fetch_one(&mut **tx).await.map_err(internal)?;
    let mut i = n + 1;
    for _ in 0..70_000 {
        let (hi, lo) = ((i / 256) as u8, (i % 256) as u8);
        if lo != 0 && lo != 255 && hi < 255 {
            let ip = format!("10.77.{hi}.{lo}");
            let taken: Option<(String,)> = sqlx::query_as("SELECT ip FROM mesh_agents WHERE ip = $1").bind(&ip).fetch_optional(&mut **tx).await.map_err(internal)?;
            if taken.is_none() {
                return Ok(ip);
            }
        }
        i += 1;
    }
    Err(err(StatusCode::SERVICE_UNAVAILABLE, "mesh is full"))
}

#[derive(serde::Deserialize)]
pub struct RegisterReq {
    wg: String,
    kem: String,
    #[serde(default, rename = "listenPort")] listen_port: Option<u16>,
    #[serde(default)] endpoints: Vec<String>,
    #[serde(default)] hostname: Option<String>,
    #[serde(default)] a2a: Option<u16>,
}

fn valid_b64(s: &str, len: usize) -> bool {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s.trim()).map(|b| b.len() == len).unwrap_or(false)
}

/// `POST /v1/mesh/register` — signed request by the agent (linked or in
/// grace). Idempotent: keys/endpoints refresh, the address stays.
pub async fn register(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<RegisterReq>) -> ApiResult {
    let (agent, account) = require_linked(&app, &auth).await?;
    if !valid_b64(&req.wg, 32) {
        return Err(err(StatusCode::BAD_REQUEST, "wg must be a 32-byte base64 key"));
    }
    if !valid_b64(&req.kem, 1184) {
        return Err(err(StatusCode::BAD_REQUEST, "kem must be a 1184-byte ML-KEM-768 encapsulation key (base64)"));
    }
    let endpoints: Vec<String> = req.endpoints.iter().filter(|e| e.len() < 64 && e.contains(':')).take(8).cloned().collect();
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let existing: Option<(String,)> = sqlx::query_as("SELECT ip FROM mesh_agents WHERE agent = $1").bind(&agent).fetch_optional(&mut *tx).await.map_err(internal)?;
    let ip = match existing { Some((ip,)) => ip, None => allocate_ip(&mut tx).await? };
    sqlx::query(
        "INSERT INTO mesh_agents (agent, account, ip, wg, kem, listen_port, endpoints, hostname, a2a_port) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
         ON CONFLICT (agent) DO UPDATE SET account = excluded.account, wg = excluded.wg, kem = excluded.kem, listen_port = excluded.listen_port, \
           endpoints = excluded.endpoints, hostname = excluded.hostname, a2a_port = excluded.a2a_port, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(&agent).bind(&account).bind(&ip).bind(req.wg.trim()).bind(req.kem.trim()).bind(req.listen_port.map(|p| p as i64))
    .bind(serde_json::to_string(&endpoints).unwrap_or_else(|_| "[]".into())).bind(req.hostname.as_deref().map(|h| h.chars().take(64).collect::<String>())).bind(req.a2a.map(|p| p as i64))
    .execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    tracing::info!(%agent, %ip, endpoints = endpoints.len(), "mesh registered");
    let mut v = info(&app);
    v["ip"] = json!(ip);
    v["agent"] = json!(agent);
    Ok(Json(v).into_response())
}

async fn record(app: &App, agent: &str) -> Result<Option<Value>, (StatusCode, Json<Value>)> {
    let row: Option<(String, String, String, Option<i64>, String, Option<String>, Option<i64>, String)> = sqlx::query_as(
        "SELECT ip, wg, kem, listen_port, endpoints, hostname, a2a_port, updated_at FROM mesh_agents WHERE agent = $1",
    ).bind(agent).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((ip, wg, kem, port, endpoints, hostname, a2a, updated)) = row else { return Ok(None) };
    let name: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT name, handle FROM agents WHERE agent = $1").bind(agent).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(Some(json!({
        "agent": agent, "ip": ip, "wg": wg, "kem": kem, "listenPort": port,
        "endpoints": serde_json::from_str::<Value>(&endpoints).unwrap_or(json!([])),
        "hostname": hostname, "a2a": a2a.map(|p| format!("http://{ip}:{p}/a2a")),
        "name": name.as_ref().and_then(|n| n.0.clone()), "handle": name.and_then(|n| n.1),
        "updatedAt": updated,
    })))
}

/// `GET /v1/mesh/agents/{id}` — a peer's keys and address (any bearer).
pub async fn get_agent(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    crate::agent_of(&app, &auth).await?;
    match record(&app, &id).await? {
        Some(v) => Ok(Json(v).into_response()),
        None => Err(err(StatusCode::NOT_FOUND, "that agent is not on the mesh")),
    }
}

#[derive(serde::Deserialize)]
pub struct LinkReq { peer: String }

/// `POST /v1/mesh/links` — a relay pair for (me, peer), reused while it
/// lives. Without a relay configured the answer says so and the peers go
/// direct.
pub async fn link(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<LinkReq>) -> ApiResult {
    let (me, _) = require_linked(&app, &auth).await?;
    let peer = req.peer.trim().to_string();
    if peer == me || !peer.starts_with("agent:") {
        return Err(err(StatusCode::BAD_REQUEST, "bad peer"));
    }
    if record(&app, &peer).await?.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "peer is not on the mesh"));
    }
    let Some(relay) = app.mesh_relay.as_ref() else {
        return Ok(Json(json!({"relay": false, "note": "no public relay configured; use the reported endpoints"})).into_response());
    };
    let now = chrono::Utc::now().to_rfc3339();
    let live: Option<(String, Option<i64>, Option<i64>, Option<String>, String)> = sqlx::query_as(
        "SELECT relay_host, port_a, port_b, expires_at, a FROM mesh_links WHERE ((a = $1 AND b = $2) OR (a = $2 AND b = $1)) AND (expires_at IS NULL OR expires_at > $3)",
    ).bind(&me).bind(&peer).bind(&now).fetch_optional(&app.db).await.map_err(internal)?;
    if let Some((host, Some(pa), Some(pb), exp, a)) = live {
        let (mine, theirs) = if a == me { (pa, pb) } else { (pb, pa) };
        return Ok(Json(json!({"relay": true, "relayHost": host, "myPort": mine, "peerPort": theirs, "expiresAt": exp, "reused": true})).into_response());
    }
    // Each new pair holds two public relay ports for up to the TTL: bounded
    // per agent (a live pair is reused above for free).
    if !app.limiter.take(&format!("mesh-link:{me}"), yaya_wire::ratelimit::Quota::from_env("MESH_LINKS_PER_DAY", 50.0, 86_400.0)) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many new mesh links today"));
    }
    let ttl = crate::env_or("MESH_RELAY_TTL_SECS", "2592000").parse::<u64>().unwrap_or(2_592_000);
    let (pa, pb, exp) = relay.pair(ttl).await.map_err(|e| { tracing::error!("relay: {e}"); err(StatusCode::BAD_GATEWAY, "relay unavailable") })?;
    sqlx::query("INSERT INTO mesh_links (id, a, b, relay_host, port_a, port_b, expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7) \
                 ON CONFLICT (a, b) DO UPDATE SET relay_host = excluded.relay_host, port_a = excluded.port_a, port_b = excluded.port_b, expires_at = excluded.expires_at, created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&me).bind(&peer).bind(&relay.host).bind(pa as i64).bind(pb as i64).bind(&exp)
        .execute(&app.db).await.map_err(internal)?;
    tracing::info!(a = %me, b = %peer, pa, pb, "mesh relay pair allocated");
    Ok(Json(json!({"relay": true, "relayHost": relay.host, "myPort": pa, "peerPort": pb, "expiresAt": exp})).into_response())
}

/// `GET /v1/mesh/agents` — who is on the mesh (public keys only), for the
/// console and `yaya mesh who`.
pub async fn list(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let _ = crate::agent_of(&app, &auth).await?;
    let rows: Vec<(String, String, Option<String>, Option<i64>, String)> = sqlx::query_as(
        "SELECT m.agent, m.ip, a.handle, m.a2a_port, m.updated_at FROM mesh_agents m LEFT JOIN agents a ON a.agent = m.agent ORDER BY m.updated_at DESC LIMIT 500",
    ).fetch_all(&app.db).await.map_err(internal)?;
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_agents").fetch_one(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"count": n, "agents": rows.into_iter().map(|(a, ip, h, p, u)| json!({"agent": a, "ip": ip, "handle": h, "a2a": p.map(|p| format!("http://{ip}:{p}/a2a")), "updatedAt": u})).collect::<Vec<_>>()})).into_response())
}

/// Same-account helper for the phone's own devices, exposed for tests.
#[allow(dead_code)]
pub async fn same_account(app: &App, a: &str, b: &str) -> bool {
    accounts::same_account(app, a, b).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_agent, Keypair, Mock};
    use base64::Engine;

    fn keys() -> Value {
        let b = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![7u8; n]);
        json!({"wg": b(32), "kem": b(1184), "listenPort": 51820, "endpoints": ["190.1.2.3:51820", "garbage", "x".repeat(80)], "hostname": "moto", "a2a": 7770})
    }

    async fn linked(app: &Shared, n: usize) -> Vec<Keypair> {
        let mut out = Vec::new();
        for i in 0..n {
            let kp = Keypair::generate();
            account_with_agent(app, &format!("acct{i}"), &format!("5190000{i:04}"), &kp).await;
            out.push(kp);
        }
        out
    }

    #[tokio::test]
    async fn registering_keeps_one_address_per_agent() {
        let app = testkit::app().await;
        let kp = linked(&app, 1).await.remove(0);
        let mut bad = keys();
        bad["wg"] = json!("short");
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/mesh/register", Some(bad)).await.0, 400);
        let mut bad = keys();
        bad["kem"] = json!("AAAA");
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/mesh/register", Some(bad)).await.0, 400);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/mesh/register", Some(keys())).await;
        assert_eq!((st, v["ip"].clone()), (200, json!("10.77.0.1")), "{v}");
        let (_, again) = as_agent(&app, &kp, "POST", "/v1/mesh/register", Some(keys())).await;
        assert_eq!(again["ip"], "10.77.0.1", "re-registering keeps the address");
        let (_, me) = as_agent(&app, &kp, "GET", "/v1/mesh/info", None).await;
        assert_eq!(me["me"]["endpoints"], json!(["190.1.2.3:51820"]), "junk endpoints are dropped");
        assert_eq!(me["me"]["a2a"], "http://10.77.0.1:7770/a2a");
        assert_eq!(anon(&app, "GET", "/v1/mesh/info", None).await.1["subnet"], SUBNET);
        let id = kp.id().to_string();
        assert_eq!(as_agent(&app, &kp, "GET", &format!("/v1/mesh/agents/{id}"), None).await.1["ip"], "10.77.0.1");
        assert_eq!(as_agent(&app, &kp, "GET", "/v1/mesh/agents/agent:nobody", None).await.0, 404);
        assert_eq!(as_agent(&app, &kp, "GET", "/v1/mesh/agents", None).await.1["count"], 1);
        assert_eq!(as_agent(&app, &Keypair::generate(), "POST", "/v1/mesh/register", Some(keys())).await.0, 401, "unlinked");
    }

    #[tokio::test]
    async fn agents_joining_at_once_all_get_distinct_addresses() {
        let app = testkit::app().await;
        let kps = linked(&app, 8).await;
        let mut tasks = Vec::new();
        for kp in kps {
            let app = app.clone();
            tasks.push(tokio::spawn(async move { as_agent(&app, &kp, "POST", "/v1/mesh/register", Some(keys())).await }));
        }
        let mut res = Vec::new();
        for t in tasks { res.push(t.await.unwrap()); }
        assert!(res.iter().all(|(st, _)| *st == 200), "{res:?}");
        let mut ips: Vec<String> = res.iter().map(|(_, v)| v["ip"].as_str().unwrap().to_string()).collect();
        ips.sort();
        ips.dedup();
        assert_eq!(ips.len(), 8);
    }

    async fn link(app: &Shared, kp: &Keypair, peer: String) -> (u16, Value) {
        as_agent(app, kp, "POST", "/v1/mesh/links", Some(json!({"peer": peer}))).await
    }

    #[tokio::test]
    async fn links_borrow_a_relay_pair_once_per_couple() {
        let m = Mock::start().await;
        m.on("/pairs", json!({"a": 40001, "b": 40002, "expiresAt": "2099-01-01T00:00:00Z"}));
        let base = m.base.clone();
        let app = testkit::app_with(move |a| crate::App { mesh_relay: Some(Relay::for_test(&base, "relay.yaya.tech")), ..a }).await;
        let kps = linked(&app, 3).await;
        for kp in &kps[..2] { as_agent(&app, kp, "POST", "/v1/mesh/register", Some(keys())).await; }
        let (a, b) = (&kps[0], &kps[1]);
        assert_eq!(link(&app, a, a.id().to_string()).await.0, 400);
        assert_eq!(link(&app, a, "x".into()).await.0, 400);
        assert_eq!(link(&app, a, kps[2].id().to_string()).await.0, 404, "the peer must be on the mesh");
        let (st, v) = link(&app, a, b.id().to_string()).await;
        assert_eq!((st, v["myPort"].clone(), v["peerPort"].clone()), (200, json!(40001), json!(40002)), "{v}");
        assert_eq!(m.seen_path("/pairs")[0].headers["x-relay-key"], "rk");
        let (_, back) = link(&app, b, a.id().to_string()).await;
        assert_eq!((back["reused"].clone(), back["myPort"].clone(), back["peerPort"].clone()), (json!(true), json!(40002), json!(40001)));
        assert_eq!(m.seen_path("/pairs").len(), 1, "one pair per couple while it lives");
        sqlx::query("UPDATE mesh_links SET expires_at = '2000-01-01T00:00:00Z'").execute(&app.db).await.unwrap();
        m.on_status("/pairs", 503, json!({"error": "full"}));
        assert_eq!(link(&app, a, b.id().to_string()).await.0, 502);
    }

    #[tokio::test]
    async fn relay_pairs_are_bounded_per_agent() {
        std::env::set_var("MESH_LINKS_PER_DAY", "2");
        let m = Mock::start().await;
        m.on("/pairs", json!({"a": 1, "b": 2, "expiresAt": "2099-01-01T00:00:00Z"}));
        let base = m.base.clone();
        let app = testkit::app_with(move |a| crate::App { mesh_relay: Some(Relay::for_test(&base, "r")), ..a }).await;
        let kps = linked(&app, 4).await;
        for kp in &kps { as_agent(&app, kp, "POST", "/v1/mesh/register", Some(keys())).await; }
        let me = &kps[0];
        for peer in &kps[1..3] {
            assert_eq!(as_agent(&app, me, "POST", "/v1/mesh/links", Some(json!({"peer": peer.id().to_string()}))).await.0, 200);
        }
        assert_eq!(as_agent(&app, me, "POST", "/v1/mesh/links", Some(json!({"peer": kps[1].id().to_string()}))).await.0, 200, "a live pair is reused for free");
        assert_eq!(as_agent(&app, me, "POST", "/v1/mesh/links", Some(json!({"peer": kps[3].id().to_string()}))).await.0, 429);
        std::env::remove_var("MESH_LINKS_PER_DAY");
    }

    #[tokio::test]
    async fn without_a_relay_peers_go_direct() {
        let app = testkit::app().await;
        let kps = linked(&app, 2).await;
        for kp in &kps { as_agent(&app, kp, "POST", "/v1/mesh/register", Some(keys())).await; }
        let (_, v) = as_agent(&app, &kps[0], "POST", "/v1/mesh/links", Some(json!({"peer": kps[1].id().to_string()}))).await;
        assert_eq!(v["relay"], false);
        assert!(same_account(&app, &kps[0].id().to_string(), &kps[0].id().to_string()).await);
    }
}

