//! Gateway test fixtures: scratch dirs on disk, the real router in-process,
//! agent-signed and session requests, and a scriptable fake upstream.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::{body::Bytes, extract::State, http::{HeaderMap, Method, StatusCode, Uri}, response::IntoResponse, Router};
use serde_json::{json, Value};
pub use yaya_wire::Keypair;

use crate::Shared;

/// A fresh directory under the crate's target dir (never the tmpfs /tmp).
pub fn scratch(what: &str) -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join(format!("{what}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub const ADMIN_KEY: &str = "test-admin-key-test-admin-key-test";
pub const COLLECTOR_KEY: &str = "test-collector-key-test-collector-key";

pub async fn app() -> Shared {
    Arc::new(crate::test_app().await)
}

/// `app` with fields changed (e.g. `|a| App { free_cap: 1, ..a }`).
pub async fn app_with(f: impl FnOnce(crate::App) -> crate::App) -> Shared {
    Arc::new(f(crate::test_app().await))
}

/// A raw call through the whole router (auth layer included).
pub async fn call(app: &Shared, method: &str, path: &str, headers: &[(&str, String)], body: Option<Vec<u8>>) -> (u16, Value) {
    let (status, bytes) = raw(app, method, path, headers, body).await;
    (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
}

/// Like [`call`], with the response body as bytes (downloads).
pub async fn raw(app: &Shared, method: &str, path: &str, headers: &[(&str, String)], body: Option<Vec<u8>>) -> (u16, Vec<u8>) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(path);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let mut req = req.body(axum::body::Body::from(body.unwrap_or_default())).unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 40000))));
    let resp = crate::router(app.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, axum::body::to_bytes(resp.into_body(), 32 << 20).await.unwrap().to_vec())
}

/// The auth headers of a request signed as `kp` over `body`.
pub fn agent_headers(kp: &Keypair, method: &str, path: &str, body: &[u8]) -> Vec<(&'static str, String)> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    vec![("authorization", format!("Bearer {}", kp.id())), (yaya_wire::reqsig::HEADER, yaya_wire::reqsig::sign(kp, method, path, Some(body), now))]
}

/// A request signed as `kp` (bearer + X-Agent-Auth over method, path, body).
pub async fn as_agent(app: &Shared, kp: &Keypair, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let bytes = body.map(|b| serde_json::to_vec(&b).unwrap());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let sig = yaya_wire::reqsig::sign(kp, method, path, Some(bytes.as_deref().unwrap_or(b"")), now);
    let mut h = vec![("authorization", format!("Bearer {}", kp.id())), (yaya_wire::reqsig::HEADER, sig)];
    if bytes.is_some() {
        h.push(("content-type", "application/json".into()));
    }
    call(app, method, path, &h, bytes).await
}

pub async fn as_admin(app: &Shared, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut h = vec![("x-admin-key", ADMIN_KEY.to_string())];
    if body.is_some() {
        h.push(("content-type", "application/json".into()));
    }
    call(app, method, path, &h, body.map(|b| serde_json::to_vec(&b).unwrap())).await
}

pub async fn anon(app: &Shared, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let h = if body.is_some() { vec![("content-type", "application/json".to_string())] } else { vec![] };
    call(app, method, path, &h, body.map(|b| serde_json::to_vec(&b).unwrap())).await
}

/// An account (id, email) with `kp` linked to it as a phone agent.
pub async fn account_with_agent(app: &Shared, id: &str, phone: &str, kp: &Keypair) {
    sqlx::query("INSERT INTO accounts (id, email, password_hash, phone, country) VALUES ($1, $2, '-', $3, 'PE')")
        .bind(id).bind(format!("{id}@test.pe")).bind(phone).execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO account_agents (agent, account) VALUES ($1, $2)").bind(kp.id().to_string()).bind(id).execute(&app.db).await.unwrap();
}

