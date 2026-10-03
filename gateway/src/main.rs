//! yaya-gateway: the network edge that on-device agente agents talk to.
//!
//! 1. `/v1/chat/completions` + media proxies — every agent authenticates
//!    with its own identity (bearer = agent id, proven by a request
//!    signature), gets a free daily allowance, and paid plans lift it. The
//!    upstream keys never leave this process.
//! 2. `/v1/agents` — the registry: agents publish a signed card (AgentFacts)
//!    and anyone can discover them. Signatures are verified against the
//!    agent id, so a card can only be written by the phone that holds the key.
//! 3. `/v1/agents/{id}/inbox` — an end-to-end encrypted relay.
//! 4. `/v1/match`, reputation, plans — see `market.rs`, `plans.rs`.

mod accounts;
mod attest;
mod audit;
mod auth;
mod backup;
mod billing;
mod books;
mod confidential;
mod credits;
mod drop;
mod exchange;
mod mesh;
mod guests;
mod listings;
mod market;
mod referrals;
mod media;
mod meter;
mod otp;
mod seller;
mod sources;
mod support;
mod plans;
mod pools;
mod tools;
mod verified;
mod prepaid;
mod wallet;
mod yayacash;
mod yape;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod main_tests;

use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use yaya_wire::{envelope, ratelimit::Limiter, reqsig::NonceCache, secret, AgentId, Keypair};

pub(crate) use auth::{agent_of, require_linked, Auth};

pub(crate) struct App {
    pub(crate) db: SqlitePool,
    /// Registry signing key: lean index records carry its signature.
    registry: Keypair,
    pub(crate) public_url: String,
    pub(crate) http: reqwest::Client,
    /// Chat upstreams in failover order: `UPSTREAM_*` first, then
    /// `UPSTREAM2_*`, `UPSTREAM3_*`, … Each is an OpenAI-compatible
    /// chat-completions endpoint with its own key and model.
    upstreams: Vec<Upstream>,
    max_tokens_cap: i64,
    pub(crate) free_cap: i64,
    pub(crate) free_media_cap: i64,
    new_agents_per_ip_per_day: i64,
    calls_per_ip_per_day: i64,
    relay_pair_per_day: i64,
    admin_key: String,
    /// Shared secret of the house phone that forwards Yape notifications
    /// (`YAPE_COLLECTOR_KEY`); unset = the collector endpoint is closed.
    yape_collector_key: Option<String>,
    pub(crate) attestation: attest::Attestation,
    /// Attested confidential inference: the model host is proven to be a
    /// real TEE before any customer text is sent to it.
    pub(crate) aci: confidential::Aci,
    /// Our payment processor, for business plans. None = manual confirmation only.
    pub(crate) yayacash: Option<yayacash::YayaCash>,
    /// Card checkouts (Dodo / Izipay) and boletas; each provider is absent without its keys.
    pub(crate) billing: billing::Billing,
    /// One-time codes over WhatsApp / email.
    pub(crate) otp: otp::Delivery,
    pub(crate) nonces: NonceCache,
    pub(crate) auth_window_secs: u64,
    auth_grace_until: Option<chrono::DateTime<chrono::Utc>>,
    /// Brakes on the unauthenticated account surface.
    pub(crate) limiter: Limiter,
    pub(crate) backup_dir: std::path::PathBuf,
    pub(crate) backup_keep: usize,
    pub(crate) backups_free: bool,
    /// Network credits: one lead's price and the welcome grant (minor units).
    pub(crate) lead_price_minor: i64,
    pub(crate) starter_credits_minor: i64,
    pub(crate) training_per_agent_per_day: i64,
    /// Where market files live (`LISTING_DIR`).
    pub(crate) listing_dir: std::path::PathBuf,
    /// The yaya exchange: blind-signed coins per denomination.
    pub(crate) exchange: exchange::Exchange,
    /// The public UDP relay for the mesh (`MESH_RELAY_URL/KEY/HOST`); None = direct only.
    pub(crate) mesh_relay: Option<mesh::Relay>,
    /// Card top-ups for prepaid credits (Dodo Payments); None = card off.
    pub(crate) prepaid_dodo: Option<prepaid::DodoTopup>,
}
pub(crate) type Shared = Arc<App>;
pub(crate) type ApiResult = Result<axum::response::Response, (StatusCode, Json<Value>)>;

impl App {
    pub(crate) fn grace_open(&self) -> bool {
        self.auth_grace_until.is_some_and(|t| chrono::Utc::now() < t)
    }
}

pub(crate) fn err(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (code, Json(json!({"error": {"message": msg.to_string(), "type": "gateway"}})))
}
pub(crate) fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!("internal: {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

pub(crate) fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| d.into())
}
fn env_i64(k: &str, d: i64) -> i64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// One OpenAI-compatible chat-completions endpoint a turn can be served
/// from: our own GPU while the mesh has one, and the cloud providers that
/// keep answering when it does not.
pub(crate) struct Upstream {
    url: String,
    key: String,
    model: String,
}

impl Upstream {
    /// The provider's host, for logs and the `x-yaya-upstream` header. The
    /// URL carries no secret; the key beside it does, so only this is ever
    /// written down.
    fn host(&self) -> &str {
        self.url.split('/').nth(2).unwrap_or("upstream")
    }

    /// The spend cap, under the name this model accepts. OpenAI's newer
    /// models (gpt-5.x, o-series) reject `max_tokens` outright and want
    /// `max_completion_tokens`; DeepSeek and our own vLLM want `max_tokens`.
    fn cap_tokens(&self, body: &mut Value, max: i64) {
        let m = self.model.as_str();
        let newer = ["gpt-5", "o1", "o3", "o4"].iter().any(|p| m.starts_with(p));
        let (set, clear) = if newer {
            ("max_completion_tokens", "max_tokens")
        } else {
            ("max_tokens", "max_completion_tokens")
        };
        body[set] = json!(max);
        body.as_object_mut().map(|o| o.remove(clear));
    }
}

/// The failover chain, from the environment. `UPSTREAM_URL`/`_KEY`/`_MODEL`
/// is the primary — historically the only upstream there was — and
/// `UPSTREAM2_*`, `UPSTREAM3_*`, … are tried in order when the one before
/// cannot answer. The first slot without a URL ends the chain; a slot that
/// has a URL must carry a key and a model, or the gateway refuses to start
/// rather than fail over to an upstream that will 401 every turn.
fn upstream_chain(primary_key: String) -> anyhow::Result<Vec<Upstream>> {
    let mut chain = vec![Upstream {
        url: env_or("UPSTREAM_URL", "https://api.deepseek.com/v1/chat/completions"),
        key: primary_key,
        model: env_or("UPSTREAM_MODEL", "deepseek-chat"),
    }];
    for n in 2.. {
        let url = env_or(&format!("UPSTREAM{n}_URL"), "");
        if url.is_empty() {
            break;
        }
        let key_name = format!("UPSTREAM{n}_KEY");
        let key = secret::validate(&key_name, std::env::var(&key_name).ok().as_deref(), 16)?;
        let model = env_or(&format!("UPSTREAM{n}_MODEL"), "");
        if model.is_empty() {
            anyhow::bail!("UPSTREAM{n}_URL is set without UPSTREAM{n}_MODEL");
        }
        chain.push(Upstream { url, key, model });
    }
    Ok(chain)
}

