//! wa-otp — WhatsApp bridge for agente's phone verification.
//!
//! Owns the linked-device session (pairing, sqlite state, reconnects) so the
//! agente server doesn't have to: the server talks plain HTTP to this process
//! and can be redeployed freely without touching the WhatsApp session.
//!
//! Endpoints (all require X-Bridge-Key):
//!   GET  /status  → {connected, loggedIn, pairCode, qr, pairing, error}
//!   POST /send    → {"to": "<E.164 digits>", "text": "..."} → {messageId}
//!   POST /pair    → {"phone": "+51…"} → 202; the session store is set aside
//!                   and the process exits so systemd starts it fresh, pairing
//!                   that number. The ops console drives this.
//!
//! Pairing: the number comes from `pair.phone` next to WA_DB (written by
//! /pair) or from WA_PAIR_PHONE. Both the 8-char code and the QR payload land
//! in /status (and the log): scan the QR under WhatsApp → Linked Devices →
//! Link a device, or type the code under "Link with phone number". Once a
//! session exists in WA_DB the pairing options are ignored.
//!
//! This rides the WhatsApp Web protocol, not the Business Cloud API. Meta's
//! terms frown on custom clients — run it on a dedicated number.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use log::{error, info, warn};
use serde_json::{json, Value};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::*;

/// Pairing state shared between the bot callbacks and the HTTP side.
#[derive(Default)]
struct Pairing {
    pair_code: Option<String>,
    qr: Option<String>,
    error: Option<String>,
}

struct Bridge {
    client: Arc<whatsapp_rust::client::Client>,
    key: String,
    connected: Arc<AtomicBool>,
    pairing: Arc<Mutex<Pairing>>,
    /// Digits of the number being linked (empty when a session exists).
    pairing_phone: String,
    db: PathBuf,
}

