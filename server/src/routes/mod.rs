use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{agents, SharedState};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub fn router(state: SharedState) -> Router {
    let api = Router::new()
        .route("/api/onboard_business", post(onboard_business))
        .route("/api/onboarding_message", post(onboarding_message))
        .route("/api/execute_action", post(execute_action))
        .route("/api/payment_event", post(payment_event))
        .route("/api/payment_sources", get(payment_sources))
        .route(
            "/api/voice_message",
            post(voice_message).layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
        .route(
            "/api/catalog_photo",
            post(catalog_photo).layer(axum::extract::DefaultBodyLimit::max(15 * 1024 * 1024)),
        )
        .route("/api/dashboard", get(dashboard))
        .route("/api/appointments", get(list_appointments))
        // D15: the owner's work queue, the catalog's photos, the UI spec.
        .route("/api/orders/{id}", post(order_status))
        .route("/api/appointments/{id}", post(appointment_status))
        .route("/api/ui", get(ui_get))
        .route("/api/audit", get(audit_list))
        .route("/api/audit/verify", get(audit_verify))
        .route("/api/audit/anchor", post(audit_anchor))
        .route(
            "/api/media",
            get(media_list).post(media_put).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/api/media/share", post(media_share))
        .route("/api/media/{id}", get(media_get).post(media_update))
        .route("/api/media/{id}/delete", post(media_delete))
        .route("/api/answer_gap", post(answer_gap))
        .route("/api/verify/start", post(verify_start))
        .route("/api/verify/check", post(verify_check))
        .route("/api/wa/relay/send_otp", post(wa_relay::send_otp_relay))
        .route("/api/kernel", get(kernel_inspect))
        .route("/api/agent", get(agent_card))
        .route("/api/agent/publish", post(agent_publish))
        .route("/api/location", post(set_location))
        .route("/api/plan", get(plan_get))
        // Prepaid credits (agente/docs/CREDITS.md).
        .route("/api/credits", get(credits_get))
        .route("/api/topup/session", post(topup_session))
        .route("/api/wallets", get(wallets_get))
        .route("/api/categories", get(categories_get))
        .route("/api/outcomes/{id}/reverse", post(outcome_reverse))
        .route("/api/contacts", get(contacts_list))
        .route("/api/agro/participants", get(agro_participants))
        .route("/api/ops", get(ops_get).post(ops_set))
        .route("/api/ops/secret", post(ops_rotate))
        .route("/api/ops/test", post(ops_test))
        .route("/api/ops/events", get(ops_events))
        .route("/api/contacts/{id}", post(contact_update))
        .route("/api/conversations", get(conversations))
        .route("/api/conversations/{peer}", get(conversation))
        .route("/api/rails", get(rails_catalog))
        .route("/api/payout", get(payout_get).post(payout_set))
        .route("/api/mesh/card", get(mesh::a2a_card))
        // Routes that control the device itself: only this machine may call
        // them (see `require_local`), whatever key a caller holds.
        .merge(
            Router::new()
                .route("/api/agent/device", post(agent_device))
                .route("/api/pair/start", post(pair_start))
                .route("/api/pair/code", get(pair_code))
                .route("/api/assistant/message", post(assistant_message))
                .route("/api/assistant/history", get(assistant_history))
                .route("/api/assistant/reset", post(assistant_reset))
                .route("/api/account", get(account_status))
                .route("/api/account/otp/start", post(account_otp_start))
                .route("/api/account/otp/check", post(account_otp_check))
                .route("/api/account/logout", post(account_logout))
                .route("/api/account/plan/request", post(account_plan_request))
                .route("/api/account/plan/request/{ref}", get(account_plan_request_status))
                .route("/api/account/adopt", post(account_adopt))
                .route("/api/node/message", post(node_message))
                .route("/api/node/turn", post(node_turn))
                .route("/api/node/call_ended", post(node_call_ended))
                .route("/api/account/guest", post(account_guest))
                .route("/api/account/share", post(account_share))
                .route("/api/backup", post(backup_now))
                .route("/api/mesh", get(mesh::status))
                .route("/api/mesh/register", post(mesh::register))
                .route("/api/mesh/link", post(mesh::link))
                .route("/api/mesh/invite", post(mesh::invite))
                .route("/api/mesh/accept", post(mesh::accept))
                .route("/api/mesh/decline", post(mesh::decline))
                .route("/api/mesh/peers/{agent}", axum::routing::delete(mesh::forget))
                .route("/api/mesh/config", get(mesh::config))
                .route("/api/mesh/apply", post(mesh::apply))
                .route("/api/coins", get(coins::status))
                .route("/api/coins/keys", post(coins::keys))
                .route("/api/coins/withdraw", post(coins::withdraw))
                .route("/api/coins/pay", post(coins::pay))
                .route("/api/coins/receive", post(coins::receive))
                .route("/api/coins/refresh", post(coins::refresh))
                .route("/api/coins/deposit", post(coins::deposit))
                .route("/api/restore", post(restore_latest))
                .layer(axum::middleware::from_fn(require_local)),
        )
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_app_key));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        // Founder's browser has the admin key, not the app key; require_admin
        // inside each handler is the whole gate.
        .route("/api/admin/metrics", get(admin_metrics))
        .route("/api/admin/plan", post(admin_set_plan))
        .merge(api)
        .with_state(state)
}

