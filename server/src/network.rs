//! The agent on the network. Every installation carries an identity
//! (`identity.rs`); this module turns it into presence: a signed card
//! (NANDA-style AgentFacts) published to the yaya.tech registry so other
//! agents — and people — can discover this business and, later, transact
//! with it. Publishing is best-effort and never blocks a customer turn.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::AppState;

pub const DEFAULT_REGISTRY_URL: &str = "https://llm.yaya.tech";

/// The registry / relay / gateway host, spoken to as this identity. Every
/// request carries the bearer and a request signature (yaya-wire reqsig),
/// so the other end can prove who is calling.
#[derive(Clone)]
pub struct Registry {
    base: String,
    identity: crate::identity::Identity,
}

impl Registry {
    pub fn new(base: impl Into<String>, identity: crate::identity::Identity) -> Self {
        Self { base: base.into().trim_end_matches('/').to_string(), identity }
    }

    pub fn from_env(identity: crate::identity::Identity) -> Self {
        Self::new(registry_url(), identity)
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn identity(&self) -> &crate::identity::Identity {
        &self.identity
    }

    fn signed(&self, req: reqwest::RequestBuilder, method: &str, path: &str, body: Option<&[u8]>) -> reqwest::RequestBuilder {
        crate::identity::signed(req, &self.identity, method, &crate::upstream::server_path(&self.base, path), body)
    }

    /// GET; `path` includes any query string. Non-2xx becomes an error
    /// carrying the server's message.
    pub async fn get(&self, path: &str, timeout: Duration) -> anyhow::Result<Value> {
        let req = crate::net::client(timeout).get(format!("{}{path}", self.base));
        Self::finish(self.signed(req, "GET", path, None).send().await?).await
    }

    /// JSON POST.
    pub async fn post(&self, path: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
        let bytes = serde_json::to_vec(body)?;
        let req = crate::net::client(timeout)
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(bytes.clone());
        Self::finish(self.signed(req, "POST", path, Some(&bytes)).send().await?).await
    }

    /// PUT raw bytes with extra headers (backups). Signed over the body.
    pub async fn put_bytes(&self, path: &str, body: Vec<u8>, headers: &[(&str, String)], timeout: Duration) -> anyhow::Result<Value> {
        let mut req = crate::net::client(timeout)
            .put(format!("{}{path}", self.base))
            .header("content-type", "application/octet-stream")
            .body(body.clone());
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        Self::finish(self.signed(req, "PUT", path, Some(&body)).send().await?).await
    }

    /// GET raw bytes; returns the body and the response headers.
    pub async fn get_bytes(&self, path: &str, timeout: Duration) -> anyhow::Result<(Vec<u8>, reqwest::header::HeaderMap)> {
        let req = crate::net::client(timeout).get(format!("{}{path}", self.base));
        let r = self.signed(req, "GET", path, None).send().await?;
        let status = r.status();
        let headers = r.headers().clone();
        if !status.is_success() {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            let msg = v["error"]["message"].as_str().unwrap_or(status.as_str()).to_string();
            anyhow::bail!("registry {status}: {msg}");
        }
        Ok((r.bytes().await?.to_vec(), headers))
    }

    /// JSON POST as a Yaya session (account endpoints), not as the agent.
    pub async fn post_as_session(&self, path: &str, session: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
        let r = crate::net::client(timeout)
            .post(format!("{}{path}", self.base))
            .bearer_auth(session)
            .json(body)
            .send()
            .await?;
        Self::finish(r).await
    }

    /// GET as a Yaya session (account endpoints), not as the agent.
    pub async fn get_as_session(&self, path: &str, session: &str, timeout: Duration) -> anyhow::Result<Value> {
        let r = crate::net::client(timeout)
            .get(format!("{}{path}", self.base))
            .bearer_auth(session)
            .send()
            .await?;
        Self::finish(r).await
    }

    async fn finish(r: reqwest::Response) -> anyhow::Result<Value> {
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = v["error"]["message"].as_str().or_else(|| v["error"].as_str()).unwrap_or(status.as_str()).to_string();
            anyhow::bail!("registry {status}: {msg}");
        }
        Ok(v)
    }
}

/// Throttle state: publish at most every 6 hours unless forced.
pub struct Publisher {
    last: Mutex<Option<Instant>>,
}

impl Default for Publisher {
    fn default() -> Self {
        Self { last: Mutex::new(None) }
    }
}

