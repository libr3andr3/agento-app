//! Owner scope on the relay — "Agent0".
//!
//! The web console (and any other device on the same Yaya account) reaches
//! this phone through the very same sealed boxes other agents use, but is
//! answered by the *manager* agent and may read the business: the live
//! dashboard, conversations, the tool trace, the composed harness — and
//! write to it. Trust is membership, never a self-asserted flag: the sender
//! must be linked to this phone's own account, which the phone checks with
//! its own session at the gateway (`GET /v1/account`, cached).
//!
//! Wire shape (plaintext inside the box):
//!   request  {"scope":"owner","id":"…","cmd":"chat|snapshot|…", …args}
//!   reply    {"scope":"owner","inReplyTo":"…","cmd":"…","agent":"agent:…", …result | "error":"…"}
//!
//! Trust (DECISIONS D3): a sender is an owner device when it is in this
//! core's own `owner_devices` allowlist. A device gets there by sending
//! `pair {code, label}` with the short-lived code the owner read off this
//! core (`POST /api/pair/start`, or the log line in client mode). Only while
//! the allowlist is still empty — an install that predates pairing — does
//! the gateway roster decide, and that fallback is logged every time.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{AppState, SharedState};

const TRUST_OK_TTL: Duration = Duration::from_secs(600);
const TRUST_NO_TTL: Duration = Duration::from_secs(30);

/// Pairing codes: 6 characters from an alphabet with no 0/O/1/I, five
/// minutes, single use, five wrong guesses and the code is gone.
pub const PAIR_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const PAIR_LEN: usize = 6;
pub const PAIR_TTL: Duration = Duration::from_secs(300);
pub const PAIR_MAX_ATTEMPTS: u32 = 5;

/// The one pairing code that may be open at a time, kept in memory only.
#[derive(Debug, Clone)]
pub struct Pairing {
    code: String,
    issued: Instant,
    attempts: u32,
}

impl Pairing {
    pub fn new() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let code: String = (0..PAIR_LEN).map(|_| PAIR_ALPHABET[rng.gen_range(0..PAIR_ALPHABET.len())] as char).collect();
        Self { code, issued: Instant::now(), attempts: 0 }
    }
    pub fn code(&self) -> &str { &self.code }
    pub fn expires_in(&self) -> Duration { PAIR_TTL.saturating_sub(self.issued.elapsed()) }
    pub fn is_open(&self) -> bool { self.issued.elapsed() < PAIR_TTL && self.attempts < PAIR_MAX_ATTEMPTS }
    /// One guess. `Ok` consumes the code; a wrong guess counts against it.
    fn check(&mut self, guess: &str) -> bool {
        if !self.is_open() { return false; }
        let g: String = guess.trim().chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect();
        if g.len() == PAIR_LEN && yaya_wire::secret::ct_eq(&g, &self.code) {
            // Consumed: the same code cannot pair a second device.
            self.attempts = PAIR_MAX_ATTEMPTS;
            true
        } else {
            self.attempts += 1;
            false
        }
    }
}

/// Mint a fresh code (replacing any open one) and return it with its TTL.
pub fn start_pairing(state: &AppState) -> Value {
    let p = Pairing::new();
    let v = pairing_json(&p);
    *state.pairing.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    v
}

/// The open code, if any, in the same shape as [`start_pairing`].
pub fn current_pairing(state: &AppState) -> Option<Value> {
    state.pairing.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|p| p.is_open()).map(pairing_json)
}

fn pairing_json(p: &Pairing) -> Value {
    json!({
        "code": p.code(),
        "expiresAt": (chrono::Utc::now() + chrono::Duration::from_std(p.expires_in()).unwrap_or_default()).to_rfc3339(),
        "expiresInSecs": p.expires_in().as_secs(),
    })
}

/// Guess against the open code; consumes it on success.
fn try_pairing(state: &AppState, guess: &str) -> bool {
    let mut g = state.pairing.lock().unwrap_or_else(|e| e.into_inner());
    let ok = g.as_mut().map(|p| p.check(guess)).unwrap_or(false);
    if ok || g.as_ref().is_some_and(|p| !p.is_open()) { *g = None; }
    ok
}

/// What the allowlist says about a sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed { Active, Revoked, Unknown }

/// Trust precedence, kept pure so it can be tested: a listed device answers
/// for itself; an unknown sender is trusted by the roster only while nobody
/// has paired yet.
pub fn resolve_trust(listed: Listed, allowlist_empty: bool, roster: impl FnOnce() -> bool) -> bool {
    match listed {
        Listed::Active => true,
        Listed::Revoked => false,
        Listed::Unknown if allowlist_empty => roster(),
        Listed::Unknown => false,
    }
}

/// Is `from` a device this core takes owner commands from? (See module doc.)
pub async fn is_owner_device(state: &AppState, from: &str) -> bool {
    if from.is_empty() || from == state.identity.id() {
        return false;
    }
    if let Some((at, ok)) = state.owner_cache.lock().unwrap_or_else(|e| e.into_inner()).get(from).cloned() {
        if at.elapsed() < if ok { TRUST_OK_TTL } else { TRUST_NO_TTL } {
            return ok;
        }
    }
    let listed = listed_status(state, from).await;
    let empty = allowlist_empty(state).await;
    let ok = match listed {
        Listed::Active => {
            let _ = sqlx::query("UPDATE owner_devices SET last_seen_at = $2 WHERE agent = $1")
                .bind(from).bind(crate::db::now()).execute(&state.db).await;
            true
        }
        Listed::Revoked => false,
        Listed::Unknown if empty => {
            let ok = check_membership(state, from).await;
            if ok {
                tracing::warn!(%from, "owner device trusted by gateway roster: no device has paired yet (D3 fallback)");
            }
            ok
        }
        Listed::Unknown => false,
    };
    state.owner_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(from.to_string(), (Instant::now(), ok));
    ok
}

