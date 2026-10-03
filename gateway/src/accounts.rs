//! Yaya ID — the identity we own from day one, without passwords. An
//! account is a name, an email and a phone; proof is a one-time code sent
//! over WhatsApp and email (`otp.rs`). It owns agents (phones, the CLI) and
//! carries the plan every one of them runs under. Sessions are opaque
//! bearers (`ysess_…`), hashed at rest, minted for a device or the web.

use axum::{
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};
use yaya_wire::{ratelimit::Quota, sha256_hex, AgentId};

use crate::{agent_of, client_ip, err, internal, plans, ApiResult, App, Auth, Shared};

pub const SESSION_PREFIX: &str = "ysess_";
const SESSION_DAYS: i64 = 30;

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub phone: Option<String>,
}

pub fn subject(account_id: &str) -> String {
    format!("acct:{account_id}")
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn valid_email(e: &str) -> bool {
    e.len() >= 5 && e.len() <= 254 && e.matches('@').count() == 1 && !e.starts_with('@') && !e.ends_with('@')
        && e.rsplit('@').next().map_or(false, |d| d.contains('.')) && !e.chars().any(|c| c.is_whitespace() || c.is_control())
}


/// The account behind a web/device session bearer, or 401.
pub async fn session_of(app: &App, auth: &Auth) -> Result<Account, (StatusCode, Json<Value>)> {
    let Auth::Session(token) = auth else {
        return Err(err(StatusCode::UNAUTHORIZED, "an agente session is required (Bearer ysess_…)"));
    };
    account_of_session(app, token).await?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "session expired or unknown"))
}

pub async fn account_of_session(app: &App, token: &str) -> Result<Option<Account>, (StatusCode, Json<Value>)> {
    let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT a.id, a.email, a.name, a.phone FROM sessions s JOIN accounts a ON a.id = s.account \
         WHERE s.token_hash = $1 AND s.expires_at > $2",
    ).bind(sha256_hex(token.as_bytes())).bind(now()).fetch_optional(&app.db).await.map_err(internal)?;
    if row.is_some() {
        let _ = sqlx::query("UPDATE sessions SET last_seen = $2 WHERE token_hash = $1").bind(sha256_hex(token.as_bytes())).bind(now()).execute(&app.db).await;
    }
    Ok(row.map(|(id, email, name, phone)| Account { id, email, name, phone }))
}

/// The account an agent is linked to, if any.
pub async fn account_of_agent(app: &App, agent: &str) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let row: Option<(String,)> = sqlx::query_as("SELECT account FROM account_agents WHERE agent = $1").bind(agent).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(row.map(|r| r.0))
}

/// Web session or linked agent → the account id. The two ways a caller can
/// be "a real user who signed up".
pub async fn account_of_auth(app: &App, auth: &Auth) -> Result<String, (StatusCode, Json<Value>)> {
    match auth {
        Auth::Session(_) => Ok(session_of(app, auth).await?.id),
        _ => {
            let agent = agent_of(app, auth).await?;
            account_of_agent(app, &agent).await?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "agent is not linked to an agente account"))
        }
    }
}

async fn mint_session(app: &App, account: &str, kind: &str, label: Option<&str>) -> Result<(String, String), (StatusCode, Json<Value>)> {
    let mut b = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
    let token = format!("{SESSION_PREFIX}{}", hex::encode(b));
    let expires = (chrono::Utc::now() + chrono::Duration::days(SESSION_DAYS)).to_rfc3339();
    sqlx::query("INSERT INTO sessions (token_hash, account, kind, label, expires_at) VALUES ($1,$2,$3,$4,$5)")
        .bind(sha256_hex(token.as_bytes())).bind(account).bind(kind).bind(label).bind(&expires)
        .execute(&app.db).await.map_err(internal)?;
    Ok((token, expires))
}

