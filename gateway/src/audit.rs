//! Audit anchors (2026-08-30): a business agent presents the head of its
//! append-only audit chain; the gateway countersigns it with ITS clock and
//! keeps a copy. Neither side can later move an entry across an anchor: the
//! phone cannot forge the gateway's signature, the gateway never saw the
//! entries — only the hash the phone committed to.
//!
//! The signing key is the gateway's own Ed25519 keypair, generated once and
//! kept in `settings` (`audit_signing_seed`), or provided as
//! `AUDIT_SIGNING_SEED_HEX`. Its public id is published at `GET /v1/audit/key`
//! so anyone holding a phone's chain + anchors can verify both offline.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;
use yaya_wire::Keypair;

use crate::{agent_of, err, internal, ApiResult, App, Auth, Shared};

static KEY: OnceLock<Keypair> = OnceLock::new();

async fn key(app: &App) -> Result<&'static Keypair, (StatusCode, Json<serde_json::Value>)> {
    if let Some(k) = KEY.get() {
        return Ok(k);
    }
    let seed_hex = match std::env::var("AUDIT_SIGNING_SEED_HEX").ok().filter(|s| s.len() == 64) {
        Some(s) => s,
        None => {
            let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = 'audit_signing_seed'")
                .fetch_optional(&app.db)
                .await
                .map_err(internal)?;
            match row {
                Some((s,)) => s,
                None => {
                    let fresh = Keypair::generate();
                    let s = hex::encode(fresh.seed());
                    sqlx::query("INSERT INTO settings (key, value) VALUES ('audit_signing_seed', $1) ON CONFLICT (key) DO NOTHING")
                        .bind(&s)
                        .execute(&app.db)
                        .await
                        .map_err(internal)?;
                    // Another worker may have won the race: read back.
                    let (v,): (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = 'audit_signing_seed'")
                        .fetch_one(&app.db)
                        .await
                        .map_err(internal)?;
                    v
                }
            }
        }
    };
    let mut seed = [0u8; 32];
    hex::decode_to_slice(&seed_hex, &mut seed).map_err(|_| internal("bad audit signing seed"))?;
    let _ = KEY.set(Keypair::from_seed(seed));
    Ok(KEY.get().expect("set above"))
}

/// What the gateway signs: fixed field order, newline-separated.
fn statement(agent: &str, seq: i64, head: &str, phone_ts: &str, anchored_at: &str) -> Vec<u8> {
    format!("agente-audit-anchor-v1\n{agent}\n{seq}\n{head}\n{phone_ts}\n{anchored_at}").into_bytes()
}

#[derive(Deserialize)]
pub struct AnchorReq {
    seq: i64,
    head: String,
    #[serde(default)]
    ts: String,
}

/// `POST /v1/audit/anchor` — signed agent only.
pub async fn anchor(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<AnchorReq>) -> ApiResult {
    let agent = agent_of(&app, &auth).await?;
    if req.seq <= 0 || req.head.len() != 64 || !req.head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(err(StatusCode::BAD_REQUEST, "seq must be positive and head a sha256 hex"));
    }
    // A chain only grows: refuse an anchor that would move the head back.
    let last: Option<(i64,)> = sqlx::query_as("SELECT seq FROM audit_anchors WHERE agent = $1 ORDER BY seq DESC LIMIT 1")
        .bind(&agent)
        .fetch_optional(&app.db)
        .await
        .map_err(internal)?;
    if let Some((s,)) = last {
        if req.seq < s {
            return Err(err(StatusCode::CONFLICT, format!("this agent already anchored seq {s}")));
        }
    }
    let k = key(&app).await?;
    let anchored_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let sig = k.sign_hex(&statement(&agent, req.seq, &req.head, &req.ts, &anchored_at));
    // An anchor is never replaced: that would let the phone rewrite the very
    // history the countersignature pins. A retry of the same head gets the
    // stored anchor back; another head for a seq already anchored is refused.
    let stored = sqlx::query("INSERT INTO audit_anchors (agent, seq, head, phone_ts, anchored_at, sig) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (agent, seq) DO NOTHING")
        .bind(&agent)
        .bind(req.seq)
        .bind(&req.head)
        .bind(&req.ts)
        .bind(&anchored_at)
        .bind(&sig)
        .execute(&app.db)
        .await
        .map_err(internal)?
        .rows_affected();
    if stored == 0 {
        let (head, pts, at, sig): (String, String, String, String) = sqlx::query_as("SELECT head, phone_ts, anchored_at, sig FROM audit_anchors WHERE agent = $1 AND seq = $2")
            .bind(&agent).bind(req.seq).fetch_one(&app.db).await.map_err(internal)?;
        if !head.eq_ignore_ascii_case(&req.head) {
            return Err(err(StatusCode::CONFLICT, format!("seq {} is already anchored to another head", req.seq)));
        }
        return Ok(Json(json!({"agent": agent, "seq": req.seq, "head": head, "phoneTs": pts, "anchoredAt": at, "sig": sig, "key": k.id().to_string(), "duplicate": true})).into_response());
    }
    tracing::info!(%agent, seq = req.seq, "audit anchored");
    Ok(Json(json!({"agent": agent, "seq": req.seq, "head": req.head, "phoneTs": req.ts, "anchoredAt": anchored_at, "sig": sig, "key": k.id().to_string()})).into_response())
}

