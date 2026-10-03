//! Who is calling. Every request carrying `Authorization: Bearer agent:…`
//! passes through [`layer`], which checks the accompanying `X-Agent-Auth`
//! request signature (see `yaya_wire::reqsig`) and stashes the outcome as an
//! [`Auth`] extension. Handlers ask [`agent_of`] for a *proven* identity;
//! during the rollout grace window an unsigned bearer is still accepted,
//! loudly, so phones on the previous release keep working until they update.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use yaya_wire::{reqsig, AgentId};

use crate::{err, Shared};

/// Largest body the signature check will buffer. The per-route
/// `DefaultBodyLimit`s are all at or below this.
const MAX_SIGNED_BODY: usize = 25 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum Auth {
    Anonymous,
    /// Bearer present, no (valid) signature.
    Unproven(AgentId),
    /// Bearer present and the request signature verified.
    Proven(AgentId),
    /// A Yaya ID session bearer (`ysess_…`), checked against the sessions
    /// table by the handler that needs it.
    Session(String),
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub async fn layer(State(app): State<Shared>, req: Request, next: Next) -> Response {
    let (mut parts, mut body) = req.into_parts();
    let bearer = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .map(String::from);
    let auth = match bearer {
        None => Auth::Anonymous,
        Some(tok) if tok.starts_with(crate::accounts::SESSION_PREFIX) => Auth::Session(tok),
        Some(tok) => {
            let Ok(id) = tok.parse::<AgentId>() else {
                return err(StatusCode::UNAUTHORIZED, "bearer must be an agent id (agent:<ed25519 hex>) or an agente session").into_response();
            };
            match parts.headers.get(reqsig::HEADER).and_then(|v| v.to_str().ok()).map(String::from) {
                None => Auth::Unproven(id),
                Some(h) => {
                    let presented = match reqsig::parse(&h) {
                        Ok(p) => p,
                        Err(e) => return err(StatusCode::UNAUTHORIZED, e).into_response(),
                    };
                    let bytes = match axum::body::to_bytes(body, MAX_SIGNED_BODY).await {
                        Ok(b) => b,
                        Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
                    };
                    let path_q = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
                    let now = now_unix();
                    if let Err(e) = reqsig::verify(&id, &presented, parts.method.as_str(), &path_q, &yaya_wire::sha256_hex(&bytes), now, app.auth_window_secs) {
                        return err(StatusCode::UNAUTHORIZED, e).into_response();
                    }
                    if let Err(e) = app.nonces.claim(&id, presented.nonce, now) {
                        return err(StatusCode::UNAUTHORIZED, e).into_response();
                    }
                    body = Body::from(bytes);
                    Auth::Proven(id)
                }
            }
        }
    };
    parts.extensions.insert(auth);
    next.run(Request::from_parts(parts, body)).await
}

/// The caller's identity, proven — or tolerated unsigned while the grace
/// window is open. Revoked identities are refused either way.
pub async fn agent_of(app: &crate::App, auth: &Auth) -> Result<String, (StatusCode, Json<Value>)> {
    let id = match auth {
        Auth::Proven(id) => id,
        Auth::Unproven(id) if app.grace_open() => {
            tracing::warn!(agent = %id, "unsigned request accepted under AUTH_GRACE_UNTIL");
            id
        }
        Auth::Unproven(_) => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "request signature required: add X-Agent-Auth (yaya-wire reqsig v1)", "type": "auth"}})),
            ))
        }
        Auth::Session(_) => return Err(err(StatusCode::UNAUTHORIZED, "this endpoint needs an agent identity, not a web session")),
        Auth::Anonymous => return Err(err(StatusCode::UNAUTHORIZED, "missing bearer token")),
    };
    let revoked: Option<(Option<String>,)> = sqlx::query_as("SELECT revoked_at FROM agents WHERE agent = $1")
        .bind(id.to_string())
        .fetch_optional(&app.db)
        .await
        .map_err(crate::internal)?;
    if let Some((Some(_),)) = revoked {
        return Err(err(StatusCode::UNAUTHORIZED, "agent revoked"));
    }
    Ok(id.to_string())
}

/// A proven agent that belongs to a real user: the account it is linked to.
/// While the rollout grace window is open, unlinked agents (previous app
/// builds) are tolerated and `None` comes back.
pub async fn require_linked(app: &crate::App, auth: &Auth) -> Result<(String, Option<String>), (StatusCode, Json<Value>)> {
    let agent = agent_of(app, auth).await?;
    match crate::accounts::account_of_agent(app, &agent).await? {
        Some(acct) => Ok((agent, Some(acct))),
        // A declared guest: the receptionist without the network.
        None if crate::guests::is_guest(app, &agent).await? => Ok((agent, None)),
        None if app.grace_open() => {
            tracing::warn!(%agent, "unlinked agent accepted under AUTH_GRACE_UNTIL");
            Ok((agent, None))
        }
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "sign in with your agente account", "type": "account"}})),
        )),
    }
}
