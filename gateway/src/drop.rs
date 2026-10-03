//! Private links (DECISIONS D15): a business agent drops a few catalog photos
//! here and gets `https://privado.yaya.tech/<id>` back; the page lives for
//! minutes, in memory, and is then gone. The gateway keeps no copy, no
//! index, no log of who opened it — the link is the whole credential.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{agent_of, err, ApiResult, Auth, Shared};

const MAX_ITEMS: usize = 12;
const MAX_ITEM_BYTES: usize = 2 * 1024 * 1024;
const MAX_TTL_SECS: u64 = 30 * 60;
const DEFAULT_TTL_SECS: u64 = 5 * 60;
/// Ceiling on what all live drops may hold together; past it, new drops are
/// refused until old ones expire. A phone's whole catalog is a few MB.
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;

struct Item {
    name: String,
    caption: String,
    mime: String,
    bytes: Vec<u8>,
}

struct Drop {
    title: String,
    note: String,
    items: Vec<Item>,
    expires: Instant,
    expires_at: String,
}

fn store() -> &'static Mutex<HashMap<String, Drop>> {
    static S: OnceLock<Mutex<HashMap<String, Drop>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sweep(m: &mut HashMap<String, Drop>) {
    let now = Instant::now();
    m.retain(|_, d| d.expires > now);
}

fn total_bytes(m: &HashMap<String, Drop>) -> usize {
    m.values().map(|d| d.items.iter().map(|i| i.bytes.len()).sum::<usize>()).sum()
}

/// 22 url-safe characters from the CSPRNG: unguessable, no dictionary.
fn new_id() -> String {
    use rand_core::{OsRng, RngCore};
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b)
}

#[derive(Deserialize)]
struct ItemIn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    mime: String,
    b64: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropReq {
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    note: Option<String>,
    items: Vec<ItemIn>,
}

pub fn link_base() -> String {
    crate::env_or("PRIVATE_LINK_BASE", "https://privado.yaya.tech")
}

