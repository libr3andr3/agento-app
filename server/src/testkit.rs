//! Test fixtures: a private database per test, a whole `AppState` wired to
//! it, and a fake HTTP upstream that stands in for the LLM provider, the
//! gateway and the registry. Nothing here reads the environment or the
//! network, so tests stay parallel-safe and offline.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::{body::Bytes, extract::State, http::{HeaderMap, Method, StatusCode, Uri}, response::IntoResponse, Router};
use serde_json::{json, Value};

use crate::{db::Db, SharedState};

pub const ADMIN_KEY: &str = "test-admin-key-0123456789abcdef0123456789";
pub const APP_KEY: &str = "test-app-key-0123456789abcdef0123456789abc";

/// Where test databases live: on disk under the crate's target dir (never a
/// tmpfs — thousands of them filled a node's RAM-backed /tmp once), emptied
/// at the start of every test run.
fn db_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-dbs");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

/// A fresh on-disk SQLite database with every migration applied. On disk
/// (not `sqlite::memory:`) because the pool holds several connections and
/// each in-memory connection would otherwise see its own empty database.
pub async fn db() -> Db {
    let dir = db_dir();
    let path = dir.join(format!("{}.db", uuid::Uuid::new_v4()));
    crate::db::open(&format!("sqlite://{}", path.display())).await.unwrap()
}

/// A minimal business row (Tito's barbershop in Lima); returns its id.
pub async fn business(db: &Db) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO businesses (id, name, industry, owner_phone) VALUES ($1, 'Tito', 'barbería', '+51999000111')")
        .bind(id).execute(db).await.unwrap();
    id
}

/// The gateway's reply to a successful sign-in, and the account's backup key.
pub const BACKUP_KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

pub fn script_sign_in(m: &Mock) {
    m.on("/v1/accounts/otp/check", json!({"account": {"id": "acc_1", "email": "tito@x.pe", "name": "Tito", "phone": "+51999"}, "session": "sess_abc", "plan": "pro"}));
    m.on("/v1/account/backup-key", json!({"key": BACKUP_KEY_HEX}));
}

/// Signs `state` in to a Yaya account through the scripted gateway.
pub async fn sign_in(state: &crate::AppState, m: &Mock) -> crate::account::Signed {
    script_sign_in(m);
    crate::account::otp_check(state, Some("tito@x.pe"), None, "123456", None).await.unwrap()
}

/// A state with a WhatsApp bridge at `mock` (answers /send).
pub async fn state_with_whatsapp(mock: &Mock) -> SharedState {
    mock.on("/send", json!({"messageId": "wamid.test"}));
    let base = state_on(mock).await;
    let wa = crate::whatsapp::WhatsApp::Bridge { http: reqwest::Client::new(), bridge_url: mock.base.clone(), bridge_key: "k".repeat(32) };
    Arc::new(crate::AppState { whatsapp: Some(wa), ..Arc::try_unwrap(base).ok().unwrap() })
}

/// A state whose registry/LLM is `mock`.
pub async fn state_on(mock: &Mock) -> SharedState {
    state_with(Opts { upstream: Some(mock.base.clone()), ..Default::default() }).await
}

/// Sets the business's client patch (its own values) wholesale.
pub async fn set_values(db: &Db, business: uuid::Uuid, patch: Value) {
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2").bind(patch.to_string()).bind(business).execute(db).await.unwrap();
}

/// A tool context for `business` as the customer `peer` (None = the owner),
/// composed exactly as an agent turn composes it.
pub async fn tool_ctx<'a>(state: &'a crate::AppState, business: uuid::Uuid, peer: Option<&str>) -> crate::harness::ToolCtx<'a> {
    let c = crate::learning::compose(&state.db, &state.schemas_dir, business).await.unwrap();
    crate::harness::ToolCtx {
        state, business_id: business, doc: c.doc, values: c.values, trials: c.trials, bundle_pin: c.bundle_pin,
        peer: peer.map(String::from), session: "test-session".into(), turn: 1, message_id: None,
    }
}

/// The date `days` from today in the business's zone (Lima), as YYYY-MM-DD.
pub fn day(days: i64) -> String {
    (crate::harness::now_local(chrono_tz::America::Lima).date() + chrono::Duration::days(days)).format("%Y-%m-%d").to_string()
}

/// One recorded request to the fake upstream.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, Value) + Send + Sync>;

