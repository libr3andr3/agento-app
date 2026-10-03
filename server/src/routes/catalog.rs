//! The catalog's photos and the owner's UI spec (D15).

use super::*;
use axum::extract::Path;

fn parse_id(id: &str) -> Result<Uuid, (StatusCode, Json<Value>)> {
    Uuid::parse_str(id).map_err(|_| err(StatusCode::BAD_REQUEST, "bad id"))
}

/// `GET /api/ui` — what the app draws. Also inside `/api/dashboard`.
pub(super) async fn ui_get(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let composed = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "ui": composed.doc["_ui"],
        "uiDesigned": composed.doc["_uiDesigned"],
        "businessKind": composed.values["businessKind"],
    })))
}

/// `GET /api/media` — the photos, without bytes.
pub(super) async fn media_list(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let media = crate::media::list(&state.db, business_id).await.map_err(internal)?;
    Ok(Json(json!({"media": media})))
}

#[derive(Deserialize)]
pub(super) struct MediaQ {
    product: Option<String>,
    caption: Option<String>,
}

/// `POST /api/media?product=&caption=` — raw image bytes in, id out.
pub(super) async fn media_put(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<MediaQ>,
    body: axum::body::Bytes,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let mime = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .filter(|m| m.starts_with("image/"))
        .unwrap_or("image/jpeg")
        .to_string();
    let id = crate::media::store(&state.db, business_id, q.product.as_deref(), q.caption.as_deref(), &mime, &body)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({"id": id, "size": body.len()})))
}

/// `GET /api/media/{id}` — the bytes.
pub(super) async fn media_get(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let business_id = auth(&state, &headers).await?;
    let id = parse_id(&id)?;
    let Some((mime, bytes)) = crate::media::bytes(&state.db, business_id, id).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, "no such photo"));
    };
    Ok((
        StatusCode::OK,
        [("content-type", mime), ("cache-control", "private, max-age=86400".to_string())],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
pub(super) struct MediaPatch {
    product: Option<String>,
    caption: Option<String>,
}

/// `POST /api/media/{id}` — rename the product / caption.
pub(super) async fn media_update(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<MediaPatch>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let id = parse_id(&id)?;
    let ok = crate::media::update(&state.db, business_id, id, req.product.as_deref(), req.caption.as_deref())
        .await
        .map_err(internal)?;
    if !ok {
        return Err(err(StatusCode::NOT_FOUND, "no such photo"));
    }
    Ok(Json(json!({"id": id, "status": "ok"})))
}

/// `POST /api/media/{id}/delete`.
pub(super) async fn media_delete(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let id = parse_id(&id)?;
    let ok = crate::media::delete(&state.db, business_id, id).await.map_err(internal)?;
    if !ok {
        return Err(err(StatusCode::NOT_FOUND, "no such photo"));
    }
    Ok(Json(json!({"id": id, "status": "deleted"})))
}

#[derive(Deserialize)]
pub(super) struct ShareReq {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    products: Vec<String>,
    note: Option<String>,
}

/// `POST /api/media/share` — the owner mints a private link from the
/// Catálogo tab (to paste into any chat). Same path the customer agent uses.
pub(super) async fn media_share(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ShareReq>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let ids: Vec<Uuid> = req.ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect();
    let v = crate::media::share(&state, business_id, &ids, &req.products, req.note.as_deref())
        .await
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    if v["status"] == "no_photos" {
        return Err(err(StatusCode::NOT_FOUND, "no photos to share"));
    }
    crate::audit::record(&state, Some(business_id), crate::audit::kind::SHARE, "owner", "", json!({
        "photos": v["photos"], "products": v["products"], "url": v["url"], "expiresAt": v["expiresAt"],
    })).await;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, Mock, APP_KEY};
    use serde_json::{json, Value};

    async fn put(s: &crate::SharedState, t: &str, path: &str, mime: &str, bytes: Vec<u8>) -> (u16, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder().method("POST").uri(path)
            .header("x-app-key", APP_KEY).header("authorization", format!("Bearer {t}")).header("content-type", mime)
            .body(axum::body::Body::from(bytes)).unwrap();
        let mut req = req;
        req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
        let r = crate::routes::router(s.clone()).oneshot(req).await.unwrap();
        let st = r.status().as_u16();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn photos_upload_list_fetch_rename_share_delete() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        let (st, v) = put(&s, &t, "/api/media?product=Polo%20Yaya&caption=azul", "image/png", vec![0x89, b'P', b'N', b'G']).await;
        assert_eq!((st, v["size"].clone()), (200, json!(4)));
        let id = v["id"].as_str().unwrap().to_string();
        // Non-image content types are stored as jpeg; empty bodies refused.
        assert_eq!(put(&s, &t, "/api/media", "text/html", vec![1]).await.0, 200);
        assert_eq!(put(&s, &t, "/api/media", "image/jpeg", vec![]).await.0, 400);
        let (_, v) = testkit::api(&s, "GET", "/api/media", Some(&t), None).await;
        assert_eq!(v["media"].as_array().unwrap().len(), 2);
        assert!(v["media"].as_array().unwrap().iter().all(|m| m.get("bytes").is_none()));
        assert!(v["media"].as_array().unwrap().iter().any(|m| m["mime"] == "image/jpeg"));
        let (st, raw) = testkit::api(&s, "GET", &format!("/api/media/{id}"), Some(&t), None).await;
        assert_eq!(st, 200);
        let _ = raw;
        assert_eq!(testkit::api(&s, "POST", &format!("/api/media/{id}"), Some(&t), Some(json!({"caption": "roja"}))).await.1["status"], "ok");
        m.on("/v1/drop", json!({"url": "https://drop/x", "expiresAt": "t"}));
        let (st, v) = testkit::api(&s, "POST", "/api/media/share", Some(&t), Some(json!({"products": ["polo"]}))).await;
        assert_eq!((st, v["url"].clone()), (200, json!("https://drop/x")));
        assert_eq!(testkit::api(&s, "POST", "/api/media/share", Some(&t), Some(json!({"products": ["zapatos"]}))).await.0, 404);
        assert_eq!(testkit::api(&s, "POST", &format!("/api/media/{id}/delete"), Some(&t), None).await.1["status"], "deleted");
        assert_eq!(testkit::api(&s, "GET", &format!("/api/media/{id}"), Some(&t), None).await.0, 404);
        assert_eq!(testkit::api(&s, "POST", &format!("/api/media/{id}/delete"), Some(&t), None).await.0, 404);
        assert_eq!(testkit::api(&s, "POST", &format!("/api/media/{id}"), Some(&t), Some(json!({"caption": "x"}))).await.0, 404);
        assert_eq!(testkit::api(&s, "GET", "/api/media/nope", Some(&t), None).await.0, 400);
        let shares: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE kind = 'share'").fetch_one(&s.db).await.unwrap();
        assert_eq!(shares, 1);
    }

    #[tokio::test]
    async fn ui_spec() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        let (st, v) = testkit::api(&s, "GET", "/api/ui", Some(&t), None).await;
        assert_eq!(st, 200);
        assert_eq!(v["ui"]["version"], 1);
        assert_eq!(testkit::api(&s, "GET", "/api/ui", None, None).await.0, 401);
    }
}