use yaya_wire::secret::ct_eq;

#[axum::debug_middleware]
/// Every /api request must present the app key (compiled into the APK). Device
/// bearer tokens still authorize per-business on top of this — the app key only
/// proves the caller is our client at all, and since it ships inside the APK it
/// is anti-noise, never authentication.
///
/// The admin key is deliberately NOT accepted here. It used to be a full
/// substitute, which meant a single leaked ops key opened every tenant route;
/// it now authorizes only the curator and kernel endpoints, via `require_admin`.
async fn require_app_key(
    State(state): State<SharedState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Owned copy inside a block: borrowing req across the await would
    // make this future !Send (Body is !Sync).
    let app = {
        let h = req.headers();
        h.get("x-app-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    if ct_eq(&app, &state.app_key) {
        return next.run(req).await;
    }
    tracing::warn!(%app, "require_app_key: mismatch (tmp debug 2026-09-10)");
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or bad app key"}))).into_response()
}

fn err(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (code, Json(json!({"error": msg.to_string()})))
}

/// Logs the real error and hands the caller an opaque reference to it.
///
/// The detail is genuinely useful to us and genuinely dangerous to publish:
/// sqlx errors name tables and constraints, reqwest errors name upstream hosts,
/// and the LLM client embeds the provider's whole response body. Quote the
/// `ref` from a user's report and grep the log for it.
fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    let reference = Uuid::new_v4();
    tracing::error!(%reference, "internal error: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal error", "ref": reference})),
    )
}

/// An agent turn failed. Two failures are the caller's to act on, not ours
/// to hide: the gateway wants a signed-in account (401, `account`), or the
/// day's allowance is spent (429). Everything else is an internal error.
fn agent_err(e: anyhow::Error) -> (StatusCode, Json<Value>) {
    let m = e.to_string();
    if m.contains("\"type\":\"account\"") {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "account", "message": "sign in with your Yaya account"})));
    }
    if m.contains("\"type\":\"allowance\"") {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": "allowance", "message": "daily allowance exhausted"})));
    }
    internal(e)
}

/// Best-effort TTS for voice-first onboarding: text goes out regardless.
/// Metered per business — synthesis is local CPU rather than an API bill, but
/// it is still work an unthrottled caller could conscript.
async fn tts_b64(state: &SharedState, business_id: Uuid, text: &str) -> Value {
    if let Err(e) = crate::limits::charge(&state.db, business_id, crate::limits::Meter::Tts).await {
        tracing::warn!("tts skipped: {e}");
        return Value::Null;
    }
    let lang = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map(|c| crate::locale::Locale::from_values(&c.values).language)
        .unwrap_or_else(|_| "es".into());
    match crate::voice::synthesize(state.yaya_audio.as_ref(), state.audio.as_ref(), text, &lang).await {
        Ok(wav) => {
            use base64::Engine;
            json!(base64::engine::general_purpose::STANDARD.encode(wav))
        }
        Err(e) => {
            tracing::warn!("tts failed: {e}");
            Value::Null
        }
    }
}