pub(crate) fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Every route the gateway serves, behind the agent/session auth layer.
pub(crate) fn router(app: Shared) -> Router {
    let body = |n: usize| axum::extract::DefaultBodyLimit::max(n * 1024);
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/chat/completions", post(chat).layer(body(1024)))
        // Verified inference: sealed bodies relayed to an enclave the phone attested.
        .route("/v1/verified/pins", get(verified::pins))
        .route("/v1/verified/tinfoil/attestation", get(verified::tinfoil_attestation))
        .route("/v1/verified/amd/vcek/{product}/{chip}", get(verified::amd_vcek))
        .route("/v1/verified/tinfoil/chat/completions", post(verified::tinfoil_chat).layer(body(1024)))
        // Our verdict on the model host, public: the app checks the claim
        // without ever talking to the provider itself.
        .route("/v1/attestation", get(confidential::attestation))
        .route("/v1/agents", post(publish).get(discover))
        .route("/v1/agents/{id}", get(card))
        .route("/v1/agents/{id}/revoke", post(revoke))
        // NANDA surface: AgentFacts per agent, lean index records, and the
        // registry's own facts. Any NANDA resolver can ingest these.
        .route("/v1/agents/{id}/facts", get(facts))
        .route("/v1/index/{name}", get(index_record))
        .route("/.well-known/agent-facts", get(registry_facts))
        // E2E relay: sealed boxes in, sealed boxes out; long-poll delivery.
        .route("/v1/agents/{id}/inbox", post(inbox_post).layer(body(256)))
        .route("/v1/inbox", get(inbox_poll))
        .route("/v1/audio/transcriptions", post(media::transcriptions).layer(body(25 * 1024)))
        .route("/v1/audio/speech", post(media::speech))
        .route("/v1/vision/chat/completions", post(media::vision).layer(body(15 * 1024)))
        // Yaya ID: accounts, sessions, devices, plans (web only), backups.
        .route("/v1/plans", get(plans::public_tiers))
        .route("/v1/accounts/otp/start", post(accounts::otp_start))
        .route("/v1/accounts/otp/check", post(accounts::otp_check))
        .route("/v1/accounts/logout", post(accounts::logout))
        .route("/v1/accounts/delete", post(accounts::delete_account))
        .route("/v1/account/backup-key", get(accounts::backup_key))
        .route("/v1/account", get(accounts::me))
        // Asked once, used by every comprobante after it.
        .route("/v1/account/billing-profile", get(billing::get_profile).put(billing::put_profile))
        .route("/v1/account/agents", post(accounts::link_agent))
        .route("/v1/account/link-tokens", post(accounts::link_token))
        .route("/v1/account/agents/{id}", axum::routing::delete(accounts::unlink_agent).patch(accounts::update_agent))
        .route("/v1/account/agents/{id}/revoke", post(accounts::revoke_agent))
        .route("/v1/account/plan/request", post(accounts::plan_request))
        .route("/v1/account/plan/request/{ref}", get(accounts::plan_request_status))
        .route("/v1/account/credits/request", post(accounts::credits_request))
        // Prepaid credits (agente/docs/CREDITS.md): balance, outcomes, top-ups, catalogs.
        .route("/v1/credits", get(prepaid::credits))
        .route("/v1/outcomes/confirm", post(prepaid::outcomes_confirm).layer(body(8)))
        .route("/v1/outcomes/reverse", post(prepaid::outcomes_reverse).layer(body(8)))
        .route("/v1/account/profile", post(prepaid::profile).layer(body(8)))
        .route("/v1/topup/session", post(prepaid::topup_session).layer(body(8)))
        .route("/v1/topup/yape", post(prepaid::topup_yape).layer(body(8)))
        .route("/v1/webhooks/dodo", post(prepaid::dodo_webhook).layer(body(256)))
        .route("/v1/wallets", get(prepaid::wallets))
        .route("/v1/categories", get(prepaid::categories))
        .route("/admin/prepaid", post(prepaid::admin_grant).layer(body(8)))
        .route("/v1/account/billing/checkout", post(billing::checkout))
        .route("/v1/account/billing/checkout/{id}", get(billing::status))
        .route("/v1/account/billing/izipay/return", post(billing::izipay_return).layer(body(64)))
        .route("/v1/account/billing/subscription/cancel", post(billing::cancel_subscription))
        .route("/v1/billing/webhook/dodo", post(billing::dodo_webhook).layer(body(64)))
        .route("/v1/billing/webhook/izipay", post(billing::izipay_ipn).layer(body(64)))
        .route("/admin/metrics", get(admin_metrics))
        .route("/admin/books", get(books::admin_books))
        .route("/admin/credits", post(admin_credits))
        .route("/v1/backup", axum::routing::put(backup::put).layer(body(20 * 1024)))
        .route("/v1/backup/latest", get(backup::latest))
        .route("/v1/backup/{id}", get(backup::get))
        // Guests: run without an account; redacted conversations may train the agents.
        .route("/v1/agents/guest", post(guests::declare))
        .route("/v1/agents/share", post(guests::share))
        .route("/v1/training", post(guests::sample))
        .route("/v1/skills/{niche}", get(guests::skill))
        .route("/admin/skills", post(guests::set_skill))
        .route("/admin/training", get(guests::samples))
        // Organic wallet support: verdicts and names phones report, priors they read.
        .route("/v1/sources", get(sources::priors).post(sources::vote))
        .route("/v1/rails", get(sources::rails).post(sources::rails_vote))
        // The web app: sign in, plan, devices, dashboard from the latest backup.
        .route("/app", get(web_app))
        // The panel demo: example business, fake data, nothing connected.
        .route("/app/demo", get(web_demo))
        .route("/app/{*rest}", get(web_app))
        .route("/v1/match", get(market::match_agents).post(market::match_agents))
        .route("/v1/agents/{id}/reputation", get(market::reputation))
        .route("/v1/agents/{id}/reviews", post(market::post_review).layer(body(64)))
        .route("/v1/referrals", post(referrals::post).layer(body(16)))
        // Network-defined tools: features ship from the gateway without an APK.
        .route("/v1/tools", get(tools::manifest))
        .route("/admin/tools", post(tools::set).layer(body(256)))
        // Pools: group buying with escrow on the credit ledger (docs/POOLS.md).
        .route("/v1/pools", get(pools::list).post(pools::create).layer(body(32)))
        .route("/v1/pools/demand", get(pools::demand))
        .route("/v1/pools/intent", post(pools::intent).layer(body(4)))
        .route("/v1/pools/{id}", get(pools::get))
        .route("/v1/pools/{id}/join", post(pools::join).layer(body(8)))
        .route("/v1/pools/{id}/leave", post(pools::leave))
        .route("/v1/pools/{id}/close", post(pools::close))
        .route("/v1/pools/{id}/confirm", post(pools::confirm).layer(body(4)))
        // The wallet: one ledger in soles for recargas, consultations, the market, plans, data.
        .route("/v1/wallet", get(wallet::wallet))
        // The yaya exchange (Taler shape): keys public; minting/redeeming gated by EXCHANGE_ENABLED.
        .route("/v1/exchange/keys", get(exchange::keys))
        .route("/v1/exchange/withdraw", post(exchange::withdraw).layer(body(256)))
        .route("/v1/exchange/deposit", post(exchange::deposit).layer(body(512)))
        .route("/v1/exchange/refresh", post(exchange::refresh).layer(body(768)))
        .route("/v1/exchange/coins/{coin}", get(exchange::coin_status))
        // yaya mesh rendezvous: addresses, keys, relay pairs. Free.
        .route("/v1/mesh/info", get(mesh::info_route))
        .route("/v1/mesh/register", post(mesh::register).layer(body(8)))
        .route("/v1/mesh/agents", get(mesh::list))
        .route("/v1/mesh/agents/{id}", get(mesh::get_agent))
        .route("/v1/mesh/links", post(mesh::link))
        .route("/v1/wallet/transfer", post(wallet::transfer))
        .route("/v1/economics", get(wallet::economics_public))
        .route("/v1/account/plan/credits", post(wallet::plan_with_credits))
        .route("/v1/listings", get(listings::list).post(listings::create).layer(body(128)))
        .route("/v1/listings/{id}", get(listings::get).patch(listings::update).layer(body(128)))
        .route("/v1/listings/{id}/file", axum::routing::put(listings::upload).layer(body(25 * 1024)))
        .route("/v1/listings/{id}/buy", post(listings::buy))
        .route("/v1/listings/{id}/download", get(listings::download))
        .route("/v1/purchases", get(listings::purchases))
        .route("/admin/listings", post(listings::admin_create).layer(body(128)))
        .route("/admin/listings/{id}", axum::routing::patch(listings::admin_update).layer(body(128)))
        .route("/admin/listings/{id}/file", axum::routing::put(listings::admin_upload).layer(body(25 * 1024)))
        .route("/v1/me", get(me))
        .route("/v1/seller/lookup", get(seller::lookup))
        .route("/v1/seller/plan", post(seller::set_plan))
        .route("/admin/plan", post(set_plan))
        .route("/v1/collector/yape", post(yape::collect).layer(body(16)))
        .route("/admin/yape", get(yape::admin_list))
        .route("/admin/yape/{id}/assign", post(yape::admin_assign))
        .route("/admin/yape/{id}/dismiss", post(yape::admin_dismiss))
        .route("/admin/yape/{id}/retry", post(yape::admin_retry))
        // D15: private photo links for business agents (minutes, in memory).
        .route("/v1/drop", post(drop::create).layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)))
        .route("/p/{id}", get(drop::page))
        .route("/p/{id}/{n}", get(drop::image))
        // Audit anchors: the gateway's clock on a phone's append-only chain.
        .route("/v1/audit/anchor", post(audit::anchor))
        .route("/v1/audit/key", get(audit::public_key))
        .route("/v1/audit/anchors/{agent}", get(audit::anchors))
        .route("/v1/support", get(support::public))
        .route("/admin/settings", get(support::get_admin).post(support::set))
        .layer(axum::middleware::from_fn_with_state(app.clone(), auth::layer))
        .with_state(app)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let db_url = env_or("DATABASE_URL", "sqlite://gateway.db");
    let opts = <sqlx::sqlite::SqliteConnectOptions as std::str::FromStr>::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(4).connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let registry = load_registry_key(&db).await?;
    let exchange = exchange::Exchange::load(&db).await?;
    tracing::info!(registry = %registry.id(), "registry identity");
    let upstream_key = secret::validate("UPSTREAM_KEY", std::env::var("UPSTREAM_KEY").ok().as_deref(), 16)?;
    let admin_key = secret::validate("ADMIN_KEY", std::env::var("ADMIN_KEY").ok().as_deref(), 32)?;
    let auth_grace_until = std::env::var("AUTH_GRACE_UNTIL").ok().filter(|s| !s.trim().is_empty())
        .map(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).map(|t| t.with_timezone(&chrono::Utc)))
        .transpose()
        .map_err(|e| anyhow::anyhow!("AUTH_GRACE_UNTIL must be RFC3339: {e}"))?;
    if let Some(t) = auth_grace_until {
        tracing::warn!(until = %t, "unsigned bearers accepted until AUTH_GRACE_UNTIL");
    }
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(120)).build()?;
    let auth_window_secs = env_i64("AUTH_WINDOW_SECS", 300).max(30) as u64;
    let upstreams = upstream_chain(upstream_key)?;
    tracing::info!(
        chain = %upstreams.iter().map(|u| format!("{} ({})", u.host(), u.model)).collect::<Vec<_>>().join(" → "),
        "chat upstreams"
    );
    let app: Shared = Arc::new(App {
        attestation: attest::Attestation::from_env(http.clone())?,
        aci: confidential::Aci::from_env(),
        db,
        registry,
        public_url: env_or("PUBLIC_URL", "https://llm.yaya.tech"),
        otp: otp::Delivery::from_env()?,
        yayacash: yayacash::YayaCash::from_env(reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?),
        billing: billing::Billing::from_env(http.clone())?,
        http,
        upstreams,
        max_tokens_cap: env_i64("MAX_TOKENS_CAP", 2048),
        free_cap: env_i64("FREE_CALLS_PER_DAY", 600),
        free_media_cap: env_i64("FREE_MEDIA_PER_DAY", 150),
        new_agents_per_ip_per_day: env_i64("NEW_AGENTS_PER_IP_PER_DAY", 20),
        calls_per_ip_per_day: env_i64("CALLS_PER_IP_PER_DAY", 5000),
        relay_pair_per_day: env_i64("RELAY_PAIR_PER_DAY", 40),
        admin_key,
        yape_collector_key: std::env::var("YAPE_COLLECTOR_KEY").ok().filter(|s| !s.trim().is_empty())
            .map(|k| secret::validate("YAPE_COLLECTOR_KEY", Some(k.trim()), 32)).transpose()?,
        nonces: NonceCache::new(auth_window_secs),
        auth_window_secs,
        auth_grace_until,
        limiter: Limiter::new(),
        backup_dir: std::path::PathBuf::from(env_or("BACKUP_DIR", "backups")),
        backup_keep: env_i64("BACKUP_KEEP", 5).max(1) as usize,
        backups_free: std::env::var("BACKUPS_FREE").map(|v| v == "1").unwrap_or(false),
        lead_price_minor: env_i64("NETWORK_LEAD_PRICE_MINOR", 500),
        starter_credits_minor: env_i64("FREE_STARTER_CREDITS_MINOR", 1000),
        training_per_agent_per_day: env_i64("TRAINING_PER_AGENT_PER_DAY", 500),
        listing_dir: std::path::PathBuf::from(env_or("LISTING_DIR", "listings")),
        exchange,
        mesh_relay: mesh::Relay::from_env(reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?),
        prepaid_dodo: prepaid::DodoTopup::from_env(reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?),
    });
    // Prepaid credits dormancy: warn after 24 months idle, forfeit 30 days later. Daily.
    let sweeper = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            prepaid::dormancy_sweep(&sweeper).await;
            tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
        }
    });
    let router = router(app);
    let addr = env_or("BIND_ADDR", "127.0.0.1:8120");
    tracing::info!("yaya-gateway listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}

#[cfg(test)]
mod allowance_tests {
    use super::*;

    /// D18: being on `free` is not by itself a reason to charge anyone.
    /// Before this, `charge()` treated every `free` call as payable from
    /// credits, so an owner with no credits was refused on their first
    /// message and the "30 conversaciones al mes" free tier did not exist.
    /// Free now spends its own daily cap, and only past it does the
    /// account need credits.
    #[tokio::test]
    async fn free_is_free_up_to_its_cap() {
        let app = test_app().await;
        let a = charge(&app, "ag-free", "10.0.0.1", Kind::Chat).await.expect("a free agent may talk");
        assert_eq!(a.plan, "free");
        assert_eq!(a.cap, app.free_cap);
        assert_eq!(a.used, 1);
        // No credits were touched to get there.
        assert_eq!(crate::credits::balance(&app, "ag-free").await.unwrap(), 0);
    }

    /// Past the cap, with nothing to pay it with, the answer is still 429 —
    /// the brake is the brake. The error names the plan and points at the
    /// one place a plan can be bought.
    #[tokio::test]
    async fn past_the_cap_a_free_agent_is_refused() {
        let app = test_app().await;
        sqlx::query("INSERT INTO usage (agent, day, n, media) VALUES ('ag-spent', $1, $2, 0)")
            .bind(today()).bind(app.free_cap).execute(&app.db).await.unwrap();
        let e = charge(&app, "ag-spent", "10.0.0.2", Kind::Chat).await.expect_err("over the cap");
        assert_eq!(e.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.1["error"]["type"], "allowance");
        assert_eq!(e.1["error"]["plan"], "free");
        assert_eq!(e.1["error"]["upgrade"], "https://agente.ceo/#planes");
    }

    /// A paid plan is capacity: pro's cap is pro's, and the relay is never
    /// gated by it (owner devices must reach each other even when spent).
    #[tokio::test]
    async fn a_paid_plan_carries_its_own_cap() {
        let app = test_app().await;
        plans::set(&app, "ag-pro", "pro", 1, "test", None).await.unwrap();
        let a = charge(&app, "ag-pro", "10.0.0.3", Kind::Chat).await.unwrap();
        assert_eq!(a.plan, "pro");
        assert_eq!(a.cap, plans::PRO.calls);

        sqlx::query("UPDATE usage SET n = $1 WHERE agent = 'ag-pro'").bind(plans::PRO.calls).execute(&app.db).await.unwrap();
        assert!(charge(&app, "ag-pro", "10.0.0.3", Kind::Chat).await.is_err());
        assert!(charge(&app, "ag-pro", "10.0.0.3", Kind::Relay).await.is_ok());
    }
}

/// An `App` over a fresh in-memory database with default limits and no
/// providers, for tests that need the real settlement paths.
#[cfg(test)]
pub(crate) async fn test_app() -> App {
    let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&db).await.unwrap();
    let http = reqwest::Client::new();
    App {
        attestation: attest::Attestation::from_env(http.clone()).unwrap(),
        aci: confidential::Aci::from_env(),
        registry: Keypair::generate(),
        exchange: exchange::Exchange::load(&db).await.unwrap(),
        mesh_relay: None,
        prepaid_dodo: None,
        db,
        public_url: "http://test".into(),
        otp: otp::Delivery::from_env().unwrap(),
        yayacash: None,
        billing: billing::Billing { dodo: None, izipay: None, nubefact: None },
        http,
        upstreams: Vec::new(),
        max_tokens_cap: 2048,
        free_cap: 600,
        free_media_cap: 150,
        new_agents_per_ip_per_day: 20,
        calls_per_ip_per_day: 5000,
        relay_pair_per_day: 40,
        admin_key: "test-admin-key-test-admin-key-test".into(),
        yape_collector_key: Some(crate::testkit::COLLECTOR_KEY.into()),
        nonces: NonceCache::new(300),
        auth_window_secs: 300,
        auth_grace_until: None,
        limiter: Limiter::new(),
        backup_dir: testkit::scratch("backups"),
        backup_keep: 1,
        backups_free: false,
        lead_price_minor: 500,
        starter_credits_minor: 1000,
        training_per_agent_per_day: 500,
        listing_dir: testkit::scratch("listings"),
    }
}