async fn link(app: &App, account: &str, agent: &str, label: Option<&str>) -> Result<(), (StatusCode, Json<Value>)> {
    sqlx::query(
        "INSERT INTO account_agents (agent, account, label) VALUES ($1,$2,$3) \
         ON CONFLICT (agent) DO UPDATE SET account = excluded.account, label = COALESCE(excluded.label, account_agents.label), \
           linked_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(agent).bind(account).bind(label).execute(&app.db).await.map_err(internal)?;
    tracing::info!(%account, %agent, "agent linked");
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct OtpStartReq { #[serde(default)] email: Option<String>, #[serde(default)] phone: Option<String>, #[serde(default)] name: Option<String> }

fn clean_email(e: Option<&str>) -> Option<String> {
    e.map(|e| e.trim().to_lowercase()).filter(|e| valid_email(e))
}

/// `POST /v1/accounts/otp/start` — one code, every channel we have for this
/// person. New or existing account: the caller does not need to know.
pub async fn otp_start(State(app): State<Shared>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, headers: HeaderMap, Json(req): Json<OtpStartReq>) -> ApiResult {
    if !app.otp.any() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "code delivery is not configured"));
    }
    let ip = client_ip(&headers, peer);
    // Phone-only verification: the account's proven identity is a WhatsApp
    // number, so the code is delivered over WhatsApp and matched by phone
    // alone. The email is still collected — stored on the OTP row and carried
    // to the account at check time — but it is never verified nor delivered
    // to, purely a profile attribute.
    let Some(phone) = req.phone.as_deref().and_then(crate::otp::normalize_phone) else {
        return Err(err(StatusCode::BAD_REQUEST, "a phone with country code is required"));
    };
    let phone = Some(phone);
    let email = clean_email(req.email.as_deref());
    let ok_ip = app.limiter.take(&format!("otp-ip:{ip}"), Quota::from_env("OTP_PER_IP_PER_HOUR", 20.0, 3600.0));
    let ok_target = app.limiter.take(&format!("otp-to:{}", phone.as_deref().unwrap_or("")), Quota::from_env("OTP_PER_TARGET_PER_HOUR", 5.0, 3600.0));
    if !(ok_ip && ok_target) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many codes requested — try again later"));
    }
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| n.chars().take(80).collect::<String>());
    // Review credentials (Google Play app review): OTP_REVIEW_PHONE gets the
    // fixed OTP_REVIEW_CODE and no WhatsApp delivery — the number needs no
    // real WhatsApp. Both must be set in the environment or the path is off.
    let review = std::env::var("OTP_REVIEW_PHONE").ok().filter(|p| !p.is_empty())
        .zip(std::env::var("OTP_REVIEW_CODE").ok().filter(|c| !c.is_empty()))
        .filter(|(p, _)| Some(p.as_str()) == phone.as_deref());
    let code = match &review {
        Some((_, c)) => c.clone(),
        None => crate::otp::six_digits(),
    };
    // A new code retires the ones still open for this phone.
    sqlx::query("UPDATE account_otps SET expires_at = $2 WHERE consumed_at IS NULL AND phone IS NOT NULL AND phone = $1")
        .bind(&phone).bind(now()).execute(&app.db).await.map_err(internal)?;
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    sqlx::query("INSERT INTO account_otps (id, email, phone, name, code_hash, expires_at) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&email).bind(&phone).bind(&name).bind(sha256_hex(code.as_bytes())).bind(&expires)
        .execute(&app.db).await.map_err(internal)?;
    // WhatsApp only: email is passed None so no code is ever delivered to it.
    // The review number skips delivery — its code is fixed and shared with
    // the app-store reviewer out of band.
    let wa = if review.is_some() {
        tracing::info!(phone = ?phone, "review otp issued (no delivery)");
        true
    } else {
        let (wa, _mail) = app.otp.send(phone.as_deref(), None, &code).await;
        wa
    };
    if !wa {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "could not deliver the code right now"));
    }
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE phone = $1 LIMIT 1")
        .bind(&phone).fetch_optional(&app.db).await.map_err(internal)?;
    tracing::info!(phone = ?phone, wa, exists = exists.is_some(), "otp sent");
    Ok(Json(json!({"sent": {"whatsapp": wa, "email": false}, "expiresInSeconds": 600, "exists": exists.is_some(), "email": email, "phone": phone})).into_response())
}

#[derive(serde::Deserialize)]
pub struct OtpCheckReq { #[serde(default)] email: Option<String>, #[serde(default)] phone: Option<String>, code: String, #[serde(default)] name: Option<String>, #[serde(default)] device: Option<String>, #[serde(default)] kind: Option<String> }

/// `POST /v1/accounts/otp/check` — the code proves the person; the account
/// is created on the spot if it is their first time. A proven agent in the
/// bearer is linked and gets a device session.
pub async fn otp_check(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<OtpCheckReq>) -> ApiResult {
    // Phone-only: the code was sent over WhatsApp, so a phone is what proves
    // it. The OTP row and the account are both matched by phone alone; the
    // email is only the profile attribute typed at signup, saved as-is.
    let Some(phone) = req.phone.as_deref().and_then(crate::otp::normalize_phone) else {
        return Err(err(StatusCode::BAD_REQUEST, "a phone with country code is required"));
    };
    let phone = Some(phone);
    let profile_email = clean_email(req.email.as_deref());
    // Attempt counted in the same statement that fetches the row.
    let row: Option<(String, i64, bool, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "UPDATE account_otps SET attempts = attempts + 1 WHERE id = (SELECT id FROM account_otps \
           WHERE consumed_at IS NULL AND expires_at > $2 AND phone IS NOT NULL AND phone = $1 \
           ORDER BY created_at DESC LIMIT 1) \
         RETURNING id, attempts, code_hash = $3, email, phone, name",
    ).bind(&phone).bind(now()).bind(sha256_hex(req.code.trim().as_bytes())).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((otp_id, attempts, ok, otp_email, otp_phone, otp_name)) = row else {
        return Err(err(StatusCode::GONE, "no active code — request a new one"));
    };
    if attempts > 5 || !ok {
        return Err(err(StatusCode::UNAUTHORIZED, "wrong code"));
    }
    sqlx::query("UPDATE account_otps SET consumed_at = $2 WHERE id = $1").bind(&otp_id).bind(now()).execute(&app.db).await.map_err(internal)?;
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| n.chars().take(80).collect::<String>()).or(otp_name);
    let (email, phone) = (profile_email.or(otp_email), phone.or(otp_phone));
    let existing: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, email, name, phone FROM accounts WHERE phone = $1 LIMIT 1",
    ).bind(&phone).fetch_optional(&app.db).await.map_err(internal)?;
    let (acct, created) = match existing {
        Some((id, e, n, p)) => {
            // Fill in what we now know (a phone-only account learns its email and vice versa).
            let _ = sqlx::query("UPDATE accounts SET phone = COALESCE(phone, $2), name = COALESCE(name, $3), updated_at = $4 WHERE id = $1")
                .bind(&id).bind(&phone).bind(&name).bind(now()).execute(&app.db).await;
            let phone = p.or(phone);
            crate::prepaid::set_country_once(&app.db, &id, phone.as_deref()).await;
            (Account { id, email: e, name: n.or(name), phone }, false)
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            // The typed email is unverified (the phone is the identity), so it
            // is a contact hint only: when someone else already holds it, the
            // account keeps the phone address instead of failing sign-up.
            let fallback = format!("{}@phone.yaya.tech", phone.clone().unwrap_or_default());
            let taken = match email.as_deref() {
                Some(e) => sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM accounts WHERE email = $1")
                    .bind(e).fetch_one(&app.db).await.map_err(internal)? > 0,
                None => false,
            };
            let email = if taken { fallback } else { email.clone().unwrap_or(fallback) };
            let mut kb = [0u8; 32];
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut kb);
            sqlx::query("INSERT INTO accounts (id, email, name, phone, password_hash, backup_key) VALUES ($1,$2,$3,$4,'',$5)")
                .bind(&id).bind(&email).bind(&name).bind(&phone).bind(hex::encode(kb)).execute(&app.db).await.map_err(internal)?;
            tracing::info!(account = %id, %email, phone = ?phone, "account created");
            // Prepaid credits: the country is fixed from the WhatsApp dialling
            // code, and the welcome grant lands once per phone, ever.
            crate::prepaid::set_country_once(&app.db, &id, phone.as_deref()).await;
            if let Some(p) = phone.as_deref() {
                let _ = crate::prepaid::welcome(&app.db, &id, p).await;
            }
            plans::start_trial(&app, &subject(&id)).await;
            (Account { id, email, name, phone }, true)
        }
    };
    let mut r = finish_signin(&app, acct, &auth, req.device.as_deref(), req.kind.as_deref()).await?;
    if let Ok(v) = axum::body::to_bytes(std::mem::replace(r.body_mut(), axum::body::Body::empty()), 1 << 20).await.map_err(internal).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(internal)) {
        let mut v = v;
        v["created"] = json!(created);
        return Ok(Json(v).into_response());
    }
    Ok(r)
}