pub fn registry_url() -> String {
    std::env::var("REGISTRY_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY_URL.into())
        .trim_end_matches('/')
        .to_string()
}

/// The unsigned card: what this agent says about itself. Only the business
/// name/industry/country go public, and only once the owner finished
/// onboarding; the owner's phone never does.
pub async fn card(state: &AppState) -> Value {
    let row: Option<(String, String, String, bool, String)> = sqlx::query_as(
        "SELECT name, industry, country, onboarded, bundle FROM businesses ORDER BY created_at ASC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let caps = crate::harness::spec_names(&crate::harness::tool_specs(
        &state.kernel,
        crate::harness::Scope::Customer,
        None,
        &crate::harness::ToolCaps { booking_exists: true, seller: false },
    ));
    let (name, industry, country, onboarded, bundle) = match row {
        Some((n, i, c, o, b)) if o => (Some(n), Some(i), Some(c), true, Some(b)),
        Some((_, _, c, _, b)) => (None, None, Some(c), false, Some(b)),
        None => (None, None, None, false, None),
    };
    // Skills = the customer-facing tools this agent actually mounts, with
    // their own descriptions — the same truth the capability-honesty prompt
    // section is built from, so the card can't overclaim.
    let specs = crate::harness::tool_specs(
        &state.kernel,
        crate::harness::Scope::Customer,
        None,
        &crate::harness::ToolCaps { booking_exists: true, seller: false },
    );
    let skills: Vec<Value> = specs
        .as_array()
        .map(|a| a.iter().filter_map(|t| {
            let f = &t["function"];
            let id = f["name"].as_str()?;
            let desc: String = f["description"].as_str().unwrap_or("").chars().take(160).collect();
            Some(json!({"id": id, "description": desc}))
        }).collect())
        .unwrap_or_default();
    // Locale + a short public description from the composed values (only
    // what a customer would be told anyway), plus the OFFER: everything a
    // client agent needs to choose, quote and book — services and prices,
    // products, hours, the next free slots, payment rails, deposit policy,
    // delivery zones, location. All of it is what the business tells any
    // customer who asks; the owner's phone and customers never appear.
    let (locale, description, offer) = match business_id(state).await {
        Some(bid) => match crate::learning::compose(&state.db, &state.schemas_dir, bid).await {
            Ok(c) => {
                let v = &c.values;
                let loc = crate::locale::Locale::from_values(v);
                let kind = v["businessKind"].as_str().unwrap_or("services");
                let n_services = v["pricing"].as_object().map(|m| m.len()).unwrap_or(0);
                let n_products = v["products"].as_object().map(|m| m.len()).unwrap_or(0);
                let desc = match (name.as_deref(), industry.as_deref()) {
                    (Some(n), Some(i)) => format!(
                        "{n} — {i} ({kind}); {n_services} services, {n_products} products; \
                         books appointments, takes orders, confirms payments via {}",
                        loc.rails
                    ),
                    _ => "agente business agent (not yet onboarded)".to_string(),
                };
                let offer = if onboarded { offer_of(state, bid, v, &loc).await } else { Value::Null };
                (json!({"language": loc.language, "currency": loc.currency, "timezone": v["timezone"]}), desc, offer)
            }
            Err(_) => (Value::Null, String::new(), Value::Null),
        },
        None => (Value::Null, String::new(), Value::Null),
    };
    let x25519 = hex::encode(state.identity.public_key().to_montgomery().to_bytes());
    // Measured boot: the device key's attestation chain, when the shell has
    // provided one for THIS agent id. The registry verifies it; we carry it.
    let device: Value = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT chain, level FROM device_attestation WHERE id = 1 AND agent = $1",
    )
    .bind(state.identity.id())
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .and_then(|(chain, level)| serde_json::from_str::<Value>(&chain).ok().map(|c| json!({
        "attestation": {"format": "android-key-attestation", "chain": c, "level": level,
                         "challenge": "sha256(agente-attest:v1:<agent id>)"}
    })))
    .unwrap_or(Value::Null);
    json!({
        "device": device,
        "type": "AgentFacts",
        "version": "0.3",
        "id": state.identity.id(),
        "did": state.identity.did(),
        "name": name,
        "description": description,
        "industry": industry,
        "country": country,
        "locale": locale,
        "offer": offer,
        "bundle": bundle,
        "onboarded": onboarded,
        "capabilities": caps,
        "skills": skills,
        "protocols": ["agente/v1", "yaya-relay/v1"],
        "e2e": {"alg": crate::e2e::ALG, "x25519": x25519},
        "software": {"name": "agente", "version": env!("CARGO_PKG_VERSION"), "runtime": "on-device"},
        "publishedAt": chrono::Utc::now().to_rfc3339(),
    })
}

/// The public offer. Products are capped so a 400-item bodega doesn't
/// bloat the registry; the agent itself answers the long tail.
async fn offer_of(state: &AppState, bid: uuid::Uuid, v: &Value, loc: &crate::locale::Locale) -> Value {
    let cap_map = |m: &Value, n: usize| -> Value {
        m.as_object()
            .map(|o| Value::Object(o.iter().take(n).map(|(k, v)| (k.clone(), v.clone())).collect()))
            .unwrap_or(Value::Null)
    };
    let slots = crate::plugins::scheduling::next_free_slots(&state.db, bid, v, 7, 8).await;
    let disabled: Vec<&str> = v["disabledTools"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    json!({
        "kind": v["businessKind"],
        "location": v["location"],
        "region": v["region"],
        "geo": v["geo"].as_object().map(|g| json!({"lat": g.get("lat"), "lng": g.get("lng")})).unwrap_or(Value::Null),
        "city": v["city"],
        "district": v["district"],
        "address": v["address"],
        "timezone": v["timezone"],
        "hours": v["businessHours"],
        "services": cap_map(&v["pricing"], 40),
        "products": cap_map(&v["products"], 40),
        "productCount": v["products"].as_object().map(|m| m.len()).unwrap_or(0),
        "currency": loc.currency,
        "paymentRails": loc.rails,
        "paymentMethod": v["paymentMethod"],
        "bookingDeposit": v["bookingDeposit"],
        // Paid consultations: what another agent pays to ask this one (céntimos; 0 = free).
        "askPrice": ask_price_value(v),
        "askCurrency": loc.currency,
        "cancellationNoticeMins": v["cancellationNoticeMins"],
        "slotDuration": v["slotDuration"],
        "maxAdvanceBookingDays": v["maxAdvanceBookingDays"],
        "delivery": v["delivery"],
        "walkInsAllowed": v["walkInsAllowed"],
        "booking": !disabled.contains(&"book_appointment"),
        "orders": !disabled.contains(&"create_order"),
        "nextSlots": slots,
        "slotsComputedAt": chrono::Utc::now().to_rfc3339(),
    })
}

/// `values.askPrice` as céntimos, tolerant of how it was typed. 0 = free.
pub fn ask_price_value(v: &Value) -> i64 {
    let p = &v["askPrice"];
    p.as_i64()
        .or_else(|| p.as_f64().map(|f| f.round() as i64))
        .or_else(|| p.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f.round() as i64))
        .unwrap_or(0)
        .clamp(0, 50_000)
}

/// This business's consultation price, from the composed values.
pub async fn ask_price_of(state: &AppState) -> i64 {
    match business_id(state).await {
        Some(bid) => crate::learning::compose(&state.db, &state.schemas_dir, bid).await.map(|c| ask_price_value(&c.values)).unwrap_or(0),
        None => 0,
    }
}