pub(crate) fn client_ip(headers: &HeaderMap, peer: std::net::SocketAddr) -> String {
    yaya_wire::net::client_ip(headers, Some(peer))
}

// ---------------------------------------------------------------- budgets

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Chat,
    Media,
    Relay,
}

#[derive(Debug)]
pub(crate) struct Allowance {
    pub plan: String,
    pub cap: i64,
    pub used: i64,
}

fn exhausted_err(a: &Allowance, what: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": {"message": format!("daily {what} allowance exhausted"), "type": "allowance",
                    "plan": a.plan, "used": a.used, "cap": a.cap,
                    "upgrade": "https://agente.ceo/#planes"}})),
    )
}

/// Counts one call of `kind` and enforces every budget it falls under:
/// new-agents-per-IP, calls-per-IP, the agent's plan cap, and (for media)
/// the media cap. Increments and checks are single statements so parallel
/// turns can't share the last unit.
pub(crate) async fn charge(app: &App, agent: &str, ip: &str, kind: Kind) -> Result<Allowance, (StatusCode, Json<Value>)> {
    let db = &app.db;
    let day = today();
    let known: Option<(String,)> = sqlx::query_as("SELECT agent FROM agents_seen WHERE agent = $1").bind(agent).fetch_optional(db).await.map_err(internal)?;
    if known.is_none() {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM agents_seen WHERE ip = $1 AND first_seen >= $2")
            .bind(ip).bind(format!("{day}T00:00:00")).fetch_one(db).await.map_err(internal)?;
        if n >= app.new_agents_per_ip_per_day {
            return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many new agents from this address today"));
        }
    }
    sqlx::query(
        "INSERT INTO agents_seen (agent, ip, calls) VALUES ($1, $2, 1) \
         ON CONFLICT (agent) DO UPDATE SET calls = calls + 1, last_seen = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(agent).bind(ip).execute(db).await.map_err(internal)?;
    let (ip_used,): (i64,) = sqlx::query_as(
        "INSERT INTO ip_usage (ip, day, n) VALUES ($1, $2, 1) ON CONFLICT (ip, day) DO UPDATE SET n = n + 1 RETURNING n",
    ).bind(ip).bind(&day).fetch_one(db).await.map_err(internal)?;
    if app.calls_per_ip_per_day > 0 && ip_used > app.calls_per_ip_per_day {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many calls from this address today"));
    }
    let media = (kind == Kind::Media) as i64;
    let (used, media_used): (i64, i64) = sqlx::query_as(
        "INSERT INTO usage (agent, day, n, media) VALUES ($1, $2, 1, $3) \
         ON CONFLICT (agent, day) DO UPDATE SET n = n + 1, media = media + $3 RETURNING n, media",
    ).bind(agent).bind(&day).bind(media).fetch_one(db).await.map_err(internal)?;
    let e = plans::effective(app, agent).await?;
    let mut a = Allowance { plan: e.plan, cap: e.cap, used };
    // D18: the free plan is free. It has its own daily cap
    // (`FREE_CALLS_PER_DAY`) and 30 conversaciones a month, which the phone
    // counts; being on `free` is not by itself a reason to charge anyone.
    // Credits are what keep an agent answering *past* its plan's capacity,
    // on any plan — that is the whole "recarga" model: a plan is capacity,
    // a recarga is what buys more of it. Never into the red.
    let over_cap = a.cap > 0 && a.used > a.cap;
    if over_cap && kind != Kind::Relay {
        let account = accounts::account_of_agent(app, agent).await?;
        let paid = match account.as_deref() {
            Some(acct) => credits::spend_call(app, acct).await?,
            None => None,
        };
        match paid {
            Some(balance) => {
                tracing::debug!(%agent, plan = %a.plan, balance, "call paid from credits");
                // Credits lift the cap to Pro's; beyond that the brake is the brake.
                a.cap = plans::PRO.calls.max(a.cap);
                if a.used > a.cap {
                    return Err(exhausted_err(&a, "call"));
                }
            }
            None => {
                tracing::info!(%agent, plan = %a.plan, used = a.used, cap = a.cap, "daily allowance exhausted");
                return Err(exhausted_err(&a, "call"));
            }
        }
    }
    if kind == Kind::Media {
        let media_cap = plans::tier(&a.plan).media_cap(app);
        if media_cap > 0 && media_used > media_cap {
            return Err(exhausted_err(&Allowance { plan: a.plan.clone(), cap: media_cap, used: media_used }, "media"));
        }
    }
    Ok(a)
}

pub(crate) fn with_allowance(mut r: axum::response::Response, a: &Allowance) -> axum::response::Response {
    r.headers_mut().insert("x-yaya-plan", a.plan.parse().unwrap());
    r.headers_mut().insert("x-yaya-used", a.used.to_string().parse().unwrap());
    r.headers_mut().insert("x-yaya-cap", a.cap.to_string().parse().unwrap());
    r
}

async fn chat(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    // Attest the model host BEFORE the allowance is charged and before the
    // body goes anywhere: a refusal must cost the caller nothing and leak
    // nothing. `guard` returning Err means not one byte was sent.
    let conf = if app.aci.enabled() {
        match confidential::guard(&app).await {
            Ok(v) => Some(v),
            Err(e) if !app.aci.required() => {
                tracing::warn!(status = %e.0, "confidential upstream unavailable — falling back to the standard provider");
                None
            }
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    let a = charge(&app, &agent, &client_ip(&headers, peer), Kind::Chat).await?;
    // Spent past the grace floor? Refuse before spending anything upstream.
    let standing = meter::guard(&app, &agent).await?;
    // The gateway decides the model and bounds the spend per call; clients
    // may neither pick a pricier model nor ask for an unbounded answer.
    body["n"] = json!(1);
    // Either spelling of the cap is the caller asking for one, and neither
    // may exceed ours; the name the chosen upstream accepts is set per
    // attempt, because the same turn can end up at a different provider.
    let asked = body["max_tokens"].as_i64().or_else(|| body["max_completion_tokens"].as_i64());
    let max = asked.unwrap_or(app.max_tokens_cap).clamp(1, app.max_tokens_cap);
    // Routing is the gateway's to decide, never the caller's. `provider`,
    // `models` and `route` all steer a request to a different worker — a
    // client that could set them could walk straight out of the enclave we
    // just attested, and the attestation would still read as green.
    if let Some(o) = body.as_object_mut() {
        o.remove("stream");
        for k in ["provider", "models", "route", "transforms", "quantizations", "fallbacks"] {
            o.remove(k);
        }
        o.remove("stream");
    }
    if conf.is_some() {
        // Serve this only from an attested worker.
        body["provider"] = app.aci.provider_flags().clone();
    }
    // An attested enclave is the only place this turn may run: there is no
    // failing over out of it. Otherwise: each upstream in chain order, moving
    // on when one is unreachable, rate-limited or broken. Never on a 4xx —
    // a malformed request is malformed at every provider, and the caller has
    // already been charged for exactly one call.
    let attested;
    let chain: &[Upstream] = match &conf {
        Some(_) => {
            attested = [Upstream {
                url: app.aci.chat_url(),
                key: app.aci.key().to_string(),
                model: app.aci.model().to_string(),
            }];
            &attested
        }
        None => &app.upstreams,
    };
    let mut failure: Option<(StatusCode, Json<Value>)> = None;
    for (i, up) in chain.iter().enumerate() {
        let more = i + 1 < chain.len();
        body["model"] = json!(up.model);
        up.cap_tokens(&mut body, max);
        let sent = app.http.post(&up.url).bearer_auth(&up.key).json(&body).send().await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                failure = Some(err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")));
                tracing::warn!(upstream = %up.host(), error = %e, next = more, "chat upstream unreachable");
                if more {
                    continue;
                }
                return Err(failure.expect("just set"));
            }
        };
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if more && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
            tracing::warn!(upstream = %up.host(), %status, "chat upstream refused the turn — next in chain");
            failure = Some(err(StatusCode::BAD_GATEWAY, format!("upstream {}: {status}", up.host())));
            continue;
        }
        // The workload that answered must be the one we verified.
        if let Some(v) = &conf {
            confidential::check_response(&app, resp.headers(), v)?;
        }
        let payload: Value = resp.json().await.map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream body: {e}")))?;
        if i > 0 {
            tracing::info!(upstream = %up.host(), model = %up.model, %status, "chat served by a fallback upstream");
        }
        // The turn is already answered; metering it must not be able to fail it.
        let mut balance = None;
        if let (Some(st), Some((prompt, completion))) = (standing.as_ref(), meter::usage_of(&payload)) {
            balance = meter::record_quietly(&app, &agent, &st.account, meter::Use::Chat { prompt, completion }).await;
        }
        // The wrappers only stamp headers on the answer: who served it, then
        // the allowance, then the meter's remaining balance, then the
        // attestation verdict.
        let served = (status, [("x-yaya-upstream", up.host().to_string())], Json(payload)).into_response();
        return Ok(confidential::stamp(
            meter::with_balance(with_allowance(served, &a), balance),
            conf.as_ref(),
        ));
    }
    Err(failure.unwrap_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no chat upstream configured")))
}

/// Where the caller stands today, without spending a call.
async fn me(State(app): State<Shared>, Extension(auth): Extension<Auth>, headers: HeaderMap) -> ApiResult {
    let cur = plans::currency_of(&headers, None);
    let agent = agent_of(&app, &auth).await?;
    let day = today();
    let used: Option<(i64, i64)> = sqlx::query_as("SELECT n, media FROM usage WHERE agent = $1 AND day = $2")
        .bind(&agent).bind(&day).fetch_optional(&app.db).await.map_err(internal)?;
    plans::sweep(&app, &agent).await;
    let e = plans::effective(&app, &agent).await?;
    let t = plans::tier(&e.plan);
    let device = device_of(&app, &agent).await;
    let account: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT a.id, a.email, a.name, a.phone FROM account_agents aa JOIN accounts a ON a.id = aa.account WHERE aa.agent = $1",
    ).bind(&agent).fetch_optional(&app.db).await.map_err(internal)?;
    let state = plans::state_of(&e.plan);
    let plan_name = e.plan.clone();
    // D14: the phone enforces conversations per month offline; a seller
    // account's agents get the tools that sell agente itself.
    let seller = account.as_ref().is_some_and(|(id, email, _, phone)| seller::is_seller_account(id, email, phone.as_deref()));
    Ok(Json(json!({
        "agent": agent, "plan": plan_name, "tier": e.plan, "state": state,
        "used": used.map(|u| u.0).unwrap_or(0), "cap": e.cap, "day": day,
        "seller": seller,
        "confidential": confidential::me_json(&app),
        "account": account.map(|(id, email, name, _)| json!({"id": id, "email": email, "name": name})),
        "backups": plans::backups_allowed(&app, &e.plan),
        "support": support::support_json(&app).await,
        "mediaUsed": used.map(|u| u.1).unwrap_or(0), "mediaCap": t.media_cap(&app),
        "expiresAt": e.expires_at, "source": e.source, "trialDays": plans::trial_days(),
        "caps": {"conversationsPerMonth": t.conversations, "agents": t.agents, "callsPerDay": t.calls, "messagesPerDay": 0, "customersPerDay": 0},
        "tiers": plans::tiers_json_in(cur), "prices": plans::business_prices_in(cur),
        "device": device,
        "auth": {"signed": matches!(auth, Auth::Proven(_)), "graceOpen": app.grace_open()},
        "guest": sqlx::query_as::<_, (i64, i64)>("SELECT guest, share FROM agents_seen WHERE agent = $1").bind(&agent).fetch_optional(&app.db).await.ok().flatten().map(|(g, s)| json!({"guest": g == 1, "share": s == 1})),
    })).into_response())
}

// ------------------------------------------------------------- registry

/// Verifies a signed envelope and checks it was signed by the bearer.
pub(crate) fn verify_envelope(bearer: &str, env: &Value) -> Result<String, (StatusCode, Json<Value>)> {
    let signer = envelope::verify(env).map_err(|e| err(StatusCode::BAD_REQUEST, e))?.to_string();
    if signer != bearer {
        return Err(err(StatusCode::FORBIDDEN, "bearer and signer differ"));
    }
    Ok(signer)
}

/// An agent publishes (or refreshes) its card. Identity is the only
/// credential: the envelope must be signed by the bearer.
async fn publish(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(env): Json<Value>) -> ApiResult {
    let (bearer, _) = require_linked(&app, &auth).await?;
    let signer = verify_envelope(&bearer, &env)?;
    let p = &env["payload"];
    let s = |k: &str| p[k].as_str().map(|v| v.chars().take(120).collect::<String>());
    // Handle: keep the one already assigned; inherit a revoked predecessor's;
    // otherwise mint from the name — but only for a business that actually
    // finished onboarding, so squatting a name costs a real setup.
    let existing: Option<(Option<String>,)> = sqlx::query_as("SELECT handle FROM agents WHERE agent = $1")
        .bind(&signer).fetch_optional(&app.db).await.map_err(internal)?;
    let handle = match existing.and_then(|r| r.0) {
        Some(h) => Some(h),
        None => match inherit_handle(&app, &signer).await? {
            Some(h) => Some(h),
            None => match s("name") {
                Some(n) if p["onboarded"].as_bool() == Some(true) => Some(mint_handle(&app, &n).await.map_err(internal)?),
                _ => None,
            },
        },
    };
    sqlx::query(
        "INSERT INTO agents (agent, did, card, name, industry, country, handle, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
         ON CONFLICT (agent) DO UPDATE SET did = excluded.did, card = excluded.card, \
           name = excluded.name, industry = excluded.industry, country = excluded.country, \
           handle = COALESCE(agents.handle, excluded.handle), updated_at = excluded.updated_at",
    )
    .bind(&signer).bind(env["did"].as_str()).bind(env.to_string())
    .bind(s("name")).bind(s("industry")).bind(s("country")).bind(&handle)
    .execute(&app.db).await.map_err(internal)?;
    // Measured boot: verify an attached attestation chain and remember the
    // verdict. Absent chain = no verdict (unverified, never penalised).
    let mut device_verdict = Value::Null;
    if let Some(chain) = p["device"]["attestation"]["chain"].as_array() {
        let chain: Vec<String> = chain.iter().filter_map(|c| c.as_str().map(String::from)).collect();
        let v = app.attestation.verify(&signer, &chain).await;
        let mut vj = serde_json::to_value(&v).unwrap_or(Value::Null);
        vj["checked_at"] = json!(chrono::Utc::now().to_rfc3339());
        sqlx::query(
            "INSERT INTO device_verdicts (agent, verified, verdict, checked_at) \
             VALUES ($1, $2, $3, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
             ON CONFLICT (agent) DO UPDATE SET verified = excluded.verified, verdict = excluded.verdict, checked_at = excluded.checked_at",
        ).bind(&signer).bind(v.verified as i32).bind(vj.to_string()).execute(&app.db).await.map_err(internal)?;
        tracing::info!(agent = %signer, verified = v.verified, boot = %v.verified_boot, level = %v.security_level, reason = ?v.reason, "device attestation checked");
        device_verdict = attest::summary(&vj);
    }
    tracing::info!(agent = %signer, name = ?s("name"), handle = ?handle, "card published");
    Ok(Json(json!({
        "ok": true, "agent": signer, "handle": handle, "device": device_verdict,
        "agent_name": handle.as_ref().map(|h| format!("urn:agent:yaya:{h}")),
        "url": format!("/v1/agents/{signer}"),
        "facts_url": format!("{}/v1/agents/{signer}/facts", app.public_url),
    })).into_response())
}

/// `POST /v1/agents/{id}/revoke` — the lost-phone path. Signed by the key
/// being retired; the payload may name a successor that inherits the handle
/// on its first publish. Irreversible for the old key.
async fn revoke(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, Json(env): Json<Value>) -> ApiResult {
    let bearer = agent_of(&app, &auth).await?;
    let signer = verify_envelope(&bearer, &env)?;
    if signer != id {
        return Err(err(StatusCode::FORBIDDEN, "only an agent can revoke itself"));
    }
    let successor = match env["payload"]["successor"].as_str() {
        None => None,
        Some(s) => Some(s.parse::<AgentId>().map_err(|e| err(StatusCode::BAD_REQUEST, e))?.to_string()),
    };
    revoke_agent(&app, &signer, successor.as_deref()).await?;
    Ok(Json(json!({"ok": true, "revoked": signer, "successor": successor})).into_response())
}

/// Retires `agent`; used by self-revocation and by the account that owns it.
pub(crate) async fn revoke_agent(app: &App, agent: &str, successor: Option<&str>) -> Result<(), (StatusCode, Json<Value>)> {
    if successor == Some(agent) {
        return Err(err(StatusCode::BAD_REQUEST, "successor must differ"));
    }
    let n = sqlx::query(
        "UPDATE agents SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'), successor = $2 WHERE agent = $1 AND revoked_at IS NULL",
    ).bind(agent).bind(successor).execute(&app.db).await.map_err(internal)?.rows_affected();
    if n == 0 {
        // Never published, or already revoked: record the revocation anyway.
        sqlx::query("INSERT OR IGNORE INTO agents (agent, card, revoked_at, successor) VALUES ($1, '{}', strftime('%Y-%m-%dT%H:%M:%fZ','now'), $2)")
            .bind(agent).bind(successor).execute(&app.db).await.map_err(internal)?;
    }
    tracing::warn!(%agent, ?successor, "agent revoked");
    Ok(())
}

/// The web app is one self-contained page, shipped inside the binary.
async fn web_app() -> axum::response::Response {
    let mut r = axum::response::Html(include_str!("../web/app.html")).into_response();
    r.headers_mut().insert("cache-control", "no-cache".parse().unwrap());
    r.headers_mut().insert("content-security-policy", "default-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; script-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:".parse().unwrap());
    r
}

/// The panel demo (Pastelería Rosita): the same page on example data, so anyone can try the panel.
async fn web_demo() -> axum::response::Response {
    let mut r = axum::response::Html(include_str!("../web/demo.html")).into_response();
    r.headers_mut().insert("cache-control", "no-cache".parse().unwrap());
    r.headers_mut().insert("content-security-policy", "default-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; script-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:".parse().unwrap());
    r
}

#[cfg(test)]
mod web_tests {
    use crate::testkit;

    /// The panel and its demo are pages anyone can open. The demo has its own
    /// route (the panel's catch-all must not swallow it) and never calls the API.
    #[tokio::test]
    async fn panel_and_demo_are_served() {
        let app = testkit::app().await;
        let page = |b: Vec<u8>| String::from_utf8(b).unwrap();
        let (st, body) = testkit::raw(&app, "GET", "/app", &[], None).await;
        assert_eq!(st, 200);
        assert!(page(body).contains("<title>agento · tu panel</title>"));
        let (st, body) = testkit::raw(&app, "GET", "/app/demo", &[], None).await;
        assert_eq!(st, 200);
        let demo = page(body);
        assert!(demo.contains("<title>agento · demo del panel</title>"));
        assert!(!demo.contains("/v1/"), "the demo runs on example data only");
        let (st, body) = testkit::raw(&app, "GET", "/app/plan", &[], None).await;
        assert_eq!(st, 200);
        assert!(page(body).contains("<title>agento · tu panel</title>"));
    }
}

/// A handle left behind by a revoked predecessor that named `agent`.
async fn inherit_handle(app: &App, agent: &str) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT agent, handle FROM agents WHERE successor = $1 AND revoked_at IS NOT NULL AND handle IS NOT NULL LIMIT 1",
    ).bind(agent).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((pred, Some(handle))) = row else { return Ok(None) };
    sqlx::query("UPDATE agents SET handle = NULL WHERE agent = $1").bind(&pred).execute(&app.db).await.map_err(internal)?;
    tracing::info!(from = %pred, to = %agent, %handle, "handle inherited");
    Ok(Some(handle))
}