#[derive(Default)]
struct MockInner {
    seen: Vec<Seen>,
    /// Computed replies by path fragment; win over scripted ones.
    handlers: Vec<(String, Handler)>,
    /// Raw byte replies (with headers) by path fragment; win over both.
    raw: Vec<(String, Vec<u8>, Vec<(String, String)>)>,
    /// Scripted replies by path fragment, consumed in order; the last one
    /// sticks until something new is scripted behind it.
    routes: Vec<(String, VecDeque<(u16, Value, bool)>)>,
}

/// A local HTTP server answering whatever it was scripted to answer.
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

    /// Replaces whatever `suffix` was scripted to answer with `body`.
    pub fn set(&self, suffix: &str, body: Value) -> &Self {
        self.inner.lock().unwrap().routes.retain(|(s, _)| s != suffix);
        self.on(suffix, body)
    }

    /// Replies to any path containing `suffix` with `body` (status 200);
    /// the longest matching pattern wins.
    pub fn on(&self, suffix: &str, body: Value) -> &Self {
        self.on_status(suffix, 200, body)
    }

    pub fn on_status(&self, suffix: &str, status: u16, body: Value) -> &Self {
        let mut i = self.inner.lock().unwrap();
        match i.routes.iter_mut().find(|(s, _)| s == suffix) {
            Some((_, q)) => {
                // A reply already served (kept only because it was last) gives way.
                q.retain(|(_, _, served)| !served);
                q.push_back((status, body, false));
            }
            None => i.routes.push((suffix.to_string(), VecDeque::from([(status, body, false)]))),
        }
        self
    }

    /// Answers any path containing `suffix` by calling `f` with the request.
    pub fn on_fn(&self, suffix: &str, f: impl Fn(&Seen) -> (u16, Value) + Send + Sync + 'static) -> &Self {
        self.inner.lock().unwrap().handlers.push((suffix.to_string(), Arc::new(f)));
        self
    }

    /// Answers any path containing `part` with raw bytes and extra headers.
    pub fn on_bytes(&self, part: &str, body: Vec<u8>, headers: &[(&str, &str)]) -> &Self {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.inner.lock().unwrap().raw.push((part.to_string(), body, headers));
        self
    }

    /// Scripts one chat-completions reply carrying `content`.
    pub fn say(&self, content: &str) -> &Self {
        self.on("/chat/completions", json!({"choices": [{"message": {"role": "assistant", "content": content}}]}))
    }

    /// Scripts one chat-completions reply that calls `tool` with `args`.
    pub fn call_tool(&self, tool: &str, args: Value) -> &Self {
        self.on("/chat/completions", json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
            {"id": format!("call_{tool}"), "type": "function", "function": {"name": tool, "arguments": args.to_string()}}
        ]}}]}))
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.inner.lock().unwrap().seen.clone()
    }

    pub fn seen_path(&self, part: &str) -> Vec<Seen> {
        self.seen().into_iter().filter(|s| s.path.contains(part)).collect()
    }
}