/// Pulls what this account bought on the market and mounts the packs
/// (bundle-shaped docs) into `mounted_bundles`; the composer does the rest.
/// Returns how many packs are mounted.
pub async fn sync_purchases(state: &AppState) -> usize {
    let v = match registry_get(state, "/v1/purchases").await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "purchases not fetched");
            return 0;
        }
    };
    let mut n = 0;
    for p in v["purchases"].as_array().into_iter().flatten() {
        if p["kind"].as_str() != Some("bundle") || !p["content"].is_object() {
            continue;
        }
        let (Some(id), Some(title)) = (p["id"].as_str(), p["title"].as_str()) else { continue };
        let _ = sqlx::query(
            "INSERT INTO mounted_bundles (listing, title, kind, doc) VALUES ($1, $2, 'bundle', $3) \
             ON CONFLICT (listing) DO UPDATE SET title = excluded.title, doc = excluded.doc",
        )
        .bind(id).bind(title).bind(p["content"].to_string())
        .execute(&state.db).await;
        n += 1;
    }
    if n > 0 {
        tracing::info!(packs = n, "market packs mounted");
    }
    n
}

/// Re-publish when the card is older than `max_age` — called after a booking
/// or cancellation so the advertised free slots stay honest without a
/// network round-trip per turn.
pub async fn publish_if_stale(state: &AppState, max_age: Duration) {
    let stale = {
        let last = state.publisher.last.lock().unwrap_or_else(|e| e.into_inner());
        last.map_or(true, |t| t.elapsed() > max_age)
    };
    if stale {
        publish(state, true).await;
    }
}

/// GET on the registry, JSON back. 20s budget: these calls sit inside tool
/// calls inside an LLM turn.
pub async fn registry_get(state: &AppState, path: &str) -> anyhow::Result<Value> {
    state.registry.get(path, Duration::from_secs(20)).await
}

/// Pulls `/v1/me` and applies the plan to the local row: the gateway owns
/// entitlements, the core enforces them offline. Best-effort.
pub async fn sync_plan(state: &AppState) -> Option<Value> {
    let mut me = registry_get(state, "/v1/me").await.ok()?;
    let mut plan = me["plan"].as_str().unwrap_or("free").to_string();
    let mut msgs = me["caps"]["messagesPerDay"].as_i64();
    let mut custs = me["caps"]["customersPerDay"].as_i64();
    let mut conv = me["caps"]["conversationsPerMonth"].as_i64();
    // A self-hosted core with its own model key is not metered by the
    // gateway; while it is not linked to an account the gateway's "free/
    // expired" says nothing about it. Unlimited locally, honestly labelled.
    if !state.llm.is_gateway() && me["account"].is_null() && matches!(plan.as_str(), "free" | "expired") {
        plan = "self-hosted".into();
        msgs = Some(0);
        custs = Some(0);
        conv = Some(0);
        me["plan"] = json!("self-hosted");
        me["state"] = json!("self-hosted");
    }
    // Every row this core serves (one business, or the person's self row).
    let _ = sqlx::query("UPDATE businesses SET plan = $1, msg_cap = $2, customer_cap = $3, conv_cap = $4")
        .bind(&plan).bind(msgs).bind(custs).bind(conv)
        .execute(&state.db).await;
    state.seller.store(me["seller"].as_bool().unwrap_or(false), std::sync::atomic::Ordering::Relaxed);
    *state.plan_info.lock().unwrap_or_else(|e| e.into_inner()) = me.clone();
    tracing::info!(%plan, msgs = ?msgs, customers = ?custs, conversations = ?conv, seller = me["seller"].as_bool().unwrap_or(false), "plan synced from gateway");
    // Prepaid credits ride along: push pending outcomes, refresh the summary.
    let _ = crate::outcomes::summary(state).await;
    Some(me)
}

/// The niche this business belongs to (`industry-COUNTRY`), if onboarded.
pub async fn niche_of(state: &AppState) -> Option<String> {
    let (industry, country): (String, String) = sqlx::query_as("SELECT industry, country FROM businesses ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(&state.db).await.ok().flatten()?;
    Some(crate::niche::key(&industry, &country))
}

/// Pulls the niche skill the network curated for this business's industry
/// and country. Absent skill = empty section; failures keep the last one.
pub async fn sync_skill(state: &AppState) {
    let Some(niche) = niche_of(state).await else { return };
    match state.registry.get(&format!("/v1/skills/{niche}"), Duration::from_secs(20)).await {
        Ok(v) => {
            let skill = v["skill"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(6000).collect::<String>());
            tracing::info!(%niche, present = skill.is_some(), version = ?v["version"], "niche skill synced");
            *state.niche_skill.lock().unwrap_or_else(|e| e.into_inner()) = skill;
        }
        Err(e) => tracing::debug!(%niche, error = %e, "niche skill not fetched"),
    }
}

/// POST JSON to the registry as this agent.
pub async fn registry_post(state: &AppState, path: &str, body: &Value) -> anyhow::Result<Value> {
    state.registry.post(path, body, Duration::from_secs(30)).await
}

/// Reputation of any agent on the network (business or customer).
pub async fn reputation(state: &AppState, agent: &str) -> anyhow::Result<Value> {
    registry_get(state, &format!("/v1/agents/{agent}/reputation")).await
}

/// One line about a network peer for the customer prompt, cached 1h. Empty
/// when the registry is unreachable — the business still serves them.
pub async fn peer_note(state: &AppState, peer: &str) -> Option<String> {
    // A WhatsApp-node chat: the campaign that opened it, the live call.
    if peer.starts_with("wa:") {
        return crate::node::campaign_note(state, peer).await;
    }
    if !peer.starts_with("agent:") {
        return None;
    }
    {
        let notes = state.peer_notes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, n)) = notes.get(peer) {
            if t.elapsed() < Duration::from_secs(3600) {
                return Some(n.clone());
            }
        }
    }
    let rep = reputation(state, peer).await.ok()?;
    let n = rep["reviews"].as_i64().unwrap_or(0);
    let tags: Vec<String> = rep["tags"].as_object()
        .map(|m| m.iter().map(|(k, v)| format!("{k}×{v}")).collect())
        .unwrap_or_default();
    let note = if n == 0 {
        format!(
            "NETWORK CUSTOMER: this customer is another agent on the yaya network ({} prior interactions, \
             no reviews yet{}). Treat them like any new walk-in.",
            rep["interactions"].as_i64().unwrap_or(0),
            if rep["hardwareVerified"].as_bool() == Some(true) { ", hardware-attested phone" } else { "" }
        )
    } else {
        format!(
            "NETWORK CUSTOMER REPUTATION: {} ★ from {} businesses{}; flags: {}. A pattern of no_show / \
             cancelled_late justifies asking for the deposit (if the business has one) before holding a slot; \
             never refuse service or mention the score itself.",
            rep["rating"], n,
            if rep["hardwareVerified"].as_bool() == Some(true) { " (hardware-attested phone)" } else { "" },
            if tags.is_empty() { "none".to_string() } else { tags.join(", ") }
        )
    };
    tracing::info!(%peer, reviews = n, rating = %rep["rating"], "network customer reputation fetched");
    state.peer_notes.lock().unwrap_or_else(|e| e.into_inner())
        .insert(peer.to_string(), (Instant::now(), note.clone()));
    Some(note)
}