async fn card(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let row: Option<(String,)> = sqlx::query_as("SELECT card FROM agents WHERE agent = $1 AND revoked_at IS NULL")
        .bind(&id).fetch_optional(&app.db).await.map_err(internal)?;
    match row {
        Some((c,)) => Ok(Json(serde_json::from_str::<Value>(&c).unwrap_or(Value::Null)).into_response()),
        None => Err(err(StatusCode::NOT_FOUND, "unknown agent")),
    }
}

#[derive(serde::Deserialize)]
struct DiscoverQ {
    #[serde(default)] country: Option<String>,
    #[serde(default)] industry: Option<String>,
    #[serde(default)] q: Option<String>,
}

/// Public discovery. Cards are public by construction (the agent signed and
/// published them); this lists the summary fields only.
async fn discover(State(app): State<Shared>, Query(q): Query<DiscoverQ>) -> ApiResult {
    let like = |s: &Option<String>| s.as_deref().map(|v| format!("%{}%", v.to_lowercase()));
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>, String, Option<String>, String, Option<i64>)> = sqlx::query_as(
        "SELECT a.agent, a.did, a.name, a.industry, a.country, a.updated_at, a.handle, a.card, v.verified \
         FROM agents a LEFT JOIN device_verdicts v ON v.agent = a.agent \
         WHERE name IS NOT NULL AND a.revoked_at IS NULL \
           AND ($1 IS NULL OR lower(country) = lower($1)) \
           AND ($2 IS NULL OR lower(industry) LIKE $2) \
           AND ($3 IS NULL OR lower(name) LIKE $3 OR lower(industry) LIKE $3 OR lower(card) LIKE $3) \
         ORDER BY COALESCE(v.verified, 0) DESC, a.updated_at DESC LIMIT 100",
    )
    .bind(q.country.as_deref()).bind(like(&q.industry)).bind(like(&q.q))
    .fetch_all(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"agents": rows.into_iter().map(|(a, d, n, i, c, u, h, card, hw)| {
        let payload = serde_json::from_str::<Value>(&card).map(|v| v["payload"].clone()).unwrap_or(Value::Null);
        json!({
            "agent": a, "did": d, "name": n, "industry": i, "country": c, "updatedAt": u,
            "hardwareVerified": hw.map(|x| x == 1),
            "handle": h, "agent_name": h.as_ref().map(|h| format!("urn:agent:yaya:{h}")),
            "description": payload["description"],
            "skills": payload["skills"].as_array().map(|s| s.iter().filter_map(|x| x["id"].as_str()).collect::<Vec<_>>()),
            "askPrice": wallet::ask_price_of_payload(&payload),
            "url": format!("/v1/agents/{a}"),
            "facts_url": format!("{}/v1/agents/{a}/facts", app.public_url),
            "inbox_url": format!("{}/v1/agents/{a}/inbox", app.public_url),
        })
    }).collect::<Vec<_>>()})).into_response())
}