/// For routes only the phone's own shell (or wa-node) may call. The app
/// key proves nothing on a public node: every APK carries the relay's.
///
/// Local = a loopback peer that is not a reverse proxy relaying someone
/// else: on the node, Caddy on the same host forwards the internet with
/// `X-Forwarded-For`, so a loopback peer alone proves nothing there.
fn local_request(peer: &std::net::SocketAddr, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let proxied = ["x-forwarded-for", "forwarded", "x-real-ip"].iter().any(|h| headers.contains_key(*h));
    if peer.ip().is_loopback() && !proxied {
        Ok(())
    } else {
        tracing::warn!(%peer, "device-only route refused: not on the loopback");
        Err(err(StatusCode::FORBIDDEN, "only available on this device"))
    }
}

/// Layer for device-only routes (pairing, attestation, account, backups,
/// coins, mesh control, the node and assistant brains).
async fn require_local(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let peer = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    let verdict = match peer {
        Some(p) => local_request(&p, req.headers()),
        None => Err(err(StatusCode::FORBIDDEN, "only available on this device")),
    };
    match verdict {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_owned)
}

/// Resolves the device token to the business it belongs to.
///
/// Only the SHA-256 of the token is stored, so a database dump or backup is no
/// longer a set of working credentials. Hashing happens in Postgres rather than
/// Rust so the comparison stays a plain indexed lookup. The tokens are 40
/// characters from a CSPRNG, which is far too much entropy to enumerate — no
/// salt or password KDF is warranted.
async fn auth(state: &SharedState, headers: &HeaderMap) -> Result<Uuid, (StatusCode, Json<Value>)> {
    let token =
        bearer(headers).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT business_id FROM devices \
         WHERE token_hash = $1 \
           AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > $2)",
    )
    .bind(crate::db::sha256_hex(token.as_bytes()))
    .bind(crate::db::now())
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;
    row.map(|r| r.0)
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid device token"))
}

/// The prompt window: the last 60 turns, oldest first. The full conversation
/// stays in `messages` — this only bounds what the model re-reads.
pub(crate) async fn load_history(
    state: &crate::AppState,
    business_id: Uuid,
    agent_type: &str,
    peer: &str,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM ( \
            SELECT role, content, idx FROM messages \
            WHERE business_id = $1 AND agent_type = $2 AND peer = $3 \
            ORDER BY idx DESC LIMIT 60 \
         ) t ORDER BY idx",
    )
    .bind(business_id)
    .bind(agent_type)
    .bind(peer)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect(),
    ))
}

/// Appends one message row and returns (id, idx). The predecessor was a
/// read-modify-write of one JSONB blob: two concurrent turns from the same
/// peer meant the second write silently erased the first turn. Here the race
/// surfaces as a unique-constraint violation on idx and simply retries.
pub(crate) async fn append_message(
    state: &crate::AppState,
    business_id: Uuid,
    agent_type: &str,
    peer: &str,
    role: &str,
    content: &str,
) -> Result<(Uuid, i32), (StatusCode, Json<Value>)> {
    for _ in 0..3 {
        let r: Result<(Uuid, i32), sqlx::Error> = sqlx::query_as(
            "INSERT INTO messages (id, business_id, agent_type, peer, idx, role, content) \
             SELECT $6, $1, $2, $3, COALESCE(MAX(idx) + 1, 0), $4, $5 FROM messages \
             WHERE business_id = $1 AND agent_type = $2 AND peer = $3 \
             RETURNING id, idx",
        )
        .bind(business_id)
        .bind(agent_type)
        .bind(peer)
        .bind(role)
        .bind(content)
        .bind(Uuid::new_v4())
        .fetch_one(&state.db)
        .await;
        match r {
            Ok(row) => return Ok(row),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => continue,
            Err(e) => return Err(internal(e)),
        }
    }
    Err(internal("message append kept colliding"))
}

// The handlers, one module per concern. Each sees this module's helpers
// through `use super::*`; this module sees their handlers through the
// globs below, which is all `router()` needs.
mod mesh;
mod coins;
mod account;
mod admin;
mod agent;
mod assistant;
mod audit;
mod catalog;
mod crm;
mod customer;
mod dashboard;
mod media;
mod node;
mod onboarding;
mod ops;
mod pair;
mod payments;
mod payout;
mod plans;
mod queue;
mod verify;
mod wa_relay;

use account::*;
use admin::*;
use agent::*;
use assistant::*;
use audit::*;
use catalog::*;
use crm::*;
pub use customer::customer_turn;
pub(crate) use crm::{conversation_json, conversations_json};
pub(crate) use dashboard::dashboard_json;
use customer::*;
use dashboard::*;
use media::*;
use node::*;
use onboarding::*;
use ops::*;
use pair::*;
use payments::*;
use payout::*;
use plans::*;
use queue::*;
use verify::*;