/// `pair` is the one command an untrusted sender may issue.
pub async fn may_handle(state: &AppState, from: &str, req: &Value) -> bool {
    req["cmd"].as_str() == Some("pair") || is_owner_device(state, from).await
}

async fn listed_status(state: &AppState, agent: &str) -> Listed {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT revoked_at FROM owner_devices WHERE agent = $1")
        .bind(agent).fetch_optional(&state.db).await.unwrap_or(None);
    match row {
        None => Listed::Unknown,
        Some((None,)) => Listed::Active,
        Some((Some(_),)) => Listed::Revoked,
    }
}

async fn allowlist_empty(state: &AppState) -> bool {
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM owner_devices WHERE revoked_at IS NULL")
        .fetch_one(&state.db).await.unwrap_or((0,));
    n.0 == 0
}

/// `pair {code, label}` from any sender: right code → the sender joins the
/// allowlist (re-pairing an active or revoked device re-activates it).
async fn pair(state: &AppState, from: &str, code: &str, label: &str) -> Result<Value> {
    if from.is_empty() || from == state.identity.id() {
        return Err(anyhow!("invalid_code"));
    }
    if !try_pairing(state, code) {
        tracing::warn!(%from, "pairing attempt with a wrong or expired code");
        return Err(anyhow!("invalid_code"));
    }
    let label: String = label.trim().chars().filter(|c| !c.is_control()).take(60).collect();
    let label = if label.is_empty() { None } else { Some(label) };
    sqlx::query(
        "INSERT INTO owner_devices (agent, label, added_at, last_seen_at, revoked_at) VALUES ($1, $2, $3, $3, NULL)          ON CONFLICT (agent) DO UPDATE SET label = COALESCE(excluded.label, owner_devices.label),            last_seen_at = excluded.last_seen_at, revoked_at = NULL",
    ).bind(from).bind(&label).bind(crate::db::now()).execute(&state.db).await?;
    state.owner_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(from.to_string(), (Instant::now(), true));
    tracing::info!(%from, label = label.as_deref().unwrap_or(""), "owner device paired");
    Ok(json!({"paired": true, "agent": state.identity.id(), "label": label}))
}

async fn devices(state: &AppState, from: &str) -> Result<Value> {
    let rows: Vec<(String, Option<String>, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT agent, label, added_at, last_seen_at, revoked_at FROM owner_devices ORDER BY added_at ASC",
    ).fetch_all(&state.db).await?;
    Ok(json!({
        "devices": rows.into_iter().map(|(agent, label, added, seen, revoked)| json!({
            "agent": agent, "label": label, "addedAt": added, "lastSeenAt": seen,
            "revoked": revoked.is_some(), "revokedAt": revoked, "me": agent == from,
        })).collect::<Vec<_>>(),
    }))
}

/// Soft-revoke a device. The last active device cannot lock everyone out.
async fn revoke(state: &AppState, from: &str, agent: &str) -> Result<Value> {
    let agent = agent.trim();
    if listed_status(state, agent).await != Listed::Active {
        return Err(anyhow!("unknown_device"));
    }
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM owner_devices WHERE revoked_at IS NULL").fetch_one(&state.db).await?;
    if agent == from && n.0 <= 1 {
        return Err(anyhow!("cannot_revoke_last_device"));
    }
    sqlx::query("UPDATE owner_devices SET revoked_at = $2 WHERE agent = $1")
        .bind(agent).bind(crate::db::now()).execute(&state.db).await?;
    state.owner_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(agent.to_string(), (Instant::now(), false));
    tracing::info!(%agent, by = %from, "owner device revoked");
    Ok(json!({"revoked": true, "agent": agent}))
}

async fn check_membership(state: &AppState, from: &str) -> bool {
    let Ok(Some(s)) = crate::account::load(state).await else { return false };
    let Ok(v) = state.registry.get_as_session("/v1/account", &s.session, Duration::from_secs(20)).await else { return false };
    v["agents"].as_array().is_some_and(|a| {
        a.iter().any(|x| x["agent"].as_str() == Some(from) && x["revoked"].as_bool() != Some(true))
    })
}

fn api_err(e: (axum::http::StatusCode, axum::Json<Value>)) -> anyhow::Error {
    anyhow!("{}: {}", e.0, e.1 .0)
}