#[derive(serde::Deserialize)]
struct PlanReq {
    /// Any of these names the subject: an account id, an email, or an agent
    /// (resolved to its account when linked).
    #[serde(default)] account: Option<String>,
    #[serde(default)] email: Option<String>,
    #[serde(default)] agent: Option<String>,
    #[serde(default)] plan: Option<String>,
    #[serde(default)] cap: Option<i64>, #[serde(default)] note: Option<String>,
    /// Confirm a Yape/Plin request by reference instead.
    #[serde(default)] r#ref: Option<String>,
    #[serde(default)] months: Option<i64>,
}

/// `POST /admin/plan` — sales sets a plan (or confirms a payment reference).
/// Plans live on accounts; an agent is accepted as a convenience and mapped
/// to its account, so the whole account (every device) gets the plan and
/// the credits that come with it.
async fn set_plan(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<PlanReq>) -> ApiResult {
    let k = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !secret::ct_eq(k, &app.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    if let Some(r) = req.r#ref.as_deref() {
        return Ok(Json(plans::confirm_request(&app, r).await?).into_response());
    }
    let Some(plan) = req.plan.as_deref() else {
        return Err(err(StatusCode::BAD_REQUEST, "need ref, or a subject (account | email | agent) + plan"));
    };
    let subject = if let Some(id) = req.account.as_deref() {
        accounts::subject(id)
    } else if let Some(email) = req.email.as_deref() {
        let row: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE email = $1").bind(email.trim().to_lowercase()).fetch_optional(&app.db).await.map_err(internal)?;
        accounts::subject(&row.ok_or_else(|| err(StatusCode::NOT_FOUND, "no account with that email"))?.0)
    } else if let Some(agent) = req.agent.as_deref() {
        if !AgentId::looks_valid(agent) {
            return Err(err(StatusCode::BAD_REQUEST, "bad agent id"));
        }
        plans::subject_of(&app, agent).await?
    } else {
        return Err(err(StatusCode::BAD_REQUEST, "need ref, or a subject (account | email | agent) + plan"));
    };
    let expires = plans::set(&app, &subject, plan, req.months.unwrap_or(1), "admin", req.note.as_deref()).await?;
    if let Some(c) = req.cap {
        sqlx::query("UPDATE plans SET cap = $1 WHERE agent = $2").bind(c).bind(&subject).execute(&app.db).await.map_err(internal)?;
    }
    let credits = match subject.strip_prefix("acct:") { Some(a) => json!(credits::balance(&app, a).await?), None => Value::Null };
    Ok(Json(json!({"ok": true, "subject": subject, "plan": plans::tier(plan).name, "expiresAt": expires, "credits": credits})).into_response())
}