async fn finish_signin(app: &App, acct: Account, auth: &Auth, device: Option<&str>, agent_kind: Option<&str>) -> ApiResult {
    let (kind, label) = match auth {
        Auth::Proven(agent) => {
            link(app, &acct.id, &agent.to_string(), device).await?;
            sqlx::query("UPDATE account_agents SET kind = $2 WHERE agent = $1").bind(agent.to_string()).bind(clean_kind(agent_kind)).execute(&app.db).await.map_err(internal)?;
            ("device", device)
        }
        _ => ("web", None),
    };
    let (session, expires) = mint_session(app, &acct.id, kind, label).await?;
    let plan = plans::effective_for(app, &subject(&acct.id)).await?;
    Ok(Json(json!({
        "account": {"id": acct.id, "email": acct.email, "name": acct.name, "phone": acct.phone},
        "session": session, "expiresAt": expires, "kind": kind,
        "plan": plan.plan, "planExpiresAt": plan.expires_at,
    })).into_response())
}

/// `GET /v1/account/backup-key` — the account's backup key, for a signed-in
/// device or browser. Minted at account creation (or here, for accounts
/// that predate passwordless sign-in).
pub async fn backup_key(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let account = account_of_auth(&app, &auth).await?;
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT backup_key FROM accounts WHERE id = $1").bind(&account).fetch_optional(&app.db).await.map_err(internal)?;
    let key = match row.and_then(|r| r.0) {
        Some(k) => k,
        None => {
            let mut kb = [0u8; 32];
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut kb);
            let k = hex::encode(kb);
            sqlx::query("UPDATE accounts SET backup_key = $2 WHERE id = $1 AND backup_key IS NULL").bind(&account).bind(&k).execute(&app.db).await.map_err(internal)?;
            let (k2,): (Option<String>,) = sqlx::query_as("SELECT backup_key FROM accounts WHERE id = $1").bind(&account).fetch_one(&app.db).await.map_err(internal)?;
            k2.unwrap_or(k)
        }
    };
    Ok(Json(json!({"key": key, "alg": "aes-256-gcm"})).into_response())
}

/// `POST /v1/accounts/logout` — ends this session.
pub async fn logout(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let Auth::Session(token) = &auth else { return Err(err(StatusCode::UNAUTHORIZED, "no session")) };
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1").bind(sha256_hex(token.as_bytes())).execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(serde::Deserialize)]
pub struct LinkReq { #[serde(default)] session: Option<String>, #[serde(default)] token: Option<String>, #[serde(default)] label: Option<String>, #[serde(default)] kind: Option<String>, #[serde(default)] role: Option<String> }

#[derive(serde::Deserialize)]
pub struct LinkTokenReq { #[serde(default)] label: Option<String> }

/// `POST /v1/account/link-tokens` — the owner mints a one-time token (10
/// minutes) so a node they are provisioning can join the account without
/// a code round trip: the node presents it signed, as itself. The console
/// shows it as a command to paste on the machine.
pub async fn link_token(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<LinkTokenReq>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    let mut b = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
    let token = format!("ylink_{}", hex::encode(b));
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    sqlx::query("INSERT INTO link_tokens (token_hash, account, label, expires_at) VALUES ($1,$2,$3,$4)")
        .bind(sha256_hex(token.as_bytes())).bind(&acct.id).bind(req.label.as_deref()).bind(&expires)
        .execute(&app.db).await.map_err(internal)?;
    tracing::info!(account = %acct.id, "link token minted");
    Ok(Json(json!({"token": token, "expiresAt": expires, "account": acct.id})).into_response())
}

/// Burns a link token; returns the account it belongs to.
async fn consume_link_token(app: &App, token: &str) -> Result<(String, Option<String>), (StatusCode, Json<Value>)> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "UPDATE link_tokens SET used_at = $2 WHERE token_hash = $1 AND used_at IS NULL AND expires_at > $2 RETURNING account, label",
    ).bind(sha256_hex(token.as_bytes())).bind(now()).fetch_optional(&app.db).await.map_err(internal)?;
    row.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "link token unknown, used or expired"))
}