/// Signed review about `about` (business → customer or customer → business).
pub async fn post_review(state: &AppState, about: &str, stars: i64, comment: &str, tags: &[String]) -> anyhow::Result<Value> {
    let env = state.identity.envelope(json!({
        "about": about, "stars": stars, "comment": comment, "tags": tags,
        "at": chrono::Utc::now().to_rfc3339(),
    }));
    state.registry.post(&format!("/v1/agents/{about}/reviews"), &env, Duration::from_secs(20)).await
}

/// Ranked businesses for a need.
pub async fn find(state: &AppState, q: &str, country: Option<&str>, city: Option<&str>, industry: Option<&str>, geo: Option<(f64, f64)>, limit: usize) -> anyhow::Result<Value> {
    state.registry.post("/v1/match", &json!({"q": q, "country": country, "city": city, "industry": industry, "limit": limit,
                      "lat": geo.map(|g| g.0), "lng": geo.map(|g| g.1)}), Duration::from_secs(20)).await
}

/// Sealed boxes waiting for us on the relay (long-poll up to `wait` s).
pub async fn poll_inbox(state: &AppState, wait: u64) -> anyhow::Result<Vec<(String, String, Value)>> {
    let me = state.identity.id();
    let sk = state.identity.x25519_secret();
    let body = state.registry.get(&format!("/v1/inbox?wait={}", wait.min(30)), Duration::from_secs(wait + 15)).await?;
    let mut out = Vec::new();
    for m in body["messages"].as_array().cloned().unwrap_or_default() {
        let from = m["from"].as_str().unwrap_or("").to_string();
        let id = m["id"].as_str().unwrap_or("").to_string();
        if m["box"]["from"].as_str() != Some(from.as_str()) {
            tracing::warn!(%from, "relay message whose label and seal disagree dropped");
            continue;
        }
        match crate::e2e::open(&sk, &me, &m["box"]) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                let v = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({"text": text}));
                out.push((id, from, v));
            }
            Err(e) => tracing::warn!(%from, error = %e, "undecryptable inbox message dropped"),
        }
    }
    Ok(out)
}

/// The (single) business this installation serves.
pub async fn business_id(state: &AppState) -> Option<uuid::Uuid> {
    sqlx::query_as::<_, (uuid::Uuid,)>("SELECT id FROM businesses ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| r.0)
}

/// Owner opt-out: `networkPublish: false` in the business values (set by
/// talking to the manager agent) keeps this agent off the registry.
pub async fn publish_allowed(state: &AppState) -> bool {
    match business_id(state).await {
        Some(bid) => crate::learning::compose(&state.db, &state.schemas_dir, bid)
            .await
            .map(|c| c.values["networkPublish"].as_bool().unwrap_or(true))
            .unwrap_or(true),
        None => true,
    }
}

/// Signed envelope ready to publish.
pub async fn signed_card(state: &AppState) -> Value {
    let payload = card(state).await;
    state.identity.envelope(payload)
}

/// Publishes the card if the throttle allows (or `force`). Errors are logged,
/// never surfaced: the network is a bonus, the business runs without it.
pub async fn publish(state: &AppState, force: bool) -> Option<Value> {
    if !publish_allowed(state).await {
        return None;
    }
    {
        let mut last = state.publisher.last.lock().unwrap_or_else(|e| e.into_inner());
        let due = force || last.map_or(true, |t| t.elapsed() > Duration::from_secs(6 * 3600));
        if !due {
            return None;
        }
        *last = Some(Instant::now());
    }
    let env = signed_card(state).await;
    match state.registry.post("/v1/agents", &env, Duration::from_secs(20)).await {
        Ok(body) => {
            tracing::info!(agent = %state.identity.id(), registry = %state.registry.base(), "agent card published");
            Some(body)
        }
        Err(e) => {
            tracing::warn!(error = %e, "card not published");
            None
        }
    }
}


// ------------------------------------------------------------------ inbox
//
// The other half of presence: being reachable. Other agents (a customer's
// personal agent, another business) drop sealed boxes in this agent's
// inbox on the registry; we long-poll it, decrypt, run the SAME customer
// turn a WhatsApp message would, and seal the reply back to the sender.
// The relay never sees plaintext.

/// Runs forever. Backs off on errors, keeps going without the network. An
/// owner who opted out of the network (`networkPublish: false`) is not
/// reachable either: opting out means out.
/// Client mode: who is waiting for which business to answer. The inbox loop
/// owns the relay inbox, so `ask_business` cannot poll it itself; it
/// registers here before sending and the loop hands the reply over. A reply
/// nobody waits for (the tool timed out, the app was closed) is kept as the
/// thread's unread text so `my_bookings` can surface it later.
#[derive(Default)]
pub struct Asks {
    live: std::sync::atomic::AtomicBool,
    waiting: std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<Value>>>,
}

impl Asks {
    /// The inbox loop is running: replies come through [`Asks::deliver`].
    pub fn set_live(&self) {
        self.live.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn is_live(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Start waiting for `business`. Replaces an older waiter for the same
    /// business (its receiver simply closes).
    pub fn register(&self, business: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).insert(business.to_string(), tx);
        rx
    }
    pub fn unregister(&self, business: &str) {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(business);
    }
    /// Hands `msg` to whoever waits on `from`. `false` when nobody does.
    pub fn deliver(&self, from: &str, msg: Value) -> bool {
        let mut w = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        match w.get(from) {
            Some(tx) if tx.send(msg).is_ok() => true,
            Some(_) => {
                w.remove(from);
                false
            }
            None => false,
        }
    }
}

pub async fn inbox_loop(state: crate::SharedState) {
    let me = state.identity.id();
    let mut backoff = 2u64;
    tracing::info!(agent = %me, client_mode = state.client_mode, "relay inbox loop started");
    loop {
        // Customers reach this agent only while it is on the network; the
        // owner's own console reaches it always (opting out of the network
        // must not lock the owner out of their own phone). A personal
        // orchestrator has no customers at all.
        let open_to_customers = !state.client_mode && publish_allowed(&state).await;
        let body: Value = match state.registry.get("/v1/inbox?wait=25", Duration::from_secs(40)).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "inbox poll failed");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(300);
                continue;
            }
        };
        backoff = 2;
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        for m in msgs {
            relay_message(&state, &m, open_to_customers).await;
        }
    }
}