async fn handle(State(inner): State<Arc<Mutex<MockInner>>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let body_json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut i = inner.lock().unwrap();
    let path = uri.path().to_string();
    let seen = Seen { method: method.to_string(), path: uri.to_string(), headers, body: body_json };
    i.seen.push(seen.clone());
    if let Some((_, body, headers)) = i.raw.iter().filter(|(s, _, _)| path.contains(s.as_str())).max_by_key(|(s, _, _)| s.len()).cloned() {
        let mut resp = body.into_response();
        for (k, v) in headers {
            resp.headers_mut().insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        return resp;
    }
    if let Some(h) = i.handlers.iter().filter(|(s, _)| path.contains(s.as_str())).max_by_key(|(s, _)| s.len()).map(|(_, h)| h.clone()) {
        drop(i);
        let (status, v) = h(&seen);
        return (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response();
    }
    let reply = i.routes.iter_mut().filter(|(s, _)| path.contains(s.as_str())).max_by_key(|(s, _)| s.len()).map(|(_, q)| {
        if q.len() > 1 {
            let (st, b, _) = q.pop_front().unwrap();
            (st, b)
        } else {
            let front = q.front_mut().unwrap();
            front.2 = true;
            (front.0, front.1.clone())
        }
    });
    match reply {
        Some((status, v)) => (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response(),
        None => (StatusCode::NOT_FOUND, axum::Json(json!({"error": "unscripted", "path": path}))).into_response(),
    }
}

/// Options for [`state_with`].
#[derive(Default)]
pub struct Opts {
    pub client_mode: bool,
    /// Where the LLM and the registry live; unreachable when `None`.
    pub upstream: Option<String>,
}

/// [`state`] for synchronous tests.
pub fn state_sync() -> SharedState {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(state())
}

/// A whole application over a fresh database, talking to nothing.
pub async fn state() -> SharedState {
    state_with(Opts::default()).await
}

pub async fn state_with(o: Opts) -> SharedState {
    // Mesh config files go to a scratch dir, never the user's ~/.yaya.
    static MESH_DIR: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    MESH_DIR.get_or_init(|| {
        std::env::set_var("MESH_CONF_PATH", db_dir().join("yaya0-test.conf"));
    });
    let db = db().await;
    let identity = crate::identity::Identity::load_or_create(&db).await.unwrap();
    // Port 9 (discard) refuses connections: any accidental call fails fast.
    let base = o.upstream.unwrap_or_else(|| "http://127.0.0.1:9".into());
    let llm = Arc::new(crate::llm::Llm::with_upstream(
        crate::upstream::Upstream::Direct { base: format!("{base}/v1"), key: "test-llm-key".into() },
        "test-model",
    ));
    let mut kernel = crate::harness::Kernel::new();
    kernel.load(crate::plugins::all(llm.clone(), o.client_mode), &[]).unwrap();
    let mesh = crate::mesh::load_or_create(&db).await.unwrap();
    Arc::new(crate::AppState {
        db,
        llm,
        admin_key: ADMIN_KEY.into(),
        app_key: APP_KEY.into(),
        schemas_dir: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas"),
        kernel,
        limits: crate::limits::Limiter::from_env(),
        whatsapp: None,
        require_phone_verification: false,
        vision: None,
        registry: crate::network::Registry::new(base, identity.clone()),
        identity,
        reqsig_nonces: yaya_wire::reqsig::NonceCache::new(yaya_wire::reqsig::DEFAULT_WINDOW_SECS),
        audio: None,
        yaya_audio: None,
        publisher: crate::network::Publisher::default(),
        plan_info: Mutex::new(Value::Null),
        seller: Default::default(),
        niche_skill: Mutex::new(None),
        peer_notes: Default::default(),
        client_mode: o.client_mode,
        mesh,
        asks: Default::default(),
        owner_cache: Default::default(),
        pairing: Default::default(),
    })
}

/// Calls an /api route as the app (app key) and, when given, a device.
pub async fn api(state: &SharedState, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let bearer = token.map(|t| format!("Bearer {t}"));
    let mut h: Vec<(&str, &str)> = vec![("x-app-key", APP_KEY)];
    if let Some(b) = &bearer {
        h.push(("authorization", b.as_str()));
    }
    call(state, method, path, &h, body).await
}

/// Registers a business through the real route (the LLM mock says hello);
/// returns (device token, business id).
pub async fn onboard(state: &SharedState, mock: &Mock) -> (String, uuid::Uuid) {
    mock.say("¡Hola! Soy tu agente. ¿Cómo se llama tu negocio?");
    let (st, v) = api(state, "POST", "/api/onboard_business", None, Some(json!({
        "businessName": "Barbería Tito", "industry": "barbería", "ownerPhone": "+51 999 000 111", "country": "pe",
    }))).await;
    assert_eq!(st, 200, "{v}");
    (v["deviceToken"].as_str().unwrap().to_string(), serde_json::from_value(v["businessId"].clone()).unwrap())
}

/// An appointment row for `business` starting in `hours_from_now`.
pub async fn appointment(db: &Db, business: uuid::Uuid, name: &str, status: &str, paid: bool, price: Option<f64>, hours_from_now: i64) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO appointments (id, business_id, customer_name, phone, starts_at, status, paid, price) VALUES ($1,$2,$3,'+51 977 000 111',$4,$5,$6,$7)")
        .bind(id).bind(business).bind(name).bind(crate::db::hence(chrono::Duration::hours(hours_from_now))).bind(status).bind(paid).bind(price)
        .execute(db).await.unwrap();
    id
}

/// An order row for `business`.
pub async fn order(db: &Db, business: uuid::Uuid, name: &str, status: &str, paid: bool, total: Option<f64>) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, business_id, customer_name, phone, items, total, status, paid) VALUES ($1,$2,$3,'+51 988 000 000','[{\"name\":\"Polo\",\"qty\":1}]',$4,$5,$6)")
        .bind(id).bind(business).bind(name).bind(total).bind(status).bind(paid)
        .execute(db).await.unwrap();
    id
}