/// What a linked agent is on the account. `phone` is the business runtime
/// (the console may drive it), `node` a headless core, `web` the console,
/// and `android` / `ios` the consumer app on a handset — a peer that
/// talks to *its own* orchestrator and must never be mistaken for a
/// business phone. Unknown kinds stay `phone` for older clients.
fn clean_kind(k: Option<&str>) -> &'static str {
    match k {
        Some("web") => "web",
        Some("node") => "node",
        Some("android") => "android",
        Some("ios") => "ios",
        _ => "phone",
    }
}
fn clean_role(r: Option<&str>) -> Option<String> {
    r.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().filter(|c| !c.is_control()).take(40).collect())
}

/// `POST /v1/account/agents` — a proven agent joins the account that owns
/// `session` (a phone that signed in through the web, the CLI, or the web
/// console's own key). `kind` says what it is; `role` what the owner wants
/// it to be (ventas, soporte, recepción…).
pub async fn link_agent(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<LinkReq>) -> ApiResult {
    let Auth::Proven(agent) = &auth else { return Err(err(StatusCode::UNAUTHORIZED, "a signed agent request is required")) };
    // Two doors: a live session (phone signed in on the web / CLI / the
    // console's own key) or a one-time link token (a node being provisioned).
    let (acct, minted) = match (req.session.as_deref(), req.token.as_deref()) {
        (Some(s), _) => (account_of_session(&app, s).await?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "session expired or unknown"))?, None),
        (None, Some(t)) => {
            let (account_id, token_label) = consume_link_token(&app, t).await?;
            let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as("SELECT id, email, name, phone FROM accounts WHERE id = $1")
                .bind(&account_id).fetch_optional(&app.db).await.map_err(internal)?;
            let (id, email, name, phone) = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "account gone"))?;
            let label = req.label.clone().or(token_label);
            let session = mint_session(&app, &id, "device", label.as_deref()).await?;
            (Account { id, email, name, phone }, Some(session))
        }
        _ => return Err(err(StatusCode::BAD_REQUEST, "session or token required")),
    };
    link(&app, &acct.id, &agent.to_string(), req.label.as_deref()).await?;
    let kind = clean_kind(req.kind.as_deref());
    sqlx::query("UPDATE account_agents SET kind = $2, role = COALESCE($3, role) WHERE agent = $1")
        .bind(agent.to_string()).bind(kind).bind(clean_role(req.role.as_deref())).execute(&app.db).await.map_err(internal)?;
    let plan = plans::effective_for(&app, &subject(&acct.id)).await?;
    let mut out = json!({"ok": true, "account": {"id": acct.id, "email": acct.email, "name": acct.name, "phone": acct.phone}, "plan": plan.plan, "kind": kind});
    if let Some((session, expires)) = minted {
        out["session"] = json!(session);
        out["expiresAt"] = json!(expires);
    }
    Ok(Json(out).into_response())
}

#[derive(serde::Deserialize)]
pub struct UpdateAgentReq { #[serde(default)] label: Option<String>, #[serde(default)] role: Option<String> }

/// `PATCH /v1/account/agents/{id}` — rename a device or give it a role.
pub async fn update_agent(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, Json(req): Json<UpdateAgentReq>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    let label = req.label.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(60).collect::<String>());
    let n = sqlx::query("UPDATE account_agents SET label = COALESCE($3, label), role = COALESCE($4, role) WHERE agent = $1 AND account = $2")
        .bind(&id).bind(&acct.id).bind(label).bind(clean_role(req.role.as_deref())).execute(&app.db).await.map_err(internal)?.rows_affected();
    if n == 0 {
        return Err(err(StatusCode::NOT_FOUND, "that agent is not on this account"));
    }
    Ok(Json(json!({"ok": true})).into_response())
}

/// Do these two agents belong to the same account? (The console talking to
/// its own phone.)
pub async fn same_account(app: &App, a: &str, b: &str) -> bool {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT count(*) FROM account_agents x JOIN account_agents y ON x.account = y.account WHERE x.agent = $1 AND y.agent = $2",
    ).bind(a).bind(b).fetch_optional(&app.db).await.ok().flatten();
    row.is_some_and(|r| r.0 > 0)
}