/// Runs one command and shapes the reply. Never panics, never leaks
/// internals: errors come back as a short message.
pub async fn handle(state: &SharedState, from: &str, req: &Value, relay_id: &Value) -> Value {
    let cmd = req["cmd"].as_str().unwrap_or("chat").to_string();
    let mut reply = json!({
        "scope": "owner", "inReplyTo": req["id"].clone(), "relayId": relay_id, "cmd": cmd,
        "agent": state.identity.id(), "at": chrono::Utc::now().to_rfc3339(),
    });
    let started = Instant::now();
    let result = match cmd.as_str() {
        "ping" => Ok(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})),
        "pair" => pair(state, from, req["code"].as_str().unwrap_or(""), req["label"].as_str().unwrap_or("")).await,
        "devices" => devices(state, from).await,
        "revoke" => revoke(state, from, req["agent"].as_str().unwrap_or("")).await,
        "chat" => chat(state, req["text"].as_str().unwrap_or("")).await,
        "history" => history(state, req["limit"].as_i64().unwrap_or(40)).await,
        "snapshot" => snapshot(state).await,
        "conversations" => conversations(state).await,
        "conversation" => conversation(state, req["peer"].as_str().unwrap_or("")).await,
        "export" => export(state).await,
        "apply" => apply(state, &req["patch"]).await,
        "kernel" => Ok(state.kernel.inspect()),
        "trace" => trace(state, req["limit"].as_i64().unwrap_or(60)).await,
        "answer_gap" => answer_gap(state, req["gapId"].as_str().unwrap_or(""), req["answer"].as_str().unwrap_or("")).await,
        "plan" => plan(state).await,
        "audit" => crate::audit::list(state, req["before"].as_i64(), req["limit"].as_i64().unwrap_or(100)).await.map(|e| json!({"entries": e})),
        "audit_verify" => crate::audit::verify(state).await,
        "publish" => Ok(json!({"published": crate::network::publish(state, true).await.is_some()})),
        "sync_market" => Ok(json!({"mounted": crate::network::sync_purchases(state).await})),
        other => Err(anyhow!("unknown command '{other}'")),
    };
    match result {
        Ok(v) => {
            if let (Some(r), Some(o)) = (reply.as_object_mut(), v.as_object()) {
                for (k, val) in o { r.insert(k.clone(), val.clone()); }
            }
        }
        Err(e) => {
            tracing::warn!(%from, %cmd, error = %e, "owner command failed");
            reply["error"] = json!(e.to_string());
        }
    }
    reply["tookMs"] = json!(started.elapsed().as_millis() as u64);
    // Every remote hand on the business is on the record: who (the paired
    // device's agent id), what, with what, and whether it worked. Reads
    // (snapshot, history…) too — access is behaviour.
    if cmd != "ping" {
        let mut args = req.clone();
        if let Some(o) = args.as_object_mut() { o.remove("scope"); o.remove("id"); o.remove("cmd"); if o.contains_key("code") { o.insert("code".into(), json!("<redacted>")); } }
        crate::audit::record(state, crate::network::business_id(state).await, crate::audit::kind::OWNER_CMD, &format!("owner:{from}"), &cmd, json!({
            "args": crate::audit::digest_of(&args.to_string()),
            "ok": reply.get("error").is_none(),
            "error": reply.get("error").cloned(),
        })).await;
    }
    reply
}

async fn business(state: &AppState) -> Result<(Uuid, String)> {
    let row: Option<(Uuid, String)> = sqlx::query_as("SELECT id, owner_phone FROM businesses ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(&state.db).await?;
    row.ok_or_else(|| anyhow!("this phone has no business yet — finish registration in the app"))
}

/// One turn with the manager agent — the same chat the owner has in the
/// app, so what was said on the phone is known on the web and back.
pub(crate) async fn chat(state: &SharedState, text: &str) -> Result<Value> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("empty message"));
    }
    if state.client_mode {
        return assistant_chat(state, text).await;
    }
    let (bid, owner_phone) = business(state).await?;
    let history = crate::routes::load_history(state, bid, "onboarding", &owner_phone).await.map_err(api_err)?;
    let t0 = crate::db::now();
    let outcome = crate::agents::run_onboarding_agent(state, bid, Some(text), &history).await?;
    crate::routes::append_message(state, bid, "onboarding", &owner_phone, "user", text).await.map_err(api_err)?;
    crate::routes::append_message(state, bid, "onboarding", &owner_phone, "assistant", &outcome.reply).await.map_err(api_err)?;
    let (action, action_data) = outcome.action.map(|(a, d)| (json!(a), d)).unwrap_or((Value::Null, Value::Null));
    if action == "finish_onboarding" || action == "save_business_schema" || action == "set_capability" || action == "remove_field" {
        let bg = state.clone();
        tokio::spawn(async move { crate::network::publish(&bg, true).await; });
    }
    // The tools this turn called, in order: the console shows them live.
    let trace: Vec<(String, String, String, i32, String)> = sqlx::query_as(
        "SELECT tool, args, result, latency_ms, created_at FROM tool_events \
         WHERE business_id = $1 AND agent_type = 'onboarding' AND created_at >= $2 ORDER BY created_at ASC LIMIT 40",
    ).bind(bid).bind(t0).fetch_all(&state.db).await.unwrap_or_default();
    Ok(json!({
        "text": outcome.reply, "action": action, "actionData": action_data,
        "trace": trace.into_iter().map(|(tool, args, result, ms, at)| json!({
            "tool": tool, "args": parse(&args), "result": parse(&result), "latencyMs": ms, "at": at,
        })).collect::<Vec<_>>(),
    }))
}

