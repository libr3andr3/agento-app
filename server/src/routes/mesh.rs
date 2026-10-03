//! `/api/mesh/*` — the owner's controls for the post-quantum p2p VPN
//! (app-key routes: the phone's own app, the Linux runtime's CLI, the console
//! through the owner relay).

use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::{mesh, SharedState};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()})))
}

pub(super) async fn status(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(mesh::status(&state).await))
}

pub(super) async fn register(State(state): State<SharedState>) -> ApiResult {
    if let Err(e) = mesh::register(&state).await { tracing::warn!(error = %e, "mesh register failed"); return Err(bad(e)) }
    mesh::apply(&state).await;
    Ok(Json(mesh::status(&state).await))
}

#[derive(serde::Deserialize)]
pub(super) struct LinkReq {
    peer: String,
    #[serde(default)] direct: bool,
}

pub(super) async fn link(State(state): State<SharedState>, Json(req): Json<LinkReq>) -> ApiResult {
    let peer = resolve(&state, &req.peer).await.map_err(bad)?;
    Ok(Json(mesh::link(&state, &peer, req.direct).await.map_err(bad)?))
}

#[derive(serde::Deserialize)]
pub(super) struct PeerReq { peer: String }

pub(super) async fn invite(State(state): State<SharedState>, Json(req): Json<PeerReq>) -> ApiResult {
    let peer = resolve(&state, &req.peer).await.map_err(bad)?;
    mesh::invite(&state, &peer).await.map_err(bad)?;
    Ok(Json(mesh::status(&state).await))
}

pub(super) async fn accept(State(state): State<SharedState>, Json(req): Json<PeerReq>) -> ApiResult {
    let peer = resolve(&state, &req.peer).await.map_err(bad)?;
    Ok(Json(mesh::accept(&state, &peer).await.map_err(bad)?))
}

pub(super) async fn decline(State(state): State<SharedState>, Json(req): Json<PeerReq>) -> ApiResult {
    let peer = resolve(&state, &req.peer).await.map_err(bad)?;
    Ok(Json(mesh::decline(&state, &peer).await.map_err(bad)?))
}

pub(super) async fn forget(State(state): State<SharedState>, Path(agent): Path<String>) -> ApiResult {
    Ok(Json(mesh::forget(&state, &agent).await.map_err(bad)?))
}

/// The wg-quick file (private key inside — app key only).
pub(super) async fn config(State(state): State<SharedState>) -> impl axum::response::IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], mesh::config(&state, false).await)
}

/// Linux: bring the tunnel up / sync it now (`yaya mesh up`).
pub(super) async fn apply(State(state): State<SharedState>) -> ApiResult {
    let path = std::path::PathBuf::from(mesh::status(&state).await["confPath"].as_str().unwrap_or_default());
    let conf = mesh::config(&state, false).await;
    if let Some(d) = path.parent() { let _ = std::fs::create_dir_all(d); }
    // The file holds the WireGuard private key: owner-only, like mesh::apply writes it.
    mesh::write_private(&path, &conf).map_err(bad)?;
    let msg = mesh::apply_linux(&path, &mesh::config(&state, true).await).await.map_err(bad)?;
    mesh::ensure_a2a(state.clone());
    Ok(Json(json!({"ok": true, "message": msg, "status": mesh::status(&state).await})))
}

/// The A2A card as seen on the mesh.
pub(super) async fn a2a_card(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(mesh::agent_card_json(&state).await))
}

/// "@handle", "urn:agent:yaya:x" or "agent:hex" → agent id.
async fn resolve(state: &SharedState, who: &str) -> anyhow::Result<String> {
    let who = who.trim();
    if who.starts_with("agent:") {
        return Ok(who.to_string());
    }
    let handle = who.trim_start_matches("urn:agent:yaya:").trim_start_matches('@');
    let rec = crate::network::registry_get(state, &format!("/v1/index/urn:agent:yaya:{handle}")).await?;
    rec["agent_id"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("unknown agent '{who}'"))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn the_config_file_is_private_even_when_applying_fails() {
        let s = testkit::state().await;
        let path = std::path::PathBuf::from(crate::mesh::status(&s).await["confPath"].as_str().unwrap());
        let _ = std::fs::remove_file(&path); // a first apply creates the file
        let (_st, _) = api(&s, "POST", "/api/mesh/apply", None, None).await; // wg-quick is absent here
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "the private key is not world-readable");
        }
    }

    #[tokio::test]
    async fn owner_mesh_controls() {
        let m = Mock::start().await;
        m.on("/v1/mesh/info", json!({})).on("/v1/mesh/register", json!({"ip": "10.77.3.3"})).on("/v1/index/urn:agent:yaya:bodega", json!({"agent_id": "agent:bodega"}));
        let s = testkit::state_on(&m).await;
        let (st, v) = api(&s, "GET", "/api/mesh", None, None).await;
        assert_eq!((st, v["ip"].clone()), (200, json!(null)));
        let (st, v) = api(&s, "POST", "/api/mesh/register", None, None).await;
        assert_eq!((st, v["ip"].clone()), (200, json!("10.77.3.3")));
        assert_eq!(api(&s, "POST", "/api/mesh/invite", None, Some(json!({"peer": "@bodega"}))).await.1["peers"][0]["agent"], "agent:bodega");
        assert_eq!(api(&s, "POST", "/api/mesh/invite", None, Some(json!({"peer": "@nadie"}))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/mesh/accept", None, Some(json!({"peer": "agent:bodega"}))).await.0, 400, "no offer to accept");
        assert_eq!(api(&s, "POST", "/api/mesh/decline", None, Some(json!({"peer": "agent:bodega"}))).await.1["peers"][0]["status"], "declined");
        assert_eq!(api(&s, "DELETE", "/api/mesh/peers/agent:bodega", None, None).await.1["peers"], json!([]));
        let (st, conf) = api(&s, "GET", "/api/mesh/config", None, None).await;
        assert_eq!(st, 200);
        assert!(conf.as_str().unwrap().starts_with("[Interface]"));
        assert_eq!(api(&s, "GET", "/api/mesh/card", None, None).await.1["yaya"]["agent"], json!(s.identity.id()));
        assert_eq!(api(&s, "POST", "/api/mesh/link", None, Some(json!({"peer": "agent:zz"}))).await.0, 400);
    }
}