#[derive(serde::Deserialize)]
pub struct CreditsReq { #[serde(rename = "amountMinor")] amount_minor: i64 }

/// `POST /v1/account/credits/request` — a recarga: buy credits by Yape/Plin.
pub async fn credits_request(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<CreditsReq>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    Ok(Json(plans::open_credits_request(&app, &subject(&acct.id), req.amount_minor).await?).into_response())
}

/// `GET /v1/account` — the account, its plan, its devices and its backups.
pub async fn me(State(app): State<Shared>, Extension(auth): Extension<Auth>, headers: HeaderMap) -> ApiResult {
    let cur = plans::currency_of(&headers, None).to_string();
    let acct = session_of(&app, &auth).await?;
    let plan = plans::effective_for(&app, &subject(&acct.id)).await?;
    plans::sweep(&app, &subject(&acct.id)).await;
    let t = plans::tier(&plan.plan);
    let agents: Vec<(String, Option<String>, String, Option<String>, Option<String>, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT aa.agent, aa.label, aa.linked_at, a.name, a.revoked_at, aa.kind, aa.role, a.industry, a.handle, \
                s.last_poll, v.verified, a.updated_at \
         FROM account_agents aa \
         LEFT JOIN agents a ON a.agent = aa.agent \
         LEFT JOIN agents_seen s ON s.agent = aa.agent \
         LEFT JOIN device_verdicts v ON v.agent = aa.agent \
         WHERE aa.account = $1 ORDER BY aa.linked_at",
    ).bind(&acct.id).fetch_all(&app.db).await.map_err(internal)?;
    let backups: Vec<(String, String, i64, String, String)> = sqlx::query_as(
        "SELECT id, agent, size, meta, created_at FROM backups WHERE account = $1 ORDER BY created_at DESC LIMIT 10",
    ).bind(&acct.id).fetch_all(&app.db).await.map_err(internal)?;
    let pending: Option<(String, String, f64, String, String, i64)> = sqlx::query_as(
        "SELECT ref, plan, amount, currency, created_at, months FROM plan_requests WHERE agent = $1 AND status = 'pending' ORDER BY created_at DESC LIMIT 1",
    ).bind(subject(&acct.id)).fetch_optional(&app.db).await.map_err(internal)?;
    let state = plans::state_of(&plan.plan);
    let now = chrono::Utc::now();
    let days_left = plan.expires_at.as_deref()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
        .map(|e| ((e.with_timezone(&chrono::Utc) - now).num_seconds() as f64 / 86400.0).ceil().max(0.0) as i64);
    let online_after = (now - chrono::Duration::seconds(90)).to_rfc3339();
    Ok(Json(json!({
        "account": {"id": acct.id, "email": acct.email, "name": acct.name, "phone": acct.phone},
        "plan": {"name": plan.plan, "state": state, "expiresAt": plan.expires_at, "daysLeft": days_left, "source": plan.source,
                 "trialDays": plans::trial_days(),
                 "caps": {"conversationsPerMonth": t.conversations, "agents": t.agents, "callsPerDay": t.calls},
                 "backups": plans::backups_allowed(&app, &plan.plan)},
        "tiers": plans::tiers_json_in(&cur), "prices": plans::business_prices_in(&cur),
        "seller": crate::seller::is_seller_account(&acct.id, &acct.email, acct.phone.as_deref()),
        "pendingRequest": pending.map(|(r, p, a, c, at, m)| json!({"ref": r, "plan": p, "amount": a, "currency": c, "createdAt": at, "months": m,
                                                                 "creditsMinor": if p == "credits" { Some(m) } else { None }, "pay": plans::payment_channels()})),
        "agents": agents.into_iter().map(|(id, label, at, name, revoked, kind, role, industry, handle, last_poll, verified, updated)| json!({
            "agent": id, "label": label, "linkedAt": at, "businessName": name, "revoked": revoked.is_some(),
            "kind": kind, "role": role, "industry": industry, "handle": handle,
            "online": last_poll.as_deref().is_some_and(|p| p > online_after.as_str()), "lastSeenAt": last_poll,
            "hardwareVerified": verified.map(|v| v == 1), "publishedAt": updated,
        })).collect::<Vec<_>>(),
        "backups": backups.into_iter().map(|(id, agent, size, meta, at)| json!({"id": id, "agent": agent, "size": size, "createdAt": at, "meta": serde_json::from_str::<Value>(&meta).unwrap_or(Value::Null)})).collect::<Vec<_>>(),
        "billing": crate::billing::summary(&app, &acct).await?,
        "support": crate::support::support_json(&app).await,
    })).into_response())
}

/// `DELETE /v1/account/agents/{id}` — unlink a device.
pub async fn unlink_agent(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    let n = sqlx::query("DELETE FROM account_agents WHERE agent = $1 AND account = $2").bind(&id).bind(&acct.id).execute(&app.db).await.map_err(internal)?.rows_affected();
    Ok(Json(json!({"ok": n > 0})).into_response())
}

#[derive(serde::Deserialize)]
pub struct RevokeReq { #[serde(default)] successor: Option<String> }

/// `POST /v1/account/agents/{id}/revoke` — the account retires a device it
/// owns (lost phone: the old key cannot sign anything any more). The
/// successor, if named, must belong to the same account and inherits the
/// handle on its next publish.
pub async fn revoke_agent(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, Json(req): Json<RevokeReq>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    let owned: Option<(String,)> = sqlx::query_as("SELECT agent FROM account_agents WHERE agent = $1 AND account = $2").bind(&id).bind(&acct.id).fetch_optional(&app.db).await.map_err(internal)?;
    if owned.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "that agent is not on this account"));
    }
    let successor = match req.successor.as_deref() {
        None => None,
        Some(s) => {
            let s = s.parse::<AgentId>().map_err(|e| err(StatusCode::BAD_REQUEST, e))?.to_string();
            let same: Option<(String,)> = sqlx::query_as("SELECT agent FROM account_agents WHERE agent = $1 AND account = $2").bind(&s).bind(&acct.id).fetch_optional(&app.db).await.map_err(internal)?;
            if same.is_none() { return Err(err(StatusCode::BAD_REQUEST, "successor must be a device on this account")); }
            Some(s)
        }
    };
    crate::revoke_agent(&app, &id, successor.as_deref()).await?;
    Ok(Json(json!({"ok": true, "revoked": id, "successor": successor})).into_response())
}

#[derive(serde::Deserialize)]
pub struct PlanReq { plan: String, #[serde(default)] months: Option<i64>, #[serde(default)] lang: Option<String> }

/// `POST /v1/account/plan/request` — the only place a plan is bought:
/// on the web, by the account, for all its devices.
pub async fn plan_request(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<PlanReq>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    Ok(Json(plans::open_request(&app, &subject(&acct.id), &req.plan, req.months.unwrap_or(1)).await?).into_response())
}

pub async fn plan_request_status(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(reference): Path<String>) -> ApiResult {
    let acct = session_of(&app, &auth).await?;
    Ok(Json(plans::request_status_for(&app, &subject(&acct.id), &reference).await?).into_response())
}

#[cfg(test)]
mod kind_tests {
    use super::clean_kind;