fn require_env(name: &str) -> anyhow::Result<String> {
    Ok(yaya_wire::secret::validate(name, std::env::var(name).ok().as_deref(), 32)?)
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// `pair.phone` lives next to the session store: written by /pair, read at boot.
fn pair_phone_file(db: &Path) -> PathBuf {
    db.with_file_name("pair.phone")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let key = require_env("WA_OTP_KEY")?;
    let db = PathBuf::from(std::env::var("WA_DB").unwrap_or_else(|_| "wa-otp.db".into()));
    let store = SqliteStore::new(&db.to_string_lossy()).await?;

    let connected = Arc::new(AtomicBool::new(false));
    let pairing: Arc<Mutex<Pairing>> = Arc::new(Mutex::new(Pairing::default()));

    let (c1, c2) = (connected.clone(), connected.clone());
    let (p1, p2, p3, p4) = (pairing.clone(), pairing.clone(), pairing.clone(), pairing.clone());
    let mut builder = Bot::builder()
        .with_backend(store)
        .on_qr_code(move |code, timeout| {
            let p = p1.clone();
            async move {
                info!("QR issued (valid {}s) — scan it from the ops console", timeout.as_secs());
                p.lock().unwrap_or_else(|e| e.into_inner()).qr = Some(code);
            }
        })
        .on_pair_code(move |code, timeout| {
            let p = p2.clone();
            async move {
                info!("PAIR CODE (valid {}s): {code}", timeout.as_secs());
                info!("Enter on the phone: WhatsApp → Linked Devices → Link with phone number");
                p.lock().unwrap_or_else(|e| e.into_inner()).pair_code = Some(code);
            }
        })
        .on_connected(move |_client| {
            let (flag, p) = (c1.clone(), p3.clone());
            async move {
                info!("connected to WhatsApp");
                flag.store(true, Ordering::Relaxed);
                *p.lock().unwrap_or_else(|e| e.into_inner()) = Pairing::default();
            }
        })
        .on_logged_out(move |info| {
            let (flag, p) = (c2.clone(), p4.clone());
            async move {
                error!("logged out ({info:?}) — link the number again from the ops console");
                flag.store(false, Ordering::Relaxed);
                p.lock().unwrap_or_else(|e| e.into_inner()).error =
                    Some("WhatsApp logged this device out — link the number again".into());
            }
        });

    // The number to link: the file the console wrote wins over the env var.
    // Ignored by the bot when a session already exists, so both are harmless
    // across restarts.
    let mut pairing_phone = std::env::var("WA_PAIR_PHONE").map(|s| digits(&s)).unwrap_or_default();
    if let Ok(s) = std::fs::read_to_string(pair_phone_file(&db)) {
        let d = digits(&s);
        if !d.is_empty() {
            pairing_phone = d;
        }
    }
    if !pairing_phone.is_empty() {
        info!("pairing enabled for +{pairing_phone}");
        builder = builder.with_pair_code(PairCodeOptions {
            phone_number: pairing_phone.clone(),
            ..Default::default()
        });
    }

    let bot = builder.build().await.map_err(|e| anyhow::anyhow!("bot build: {e}"))?;
    let handle = bot.spawn();

    let bridge = Arc::new(Bridge {
        client: handle.client(),
        key,
        connected,
        pairing,
        pairing_phone,
        db,
    });

    let addr = std::env::var("WA_OTP_BIND").unwrap_or_else(|_| "127.0.0.1:8123".into());
    let app = Router::new()
        .route("/status", get(status))
        .route("/send", post(send))
        .route("/pair", post(pair))
        .with_state(bridge);
    info!("wa-otp listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tokio::select! {
        r = axum::serve(listener, app) => r?,
        _ = handle => error!("whatsapp bot exited"),
    }
    Ok(())
}

fn key_ok(bridge: &Bridge, headers: &axum::http::HeaderMap) -> bool {
    let got = headers.get("x-bridge-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    yaya_wire::secret::ct_eq(got, &bridge.key)
}

async fn status(
    State(bridge): State<Arc<Bridge>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !key_ok(&bridge, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let logged_in = bridge.client.is_logged_in();
    let p = bridge.pairing.lock().unwrap_or_else(|e| e.into_inner());
    Ok(Json(json!({
        "connected": bridge.connected.load(Ordering::Relaxed),
        "loggedIn": logged_in,
        "pairCode": p.pair_code,
        "qr": p.qr,
        "pairing": if logged_in { "" } else { bridge.pairing_phone.as_str() },
        "error": p.error,
    })))
}

#[derive(serde::Deserialize)]
struct PairReq {
    phone: String,
}

/// Re-link: remember the number, set the session store aside (kept as a
/// .bak), and exit so systemd (Restart=always) starts a fresh pairing.
async fn pair(
    State(bridge): State<Arc<Bridge>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PairReq>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !key_ok(&bridge, &headers) {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "bad bridge key"}))));
    }
    let phone = digits(&req.phone);
    if phone.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "phone with country code required"}))));
    }
    if let Err(e) = std::fs::write(pair_phone_file(&bridge.db), format!("+{phone}\n")) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("pair.phone: {e}")}))));
    }
    let ts = chrono_like_stamp();
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", bridge.db.display()));
        if from.exists() {
            let to = PathBuf::from(format!("{}.bak-{ts}{suffix}", bridge.db.display()));
            if let Err(e) = std::fs::rename(&from, &to) {
                warn!("could not set aside {}: {e}", from.display());
            }
        }
    }
    info!("re-link requested for +{phone}: session store set aside, restarting to pair");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    Ok((StatusCode::ACCEPTED, Json(json!({"ok": true, "pairing": phone, "next": "restarting; the QR and code show in /status within seconds"}))))
}

/// UTC-ish timestamp for backup names without pulling in chrono.
fn chrono_like_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[derive(serde::Deserialize)]
struct SendReq {
    /// E.164 digits, no '+' (the caller — agente's whatsapp.rs — normalizes).
    to: String,
    text: String,
}

async fn send(
    State(bridge): State<Arc<Bridge>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SendReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !key_ok(&bridge, &headers) {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "bad bridge key"}))));
    }
    let digits = digits(&req.to);
    if digits.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "bad phone"}))));
    }
    if !bridge.client.is_logged_in() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "whatsapp session not paired/connected"})),
        ));
    }
    let jid: Jid = format!("{digits}@s.whatsapp.net")
        .parse()
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({"error": format!("bad jid: {e}")}))))?;
    match bridge.client.send_text(jid, req.text).await {
        Ok(sent) => Ok(Json(json!({"messageId": sent.message_id}))),
        Err(e) => {
            error!("send failed: {e}");
            Err((StatusCode::BAD_GATEWAY, Json(json!({"error": format!("send failed: {e}")}))))
        }
    }
}