/// One message from the relay inbox. The sender is the one the sealed box
/// names — the key agreement binds it — and the relay's own `from` label
/// must agree: a courier that can read nothing must not be able to decide
/// who wrote (the owner's console gets owner scope on that name alone).
pub async fn relay_message(state: &crate::SharedState, m: &Value, open_to_customers: bool) {
    let me = state.identity.id();
    let sk = state.identity.x25519_secret();
    let from = m["from"].as_str().unwrap_or("").to_string();
    if m["box"]["from"].as_str() != Some(from.as_str()) {
        tracing::warn!(%from, sealed_by = ?m["box"]["from"].as_str(), "relay message whose label and seal disagree dropped");
        return;
    }
    let alg = crate::e2e::alg_of(&m["box"]).unwrap_or(crate::e2e::ALG);
    let text = match crate::e2e::open(&sk, &me, &m["box"]) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(e) => {
            tracing::warn!(%from, error = %e, "undecryptable inbox message dropped");
            return;
        }
    };
    let parsed = serde_json::from_str::<Value>(&text).ok();
    // The owner's console: a command for the manager agent, answered
    // only when the sender is a device on this phone's own account.
    if parsed.as_ref().is_some_and(|v| v["scope"].as_str() == Some("owner")) {
        let req = parsed.unwrap();
        if !crate::owner::may_handle(&state, &from, &req).await {
            tracing::warn!(%from, "owner-scope message from a device not paired to this core dropped");
            return;
        }
        let reply = crate::owner::handle(&state, &from, &req, &m["id"]).await;
        if let Err(e) = send_with(&state, &from, &reply, alg).await {
            tracing::warn!(%from, error = %e, "owner reply not sent");
        }
        return;
    }
    // Agent-to-agent protocol messages never run a customer turn: a coins
    // payment or a mesh offer is data for the wallet and the mesh, and
    // feeding it to the model would both lose it and show it coin
    // signatures and keys. (Dropped when the network was parked; the
    // routes that send them came back with the one-app merge.)
    if let Some(v) = parsed.as_ref() {
        if crate::mesh::handle_inbox(state, &from, v, &m["id"]).await {
            return;
        }
        if crate::coins::handle_inbox(state, &from, v).await {
            return;
        }
    }
    // A personal orchestrator is nobody's storefront: the only
    // stranger traffic it accepts is a business answering something
    // it asked (`{text, inReplyTo, action, actionData, business}`).
    // Anything else is dropped silently — answering would let two
    // assistants talk each other into a relay-quota hole.
    if state.client_mode {
        match parsed {
            Some(v) if v.get("inReplyTo").is_some() || v["business"].is_object() => {
                if !state.asks.deliver(&from, v.clone()) {
                    crate::plugins::market::late_reply(&state, &from, &v).await;
                }
            }
            _ => tracing::debug!(%from, "unsolicited network message to a personal agent dropped"),
        }
        return;
    }
    if !open_to_customers {
        tracing::debug!(%from, "customer relay message dropped: network presence is off");
        return;
    }
    // Paid consultations: when this agent prices its answers, a
    // question that did not carry the price (the gateway marks what
    // was paid) is answered with the price, not with the answer.
    let ask_price = ask_price_of(&state).await;
    if ask_price > 0 && m["paid"].as_i64().unwrap_or(0) < ask_price && !crate::owner::is_owner_device(&state, &from).await {
        let loc = match business_id(&state).await {
            Some(bid) => crate::learning::compose(&state.db, &state.schemas_dir, bid).await.map(|c| crate::locale::Locale::from_values(&c.values)).ok(),
            None => None,
        };
        let (money, cur) = match &loc {
            Some(l) => (l.money(ask_price as f64 / 100.0), l.currency.clone()),
            None => (format!("S/ {:.2}", ask_price as f64 / 100.0), "PEN".to_string()),
        };
        let _ = send(&state, &from, &json!({
            "text": format!("Esta consulta tiene un costo de {money}. Tu agente puede pagarla con tu saldo Yaya y te respondo al instante."),
            "inReplyTo": m["id"], "action": "payment_required",
            "actionData": {"priceMinor": ask_price, "currency": cur},
            "business": {"id": state.identity.id()},
        })).await;
        return;
    }
    // Agent-to-agent messages carry a tiny JSON: {"text": "..."} so
    // future fields (offers, receipts) have a home. Plain text works too.
    let text = parsed
        .and_then(|v| v["text"].as_str().map(String::from))
        .unwrap_or(text);
    let Some(bid) = business_id(&state).await else { return };
    // Who is this? Their reputation rides into the prompt (cached).
    peer_note(&state, &from).await;
    let turn = match crate::routes::customer_turn(&state, bid, &from, &text).await {
        Ok(v) => v,
        Err((code, body)) => {
            tracing::warn!(%from, %code, body = %body.0, "customer turn failed for relay message");
            return;
        }
    };
    let reply = turn["agentResponse"].as_str().unwrap_or("").to_string();
    if reply.is_empty() {
        return;
    }
    // The structured outcome travels with the text so the customer's
    // agent can record a booking/order without parsing prose. It is
    // the same {action, actionData} the owner's own app sees.
    let _ = send(&state, &from, &json!({
        "text": reply, "inReplyTo": m["id"],
        "action": turn["action"], "actionData": turn["actionData"],
        "business": {"id": state.identity.id()},
    })).await;
}