    #[test]
    fn app_devices_are_not_business_phones() {
        assert_eq!(clean_kind(Some("ios")), "ios");
        assert_eq!(clean_kind(Some("android")), "android");
        assert_eq!(clean_kind(Some("web")), "web");
        assert_eq!(clean_kind(Some("node")), "node");
        assert_eq!(clean_kind(Some("phone")), "phone");
        assert_eq!(clean_kind(Some("toaster")), "phone");
        assert_eq!(clean_kind(None), "phone");
    }
}

#[derive(serde::Deserialize)]
pub struct DeleteAccountReq { phone: String, code: String }

/// `POST /v1/accounts/delete` — self-service deletion, reached same-origin
/// from yaya.tech/eliminar-cuenta (Caddy proxies that path and `otp/start`
/// to us). The person asks for a code with the usual `otp/start`; presenting
/// it here, instead of signing in, erases the account: every device revoked,
/// sessions, backups, links and plan state removed, the account row last.
/// Billing ledgers keep the bare account uuid (legal retention); the
/// tombstone in `account_deletions` answers later disputes.
pub async fn delete_account(State(app): State<Shared>, Json(req): Json<DeleteAccountReq>) -> ApiResult {
    let Some(phone) = crate::otp::normalize_phone(&req.phone) else {
        return Err(err(StatusCode::BAD_REQUEST, "a phone with country code is required"));
    };
    // Same consume-with-attempt-count as otp_check, phone-only.
    let row: Option<(String, i64, bool)> = sqlx::query_as(
        "UPDATE account_otps SET attempts = attempts + 1 WHERE id = (SELECT id FROM account_otps \
           WHERE consumed_at IS NULL AND expires_at > $1 AND phone = $2 \
           ORDER BY created_at DESC LIMIT 1) \
         RETURNING id, attempts, code_hash = $3",
    ).bind(now()).bind(&phone).bind(sha256_hex(req.code.trim().as_bytes())).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((otp_id, attempts, ok)) = row else {
        return Err(err(StatusCode::GONE, "no active code — request a new one"));
    };
    if attempts > 5 || !ok {
        return Err(err(StatusCode::UNAUTHORIZED, "wrong code"));
    }
    sqlx::query("UPDATE account_otps SET consumed_at = $2 WHERE id = $1").bind(&otp_id).bind(now()).execute(&app.db).await.map_err(internal)?;
    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE phone = $1 LIMIT 1")
        .bind(&phone).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((acct,)) = existing else {
        return Err(err(StatusCode::NOT_FOUND, "no account with that number"));
    };
    let agents: Vec<(String,)> = sqlx::query_as("SELECT agent FROM account_agents WHERE account = $1")
        .bind(&acct).fetch_all(&app.db).await.map_err(internal)?;
    for (agent,) in &agents {
        crate::revoke_agent(&app, agent, None).await?;
        sqlx::query("DELETE FROM plans WHERE agent = $1").bind(agent).execute(&app.db).await.map_err(internal)?;
    }
    for sql in [
        "DELETE FROM sessions WHERE account = $1",
        "DELETE FROM backups WHERE account = $1",
        "DELETE FROM account_agents WHERE account = $1",
        "DELETE FROM accounts WHERE id = $1",
    ] {
        sqlx::query(sql).bind(&acct).execute(&app.db).await.map_err(internal)?;
    }
    sqlx::query("DELETE FROM plans WHERE agent = $1").bind(subject(&acct)).execute(&app.db).await.map_err(internal)?;
    sqlx::query("DELETE FROM account_otps WHERE phone = $1").bind(&phone).execute(&app.db).await.map_err(internal)?;
    sqlx::query("INSERT INTO account_deletions (id, phone_hash, devices) VALUES ($1,$2,$3)")
        .bind(&acct).bind(sha256_hex(phone.as_bytes())).bind(agents.len() as i64)
        .execute(&app.db).await.map_err(internal)?;
    tracing::warn!(account = %acct, devices = agents.len(), "account deleted (self-service)");
    Ok(Json(json!({"deleted": true, "devices": agents.len()})).into_response())
}

#[cfg(test)]
mod signin_tests {
    use super::*;
    use crate::testkit::{self, anon, as_agent, as_session, Keypair, Mock};

    async fn setup() -> (Mock, Shared) {
        let m = Mock::start().await;
        m.on("/send", json!({"messageId": "w1"}));
        let base = m.base.clone();
        let app = testkit::app_with(move |a| crate::App { otp: crate::otp::Delivery::for_test(&base), ..a }).await;
        (m, app)
    }

    /// The code the bridge was asked to deliver last.
    fn last_code(m: &Mock) -> String {
        m.seen_path("/send").last().unwrap().body["text"].as_str().unwrap()[..6].to_string()
    }

