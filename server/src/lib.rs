//! agente-core: the business agent as a library. The same crate runs as a
//! plain server binary (`main.rs`) and as `libagente_core.so` inside the
//! Android app (`android.rs`) — the phone is the server.

pub mod account;
pub mod agents;
pub mod agro;
pub mod audit;
pub mod backup;
pub mod contacts;
pub mod db;
pub mod harness;
pub mod identity;
pub mod e2e;
pub mod learning;
pub mod mesh;
pub mod coins;
pub mod node;
pub mod limits;
pub mod outcomes;
pub mod llm;
pub mod locale;
pub mod media;
pub mod net;
pub mod network;
pub mod ops;
pub mod owner;
pub mod niche;
pub mod payout;
pub mod people;
pub mod plugins;
pub mod routes;
pub mod settlement;
pub mod sources;
pub mod ui;
pub mod upstream;
pub mod verified;
pub mod vision;
pub mod voice;
pub mod whatsapp;

#[cfg(test)]
pub mod testkit;

#[cfg(target_os = "android")]
pub mod android;

use std::sync::Arc;

pub struct AppState {
    pub db: db::Db,
    pub llm: Arc<llm::Llm>,
    pub admin_key: String,
    /// Shared secret between the Kotlin shell and the core; on the phone it is
    /// minted per installation. Anti-noise on the loopback, not authentication.
    pub app_key: String,
    /// core.yml + vertical bundles live here; client patches live in the DB.
    pub schemas_dir: std::path::PathBuf,
    /// The plugin kernel: every agent capability is mounted through it.
    pub kernel: harness::Kernel,
    /// Registration throttling; per-business spend ceilings live in the DB.
    pub limits: limits::Limiter,
    /// WhatsApp OTP bridge — present only when WA_BRIDGE_URL is set.
    pub whatsapp: Option<whatsapp::WhatsApp>,
    /// When set, /api/onboard_business demands a proof from /api/verify/check.
    pub require_phone_verification: bool,
    /// Catalog-photo extraction — present only when VISION_API_KEY is set.
    pub vision: Option<vision::Vision>,
    /// This installation's agent identity.
    pub identity: identity::Identity,
    /// The registry/relay/gateway, spoken to as this identity.
    pub registry: network::Registry,
    /// Single-use nonces for inbound yaya_wire::reqsig-authenticated
    /// requests (currently just the WhatsApp OTP relay for on-device cores
    /// that have no bridge session of their own).
    pub reqsig_nonces: yaya_wire::reqsig::NonceCache,
    /// Whisper + TTS: the provider directly (own key) or the gateway.
    pub audio: Option<upstream::Upstream>,
    /// Self-hosted whisper/TTS on our own GPU nodes, tried before
    /// `audio` when configured (YAYA_AUDIO_KEY). Node availability follows
    /// SLURM job churn, so `audio` stays wired as the fallback.
    pub yaya_audio: Option<upstream::Upstream>,
    /// Registry publishing throttle.
    pub publisher: network::Publisher,
    /// Last `/v1/me` from the gateway: plan, caps, usage, tiers, prices.
    pub plan_info: std::sync::Mutex<serde_json::Value>,
    /// D14: the gateway says this agent belongs to a seller account.
    pub seller: std::sync::atomic::AtomicBool,
    /// The niche skill (what agents in this industry+country learned), mounted
    /// into the customer prompt. Synced with the plan.
    pub niche_skill: std::sync::Mutex<Option<String>>,
    /// Reputation notes about network peers (agent ids), fetched when their
    /// message arrives and injected into the customer prompt. TTL 1h.
    pub peer_notes: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
    /// `AGENT_MODE=client`: this core powers the consumer app (a personal
    /// assistant), not a business. Network presence is off — phase 2.
    pub client_mode: bool,
    /// yaya mesh: this device's post-quantum VPN keys and link state.
    pub mesh: crate::mesh::Mesh,
    /// Client mode: `ask_business` calls waiting for a business agent's
    /// reply, fed by the inbox loop (which owns the relay inbox).
    pub asks: network::Asks,
    /// Agents proven to belong to this phone's own Yaya account (the web
    /// console, a sibling phone) and when that was last checked. Owner-scope
    /// relay messages are honoured only from these.
    pub owner_cache: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, bool)>>,
    /// The open pairing code (DECISIONS D3), if the owner started one.
    pub pairing: std::sync::Mutex<Option<owner::Pairing>>,
}