/// `POST /v1/drop` — a signed agent mints a link.
pub async fn create(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<DropReq>) -> ApiResult {
    let agent = agent_of(&app, &auth).await?;
    if req.items.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "no items"));
    }
    if req.items.len() > MAX_ITEMS {
        return Err(err(StatusCode::BAD_REQUEST, format!("at most {MAX_ITEMS} items per link")));
    }
    use base64::Engine;
    let mut items = Vec::with_capacity(req.items.len());
    for it in req.items {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(it.b64.trim())
            .map_err(|_| err(StatusCode::BAD_REQUEST, "item b64 is not base64"))?;
        if bytes.is_empty() || bytes.len() > MAX_ITEM_BYTES {
            return Err(err(StatusCode::BAD_REQUEST, "item must be 1 byte to 2 MB"));
        }
        // Raster types only: an SVG is a document that runs script on this
        // origin, and anything unknown is served as the JPEG it claims to be.
        let mime = match it.mime.trim().to_ascii_lowercase().as_str() {
            m @ ("image/jpeg" | "image/png" | "image/webp" | "image/gif" | "image/avif" | "image/heic" | "image/heif") => m.to_string(),
            _ => "image/jpeg".into(),
        };
        items.push(Item {
            name: it.name.chars().take(80).collect(),
            caption: it.caption.unwrap_or_default().chars().take(160).collect(),
            mime,
            bytes,
        });
    }
    let ttl = req.ttl_secs.unwrap_or(DEFAULT_TTL_SECS).clamp(60, MAX_TTL_SECS);
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(ttl as i64)).to_rfc3339();
    let id = new_id();
    {
        let mut m = store().lock().unwrap_or_else(|e| e.into_inner());
        sweep(&mut m);
        let incoming: usize = items.iter().map(|i| i.bytes.len()).sum();
        if total_bytes(&m) + incoming > MAX_TOTAL_BYTES {
            return Err(err(StatusCode::SERVICE_UNAVAILABLE, "private links are full right now — try again in a few minutes"));
        }
        m.insert(
            id.clone(),
            Drop {
                title: req.title.chars().take(80).collect(),
                note: req.note.unwrap_or_default().chars().take(200).collect(),
                items,
                expires: Instant::now() + Duration::from_secs(ttl),
                expires_at: expires_at.clone(),
            },
        );
    }
    tracing::info!(%agent, ttl, "private link minted");
    let url = format!("{}/{id}", link_base().trim_end_matches('/'));
    Ok(Json(json!({"id": id, "url": url, "expiresAt": expires_at, "ttlSecs": ttl})).into_response())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const NO_STORE: [(header::HeaderName, &str); 5] = [
    (header::CACHE_CONTROL, "no-store"),
    (header::HeaderName::from_static("x-robots-tag"), "noindex, nofollow, noarchive"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    // The page's own inline style and countdown, its images, nothing else.
    (header::CONTENT_SECURITY_POLICY, "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
];

fn gone() -> axum::response::Response {
    let html = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
        <title>Enlace vencido</title><body style=\"font-family:system-ui;background:#F6F5F0;color:#1B1F1C;margin:0;padding:48px 24px;text-align:center\">\
        <h1 style=\"font-size:22px\">Este enlace ya venció</h1><p style=\"color:#6D726C\">Las fotos se comparten por unos minutos. Pide otro enlace en el chat.</p></body>";
    (StatusCode::NOT_FOUND, NO_STORE, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// `GET /p/{id}` — the gallery page.
pub async fn page(Path(id): Path<String>) -> axum::response::Response {
    let m = store().lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = m.get(&id).filter(|d| d.expires > Instant::now()) else { return gone() };
    let mut figs = String::new();
    for (n, it) in d.items.iter().enumerate() {
        let cap = if it.caption.is_empty() { it.name.clone() } else if it.name.is_empty() { it.caption.clone() } else { format!("{} · {}", it.name, it.caption) };
        figs.push_str(&format!(
            "<figure><img src=\"/{id}/{n}\" alt=\"{alt}\" loading=\"lazy\"><figcaption>{cap}</figcaption></figure>",
            alt = esc(&it.name),
            cap = esc(&cap)
        ));
    }
    let html = format!(
        "<!doctype html><html lang=es><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=\"noindex,nofollow\"><title>{title}</title>\
         <style>body{{font-family:system-ui,-apple-system,sans-serif;background:#F6F5F0;color:#1B1F1C;margin:0}}\
         header{{padding:20px 20px 8px}}h1{{font-size:20px;margin:0}}p{{margin:6px 0 0;color:#6D726C;font-size:14px}}\
         main{{display:grid;grid-template-columns:repeat(auto-fill,minmax(160px,1fr));gap:10px;padding:12px 16px 40px}}\
         figure{{margin:0;background:#fff;border-radius:18px;overflow:hidden;box-shadow:0 1px 6px rgba(0,0,0,.06)}}\
         img{{display:block;width:100%;aspect-ratio:1;object-fit:cover}}figcaption{{padding:8px 10px;font-size:13px;font-weight:600}}\
         .t{{position:fixed;bottom:0;left:0;right:0;background:#0B7B5B;color:#fff;text-align:center;padding:10px;font-size:13px}}</style>\
         <header><h1>{title}</h1><p>{note}</p></header><main>{figs}</main>\
         <div class=t id=t>Este enlace vence en unos minutos</div>\
         <script>var e=new Date({exp:?}).getTime();setInterval(function(){{var s=Math.max(0,Math.round((e-Date.now())/1000));\
         document.getElementById('t').textContent=s>0?'Este enlace vence en '+Math.floor(s/60)+':'+('0'+s%60).slice(-2):'Este enlace venció';}},1000);</script></html>",
        title = esc(&d.title),
        note = esc(&d.note),
        figs = figs,
        exp = d.expires_at,
    );
    (StatusCode::OK, NO_STORE, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// `GET /p/{id}/{n}` — one image.
pub async fn image(Path((id, n)): Path<(String, usize)>) -> axum::response::Response {
    let m = store().lock().unwrap_or_else(|e| e.into_inner());
    let Some(it) = m.get(&id).filter(|d| d.expires > Instant::now()).and_then(|d| d.items.get(n)) else { return gone() };
    (StatusCode::OK, NO_STORE, [(header::CONTENT_TYPE, it.mime.clone())], it.bytes.clone()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, anon, as_agent, raw, Keypair};
    use base64::Engine;

    fn item(mime: &str, bytes: &[u8]) -> serde_json::Value {
        json!({"name": "<b>silla</b>", "caption": "roble", "mime": mime, "b64": base64::engine::general_purpose::STANDARD.encode(bytes)})
    }

    #[tokio::test]
    async fn a_link_shows_its_photos_escaped_and_nothing_else() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        assert_eq!(anon(&app, "POST", "/v1/drop", Some(json!({"items": [item("image/png", b"x")]}))).await.0, 401);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/drop", Some(json!({"items": []}))).await.0, 400);
        let many: Vec<_> = (0..=MAX_ITEMS).map(|_| item("image/png", b"x")).collect();
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/drop", Some(json!({"items": many}))).await.0, 400);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/drop", Some(json!({"items": [{"b64": "!!"}]}))).await.0, 400);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/drop", Some(json!({"title": "<script>x</script>", "ttlSecs": 5, "items": [item("image/png", b"PNG"), item("text/html", b"<h1>")]}))).await;
        assert_eq!((st, v["ttlSecs"].clone()), (200, json!(60)), "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(id.len(), 22);
        let (st, page) = raw(&app, "GET", &format!("/p/{id}"), &[], None).await;
        let page = String::from_utf8(page).unwrap();
        assert_eq!(st, 200);
        assert!(page.contains("&lt;script&gt;x") && !page.contains("<script>x") && page.contains("&lt;b&gt;silla"));
        let (st, img) = raw(&app, "GET", &format!("/p/{id}/0"), &[], None).await;
        assert_eq!((st, img), (200, b"PNG".to_vec()));
        let res = tower::ServiceExt::oneshot(crate::router(app.clone()), axum::http::Request::get(format!("/p/{id}/1")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.headers()["content-type"], "image/jpeg", "a non-image type is served as a JPEG");
        assert_eq!(raw(&app, "GET", &format!("/p/{id}/9"), &[], None).await.0, 404);
        assert_eq!(raw(&app, "GET", "/p/nope", &[], None).await.0, 404);
    }

    #[tokio::test]
    async fn scriptable_images_are_never_served_as_such() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(document.cookie)</script></svg>"#;
        let (_, v) = as_agent(&app, &kp, "POST", "/v1/drop", Some(json!({"items": [item("image/svg+xml", svg), item("image/png\r\nx: y", b"x")]}))).await;
        let id = v["id"].as_str().unwrap().to_string();
        for n in 0..2 {
            let res = tower::ServiceExt::oneshot(crate::router(app.clone()), axum::http::Request::get(format!("/p/{id}/{n}")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers()["content-type"], "image/jpeg", "item {n}");
            assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        }
        let res = tower::ServiceExt::oneshot(crate::router(app.clone()), axum::http::Request::get(format!("/p/{id}")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert!(res.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    }
}