// ------------------------------------------------------------- NANDA surface

async fn load_registry_key(db: &SqlitePool) -> anyhow::Result<Keypair> {
    if let Ok(seed) = std::env::var("REGISTRY_SEED") {
        let bytes: [u8; 32] = hex::decode(seed.trim())?.try_into().map_err(|_| anyhow::anyhow!("REGISTRY_SEED must be 32 bytes hex"))?;
        return Ok(Keypair::from_seed(bytes));
    }
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT secret_key FROM registry_key WHERE id = 1").fetch_optional(db).await?;
    if let Some((sk,)) = row {
        let bytes: [u8; 32] = sk.try_into().map_err(|_| anyhow::anyhow!("corrupt registry key"))?;
        return Ok(Keypair::from_seed(bytes));
    }
    let kp = Keypair::generate();
    sqlx::query("INSERT INTO registry_key (id, secret_key, public_key) VALUES (1, $1, $2)")
        .bind(kp.seed().to_vec()).bind(kp.id().bytes().to_vec()).execute(db).await?;
    Ok(kp)
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in name.chars() {
        let c = match ch {
            'á' | 'à' | 'ä' | 'â' | 'ã' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o', 'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', 'ç' => 'c',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 40 { break; }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "agent".into() } else { out }
}

async fn mint_handle(app: &App, name: &str) -> anyhow::Result<String> {
    let base = slugify(name);
    for n in 0..1000u32 {
        let candidate = if n == 0 { base.clone() } else { format!("{base}-{n}") };
        let taken: Option<(String,)> = sqlx::query_as("SELECT agent FROM agents WHERE handle = $1").bind(&candidate).fetch_optional(&app.db).await?;
        if taken.is_none() {
            return Ok(candidate);
        }
    }
    Ok(format!("{base}-{}", uuid::Uuid::new_v4().simple().to_string().chars().take(6).collect::<String>()))
}

pub(crate) struct AgentRow { pub agent: String, did: Option<String>, pub handle: Option<String>, card: Value, updated_at: String }

/// Stored device verdict summary for an agent, if it ever published one.
pub(crate) async fn device_of(app: &App, agent: &str) -> Value {
    let row: Option<(String,)> = sqlx::query_as("SELECT verdict FROM device_verdicts WHERE agent = $1").bind(agent).fetch_optional(&app.db).await.ok().flatten();
    row.and_then(|(v,)| serde_json::from_str::<Value>(&v).ok()).map(|v| attest::summary(&v)).unwrap_or(Value::Null)
}

pub(crate) async fn agent_row(app: &App, id_or_name: &str) -> Result<AgentRow, (StatusCode, Json<Value>)> {
    let key = id_or_name.trim();
    let handle = key.strip_prefix("urn:agent:yaya:").or_else(|| key.strip_prefix('@'));
    let row: Option<(String, Option<String>, Option<String>, String, String)> = match handle {
        Some(h) => sqlx::query_as("SELECT agent, did, handle, card, updated_at FROM agents WHERE handle = $1 AND revoked_at IS NULL")
            .bind(h).fetch_optional(&app.db).await.map_err(internal)?,
        None => sqlx::query_as("SELECT agent, did, handle, card, updated_at FROM agents WHERE agent = $1 AND revoked_at IS NULL")
            .bind(key).fetch_optional(&app.db).await.map_err(internal)?,
    };
    let (agent, did, handle, card, updated_at) = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown agent"))?;
    Ok(AgentRow { agent, did, handle, card: serde_json::from_str(&card).unwrap_or(Value::Null), updated_at })
}

/// AgentFacts v0.3 (NANDA "Beyond DNS" appendix shape), assembled from the
/// agent's own signed card. The card's signature is carried through so a
/// verifier can check the business signed these facts, not the registry.
fn build_facts(app: &App, r: &AgentRow, device: &Value) -> Value {
    let hw_verified = device["verified"].as_bool() == Some(true);
    let p = &r.card["payload"];
    let handle = r.handle.clone().unwrap_or_else(|| r.agent.trim_start_matches("agent:").chars().take(12).collect());
    let agent_name = format!("urn:agent:yaya:{handle}");
    let inbox = format!("{}/v1/agents/{}/inbox", app.public_url, r.agent);
    let skills: Vec<Value> = p["skills"].as_array().cloned().unwrap_or_default();
    let langs = p["locale"]["language"].as_str().map(|l| vec![l]).unwrap_or_default();
    json!({
        "@context": ["https://www.w3.org/ns/credentials/v2", "https://projectnanda.org/ns/agentfacts/v0.3"],
        "id": r.agent,
        "agent_name": agent_name,
        "label": p["name"],
        "description": p["description"],
        "version": p["software"]["version"],
        "documentationUrl": "https://agente.ceo",
        "jurisdiction": p["country"],
        "provider": { "name": p["name"], "url": format!("{}/v1/agents/{}", app.public_url, r.agent), "did": r.did },
        "endpoints": {
            "static": [inbox],
            "adaptive_resolver": { "url": format!("{}/v1/index/{}", app.public_url, agent_name), "policies": ["relay"] }
        },
        "capabilities": {
            "modalities": ["text"], "streaming": false, "batch": false,
            "authentication": { "methods": ["ed25519-envelope", "yaya-reqsig-v1"], "requiredScopes": [] },
            "encryption": { "alg": "x25519-hkdf-sha256-chacha20poly1305", "x25519": p["e2e"]["x25519"] },
            "protocols": p["protocols"]
        },
        "skills": skills.iter().map(|s| json!({
            "id": s["id"], "description": s["description"],
            "inputModes": ["text"], "outputModes": ["text"], "supportedLanguages": langs,
        })).collect::<Vec<_>>(),
        "evaluations": { "performanceScore": Value::Null, "lastAudited": Value::Null },
        "telemetry": { "enabled": false },
        "pricing": { "askPriceMinor": wallet::ask_price_of_payload(p), "currency": credits::CURRENCY,
                     "note": "0 = answers free; otherwise each relayed question carries pay.amountMinor and settles in the network wallet" },
        "device": device,
        "certification": {
            "level": if hw_verified { "hardware-attested" } else { "self-certified" },
            "attestation": if hw_verified { json!("android-key-attestation") } else { Value::Null },
            "issuer": r.did, "issuanceDate": p["publishedAt"], "expirationDate": Value::Null
        },
        "ttl": 3600,
        "signature": { "alg": "ed25519", "agent": r.agent, "sig": r.card["sig"], "signedPayload": p },
        "updatedAt": r.updated_at,
        "registry": { "id": app.registry.id().to_string(), "did": app.registry.did(), "url": app.public_url },
    })
}

async fn facts(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let r = agent_row(&app, &id).await?;
    let device = device_of(&app, &r.agent).await;
    let mut resp = Json(build_facts(&app, &r, &device)).into_response();
    resp.headers_mut().insert("cache-control", "public, max-age=300".parse().unwrap());
    resp.headers_mut().insert("content-type", "application/ld+json".parse().unwrap());
    Ok(resp)
}

/// Lean index record (NANDA AgentAddr): agent_id → facts_url, registry-signed.
async fn index_record(State(app): State<Shared>, Path(name): Path<String>) -> ApiResult {
    let r = agent_row(&app, &name).await?;
    let handle = r.handle.clone().unwrap_or_default();
    let mut rec = json!({
        "agent_id": r.agent,
        "agent_name": format!("urn:agent:yaya:{handle}"),
        "facts_url": format!("{}/v1/agents/{}/facts", app.public_url, r.agent),
        "private_facts_url": Value::Null,
        "adaptive_router_url": format!("{}/v1/agents/{}/inbox", app.public_url, r.agent),
        "ttl": 3600,
        "registry": app.registry.id().to_string(),
    });
    let sig = app.registry.sign_hex(&yaya_wire::canonical(&rec));
    rec["signature"] = json!({"alg": "ed25519", "signer": app.registry.id().to_string(), "sig": sig});
    Ok(Json(rec).into_response())
}

/// The registry describes itself the same way its agents do.
async fn registry_facts(State(app): State<Shared>) -> ApiResult {
    Ok(Json(json!({
        "@context": ["https://www.w3.org/ns/credentials/v2", "https://projectnanda.org/ns/agentfacts/v0.3"],
        "id": app.registry.id().to_string(),
        "agent_name": "urn:agent:yaya:registry",
        "label": "yaya.tech agent registry",
        "description": "Quilt member registry for on-device agente business agents: discovery, AgentFacts, lean index records and an end-to-end encrypted relay.",
        "version": env!("CARGO_PKG_VERSION"),
        "documentationUrl": "https://yaya.tech",
        "jurisdiction": "US",
        "provider": {"name": "Yaya Tech PBC", "url": "https://yaya.tech", "did": app.registry.did()},
        "endpoints": {"static": [
            format!("{}/v1/agents", app.public_url),
            format!("{}/v1/index/{{agent_name}}", app.public_url),
            format!("{}/v1/agents/{{id}}/facts", app.public_url),
            format!("{}/v1/agents/{{id}}/inbox", app.public_url),
            format!("{}/v1/match", app.public_url),
            format!("{}/v1/agents/{{id}}/reputation", app.public_url)
        ]},
        "capabilities": {"modalities": ["text"], "streaming": false, "batch": false,
            "authentication": {"methods": ["ed25519-envelope", "yaya-reqsig-v1"], "requiredScopes": []}},
        "skills": [
            {"id": "discover", "description": "GET /v1/agents?country=&industry=&q= — find business agents"},
            {"id": "resolve", "description": "GET /v1/index/{agent_name} — lean index record → facts_url"},
            {"id": "relay", "description": "POST /v1/agents/{id}/inbox with a sealed box; GET /v1/inbox?wait= to receive"},
            {"id": "match", "description": "GET /v1/match?q=&country=&city= — ranked businesses with offer, next free slots and reputation"},
            {"id": "reputation", "description": "GET /v1/agents/{id}/reputation — reviews in both directions; POST /v1/agents/{id}/reviews (signed, interaction-gated)"},
            {"id": "revoke", "description": "POST /v1/agents/{id}/revoke — retire a key, optionally naming a successor that inherits the handle"},
            {"id": "wallet", "description": "GET /v1/wallet · GET /v1/economics — the network ledger in soles: recargas, paid consultations (pay.amountMinor on a relayed box), market sales, plans, data rewards, transfers (gated)"},
            {"id": "market", "description": "GET /v1/listings?niche=&kind= · POST /v1/listings/{id}/buy · GET /v1/purchases — files, bundles and notes for sale, paid in credits"},
            {"id": "mesh", "description": "GET /v1/mesh/info · POST /v1/mesh/register · GET /v1/mesh/agents/{id} · POST /v1/mesh/links — free rendezvous for the post-quantum p2p VPN (WireGuard + ML-KEM-768 PSK, 10.77.0.0/16); A2A at http://<mesh ip>:7770/a2a"},
            {"id": "exchange", "description": "GET /v1/exchange/keys (signed denomination keys, pin them) · POST /v1/exchange/withdraw|deposit|refresh (gated) · GET /v1/exchange/coins/{coin} — blind-signed coins in soles (Taler shape): verify offline, pass peer to peer over Bluetooth or the mesh"}
        ],
        "certification": {"level": "self-certified", "issuer": app.registry.did()},
        "ttl": 86400,
    })).into_response())
}

// ------------------------------------------------------------- E2E relay

/// Anyone with an identity may drop a sealed box in an agent's inbox. The
/// envelope must be signed by the bearer; the payload is the box itself
/// (`{alg, from, to, nonce, ct}`) and `from`/`to` must match.
async fn inbox_post(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    Path(to): Path<String>,
    Json(env): Json<Value>,
) -> ApiResult {
    let (bearer, _) = require_linked(&app, &auth).await?;
    let signer = verify_envelope(&bearer, &env)?;
    let boxed = &env["payload"];
    if boxed["from"].as_str() != Some(signer.as_str()) || boxed["to"].as_str() != Some(to.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "box from/to must match signer and path"));
    }
    if !AgentId::looks_valid(&to) {
        return Err(err(StatusCode::BAD_REQUEST, "bad recipient id"));
    }
    // Relay usage counts against the sender's allowance like an LLM call,
    // and against a per-pair ceiling so one key cannot flood one inbox.
    // The owner's own devices talking to each other (the web console to
    // the phone) are the exception: the pair cap is the console's chat
    // budget, not a spam brake, so it is far higher.
    charge(&app, &signer, &client_ip(&headers, peer), Kind::Relay).await?;
    let same_account = accounts::same_account(&app, &signer, &to).await;
    let (pair_n,): (i64,) = sqlx::query_as(
        "INSERT INTO relay_daily (from_agent, to_agent, day, n) VALUES ($1, $2, $3, 1) \
         ON CONFLICT (from_agent, to_agent, day) DO UPDATE SET n = n + 1 RETURNING n",
    ).bind(&signer).bind(&to).bind(today()).fetch_one(&app.db).await.map_err(internal)?;
    let pair_cap = if same_account { env_i64("RELAY_OWN_PAIR_PER_DAY", 3000) } else { app.relay_pair_per_day };
    if pair_cap > 0 && pair_n > pair_cap {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many messages to this agent today"));
    }
    // Paid consultations: an agent that prices its answers gets paid before
    // the question lands (402 with the price otherwise — see wallet.rs).
    let paid = wallet::settle_ask(&app, &signer, &to, &boxed["pay"], same_account).await?;
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO mailbox (id, to_agent, from_agent, body, paid_minor) VALUES ($1,$2,$3,$4,$5)")
        .bind(&id).bind(&to).bind(&signer).bind(boxed.to_string()).bind(paid).execute(&app.db).await.map_err(internal)?;
    if let Some(p) = paid {
        wallet::record_ask(&app, &id, &signer, &to, p).await;
    }
    market::note_interaction(&app, &signer, &to).await;
    let lead = credits::charge_lead_if_sourced(&app, &signer, &to).await;
    Ok(Json(json!({"ok": true, "id": id, "lead": lead, "paid": paid})).into_response())
}