/// A web session bearer for `account`.
pub async fn session_for(app: &Shared, account: &str) -> String {
    let token = format!("{}{}", crate::accounts::SESSION_PREFIX, uuid::Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash, account, kind, expires_at) VALUES ($1, $2, 'web', $3)")
        .bind(yaya_wire::sha256_hex(token.as_bytes())).bind(account).bind((chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339())
        .execute(&app.db).await.unwrap();
    token
}

pub async fn as_session(app: &Shared, token: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut h = vec![("authorization", format!("Bearer {token}"))];
    if body.is_some() {
        h.push(("content-type", "application/json".into()));
    }
    call(app, method, path, &h, body.map(|b| serde_json::to_vec(&b).unwrap())).await
}

// ------------------------------------------------------------- fake upstream

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
    pub raw: Vec<u8>,
}

#[derive(Default)]
struct MockInner {
    seen: Vec<Seen>,
    /// Per path part: (status, body, extra response headers, served).
    routes: Vec<(String, VecDeque<(u16, Value, Vec<(String, String)>, bool)>)>,
}

#[derive(Clone)]
pub struct Mock {
    pub base: String,
    inner: Arc<Mutex<MockInner>>,
}

impl Mock {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(MockInner::default()));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        Self { base: format!("http://{addr}"), inner }
    }

    pub fn on(&self, part: &str, body: Value) -> &Self {
        self.on_status(part, 200, body)
    }

    pub fn on_status(&self, part: &str, status: u16, body: Value) -> &Self {
        self.on_with_headers(part, status, body, &[])
    }

    /// A reply that also carries response headers.
    pub fn on_with_headers(&self, part: &str, status: u16, body: Value, headers: &[(&str, &str)]) -> &Self {
        let h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let mut i = self.inner.lock().unwrap();
        match i.routes.iter_mut().find(|(s, _)| s == part) {
            Some((_, q)) => {
                q.retain(|(_, _, _, served)| !served);
                q.push_back((status, body, h, false));
            }
            None => i.routes.push((part.to_string(), VecDeque::from([(status, body, h, false)]))),
        }
        self
    }

    pub fn seen_path(&self, part: &str) -> Vec<Seen> {
        self.inner.lock().unwrap().seen.iter().filter(|s| s.path.contains(part)).cloned().collect()
    }
}

async fn handle(State(inner): State<Arc<Mutex<MockInner>>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let mut i = inner.lock().unwrap();
    let path = uri.path().to_string();
    i.seen.push(Seen { method: method.to_string(), path: uri.to_string(), headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null), raw: body.to_vec() });
    let reply = i.routes.iter_mut().filter(|(s, _)| path.contains(s.as_str())).max_by_key(|(s, _)| s.len()).map(|(_, q)| {
        if q.len() > 1 {
            let (st, b, h, _) = q.pop_front().unwrap();
            (st, b, h)
        } else {
            let f = q.front_mut().unwrap();
            f.3 = true;
            (f.0, f.1.clone(), f.2.clone())
        }
    });
    match reply {
        Some((st, v, h)) => {
            let mut r = (StatusCode::from_u16(st).unwrap(), axum::Json(v)).into_response();
            for (k, val) in h {
                r.headers_mut().insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), val.parse().unwrap());
            }
            r
        }
        None => (StatusCode::NOT_FOUND, axum::Json(json!({"error": "unscripted", "path": path}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixtures_work() {
        let a = app().await;
        assert_eq!(anon(&a, "GET", "/health", None).await, (200, json!("ok")));
        let kp = Keypair::generate();
        let (st, v) = as_agent(&a, &kp, "GET", "/v1/me", None).await;
        assert_eq!(st, 200, "{v}");
        // A tampered signature is refused by the auth layer.
        let (st, _) = call(&a, "GET", "/v1/me", &[("authorization", format!("Bearer {}", kp.id())), ("x-agent-auth", "v1.1.aaaaaaaaaaaaaaaa.00".into())], None).await;
        assert_eq!(st, 401);
    }
}