/// Drives the real router in-process: `(status, json body)`.
pub async fn call(state: &SharedState, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> (u16, Value) {
    call_from(state, ([127, 0, 0, 1], 40000).into(), method, path, headers, body).await
}

/// [`call`] from a chosen peer address.
pub async fn call_from(state: &SharedState, peer: std::net::SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> (u16, Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(path);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(axum::body::Body::from(b.to_string())).unwrap(),
        None => req.body(axum::body::Body::empty()).unwrap(),
    };
    let mut req = req;
    req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
    let resp = crate::routes::router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20).await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixtures_work() {
        let s = state().await;
        assert_eq!(call(&s, "GET", "/health", &[], None).await, (200, Value::String("ok".into())));
        // Every /api route is behind the app key.
        assert_eq!(call(&s, "GET", "/api/kernel", &[], None).await.0, 401);
        let m = Mock::start().await;
        m.on("/x", json!({"a": 1})).on("/x", json!({"a": 2}));
        let c = reqwest::Client::new();
        let get = |p: &str| c.get(format!("{}{p}", m.base)).send();
        assert_eq!(get("/x").await.unwrap().json::<Value>().await.unwrap()["a"], 1);
        assert_eq!(get("/x").await.unwrap().json::<Value>().await.unwrap()["a"], 2);
        assert_eq!(get("/x").await.unwrap().json::<Value>().await.unwrap()["a"], 2);
        assert_eq!(get("/nope").await.unwrap().status(), 404);
        assert_eq!(m.seen_path("/x").len(), 3);
    }
}