#[cfg(test)]
mod exposure_tests {
    use crate::testkit::{self, APP_KEY};
    use serde_json::json;

    /// Routes that control the device itself. On a node, Caddy proxies
    /// /api/* from the internet and the app key ships in APKs, so these must
    /// answer only callers on this machine — never a proxied request.
    const DEVICE_ONLY: &[(&str, &str)] = &[
        ("POST", "/api/pair/start"), ("GET", "/api/pair/code"), ("POST", "/api/agent/device"),
        ("GET", "/api/account"), ("POST", "/api/account/otp/start"), ("POST", "/api/account/otp/check"),
        ("POST", "/api/account/logout"), ("POST", "/api/account/plan/request"), ("GET", "/api/account/plan/request/R1"),
        ("POST", "/api/account/adopt"), ("POST", "/api/account/guest"), ("POST", "/api/account/share"),
        ("POST", "/api/backup"), ("POST", "/api/restore"),
        ("GET", "/api/coins"), ("POST", "/api/coins/keys"), ("POST", "/api/coins/withdraw"), ("POST", "/api/coins/pay"),
        ("POST", "/api/coins/receive"), ("POST", "/api/coins/refresh"), ("POST", "/api/coins/deposit"),
        ("GET", "/api/mesh"), ("POST", "/api/mesh/register"), ("POST", "/api/mesh/link"), ("POST", "/api/mesh/invite"),
        ("POST", "/api/mesh/accept"), ("POST", "/api/mesh/decline"), ("DELETE", "/api/mesh/peers/agent:x"),
        ("GET", "/api/mesh/config"), ("POST", "/api/mesh/apply"),
        ("POST", "/api/node/message"), ("POST", "/api/node/turn"), ("POST", "/api/node/call_ended"),
        ("POST", "/api/assistant/message"), ("GET", "/api/assistant/history"), ("POST", "/api/assistant/reset"),
    ];

    #[tokio::test]
    async fn device_routes_refuse_remote_and_proxied_callers() {
        let s = testkit::state().await;
        let remote: std::net::SocketAddr = ([203, 0, 113, 9], 443).into();
        let local: std::net::SocketAddr = ([127, 0, 0, 1], 5000).into();
        for (m, p) in DEVICE_ONLY {
            let body = Some(json!({}));
            let (st, _) = testkit::call_from(&s, remote, m, p, &[("x-app-key", APP_KEY)], body.clone()).await;
            assert_eq!(st, 403, "{m} {p} from the internet");
            // Caddy on the same host: loopback peer, but a forwarded request.
            let (st, _) = testkit::call_from(&s, local, m, p, &[("x-app-key", APP_KEY), ("x-forwarded-for", "203.0.113.9")], body.clone()).await;
            assert_eq!(st, 403, "{m} {p} through the reverse proxy");
            for h in ["forwarded", "x-real-ip"] {
                let (st, _) = testkit::call_from(&s, local, m, p, &[("x-app-key", APP_KEY), (h, "for=203.0.113.9")], body.clone()).await;
                assert_eq!(st, 403, "{m} {p} with {h}");
            }
            // The phone shell on the loopback still gets through the gate.
            let (st, _) = testkit::call_from(&s, local, m, p, &[("x-app-key", APP_KEY)], body).await;
            assert_ne!(st, 403, "{m} {p} from the device itself");
        }
    }

    /// And the routes a node legitimately serves remotely stay reachable.
    #[tokio::test]
    async fn remote_routes_stay_remote() {
        let s = testkit::state().await;
        let remote: std::net::SocketAddr = ([203, 0, 113, 9], 443).into();
        for (m, p) in [("GET", "/api/agent"), ("GET", "/api/rails"), ("GET", "/api/categories"), ("POST", "/api/wa/relay/send_otp"), ("POST", "/api/onboard_business"), ("GET", "/api/dashboard"), ("GET", "/api/mesh/card")] {
            let (st, _) = testkit::call_from(&s, remote, m, p, &[("x-app-key", APP_KEY), ("x-forwarded-for", "203.0.113.9")], Some(json!({}))).await;
            assert_ne!(st, 403, "{m} {p}");
        }
    }
}