/// Seal `payload` to `to` and drop it in their inbox on the registry.
pub async fn send(state: &AppState, to: &str, payload: &Value) -> anyhow::Result<String> {
    send_with(state, to, payload, crate::e2e::ALG).await
}

/// Same, with the cipher the other side speaks (a browser: AES-GCM).
pub async fn send_with(state: &AppState, to: &str, payload: &Value, alg: &str) -> anyhow::Result<String> {
    send_boxed(state, to, payload, alg, 0).await
}

/// A paid consultation: the box declares `pay.amountMinor` (inside the
/// signed envelope, outside the ciphertext) and the gateway settles it from
/// this agent's account before delivering.
pub async fn send_paid(state: &AppState, to: &str, payload: &Value, pay_minor: i64) -> anyhow::Result<String> {
    send_boxed(state, to, payload, crate::e2e::ALG, pay_minor).await
}

async fn send_boxed(state: &AppState, to: &str, payload: &Value, alg: &str, pay_minor: i64) -> anyhow::Result<String> {
    let me = state.identity.id();
    let mut boxed = crate::e2e::seal_with(&state.identity.x25519_secret(), &me, to, payload.to_string().as_bytes(), alg)?;
    if pay_minor > 0 {
        boxed["pay"] = json!({"amountMinor": pay_minor});
    }
    let env = state.identity.envelope(boxed);
    let v = state.registry.post(&format!("/v1/agents/{to}/inbox"), &env, Duration::from_secs(20)).await?;
    Ok(v["id"].as_str().unwrap_or("").to_string())
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use crate::testkit::{self, Mock};

    /// What the relay would deliver: `payload` sealed by `from` to `to`.
    fn delivery(from: &crate::identity::Identity, to: &str, payload: &Value) -> Value {
        let boxed = crate::e2e::seal(&from.x25519_secret(), &from.id(), to, payload.to_string().as_bytes()).unwrap();
        json!({"id": "m1", "from": from.id(), "box": boxed})
    }

    #[tokio::test]
    async fn a_relay_label_that_disagrees_with_the_seal_is_dropped() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await;
        let before = m.seen_path("/chat/completions").len();
        let real = crate::identity::Identity::ephemeral();
        let mut d = delivery(&real, &s.identity.id(), &json!({"text": "hola"}));
        d["from"] = json!(crate::identity::Identity::ephemeral().id());
        relay_message(&s, &d, true).await;
        assert_eq!(m.seen_path("/chat/completions").len(), before, "no turn for a relabelled message");
        // Owner scope cannot be borrowed by relabelling either.
        let owner_dev = crate::identity::Identity::ephemeral();
        let code = crate::owner::start_pairing(&s)["code"].as_str().unwrap().to_string();
        relay_message(&s, &delivery(&owner_dev, &s.identity.id(), &json!({"scope": "owner", "cmd": "pair", "code": code})), true).await;
        assert!(crate::owner::is_owner_device(&s, &owner_dev.id()).await);
        let relay = crate::identity::Identity::ephemeral();
        let mut forged = delivery(&relay, &s.identity.id(), &json!({"scope": "owner", "cmd": "apply", "patch": {"values": {"agentName": "pwned"}}}));
        forged["from"] = json!(owner_dev.id());
        relay_message(&s, &forged, true).await;
        let (cfg,): (Value,) = sqlx::query_as("SELECT schema_config FROM businesses").fetch_one(&s.db).await.unwrap();
        assert!(cfg.get("agentName").is_none(), "{cfg}");
    }

    #[tokio::test]
    async fn a_customer_message_over_the_relay_is_answered_sealed() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"})).on("/inbox", json!({"id": "r"})).on("/reputation", json!({"reviews": 0, "interactions": 2}));
        m.say("¡Hola vecino!");
        let customer = crate::identity::Identity::ephemeral();
        relay_message(&s, &delivery(&customer, &s.identity.id(), &json!({"text": "¿abren hoy?"})), true).await;
        let sent = m.seen_path(&format!("/v1/agents/{}/inbox", customer.id()));
        assert_eq!(sent.len(), 1);
        let opened = crate::e2e::open(&customer.x25519_secret(), &customer.id(), &sent[0].body["payload"]).unwrap();
        let reply: Value = serde_json::from_slice(&opened).unwrap();
        assert_eq!((reply["text"].clone(), reply["inReplyTo"].clone()), (json!("¡Hola vecino!"), json!("m1")));
        // Off the network: customers are not answered.
        m.say("no");
        relay_message(&s, &delivery(&customer, &s.identity.id(), &json!({"text": "hola?"})), false).await;
        assert_eq!(m.seen_path(&format!("/v1/agents/{}/inbox", customer.id())).len(), 1);
    }

    #[tokio::test]
    async fn protocol_messages_never_reach_the_model() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"})).on("/inbox", json!({"id": "r"}));
        let before = m.seen_path("/chat/completions").len();
        let peer = crate::identity::Identity::ephemeral();
        // A mesh offer over the relay is stored for the owner.
        let (ct, _) = crate::mesh::encapsulate(&s.mesh.keys.kem_public_b64()).unwrap();
        let offer = json!({"kind": "mesh_offer", "wg": crate::mesh::MeshKeys::generate().wg_public_b64(), "ct": ct, "ip": "10.77.8.8", "endpoints": [], "name": "Vecino"});
        relay_message(&s, &delivery(&peer, &s.identity.id(), &offer), true).await;
        assert_eq!(crate::mesh::status(&s).await["peers"][0]["status"], "pending");
        // A coins payment (even a bogus one) is handled by the wallet, which answers a receipt.
        relay_message(&s, &delivery(&peer, &s.identity.id(), &json!({"kind": "coins", "to": s.identity.id(), "nonce": "n", "coins": []})), true).await;
        let receipts = m.seen_path(&format!("/v1/agents/{}/inbox", peer.id()));
        assert_eq!(receipts.len(), 1, "the payer gets a receipt");
        assert_eq!(m.seen_path("/chat/completions").len(), before, "coin signatures and mesh keys are never shown to the model");
    }

    #[tokio::test]
    async fn whatsapp_node_peers_get_their_campaign_note() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        crate::node::create_campaign(&s, b, "Promo", "vender cortes", "Hola {name}", &[json!({"phone": "51977000111", "name": "Ana"})], 80, 20).await.unwrap();
        sqlx::query("UPDATE campaign_contacts SET status = 'sent', sent_at = '2026-01-01T00:00:00+00:00'").execute(&s.db).await.unwrap();
        let note = peer_note(&s, "wa:51977000111").await.unwrap();
        assert!(note.contains("OUTBOUND CONVERSATION") && note.contains("vender cortes"), "{note}");
        assert_eq!(peer_note(&s, "com.whatsapp:+51 977").await, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, Mock};

    #[test]
    fn ask_price_parsing() {
        assert_eq!(ask_price_value(&json!({})), 0);
        assert_eq!(ask_price_value(&json!({"askPrice": 150})), 150);
        assert_eq!(ask_price_value(&json!({"askPrice": 149.6})), 150);
        assert_eq!(ask_price_value(&json!({"askPrice": " 200 "})), 200);
        assert_eq!(ask_price_value(&json!({"askPrice": -5})), 0);
        assert_eq!(ask_price_value(&json!({"askPrice": 10_000_000})), 50_000);
        assert_eq!(ask_price_value(&json!({"askPrice": "gratis"})), 0);
    }

    #[tokio::test]
    async fn the_public_card_never_carries_the_owner() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let c = card(&s).await;
        assert_eq!((c["name"].clone(), c["onboarded"].clone(), c["offer"].clone()), (Value::Null, json!(false), Value::Null));
        let (_, b) = testkit::onboard(&s, &m).await;
        let c = card(&s).await;
        assert!(c["name"].is_null(), "no name before onboarding finishes");
        assert_eq!(c["country"], "PE");
        sqlx::query("UPDATE businesses SET onboarded = 1").execute(&s.db).await.unwrap();
        let c = card(&s).await;
        assert_eq!(c["name"], "Barbería Tito");
        assert!(c["offer"].is_object());
        assert!(!c.to_string().contains("999000111"), "the owner's phone never goes public");
        assert_eq!(c["id"], json!(s.identity.id()));
        assert_eq!(c["e2e"]["x25519"].as_str().unwrap().len(), 64);
        assert!(c["capabilities"].as_array().unwrap().len() >= 1);
        // Attestation rides along only for this agent id.
        sqlx::query("INSERT INTO device_attestation (id, agent, chain, level) VALUES (1, 'agent:someone-else', '[\"A\"]', 'tee')").execute(&s.db).await.unwrap();
        assert!(card(&s).await["device"].is_null());
        let signed = signed_card(&s).await;
        assert!(yaya_wire::envelope::verify(&signed).is_ok());
        let _ = b;
    }

    #[tokio::test]
    async fn publishing_is_throttled_and_respects_the_opt_out() {
        let m = Mock::start().await;
        m.on("/v1/agents", json!({"ok": true}));
        let s = testkit::state_on(&m).await;
        assert!(publish(&s, false).await.is_some());
        assert!(publish(&s, false).await.is_none(), "throttled");
        assert!(publish(&s, true).await.is_some(), "forced");
        publish_if_stale(&s, Duration::from_secs(3600)).await;
        let posts = m.seen().iter().filter(|x| x.path == "/v1/agents").count();
        assert_eq!(posts, 2);
        let (_, b) = testkit::onboard(&s, &m).await;
        sqlx::query("UPDATE businesses SET schema_config = '{\"networkPublish\": false}' WHERE id = $1").bind(b).execute(&s.db).await.unwrap();
        assert!(!publish_allowed(&s).await);
        assert!(publish(&s, true).await.is_none(), "opting out means out");
        let off = testkit::state().await;
        assert!(publish(&off, true).await.is_none());
    }

    #[tokio::test]
    async fn plan_sync_applies_caps_and_labels_self_hosted() {
        let m = Mock::start().await;
        m.on("/v1/me", json!({"plan": "pro", "caps": {"messagesPerDay": 0, "customersPerDay": 0, "conversationsPerMonth": 0}, "seller": true}));
        m.on("/v1/credits", json!({}));
        let s = testkit::state_on(&m).await;
        testkit::business(&s.db).await;
        let me = sync_plan(&s).await.unwrap();
        // The test LLM has its own key (Direct) and no account: self-hosted wins
        // only for free/expired; a paid plan is kept.
        assert_eq!(me["plan"], "pro");
        assert!(s.seller.load(std::sync::atomic::Ordering::Relaxed));
        let (plan,): (String,) = sqlx::query_as("SELECT plan FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!(plan, "pro");
        m.set("/v1/me", json!({"plan": "free", "caps": {"messagesPerDay": 100}}));
        let me = sync_plan(&s).await.unwrap();
        assert_eq!((me["plan"].clone(), me["state"].clone()), (json!("self-hosted"), json!("self-hosted")));
        let (plan, cap): (String, Option<i64>) = sqlx::query_as("SELECT plan, msg_cap FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!((plan.as_str(), cap), ("self-hosted", Some(0)));
        assert!(sync_plan(&*testkit::state().await).await.is_none());
    }

    #[tokio::test]
    async fn niche_skill_sync() {
        let m = Mock::start().await;
        m.on("/v1/skills/barberia-PE", json!({"skill": "  Pregunta por el tipo de corte. ", "version": 3}));
        let s = testkit::state_on(&m).await;
        sync_skill(&s).await; // no business: nothing asked
        assert!(m.seen().is_empty());
        testkit::business(&s.db).await;
        assert_eq!(niche_of(&s).await.as_deref(), Some("barberia-PE"));
        sync_skill(&s).await;
        assert_eq!(s.niche_skill.lock().unwrap().as_deref(), Some("Pregunta por el tipo de corte."));
        m.set("/v1/skills/barberia-PE", json!({"skill": ""}));
        sync_skill(&s).await;
        assert!(s.niche_skill.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn peer_notes_describe_reputation_and_are_cached() {
        let m = Mock::start().await;
        m.on("/reputation", json!({"reviews": 3, "rating": 4.5, "tags": {"no_show": 2}, "hardwareVerified": true}));
        let s = testkit::state_on(&m).await;
        let n = peer_note(&s, "agent:abc").await.unwrap();
        assert!(n.contains("4.5") && n.contains("no_show×2") && n.contains("hardware-attested"), "{n}");
        peer_note(&s, "agent:abc").await;
        assert_eq!(m.seen().len(), 1, "cached an hour");
        assert_eq!(peer_note(&s, "com.whatsapp:1").await, None);
        let m2 = Mock::start().await;
        m2.on("/reputation", json!({"reviews": 0, "interactions": 4}));
        let n = peer_note(&*testkit::state_on(&m2).await, "agent:new").await.unwrap();
        assert!(n.contains("4 prior interactions") && n.contains("new walk-in"));
        assert_eq!(peer_note(&*testkit::state().await, "agent:x").await, None, "unreachable registry: no note");
    }

    #[tokio::test]
    async fn reviews_find_and_polling() {
        let m = Mock::start().await;
        m.on("/reviews", json!({"ok": true})).on("/v1/match", json!({"results": []}));
        let s = testkit::state_on(&m).await;
        post_review(&s, "agent:b", 5, "genial", &["puntual".into()]).await.unwrap();
        let env = &m.seen_path("/reviews")[0].body;
        assert_eq!(yaya_wire::envelope::verify(env).unwrap().to_string(), s.identity.id(), "reviews are signed");
        assert_eq!(env["payload"]["stars"], 5);
        find(&s, "corte", Some("PE"), None, None, Some((-12.1, -77.0)), 5).await.unwrap();
        assert_eq!(m.seen_path("/v1/match")[0].body["lat"], -12.1);
        // poll_inbox opens what is ours and drops what is not.
        let sender = crate::identity::Identity::ephemeral();
        let good = crate::e2e::seal(&sender.x25519_secret(), &sender.id(), &s.identity.id(), b"{\"text\":\"hola\"}").unwrap();
        let other = crate::identity::Identity::ephemeral();
        let not_ours = crate::e2e::seal(&sender.x25519_secret(), &sender.id(), &other.id(), b"x").unwrap();
        m.on("/v1/inbox", json!({"messages": [
            {"id": "1", "from": sender.id(), "box": good},
            {"id": "2", "from": sender.id(), "box": not_ours},
            {"id": "3", "from": other.id(), "box": good},
        ]}));
        let got = poll_inbox(&s, 99).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].0.as_str(), got[0].2["text"].as_str()), ("1", Some("hola")));
        assert!(m.seen_path("/v1/inbox")[0].path.ends_with("wait=30"), "wait is capped");
    }

    #[test]
    fn asks_hand_replies_to_whoever_waits() {
        let a = Asks::default();
        assert!(!a.is_live());
        a.set_live();
        assert!(a.is_live());
        assert!(!a.deliver("agent:b", json!(1)), "nobody waits");
        let mut rx = a.register("agent:b");
        assert!(a.deliver("agent:b", json!(2)));
        assert_eq!(rx.try_recv().unwrap(), json!(2));
        drop(rx);
        assert!(!a.deliver("agent:b", json!(3)), "a closed waiter is forgotten");
        let _rx = a.register("agent:c");
        a.unregister("agent:c");
        assert!(!a.deliver("agent:c", json!(4)));
    }

    #[tokio::test]
    async fn registry_client_signs_and_reports_errors() {
        let m = Mock::start().await;
        m.on("/ok", json!({"a": 1})).on_status("/bad", 409, json!({"error": {"message": "taken"}})).on_status("/plain", 500, json!({"error": "boom"}));
        let s = testkit::state_on(&m).await;
        let r = &s.registry;
        assert_eq!(r.base(), m.base);
        assert_eq!(r.identity().id(), s.identity.id());
        assert_eq!(r.get("/ok", Duration::from_secs(5)).await.unwrap()["a"], 1);
        let h = &m.seen()[0].headers;
        assert_eq!(h["authorization"], format!("Bearer {}", s.identity.id()).as_str());
        assert!(h.get(yaya_wire::reqsig::HEADER).is_some());
        assert_eq!(r.post("/bad", &json!({}), Duration::from_secs(5)).await.unwrap_err().to_string(), "registry 409 Conflict: taken");
        assert!(r.get("/plain", Duration::from_secs(5)).await.unwrap_err().to_string().contains("boom"));
        assert!(r.put_bytes("/ok", vec![1, 2], &[("x-k", "v".into())], Duration::from_secs(5)).await.is_ok());
        assert_eq!(m.seen().last().unwrap().headers["x-k"], "v");
        assert!(r.get_bytes("/bad", Duration::from_secs(5)).await.unwrap_err().to_string().contains("taken"));
        r.post_as_session("/ok", "sess", &json!({}), Duration::from_secs(5)).await.unwrap();
        assert_eq!(m.seen().last().unwrap().headers["authorization"], "Bearer sess");
        r.get_as_session("/ok", "sess2", Duration::from_secs(5)).await.unwrap();
        assert_eq!(m.seen().last().unwrap().headers["authorization"], "Bearer sess2");
        assert_eq!(Registry::new("http://x/", s.identity.clone()).base(), "http://x");
    }

    #[test]
    fn registry_url_default() {
        assert_eq!(registry_url(), DEFAULT_REGISTRY_URL.trim_end_matches('/'));
    }

    #[tokio::test]
    async fn purchases_mount_bundle_packs_only() {
        let m = Mock::start().await;
        m.on("/v1/purchases", json!({"purchases": [
            {"id": "L1", "title": "Barbería pro", "kind": "bundle", "content": {"fields": {}}},
            {"id": "L2", "title": "file", "kind": "file", "content": {}},
            {"id": "L3", "kind": "bundle", "content": {}},
        ]}));
        let s = testkit::state_on(&m).await;
        assert_eq!(sync_purchases(&s).await, 1);
        assert_eq!(sync_purchases(&s).await, 1, "re-sync upserts");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mounted_bundles").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(sync_purchases(&*testkit::state().await).await, 0);
        assert_eq!(ask_price_of(&s).await, 0);
    }
}