/// Stragglers: code paths only a running system reaches (loops, env
/// constructors, mount-time wiring, error paths).
#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[tokio::test]
    async fn owner_turns_after_onboarding_use_the_manager_prompt() {
        let m = Mock::start().await;
        let s = state_on(&m).await;
        let (t, b) = onboard(&s, &m).await;
        sqlx::query("UPDATE businesses SET onboarded = 1 WHERE id = $1").bind(b).execute(&s.db).await.unwrap();
        m.say("Hoy tienes 3 citas.");
        let (_, v) = api(&s, "POST", "/api/onboarding_message", Some(&t), Some(json!({"message": "¿cómo voy?"}))).await;
        assert_eq!(v["agentResponse"], "Hoy tienes 3 citas.");
        let sys = m.seen_path("/chat/completions").last().unwrap().body["messages"][0]["content"].as_str().unwrap().to_string();
        let interview = m.seen_path("/chat/completions")[0].body["messages"][0]["content"].as_str().unwrap().to_string();
        assert_ne!(sys, interview, "the manager prompt is not the interview");
    }

    #[test]
    fn env_constructors_default_to_the_yaya_gateway() {
        let id = crate::identity::Identity::ephemeral();
        let l = crate::llm::Llm::from_env(&id).unwrap();
        assert!(l.is_gateway());
        assert_eq!(l.model, "deepseek-chat");
        assert_eq!(l.base_url(), format!("{}/v1", crate::network::registry_url()));
        assert_eq!(crate::network::Registry::from_env(id).base(), crate::network::registry_url());
    }

    #[tokio::test]
    async fn registry_post_and_the_inbox_loop() {
        let m = Mock::start().await;
        let s = state_on(&m).await;
        m.on("/v1/x", json!({"ok": 1}));
        assert_eq!(crate::network::registry_post(&s, "/v1/x", &json!({})).await.unwrap()["ok"], 1);
        // One pass of the loop: a customer box arrives, a sealed reply goes back.
        onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"}));
        let customer = crate::identity::Identity::ephemeral();
        let boxed = crate::e2e::seal(&customer.x25519_secret(), &customer.id(), &s.identity.id(), br#"{"text":"hola"}"#).unwrap();
        m.on("/v1/inbox", json!({"messages": [{"id": "m1", "from": customer.id(), "box": boxed}]}));
        m.on("/v1/inbox", json!({"messages": []}));
        m.on(&format!("/v1/agents/{}/inbox", customer.id()), json!({"id": "r"}));
        m.say("¡Hola!");
        let task = tokio::spawn(crate::network::inbox_loop(s.clone()));
        for _ in 0..100 {
            if !m.seen_path(&format!("/v1/agents/{}/inbox", customer.id())).is_empty() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        task.abort();
        assert_eq!(m.seen_path(&format!("/v1/agents/{}/inbox", customer.id())).len(), 1);
    }

    #[tokio::test]
    async fn the_campaign_loop_needs_a_whatsapp_node() {
        let s = state().await;
        // No wa_node service mounted: the loop returns at once.
        tokio::time::timeout(std::time::Duration::from_secs(1), crate::node::campaign_loop(s)).await.unwrap();
    }

    #[tokio::test]
    async fn the_public_address_comes_from_the_reflector() {
        let reflector = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let raddr = reflector.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 16];
            let (_, from) = reflector.recv_from(&mut buf).await.unwrap();
            reflector.send_to(b"203.0.113.5:41000", from).await.unwrap();
        });
        let port = { let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap(); s.local_addr().unwrap().port() };
        assert_eq!(crate::mesh::reflect(&raddr.to_string(), port).await.as_deref(), Some("203.0.113.5:41000"));
        // Nobody answers: None, after the short timeout.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = { let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap(); s.local_addr().unwrap().port() };
        assert_eq!(crate::mesh::reflect(&silent.local_addr().unwrap().to_string(), port).await, None);
    }

    #[tokio::test]
    async fn the_a2a_card_is_served_on_the_mesh() {
        use tower::ServiceExt;
        let s = state().await;
        let req = axum::http::Request::builder().uri("/.well-known/agent.json").body(axum::body::Body::empty()).unwrap();
        let r = crate::mesh::a2a_router(s.clone()).oneshot(req).await.unwrap();
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v["yaya"]["agent"], json!(s.identity.id()));
    }

    #[tokio::test]
    async fn owner_scope_on_a_personal_assistant() {
        let m = Mock::start().await;
        let s = state_with(Opts { client_mode: true, upstream: Some(m.base.clone()) }).await;
        m.say("Hola, soy tu asistente.");
        let r = crate::owner::handle(&s, "agent:console", &json!({"cmd": "chat", "text": "hola"}), &Value::Null).await;
        assert_eq!(r["text"], "Hola, soy tu asistente.", "{r}");
        m.on("/v1/me", json!({"plan": "pro"})).on("/v1/credits", json!({}));
        let r = crate::owner::handle(&s, "agent:console", &json!({"cmd": "plan"}), &Value::Null).await;
        assert_eq!(r["plan"]["plan"], "pro");
        // A route failure comes back as a short message, not internals.
        sqlx::query("DROP TABLE tool_events").execute(&s.db).await.unwrap();
        let r = crate::owner::handle(&s, "agent:console", &json!({"cmd": "conversation", "peer": "x"}), &Value::Null).await;
        assert!(r["error"].is_string());
    }

    #[test]
    fn the_reminders_plugin_mounts_its_tool_after_a_booking() {
        use crate::harness::{spec_names, tool_specs, Scope, ToolCaps};
        let mut k = crate::harness::Kernel::new();
        let r = crate::plugins::reminders_for_test(std::env::temp_dir().join("never-written"));
        k.load(vec![Box::new(r)], &[]).unwrap();
        assert!(k.plugins().contains(&"reminders"));
        assert!(spec_names(&tool_specs(&k, Scope::Customer, None, &ToolCaps::default())).is_empty());
        assert_eq!(spec_names(&tool_specs(&k, Scope::Customer, None, &ToolCaps { booking_exists: true, seller: false })), vec!["schedule_reminder"]);
    }

    #[tokio::test]
    async fn internal_errors_are_opaque_with_a_reference() {
        let m = Mock::start().await;
        let s = state_on(&m).await;
        let (t, _) = onboard(&s, &m).await;
        sqlx::query("DROP TABLE contacts").execute(&s.db).await.unwrap();
        let (st, v) = api(&s, "GET", "/api/contacts", Some(&t), None).await;
        assert_eq!(st, 500);
        assert_eq!(v["error"], "internal error");
        assert!(v["ref"].is_string());
        assert!(!v.to_string().contains("contacts"), "no table names leak");
    }
}