pub type SharedState = Arc<AppState>;

/// Reads a secret that the server refuses to run without.
fn require_secret(name: &str) -> anyhow::Result<String> {
    Ok(yaya_wire::secret::validate(name, std::env::var(name).ok().as_deref(), 32)?)
}

/// Builds the whole application from the environment. Configuration is
/// environment variables on every platform: the Android shell sets them from
/// its JSON config before calling in, so there is exactly one config path.
pub async fn boot() -> anyhow::Result<SharedState> {
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://agente.db".into());
    let db = db::open(&db_url).await?;

    let schemas_dir = std::path::PathBuf::from(
        std::env::var("SCHEMAS_DIR").unwrap_or_else(|_| "schemas".into()),
    );
    anyhow::ensure!(
        schemas_dir.join("core").join("core.yml").exists(),
        "schemas dir not found at {} (set SCHEMAS_DIR)",
        schemas_dir.display()
    );

    let identity = identity::Identity::load_or_create(&db).await?;
    let registry = network::Registry::from_env(identity.clone());
    let llm = Arc::new(llm::Llm::from_env(&identity)?);
    // Speech goes to OpenAI with a key, or through the gateway as this agent.
    // `AUDIO=0` keeps a deployment silent (the consumer app speaks on-device).
    let audio = if std::env::var("AUDIO").map(|v| v == "0").unwrap_or(false) {
        None
    } else {
        Some(upstream::Upstream::resolve("OPENAI_API_KEY", "STT_BASE_URL", "https://api.openai.com/v1", "/v1", &identity))
    };
    // Self-hosted whisper/TTS on our own GPU nodes (behind the
    // local yaya-audio-router, which fans one base URL out to the separate
    // STT/TTS backends). No YAYA_AUDIO_KEY set → this stays None and voice
    // behaves exactly as before, straight to `audio`.
    let yaya_audio = if std::env::var("AUDIO").map(|v| v == "0").unwrap_or(false) {
        None
    } else {
        std::env::var("YAYA_AUDIO_KEY").ok().filter(|s| !s.is_empty()).map(|key| {
            upstream::Upstream::Direct {
                base: std::env::var("YAYA_AUDIO_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:18580/v1".into()),
                key,
            }
        })
    };

    let disabled: Vec<String> = std::env::var("DISABLED_PLUGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut kernel = harness::Kernel::new();
    let client_mode = std::env::var("AGENT_MODE").map(|m| m == "client").unwrap_or(false);
    let network_manifest = plugins::network_tools::load(&db, &network::registry_url()).await;
    kernel.load(plugins::all_with(llm.clone(), client_mode, network_manifest), &disabled)?;

    let whatsapp = whatsapp::WhatsApp::from_env(&identity)?;
    let require_phone_verification = std::env::var("REQUIRE_PHONE_VERIFICATION")
        .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
        .unwrap_or(false);
    anyhow::ensure!(
        !require_phone_verification || whatsapp.is_some(),
        "REQUIRE_PHONE_VERIFICATION is set but WhatsApp is not configured"
    );
    let vision = vision::Vision::from_env(&identity)?;
    if client_mode {
        plugins::assistant::ensure_self(
            &db,
            &std::env::var("COUNTRY").unwrap_or_else(|_| "PE".into()),
            std::env::var("LANGUAGE").ok().as_deref(),
        )
        .await?;
    }
    let mesh = mesh::load_or_create(&db).await?;
    tracing::info!(
        agent = %identity.id(),
        whatsapp = whatsapp.is_some(),
        vision = vision.is_some(),
        llm = %llm.base_url(),
        client_mode,
        "agente-core booted"
    );

    let state = Arc::new(AppState {
        db,
        llm,
        admin_key: require_secret("ADMIN_KEY")?,
        app_key: require_secret("APP_KEY")?,
        schemas_dir,
        kernel,
        limits: limits::Limiter::from_env(),
        whatsapp,
        require_phone_verification,
        vision,
        identity,
        registry,
        reqsig_nonces: yaya_wire::reqsig::NonceCache::new(yaya_wire::reqsig::DEFAULT_WINDOW_SECS),
        audio,
        yaya_audio,
        publisher: network::Publisher::default(),
        peer_notes: Default::default(),
        plan_info: std::sync::Mutex::new(serde_json::Value::Null),
        seller: Default::default(),
        niche_skill: std::sync::Mutex::new(None),
        client_mode,
        mesh,
        asks: Default::default(),
        owner_cache: Default::default(),
        pairing: Default::default(),
    });
    audit::record(&state, None, audit::kind::BOOT, "system", "", serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"), "agent": state.identity.id(), "clientMode": state.client_mode,
    })).await;
    Ok(state)
}