/// `GET /v1/audit/key` — the countersigning identity, public.
pub async fn public_key(State(app): State<Shared>) -> ApiResult {
    let k = key(&app).await?;
    Ok(Json(json!({"key": k.id().to_string(), "statement": "agente-audit-anchor-v1\\n{agent}\\n{seq}\\n{head}\\n{phoneTs}\\n{anchoredAt}"})).into_response())
}

/// `GET /v1/audit/anchors/{agent}` — what the gateway holds for one agent
/// (the console shows the owner their anchors next to the phone's chain).
pub async fn anchors(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(agent): Path<String>) -> ApiResult {
    // The agent itself, or a signed-in account that owns it.
    let allowed = match &auth {
        Auth::Proven(id) => id.to_string() == agent,
        Auth::Session(_) => {
            let acc = crate::accounts::account_of_auth(&app, &auth).await?;
            crate::accounts::account_of_agent(&app, &agent).await?.as_deref() == Some(acc.as_str())
        }
        _ => false,
    };
    if !allowed {
        return Err(err(StatusCode::FORBIDDEN, "not your agent"));
    }
    let rows: Vec<(i64, String, String, String, String)> =
        sqlx::query_as("SELECT seq, head, phone_ts, anchored_at, sig FROM audit_anchors WHERE agent = $1 ORDER BY seq DESC LIMIT 200")
            .bind(&agent)
            .fetch_all(&app.db)
            .await
            .map_err(internal)?;
    let k = key(&app).await?;
    Ok(Json(json!({
        "agent": agent, "key": k.id().to_string(),
        "anchors": rows.into_iter().map(|(seq, head, pts, at, sig)| json!({"seq": seq, "head": head, "phoneTs": pts, "anchoredAt": at, "sig": sig})).collect::<Vec<_>>(),
    })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_agent, as_session, session_for};

    fn head(c: char) -> String { std::iter::repeat(c).take(64).collect() }

    #[tokio::test]
    async fn anchors_are_countersigned_and_never_rewritten() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        let post = |seq: i64, h: String| as_agent(&app, &kp, "POST", "/v1/audit/anchor", Some(json!({"seq": seq, "head": h, "ts": "2026-09-21T10:00:00Z"})));
        for (seq, h) in [(0, head('a')), (1, "short".into()), (1, head('z'))] {
            assert_eq!(post(seq, h).await.0, 400);
        }
        let (st, v) = post(5, head('a')).await;
        assert_eq!(st, 200, "{v}");
        let (_, k) = anon(&app, "GET", "/v1/audit/key", None).await;
        assert_eq!(k["key"], v["key"]);
        let agent = kp.id().to_string();
        let msg = statement(&agent, 5, &head('a'), "2026-09-21T10:00:00Z", v["anchoredAt"].as_str().unwrap());
        // Ed25519 is deterministic: the published key's signature over the
        // documented statement is exactly what came back.
        let k = key(&app).await.unwrap();
        assert_eq!(k.id().to_string(), v["key"].as_str().unwrap());
        assert_eq!(k.sign_hex(&msg), v["sig"].as_str().unwrap(), "the countersignature covers the documented statement");

        let (st, again) = post(5, head('a')).await;
        assert_eq!((st, again["sig"].clone()), (200, v["sig"].clone()), "a retry gets the stored anchor back");
        assert_eq!(post(5, head('b')).await.0, 409, "the same seq cannot be re-anchored to another head");
        assert_eq!(post(4, head('c')).await.0, 409, "the chain only grows");
        assert_eq!(post(6, head('d')).await.0, 200);
        let (_, mine) = as_agent(&app, &kp, "GET", &format!("/v1/audit/anchors/{agent}"), None).await;
        let heads: Vec<_> = mine["anchors"].as_array().unwrap().iter().map(|a| a["head"].as_str().unwrap().chars().next().unwrap()).collect();
        assert_eq!(heads, vec!['d', 'a']);
    }

    #[tokio::test]
    async fn only_the_agent_or_its_owner_reads_the_anchors() {
        let app = testkit::app().await;
        let (kp, other) = (Keypair::generate(), Keypair::generate());
        account_with_agent(&app, "own", "51900000001", &kp).await;
        account_with_agent(&app, "else", "51900000002", &other).await;
        let path = format!("/v1/audit/anchors/{}", kp.id());
        assert_eq!(as_agent(&app, &other, "GET", &path, None).await.0, 403);
        assert_eq!(anon(&app, "GET", &path, None).await.0, 403);
        let owner = session_for(&app, "own").await;
        assert_eq!(as_session(&app, &owner, "GET", &path, None).await.0, 200);
        let stranger = session_for(&app, "else").await;
        assert_eq!(as_session(&app, &stranger, "GET", &path, None).await.0, 403);
    }
}