/// Client mode: the owner's device talks to its orchestrator. Same turn as
/// `/api/assistant/message` on the loopback — the profile row is the tenant,
/// the assistant agent (network tools, memory) answers, never the business
/// manager.
async fn assistant_chat(state: &SharedState, text: &str) -> Result<Value> {
    let id = crate::plugins::assistant::ensure_self(
        &state.db,
        &std::env::var("COUNTRY").unwrap_or_else(|_| "PE".into()),
        std::env::var("LANGUAGE").ok().as_deref(),
    ).await?;
    let history = crate::routes::load_history(state, id, "assistant", "self").await.map_err(api_err)?;
    let t0 = crate::db::now();
    let (msg_id, turn) = crate::routes::append_message(state, id, "assistant", "self", "user", text).await.map_err(api_err)?;
    let outcome = crate::agents::run_assistant_agent(state, id, text, &history, turn, msg_id).await?;
    crate::routes::append_message(state, id, "assistant", "self", "assistant", &outcome.reply).await.map_err(api_err)?;
    let (action, action_data) = outcome.action.map(|(a, d)| (json!(a), d)).unwrap_or((Value::Null, Value::Null));
    let trace: Vec<(String, String, String, i32, String)> = sqlx::query_as(
        "SELECT tool, args, result, latency_ms, created_at FROM tool_events \
         WHERE business_id = $1 AND created_at >= $2 ORDER BY created_at ASC LIMIT 40",
    ).bind(id).bind(t0).fetch_all(&state.db).await.unwrap_or_default();
    Ok(json!({
        "text": outcome.reply, "action": action, "actionData": action_data,
        "trace": trace.into_iter().map(|(tool, args, result, ms, at)| json!({
            "tool": tool, "args": parse(&args), "result": parse(&result), "latencyMs": ms, "at": at,
        })).collect::<Vec<_>>(),
    }))
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

async fn history(state: &AppState, limit: i64) -> Result<Value> {
    let (bid, owner_phone, agent_type) = if state.client_mode {
        let id = crate::plugins::assistant::ensure_self(
            &state.db,
            &std::env::var("COUNTRY").unwrap_or_else(|_| "PE".into()),
            std::env::var("LANGUAGE").ok().as_deref(),
        ).await?;
        (id, "self".to_string(), "assistant")
    } else {
        let (b, p) = business(state).await?;
        (b, p, "onboarding")
    };
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT role, content, created_at FROM (SELECT role, content, created_at, idx FROM messages \
           WHERE business_id = $1 AND agent_type = $4 AND peer = $2 ORDER BY idx DESC LIMIT $3) t ORDER BY idx",
    ).bind(bid).bind(&owner_phone).bind(limit.clamp(1, 200)).bind(agent_type).fetch_all(&state.db).await?;
    Ok(json!({"messages": rows.into_iter().map(|(r, c, at)| json!({"role": r, "text": c, "at": at})).collect::<Vec<_>>()}))
}