/// Binds BIND_ADDR (default loopback, ephemeral port), reports the bound
/// address through `on_bound`, then serves until the future is dropped.
pub async fn run(on_bound: impl FnOnce(std::net::SocketAddr)) -> anyhow::Result<()> {
    let state = boot().await?;
    // Entitlements: what the gateway says this agent may do today. Synced at
    // boot and every 6 h; the plan screens force a refresh on open.
    let plan_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        loop {
            network::sync_plan(&plan_state).await;
            network::sync_skill(&plan_state).await;
            plugins::network_tools::sync(&plan_state).await;
            if !plan_state.client_mode {
                network::sync_purchases(&plan_state).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        }
    });
    // Webhook events into the operator's own systems: retries with backoff.
    tokio::spawn(ops::flush_loop(state.clone()));
    // Presence on the network: announce this agent shortly after boot, off
    // the request path. Failure is logged and retried on the next dashboard.
    // A personal assistant is nobody's storefront: it never publishes a card.
    if !state.client_mode {
        let announce = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            network::publish(&announce, true).await;
        });
    }
    // Reachability: sealed messages arrive through the registry relay. A
    // business answers customers with the same agent as WhatsApp; a personal
    // orchestrator answers only its owner's devices and the businesses it
    // asked. Polling is also what makes the account roster show it online.
    if std::env::var("NETWORK_INBOX").map(|v| v != "0").unwrap_or(true) {
        state.asks.set_live();
        tokio::spawn(network::inbox_loop(state.clone()));
    }
    // A self-hosted assistant has no screen: the pairing code the owner types
    // into the app is printed here (and at GET /api/pair/code) at every boot.
    if state.client_mode {
        let p = owner::start_pairing(&state);
        tracing::info!(
            agent = %state.identity.id(),
            code = %p["code"].as_str().unwrap_or(""),
            valid_secs = p["expiresInSecs"].as_u64().unwrap_or(0),
            "PAIRING CODE — type it in the app under «Lo tengo en mi equipo»"
        );
    }
    // yaya mesh: once on it (or asked with MESH=1), refresh the rendezvous
    // entry at boot and every 6 h, and serve A2A on the mesh address.
    if std::env::var("MESH").map(|v| v == "1").unwrap_or(false) || mesh::my_ip(&state).is_some() {
        let m = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            loop {
                if let Err(e) = mesh::register(&m).await { tracing::debug!(error = %e, "mesh register"); }
                mesh::apply(&m).await;
                tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
            }
        });
    }
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:0".into());
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!("agente-core listening on {bound}");
    on_bound(bound);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