    async fn sign_in(m: &Mock, app: &Shared, phone: &str, email: Option<&str>) -> (u16, Value) {
        let (st, v) = anon(app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": phone, "email": email}))).await;
        assert_eq!(st, 200, "{v}");
        let code = last_code(m);
        anon(app, "POST", "/v1/accounts/otp/check", Some(json!({"phone": phone, "code": code, "email": email, "name": "Ana"}))).await
    }

    #[tokio::test]
    async fn a_first_sign_in_creates_the_account_with_its_welcome() {
        let (m, app) = setup().await;
        let (st, v) = sign_in(&m, &app, "+51 977 000 111", Some("Ana@Mail.pe")).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!((v["created"].clone(), v["kind"].clone(), v["account"]["phone"].clone()), (json!(true), json!("web"), json!("51977000111")));
        assert!(v["session"].as_str().unwrap().starts_with(SESSION_PREFIX));
        let id = v["account"]["id"].as_str().unwrap().to_string();
        assert!(crate::prepaid::balance(&app.db, &id).await.unwrap() > 0, "the welcome grant landed");
        // Signing in again finds the same account.
        let (_, again) = sign_in(&m, &app, "977000111", None).await;
        assert_eq!((again["created"].clone(), again["account"]["id"].clone()), (json!(false), json!(id)));
    }

    #[tokio::test]
    async fn codes_are_single_use_bounded_and_replaced() {
        let (m, app) = setup().await;
        anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await;
        let first = last_code(&m);
        anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await;
        let second = last_code(&m);
        if first != second {
            assert_eq!(anon(&app, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "51977000111", "code": first}))).await.0, 401, "a new code retires the old one");
        }
        let wrong = if second == "000000" { "111111" } else { "000000" };
        for _ in 0..5 {
            assert_eq!(anon(&app, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "51977000111", "code": wrong}))).await.0, 401);
        }
        assert_eq!(anon(&app, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "51977000111", "code": second}))).await.0, 401, "dead after five misses");
        assert_eq!(anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "123"}))).await.0, 400);
        assert_eq!(anon(&app, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "123", "code": "1"}))).await.0, 400);
        let off = testkit::app().await;
        assert_eq!(anon(&off, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await.0, 503);
    }

    /// The sign-up email is never verified (the phone is the identity), so
    /// someone typing another person's email must not lock that person out.
    #[tokio::test]
    async fn an_unverified_email_cannot_squat_someone_elses_sign_up() {
        let (m, app) = setup().await;
        let (st, _) = sign_in(&m, &app, "51977000666", Some("victim@company.pe")).await;
        assert_eq!(st, 200);
        let (st, v) = sign_in(&m, &app, "51977000111", Some("victim@company.pe")).await;
        assert_eq!(st, 200, "the real owner still gets an account: {v}");
        assert_eq!(v["created"], true);
        assert_eq!(v["account"]["email"], "51977000111@phone.yaya.tech");
    }

    #[tokio::test]
    async fn a_signed_phone_is_linked_and_gets_a_device_session() {
        let (m, app) = setup().await;
        let kp = Keypair::generate();
        anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await;
        let code = last_code(&m);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "51977000111", "code": code, "device": "Moto G", "kind": "android"}))).await;
        assert_eq!((st, v["kind"].clone()), (200, json!("device")), "{v}");
        let id = v["account"]["id"].as_str().unwrap().to_string();
        assert_eq!(account_of_agent(&app, &kp.id().to_string()).await.unwrap().as_deref(), Some(id.as_str()));
        let (_, k) = as_agent(&app, &kp, "GET", "/v1/account/backup-key", None).await;
        assert_eq!(k["key"].as_str().unwrap().len(), 64);
        let token = v["session"].as_str().unwrap().to_string();
        let (_, me) = as_session(&app, &token, "GET", "/v1/account", None).await;
        assert_eq!(me["agents"][0]["kind"], "android");
        assert_eq!(me["agents"][0]["label"], "Moto G");
        assert_eq!(as_session(&app, &token, "POST", "/v1/accounts/logout", None).await.1["ok"], true);
        assert_eq!(as_session(&app, &token, "GET", "/v1/account", None).await.0, 401, "the session is gone");
    }

    #[tokio::test]
    async fn link_tokens_provision_a_node_once() {
        let (m, app) = setup().await;
        let (_, v) = sign_in(&m, &app, "51977000111", None).await;
        let web = v["session"].as_str().unwrap().to_string();
        let (_, t) = as_session(&app, &web, "POST", "/v1/account/link-tokens", Some(json!({"label": "servidor"}))).await;
        let token = t["token"].as_str().unwrap().to_string();
        let node = Keypair::generate();
        let (st, l) = as_agent(&app, &node, "POST", "/v1/account/agents", Some(json!({"token": token, "kind": "node", "role": "ventas"}))).await;
        assert_eq!((st, l["kind"].clone()), (200, json!("node")), "{l}");
        assert!(l["session"].is_string());
        let other = Keypair::generate();
        assert_eq!(as_agent(&app, &other, "POST", "/v1/account/agents", Some(json!({"token": token}))).await.0, 401, "one use");
        assert_eq!(anon(&app, "POST", "/v1/account/agents", Some(json!({"token": token}))).await.0, 401, "needs a signed agent");
        assert_eq!(as_agent(&app, &other, "POST", "/v1/account/agents", Some(json!({}))).await.0, 400);
        // Rename, then unlink; another account cannot touch it.
        let nid = node.id().to_string();
        assert_eq!(as_session(&app, &web, "PATCH", &format!("/v1/account/agents/{nid}"), Some(json!({"label": "Caja 1"}))).await.0, 200);
        let (_, v2) = sign_in(&m, &app, "51977000222", None).await;
        let web2 = v2["session"].as_str().unwrap().to_string();
        assert_eq!(as_session(&app, &web2, "PATCH", &format!("/v1/account/agents/{nid}"), Some(json!({"label": "x"}))).await.0, 404);
        assert_eq!(as_session(&app, &web2, "DELETE", &format!("/v1/account/agents/{nid}"), None).await.1["ok"], false);
        assert_eq!(as_session(&app, &web, "DELETE", &format!("/v1/account/agents/{nid}"), None).await.1["ok"], true);
    }

    #[tokio::test]
    async fn deleting_needs_the_code_and_clears_everything_but_the_one_time_welcome() {
        let (m, app) = setup().await;
        let kp = Keypair::generate();
        anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await;
        let code = last_code(&m);
        let (_, v) = as_agent(&app, &kp, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "51977000111", "code": code}))).await;
        let token = v["session"].as_str().unwrap().to_string();
        let first = v["account"]["id"].as_str().unwrap().to_string();
        let welcome = crate::prepaid::balance(&app.db, &first).await.unwrap();

        assert_eq!(anon(&app, "POST", "/v1/accounts/delete", Some(json!({"phone": "51977000111", "code": "000000"}))).await.0, 410, "no code asked yet");
        anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await;
        let code = last_code(&m);
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert_eq!(anon(&app, "POST", "/v1/accounts/delete", Some(json!({"phone": "51977000111", "code": wrong}))).await.0, 401);
        let (st, d) = anon(&app, "POST", "/v1/accounts/delete", Some(json!({"phone": "+51 977 000 111", "code": code}))).await;
        assert_eq!((st, d["devices"].clone()), (200, json!(1)), "{d}");
        assert_eq!(as_session(&app, &token, "GET", "/v1/account", None).await.0, 401, "sessions die with the account");
        assert_eq!(account_of_agent(&app, &kp.id().to_string()).await.unwrap(), None, "devices are unlinked");

        let (_, again) = sign_in(&m, &app, "51977000111", None).await;
        let second = again["account"]["id"].as_str().unwrap().to_string();
        assert_ne!(second, first);
        assert!(crate::prepaid::balance(&app.db, &second).await.unwrap() < welcome.max(1), "delete + sign up again is not a second welcome grant");

        anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000999"}))).await;
        let code = last_code(&m);
        assert_eq!(anon(&app, "POST", "/v1/accounts/delete", Some(json!({"phone": "51977000999", "code": code}))).await.0, 404, "no account behind that number");
        assert_eq!(anon(&app, "POST", "/v1/accounts/delete", Some(json!({"phone": "12", "code": "1"}))).await.0, 400);
    }

    #[tokio::test]
    async fn revoking_hands_over_only_to_a_sibling_device() {
        let (m, app) = setup().await;
        let (a, b, stranger) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
        let mut token = String::new();
        for kp in [&a, &b] {
            anon(&app, "POST", "/v1/accounts/otp/start", Some(json!({"phone": "51977000111"}))).await;
            let code = last_code(&m);
            token = as_agent(&app, kp, "POST", "/v1/accounts/otp/check", Some(json!({"phone": "51977000111", "code": code}))).await.1["session"].as_str().unwrap().to_string();
        }
        let (aid, bid, sid) = (a.id().to_string(), b.id().to_string(), stranger.id().to_string());
        assert!(same_account(&app, &aid, &bid).await && !same_account(&app, &aid, &sid).await);
        let r = |id: &str| format!("/v1/account/agents/{id}/revoke");
        assert_eq!(as_session(&app, &token, "POST", &r(&aid), Some(json!({"successor": sid}))).await.0, 400);
        assert_eq!(as_session(&app, &token, "POST", &r(&aid), Some(json!({"successor": "not-an-id"}))).await.0, 400);
        assert_eq!(as_session(&app, &token, "POST", &r(&sid), Some(json!({}))).await.0, 404);
        let (st, v) = as_session(&app, &token, "POST", &r(&aid), Some(json!({"successor": bid}))).await;
        assert_eq!((st, v["successor"].clone()), (200, json!(bid)), "{v}");
    }

    #[tokio::test]
    async fn plan_and_credit_requests_need_a_session() {
        let (m, app) = setup().await;
        let (_, v) = sign_in(&m, &app, "51977000111", None).await;
        let token = v["session"].as_str().unwrap().to_string();
        assert_eq!(anon(&app, "POST", "/v1/account/plan/request", Some(json!({"plan": "pro"}))).await.0, 401);
        let (st, p) = as_session(&app, &token, "POST", "/v1/account/plan/request", Some(json!({"plan": "pro", "lang": "es"}))).await;
        assert_eq!(st, 200, "{p}");
        let reference = p["ref"].as_str().expect("a payment reference").to_string();
        assert_eq!(as_session(&app, &token, "GET", &format!("/v1/account/plan/request/{reference}"), None).await.0, 200);
        let (_, v2) = sign_in(&m, &app, "51977000222", None).await;
        let other = v2["session"].as_str().unwrap().to_string();
        assert_ne!(as_session(&app, &other, "GET", &format!("/v1/account/plan/request/{reference}"), None).await.0, 200, "someone else's request stays private");
        assert_eq!(as_session(&app, &token, "POST", "/v1/account/plan/request", Some(json!({"plan": "nope"}))).await.0, 400);
    }

    #[tokio::test]
    async fn yape_plan_requests_are_always_in_soles() {
        let (m, app) = setup().await;
        let (_, v) = sign_in(&m, &app, "51977000111", None).await;
        let token = v["session"].as_str().unwrap().to_string();
        let h = [("authorization", format!("Bearer {token}")), ("content-type", "application/json".into()), ("accept-language", "en-US".into())];
        let (st, p) = testkit::call(&app, "POST", "/v1/account/plan/request", &h, Some(serde_json::to_vec(&json!({"plan": "pro", "lang": "en"})).unwrap())).await;
        assert_eq!(st, 200, "{p}");
        assert_eq!((p["currency"].clone(), p["amount"].clone()), (json!("PEN"), json!(100.0)), "Yape and Plin move soles only: {p}");
    }

    #[test]
    fn email_and_kind_helpers() {
        assert!(valid_email("a@b.pe") && !valid_email("nope") && !valid_email("a@b"));
        assert_eq!(clean_email(Some(" A@B.PE ")).as_deref(), Some("a@b.pe"));
        assert_eq!(clean_email(Some("x")), None);
        assert_eq!(clean_role(Some("  ventas\u{7} ")).as_deref(), Some("ventas"));
        assert_eq!(clean_role(Some(" ")), None);
        assert_eq!(subject("abc"), "acct:abc");
    }
}