/// Everything the console's home needs in one round trip: the dashboard
/// the app shows, 30 days of daily series, the conversation list, plan and
/// account standing, and who this agent is.
async fn snapshot(state: &SharedState) -> Result<Value> {
    let (bid, _) = business(state).await?;
    let dash = crate::routes::dashboard_json(state, bid).await.map_err(api_err)?;
    let convos = crate::routes::conversations_json(state, bid).await.map_err(api_err)?;
    let composed = crate::learning::compose(&state.db, &state.schemas_dir, bid).await?;
    let tz = crate::harness::biz_tz(&composed.values);
    let offset = {
        use chrono::{Offset, TimeZone};
        let now = chrono::Utc::now();
        format!("{} seconds", tz.offset_from_utc_datetime(&now.naive_utc()).fix().local_minus_utc())
    };
    let since = crate::db::ago(chrono::Duration::days(30));
    let days = |sql: &'static str| {
        let db = state.db.clone();
        let offset = offset.clone();
        async move {
            sqlx::query_as::<_, (String, i64, Option<f64>)>(sql).bind(bid).bind(since).bind(&offset)
                .fetch_all(&db).await.unwrap_or_default()
                .into_iter().map(|(d, n, a)| json!({"day": d, "n": n, "amount": a.unwrap_or(0.0)})).collect::<Vec<_>>()
        }
    };
    let payments = days("SELECT strftime('%Y-%m-%d', received_at, $3), count(*), SUM(amount) FROM payments WHERE business_id = $1 AND received_at >= $2 GROUP BY 1 ORDER BY 1").await;
    let orders = days("SELECT strftime('%Y-%m-%d', created_at, $3), count(*), SUM(CASE WHEN paid THEN total END) FROM orders WHERE business_id = $1 AND created_at >= $2 AND status <> 'cancelled' GROUP BY 1 ORDER BY 1").await;
    let appointments = days("SELECT strftime('%Y-%m-%d', starts_at, $3), count(*), SUM(CASE WHEN paid THEN price END) FROM appointments WHERE business_id = $1 AND starts_at >= $2 AND status <> 'cancelled' GROUP BY 1 ORDER BY 1").await;
    let messages = days("SELECT strftime('%Y-%m-%d', created_at, $3), count(*), NULL FROM messages WHERE business_id = $1 AND created_at >= $2 AND agent_type = 'customer' AND role = 'user' GROUP BY 1 ORDER BY 1").await;
    let customers = days("SELECT strftime('%Y-%m-%d', created_at, $3), count(DISTINCT peer), NULL FROM messages WHERE business_id = $1 AND created_at >= $2 AND agent_type = 'customer' GROUP BY 1 ORDER BY 1").await;
    let totals: (i64, i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT (SELECT count(DISTINCT peer) FROM messages WHERE business_id = $1 AND agent_type = 'customer'), \
                (SELECT count(*) FROM messages WHERE business_id = $1 AND agent_type = 'customer' AND role = 'user'), \
                (SELECT count(*) FROM appointments WHERE business_id = $1 AND status <> 'cancelled') + (SELECT count(*) FROM orders WHERE business_id = $1 AND status <> 'cancelled'), \
                (SELECT SUM(amount) FROM payments WHERE business_id = $1)",
    ).bind(bid).fetch_one(&state.db).await.unwrap_or((0, 0, 0, None));
    let recent_payments: Vec<(String, Option<String>, Option<String>, String, f64, Option<String>)> = sqlx::query_as(
        "SELECT received_at, payer, payer_phone, source, amount, currency FROM payments WHERE business_id = $1 ORDER BY received_at DESC LIMIT 50",
    ).bind(bid).fetch_all(&state.db).await.unwrap_or_default();
    let account = crate::account::status(state).await.unwrap_or(Value::Null);
    let plan_info = state.plan_info.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(json!({
        "dashboard": dash,
        "conversations": convos["conversations"],
        "payments": recent_payments.into_iter().map(|(at, payer, phone, source, amount, currency)| json!({
            "at": at, "payer": payer.or(phone), "source": source, "amount": amount, "currency": currency,
        })).collect::<Vec<_>>(),
        "series": {"payments": payments, "orders": orders, "appointments": appointments, "messages": messages, "customers": customers},
        "totals": {"customers": totals.0, "messages": totals.1, "transactions": totals.2, "collected": totals.3.unwrap_or(0.0)},
        "account": account, "plan": plan_info,
        "business": {"id": bid, "name": composed.doc["name"], "industry": composed.doc["industry"], "country": composed.doc["country"],
                     "bundle": composed.bundle_pin, "onboarded": composed.doc["onboarded"], "timezone": tz.name(),
                     "agentRole": composed.values["agentRole"], "agentName": composed.values["agentName"],
                     "networkPublish": composed.values["networkPublish"].as_bool().unwrap_or(true)},
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn conversations(state: &SharedState) -> Result<Value> {
    let (bid, _) = business(state).await?;
    crate::routes::conversations_json(state, bid).await.map_err(api_err)
}

async fn conversation(state: &SharedState, peer: &str) -> Result<Value> {
    if peer.is_empty() {
        return Err(anyhow!("peer required"));
    }
    let (bid, _) = business(state).await?;
    crate::routes::conversation_json(state, bid, peer).await.map_err(api_err)
}

/// The harness, portable: what this agent is made of, minus secrets.
/// `apply` accepts `values` and `disabledTools` from it back.
async fn export(state: &AppState) -> Result<Value> {
    let (bid, _) = business(state).await?;
    let composed = crate::learning::compose(&state.db, &state.schemas_dir, bid).await?;
    let row: (String, String, String, String, bool, Value, String) = sqlx::query_as(
        "SELECT name, industry, country, bundle, onboarded, schema_config, created_at FROM businesses WHERE id = $1",
    ).bind(bid).fetch_one(&state.db).await?;
    let plan_info = state.plan_info.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let skill = state.niche_skill.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(json!({
        "format": "agente-harness/1",
        "exportedAt": chrono::Utc::now().to_rfc3339(),
        "agent": {"id": state.identity.id(), "did": state.identity.did(), "core": env!("CARGO_PKG_VERSION")},
        "business": {"id": bid, "name": row.0, "industry": row.1, "country": row.2, "bundle": row.3, "onboarded": row.4, "createdAt": row.6},
        "patch": row.5,
        "values": composed.values,
        "fields": composed.doc["fields"],
        "trials": composed.trials.iter().map(|t| json!({"id": t.id, "path": t.field_path, "value": t.value})).collect::<Vec<_>>(),
        "disabledTools": composed.doc["values"]["disabledTools"],
        "kernel": state.kernel.inspect(),
        "nicheSkill": skill,
        "plan": {"name": plan_info["plan"], "tier": plan_info["tier"], "state": plan_info["state"], "caps": plan_info["caps"]},
    }))
}

/// Customer-facing tools the owner may switch off (mirrors the manager
/// agent's `set_capability` allowlist so the console can never disable the
/// owner's own controls).
const TOGGLABLE: &[&str] = &[
    "check_availability", "book_appointment", "handle_cancellation",
    "create_order", "quote_delivery", "collect_payment", "schedule_reminder", "report_gap",
];

fn remove_path(root: &mut Value, path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    let Some(last) = parts.pop() else { return false };
    let mut cur = root;
    for p in parts {
        match cur.get_mut(p) {
            Some(v) => cur = v,
            None => return false,
        }
    }
    cur.as_object_mut().map(|o| o.remove(last).is_some()).unwrap_or(false)
}

/// Writes to the client patch: `values` merge in, `remove` paths go, and
/// `disabledTools` replaces the list. Protected keys stay out of reach.
async fn apply(state: &SharedState, patch: &Value) -> Result<Value> {
    let (bid, _) = business(state).await?;
    let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1").bind(bid).fetch_one(&state.db).await?;
    let mut cfg = row.0;
    if !cfg.is_object() { cfg = json!({}); }
    let mut changed = Vec::new();
    if let Some(values) = patch["values"].as_object() {
        let mut clean = values.clone();
        for k in ["onboarded", "bundle", "disabledTools"] { clean.remove(k); }
        let mut v = Value::Object(clean);
        if v.get("businessHours").is_some() {
            v["businessHours"] = crate::harness::canon_hours_keys(&v["businessHours"]);
        }
        crate::learning::deep_merge(&mut cfg, &v);
        changed.push(format!("values: {}", v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()));
    }
    if let Some(paths) = patch["remove"].as_array() {
        for p in paths.iter().filter_map(|p| p.as_str()) {
            if ["onboarded", "bundle"].contains(&p) { continue; }
            if remove_path(&mut cfg, p) { changed.push(format!("removed {p}")); }
        }
    }
    if let Some(off) = patch["disabledTools"].as_array() {
        let off: Vec<String> = off.iter().filter_map(|t| t.as_str()).filter(|t| TOGGLABLE.contains(t)).map(String::from).collect();
        cfg["disabledTools"] = json!(off);
        changed.push(format!("disabledTools: {}", off.join(", ")));
    }
    if changed.is_empty() {
        return Err(anyhow!("nothing to apply (send values, remove or disabledTools)"));
    }
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2").bind(&cfg).bind(bid).execute(&state.db).await?;
    tracing::info!(?changed, "harness patched from the console");
    let bg = state.clone();
    tokio::spawn(async move { crate::network::publish(&bg, true).await; });
    let composed = crate::learning::compose(&state.db, &state.schemas_dir, bid).await?;
    Ok(json!({"ok": true, "changed": changed, "patch": cfg, "values": composed.values}))
}

async fn trace(state: &AppState, limit: i64) -> Result<Value> {
    let (bid, _) = business(state).await?;
    let rows: Vec<(String, String, String, String, String, i32, String)> = sqlx::query_as(
        "SELECT tool, args, result, agent_type, peer, latency_ms, created_at FROM tool_events \
         WHERE business_id = $1 ORDER BY created_at DESC LIMIT $2",
    ).bind(bid).bind(limit.clamp(1, 300)).fetch_all(&state.db).await?;
    Ok(json!({"events": rows.into_iter().map(|(tool, args, result, agent, peer, ms, at)| json!({
        "tool": tool, "args": parse(&args), "result": parse(&result), "agent": agent, "peer": peer, "latencyMs": ms, "at": at,
    })).collect::<Vec<_>>()}))
}

/// The owner answers a question the agent could not: the fact mounts as a
/// candidate on trial, usable in the very next conversation.
async fn answer_gap(state: &AppState, gap_id: &str, answer: &str) -> Result<Value> {
    if gap_id.is_empty() || answer.trim().is_empty() {
        return Err(anyhow!("gapId and answer required"));
    }
    let (bid, _) = business(state).await?;
    let created = crate::learning::answer_gap(&state.db, &state.llm, bid, gap_id, answer.trim()).await?;
    Ok(json!({"status": "mounted", "candidates": created}))
}

async fn plan(state: &AppState) -> Result<Value> {
    let me = crate::network::sync_plan(state).await;
    Ok(json!({"plan": me.unwrap_or(Value::Null), "account": crate::account::status(state).await.unwrap_or(Value::Null)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aged(p: &mut Pairing, secs: u64) { p.issued = Instant::now() - Duration::from_secs(secs); }

    #[test]
    fn codes_use_the_unambiguous_alphabet() {
        for _ in 0..200 {
            let p = Pairing::new();
            assert_eq!(p.code().len(), PAIR_LEN);
            assert!(p.code().bytes().all(|b| PAIR_ALPHABET.contains(&b)), "{}", p.code());
            assert!(!p.code().contains(['0', 'O', '1', 'I']));
        }
    }

    #[test]
    fn right_code_pairs_once_and_tolerates_case_and_dashes() {
        let mut p = Pairing::new();
        let pretty = format!(" {}-{} ", &p.code()[..3].to_lowercase(), &p.code()[3..]);
        assert!(p.check(&pretty));
        assert!(!p.is_open(), "a used code is closed");
        assert!(!p.check(p.code().to_string().as_str()), "single use");
    }

    #[test]
    fn five_wrong_guesses_burn_the_code() {
        let mut p = Pairing::new();
        for _ in 0..PAIR_MAX_ATTEMPTS { assert!(!p.check("ZZZZZZ")); }
        assert!(!p.is_open());
        let code = p.code().to_string();
        assert!(!p.check(&code), "even the right code is refused afterwards");
    }

    #[test]
    fn codes_expire() {
        let mut p = Pairing::new();
        aged(&mut p, PAIR_TTL.as_secs() + 1);
        assert!(!p.is_open());
        assert_eq!(p.expires_in(), Duration::ZERO);
        let code = p.code().to_string();
        assert!(!p.check(&code));
    }

    #[test]
    fn allowlist_beats_roster_and_empty_list_falls_back() {
        // A paired device is trusted without asking anyone.
        assert!(resolve_trust(Listed::Active, false, || false));
        assert!(resolve_trust(Listed::Active, true, || false));
        // Revoked stays out even if the roster still lists it.
        assert!(!resolve_trust(Listed::Revoked, false, || true));
        assert!(!resolve_trust(Listed::Revoked, true, || true));
        // Unknown: the roster only decides while nobody has paired.
        assert!(resolve_trust(Listed::Unknown, true, || true));
        assert!(!resolve_trust(Listed::Unknown, true, || false));
        assert!(!resolve_trust(Listed::Unknown, false, || true));
    }

    use crate::testkit::{self, Mock};

    async fn cmd(s: &SharedState, from: &str, req: Value) -> Value {
        handle(s, from, &req, &json!("relay-1")).await
    }

    fn other() -> String { crate::identity::Identity::ephemeral().id() }

    #[tokio::test]
    async fn start_and_current_pairing() {
        let s = testkit::state().await;
        assert!(current_pairing(&s).is_none());
        let p = start_pairing(&s);
        assert_eq!(p["code"].as_str().unwrap().len(), PAIR_LEN);
        assert!(p["expiresInSecs"].as_u64().unwrap() <= PAIR_TTL.as_secs());
        assert_eq!(current_pairing(&s).unwrap()["code"], p["code"]);
        // A new code replaces the old one.
        let q = start_pairing(&s);
        assert!(!try_pairing(&s, p["code"].as_str().unwrap()) || p["code"] == q["code"]);
    }

    #[tokio::test]
    async fn pairing_devices_and_revocation_through_commands() {
        let s = testkit::state().await;
        let (a, b) = (other(), other());
        // Untrusted: only `pair` may be handled.
        assert!(!may_handle(&s, &a, &json!({"cmd": "snapshot"})).await);
        assert!(may_handle(&s, &a, &json!({"cmd": "pair"})).await);
        let bad = cmd(&s, &a, json!({"cmd": "pair", "code": "ZZZZZZ"})).await;
        assert_eq!(bad["error"], "invalid_code");
        let code = start_pairing(&s)["code"].as_str().unwrap().to_string();
        let ok = cmd(&s, &a, json!({"cmd": "pair", "code": code, "label": "  Laptop\u{7} de Tito  "})).await;
        assert_eq!((ok["paired"].clone(), ok["label"].clone(), ok["inReplyTo"].clone(), ok["relayId"].clone()), (json!(true), json!("Laptop de Tito"), Value::Null, json!("relay-1")));
        assert!(current_pairing(&s).is_none(), "the code is consumed");
        assert!(is_owner_device(&s, &a).await);
        // Second device pairs with a fresh code.
        let code = start_pairing(&s)["code"].as_str().unwrap().to_string();
        cmd(&s, &b, json!({"cmd": "pair", "code": code})).await;
        let d = cmd(&s, &a, json!({"cmd": "devices"})).await;
        let list = d["devices"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["me"], true);
        assert_eq!(list[1]["label"], Value::Null);
        // Revoke b from a; b loses access at once (cache updated).
        assert_eq!(cmd(&s, &a, json!({"cmd": "revoke", "agent": b})).await["revoked"], true);
        assert!(!is_owner_device(&s, &b).await);
        assert_eq!(cmd(&s, &a, json!({"cmd": "revoke", "agent": b})).await["error"], "unknown_device");
        // The last active device cannot revoke itself.
        assert_eq!(cmd(&s, &a, json!({"cmd": "revoke", "agent": a})).await["error"], "cannot_revoke_last_device");
        // A revoked device can come back only with a new code.
        let code = start_pairing(&s)["code"].as_str().unwrap().to_string();
        cmd(&s, &b, json!({"cmd": "pair", "code": code})).await;
        assert!(is_owner_device(&s, &b).await);
    }

    #[tokio::test]
    async fn the_core_itself_and_empty_ids_are_never_owners() {
        let s = testkit::state().await;
        assert!(!is_owner_device(&s, "").await);
        assert!(!is_owner_device(&s, &s.identity.id()).await);
        let code = start_pairing(&s)["code"].as_str().unwrap().to_string();
        let me = s.identity.id();
        assert_eq!(cmd(&s, &me, json!({"cmd": "pair", "code": code})).await["error"], "invalid_code");
        assert!(current_pairing(&s).is_some(), "a refused self-pair does not burn the code");
    }

    #[tokio::test]
    async fn roster_decides_only_while_nobody_paired() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::sign_in(&s, &m).await;
        let (a, stranger) = (other(), other());
        m.on("/v1/account", json!({"agents": [{"agent": a}, {"agent": stranger, "revoked": true}]}));
        assert!(is_owner_device(&s, &a).await, "empty allowlist: the account roster vouches");
        assert!(!is_owner_device(&s, &stranger).await, "revoked on the roster");
        assert_eq!(m.seen().iter().filter(|x| x.path == "/v1/account").next().unwrap().headers["authorization"], "Bearer sess_abc");
        // Once somebody pairs, the roster no longer counts for new senders.
        let paired = other();
        let code = start_pairing(&s)["code"].as_str().unwrap().to_string();
        cmd(&s, &paired, json!({"cmd": "pair", "code": code})).await;
        let late = other();
        m.set("/v1/account", json!({"agents": [{"agent": late}]}));
        assert!(!is_owner_device(&s, &late).await);
        // Not signed in: no roster at all.
        let off = testkit::state().await;
        assert!(!is_owner_device(&off, &a).await);
    }

    #[tokio::test]
    async fn every_command_is_audited_with_the_code_redacted() {
        let s = testkit::state().await;
        cmd(&s, &other(), json!({"cmd": "ping"})).await;
        cmd(&s, &other(), json!({"cmd": "pair", "code": "SECRET"})).await;
        cmd(&s, &other(), json!({"cmd": "nope"})).await;
        let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT actor, subject, payload FROM audit_log WHERE kind = 'owner_cmd' ORDER BY seq").fetch_all(&s.db).await.unwrap();
        assert_eq!(rows.len(), 2, "ping is not recorded");
        assert_eq!(rows[0].1, "pair");
        assert!(!rows[0].2.contains("SECRET"));
        assert!(rows[1].2.contains("unknown command"));
        assert!(rows[0].0.starts_with("owner:agent:"));
    }

    #[tokio::test]
    async fn commands_without_a_business_fail_politely() {
        let s = testkit::state().await;
        for c in ["snapshot", "conversations", "export", "trace", "history"] {
            let r = cmd(&s, &other(), json!({"cmd": c})).await;
            assert!(r["error"].as_str().unwrap().contains("no business yet"), "{c}: {r}");
        }
        assert_eq!(cmd(&s, &other(), json!({"cmd": "conversation"})).await["error"], "peer required");
        assert_eq!(cmd(&s, &other(), json!({"cmd": "chat", "text": "  "})).await["error"], "empty message");
        assert_eq!(cmd(&s, &other(), json!({"cmd": "answer_gap"})).await["error"], "gapId and answer required");
        let p = cmd(&s, &other(), json!({"cmd": "ping"})).await;
        assert_eq!(p["ok"], true);
        assert!(p["tookMs"].is_u64());
        assert!(cmd(&s, &other(), json!({"cmd": "kernel"})).await.get("error").is_none());
    }

    #[tokio::test]
    async fn apply_merges_values_and_guards_protected_keys() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let r = cmd(&s, &other(), json!({"cmd": "apply", "patch": {
            "values": {"agentName": "Tita", "onboarded": true, "bundle": "evil@9", "disabledTools": ["x"]},
            "disabledTools": ["book_appointment", "set_capability", "report_gap"],
        }})).await;
        assert_eq!(r["ok"], true, "{r}");
        let (cfg, onboarded, bundle): (Value, bool, String) = sqlx::query_as("SELECT schema_config, onboarded, bundle FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_eq!(cfg["agentName"], "Tita");
        assert!(cfg.get("onboarded").is_none() && cfg.get("bundle").is_none());
        assert_eq!(cfg["disabledTools"], json!(["book_appointment", "report_gap"]), "the owner's own controls cannot be switched off");
        assert!(!onboarded);
        assert_eq!(bundle, "generic@1");
        // Remove paths, never the protected ones.
        let r = cmd(&s, &other(), json!({"cmd": "apply", "patch": {"remove": ["agentName", "onboarded", "nope.deep"]}})).await;
        assert_eq!(r["changed"], json!(["removed agentName"]));
        assert!(cmd(&s, &other(), json!({"cmd": "apply", "patch": {}})).await["error"].as_str().unwrap().contains("nothing to apply"));
    }

    #[test]
    fn remove_path_walks_dots() {
        let mut v = json!({"a": {"b": {"c": 1, "d": 2}}, "e": 3});
        assert!(remove_path(&mut v, "a.b.c"));
        assert!(!remove_path(&mut v, "a.b.c"));
        assert!(!remove_path(&mut v, "x.y"));
        assert!(!remove_path(&mut v, ""));
        assert!(remove_path(&mut v, "e"));
        assert_eq!(v, json!({"a": {"b": {"d": 2}}}));
    }

    #[test]
    fn parse_keeps_non_json_as_text() {
        assert_eq!(parse("{\"a\":1}"), json!({"a": 1}));
        assert_eq!(parse("plain"), json!("plain"));
    }

    #[tokio::test]
    async fn read_commands_over_a_business() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        crate::routes::append_message(&s, b, "onboarding", "+51999000111", "user", "hola").await.unwrap();
        crate::routes::append_message(&s, b, "onboarding", "+51999000111", "assistant", "¡hola Tito!").await.unwrap();
        let h = cmd(&s, &other(), json!({"cmd": "history", "limit": 1})).await;
        assert_eq!(h["messages"], json!([{"role": "assistant", "text": "¡hola Tito!", "at": h["messages"][0]["at"]}]));
        let e = cmd(&s, &other(), json!({"cmd": "export"})).await;
        assert_eq!(e["format"], "agente-harness/1");
        assert_eq!(e["business"]["name"], "Tito");
        assert_eq!(e["agent"]["id"], json!(s.identity.id()));
        let t = cmd(&s, &other(), json!({"cmd": "trace"})).await;
        assert_eq!(t["events"], json!([]));
        let snap = cmd(&s, &other(), json!({"cmd": "snapshot"})).await;
        assert!(snap.get("error").is_none(), "{snap}");
        assert_eq!(snap["business"]["name"], "Tito");
        assert_eq!(snap["totals"]["messages"], 0);
        let c = cmd(&s, &other(), json!({"cmd": "conversations"})).await;
        assert!(c.get("error").is_none(), "{c}");
    }

    #[tokio::test]
    async fn chat_runs_the_manager_agent() {
        let m = Mock::start().await;
        m.say("Listo, anotado.");
        let s = testkit::state_on(&m).await;
        testkit::business(&s.db).await;
        let r = cmd(&s, &other(), json!({"cmd": "chat", "text": "abro a las 9"})).await;
        assert_eq!(r["text"], "Listo, anotado.", "{r}");
        let h = cmd(&s, &other(), json!({"cmd": "history"})).await;
        assert_eq!(h["messages"].as_array().unwrap().len(), 2);
    }
}