#[derive(serde::Deserialize)]
struct PollQ { #[serde(default)] wait: Option<u64> }

/// Long-poll: returns undelivered boxes for the bearer, marking them
/// delivered. Holds up to `wait` seconds (max 30) when the box is empty.
async fn inbox_poll(State(app): State<Shared>, Extension(auth): Extension<Auth>, Query(q): Query<PollQ>) -> ApiResult {
    let (me, _) = require_linked(&app, &auth).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(q.wait.unwrap_or(0).min(30));
    // Presence: a device that polls its inbox is online. The console shows
    // it, and it is what "your phone is reachable" means.
    let _ = sqlx::query(
        "INSERT INTO agents_seen (agent, last_poll) VALUES ($1, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
         ON CONFLICT (agent) DO UPDATE SET last_poll = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(&me).execute(&app.db).await;
    // Housekeeping: delivered boxes older than a day, undelivered older than 7.
    let _ = sqlx::query(
        "DELETE FROM mailbox WHERE (delivered_at IS NOT NULL AND delivered_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day')) \
            OR created_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-7 days')",
    ).execute(&app.db).await;
    loop {
        // Claim and read in one statement: two polls racing for the same
        // inbox (a stale long-poll and a fresh one) each get disjoint boxes,
        // so the phone never acts on one message twice.
        let mut rows: Vec<(String, String, String, String, Option<i64>)> = sqlx::query_as(
            "UPDATE mailbox SET delivered_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') \
             WHERE id IN (SELECT id FROM mailbox WHERE to_agent = $1 AND delivered_at IS NULL ORDER BY created_at LIMIT 50) \
               AND delivered_at IS NULL \
             RETURNING id, from_agent, body, created_at, paid_minor",
        ).bind(&me).fetch_all(&app.db).await.map_err(internal)?;
        rows.sort_by(|a, b| a.3.cmp(&b.3));
        if !rows.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(Json(json!({"messages": rows.into_iter().map(|(id, from, body, at, paid)| json!({
                "id": id, "from": from, "createdAt": at, "paid": paid,
                "box": serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)
            })).collect::<Vec<_>>()})).into_response());
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
}

// ------------------------------------------------------------- admin console

pub(crate) fn require_admin(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let k = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !secret::ct_eq(k, &app.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    Ok(())
}

/// `GET /admin/metrics` — the founder's view: accounts, plans, money,
/// credits, calls per day, and every account with its standing. Read from
/// the console's developer mode with the admin key.
async fn admin_metrics(State(app): State<Shared>, headers: HeaderMap) -> ApiResult {
    require_admin(&app, &headers)?;
    let db = &app.db;
    let count = |sql: &'static str| async move {
        sqlx::query_as::<_, (i64,)>(sql).fetch_one(db).await.map(|r| r.0).unwrap_or(0)
    };
    let accounts_total = count("SELECT count(*) FROM accounts").await;
    let accounts_7d = count("SELECT count(*) FROM accounts WHERE created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-7 days')").await;
    let agents_total = count("SELECT count(*) FROM agents WHERE revoked_at IS NULL AND name IS NOT NULL").await;
    let devices_total = count("SELECT count(*) FROM account_agents WHERE kind = 'phone'").await;
    let online = count("SELECT count(*) FROM agents_seen WHERE last_poll > strftime('%Y-%m-%dT%H:%M:%fZ','now','-90 seconds')").await;
    let plans_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT CASE WHEN expires_at IS NOT NULL AND expires_at < strftime('%Y-%m-%dT%H:%M:%fZ','now') THEN 'expired' ELSE plan END, count(*) \
         FROM plans WHERE agent LIKE 'acct:%' GROUP BY 1").fetch_all(db).await.unwrap_or_default();
    let money: Vec<(String, i64, f64)> = sqlx::query_as(
        "SELECT plan, count(*), COALESCE(SUM(amount), 0) FROM plan_requests WHERE status = 'paid' GROUP BY plan").fetch_all(db).await.unwrap_or_default();
    let money_30d: (Option<f64>,) = sqlx::query_as(
        "SELECT SUM(amount) FROM plan_requests WHERE status = 'paid' AND paid_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days')").fetch_one(db).await.unwrap_or((None,));
    let pending: Vec<(String, String, String, f64, String, String)> = sqlx::query_as(
        "SELECT r.ref, r.agent, r.plan, r.amount, r.created_at, COALESCE(a.email, '') FROM plan_requests r \
         LEFT JOIN accounts a ON 'acct:' || a.id = r.agent WHERE r.status = 'pending' ORDER BY r.created_at DESC LIMIT 50").fetch_all(db).await.unwrap_or_default();
    let calls_daily: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT day, SUM(n), SUM(media) FROM usage WHERE day > strftime('%Y-%m-%d','now','-30 days') GROUP BY day ORDER BY day").fetch_all(db).await.unwrap_or_default();
    let credits: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT SUM(CASE WHEN kind = 'topup' THEN delta END), SUM(CASE WHEN kind = 'call' THEN -delta END), SUM(CASE WHEN kind = 'lead' THEN -delta END) FROM credit_ledger").fetch_one(db).await.unwrap_or((None, None, None));
    let rows: Vec<(String, String, Option<String>, Option<String>, String, Option<String>, Option<String>, Option<i64>, i64)> = sqlx::query_as(
        "SELECT a.id, a.email, a.name, a.phone, a.created_at, p.plan, p.expires_at, \
                (SELECT SUM(delta) FROM credit_ledger l WHERE l.account = a.id), \
                (SELECT count(*) FROM account_agents aa WHERE aa.account = a.id AND aa.kind = 'phone') \
         FROM accounts a LEFT JOIN plans p ON p.agent = 'acct:' || a.id ORDER BY a.created_at DESC LIMIT 500").fetch_all(db).await.unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(Json(json!({
        "generatedAt": now,
        "accounts": {"total": accounts_total, "last7d": accounts_7d, "phones": devices_total, "published": agents_total, "online": online},
        "plans": plans_rows.into_iter().map(|(p, n)| json!({"plan": p, "n": n})).collect::<Vec<_>>(),
        "revenue": {"currency": credits::CURRENCY, "last30d": money_30d.0.unwrap_or(0.0),
                    "byProduct": money.into_iter().map(|(p, n, a)| json!({"plan": p, "n": n, "amount": a})).collect::<Vec<_>>()},
        "credits": {"soldMinor": credits.0.unwrap_or(0), "spentOnCallsMinor": credits.1.unwrap_or(0), "spentOnLeadsMinor": credits.2.unwrap_or(0), "callPrice": credits::call_price()},
        "pending": pending.into_iter().map(|(r, s, p, a, at, e)| json!({"ref": r, "subject": s, "plan": p, "amount": a, "createdAt": at, "email": e})).collect::<Vec<_>>(),
        "callsDaily": calls_daily.into_iter().map(|(d, n, m)| json!({"day": d, "calls": n, "media": m})).collect::<Vec<_>>(),
        "rows": rows.into_iter().map(|(id, email, name, phone, at, plan, exp, bal, phones)| {
            let expired = exp.as_deref().is_some_and(|e| e < now.as_str());
            json!({"id": id, "email": email, "name": name, "phone": phone, "createdAt": at,
                   "plan": if expired { "expired".to_string() } else { plan.unwrap_or_else(|| "free".into()) },
                   "expiresAt": exp, "credits": bal.unwrap_or(0), "phones": phones})
        }).collect::<Vec<_>>(),
    })).into_response())
}

#[derive(serde::Deserialize)]
struct AdminCreditsReq { #[serde(default)] account: Option<String>, #[serde(default)] email: Option<String>, #[serde(rename = "amountMinor")] amount_minor: i64, #[serde(default)] note: Option<String> }

/// `POST /admin/credits` — grant (or take back, negative) credits by hand.
async fn admin_credits(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<AdminCreditsReq>) -> ApiResult {
    require_admin(&app, &headers)?;
    let account = match (req.account.as_deref(), req.email.as_deref()) {
        (Some(id), _) => id.to_string(),
        (None, Some(email)) => {
            let row: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE email = $1").bind(email.trim().to_lowercase()).fetch_optional(&app.db).await.map_err(internal)?;
            row.ok_or_else(|| err(StatusCode::NOT_FOUND, "no account with that email"))?.0
        }
        _ => return Err(err(StatusCode::BAD_REQUEST, "need account or email")),
    };
    if req.amount_minor == 0 || req.amount_minor.abs() > 1_000_000 {
        return Err(err(StatusCode::BAD_REQUEST, "amountMinor must be non-zero and at most S/ 10 000"));
    }
    let balance = credits::add(&app, &account, req.amount_minor, "adjust", None, req.note.as_deref().or(Some("admin"))).await?;
    Ok(Json(json!({"ok": true, "account": account, "balance": balance})).into_response())
}
