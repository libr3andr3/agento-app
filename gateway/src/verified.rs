//! Verified inference: the gateway's side of `server/src/verified`.
//!
//! The phone verifies the provider's enclave itself and seals request bodies
//! to it (EHBP), so this module is deliberately dumb about content: it
//! authenticates the agent, charges the call against its plan like
//! `/v1/chat/completions`, attaches our provider key, and relays ciphertext
//! both ways. It never sees a prompt or an answer.
//!
//! What that costs: the sealed body hides `model` and `max_tokens`, so unlike
//! `chat` this route cannot pin the model or clamp the spend per call. The
//! bounds here are the per-call charge, the request size limit on the route,
//! and a response size cap; the model is chosen by the core from the pin set.
//! A modified client could pick a pricier model within those bounds.
//!
//! Evidence (the attestation document, AMD VCEKs) is relayed and cached for
//! convenience only — the phone verifies every byte against roots it ships
//! with, so a wrong answer here fails verification rather than adding trust.
//! Pin updates are served, never written, by the gateway: `VERIFIED_PINS_FILE`
//! holds a bundle signed offline, and the phone checks that signature.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::auth::{require_linked, Auth};
use crate::{charge, client_ip, env_or, err, with_allowance, ApiResult, Kind, Shared};

const ENC_HEADER: &str = "ehbp-encapsulated-key";
const NONCE_HEADER: &str = "ehbp-response-nonce";
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const ATTESTATION_TTL: Duration = Duration::from_secs(60);
const VCEK_CACHE_MAX: usize = 4096;

fn tinfoil_base() -> String {
    env_or("TINFOIL_URL", "https://inference.tinfoil.sh").trim_end_matches('/').to_string()
}

fn tinfoil_key() -> Option<String> {
    std::env::var("TINFOIL_API_KEY").ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

fn attestation_cache() -> &'static Mutex<Option<(Instant, Bytes)>> {
    static C: OnceLock<Mutex<Option<(Instant, Bytes)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn vcek_cache() -> &'static Mutex<HashMap<String, Bytes>> {
    static C: OnceLock<Mutex<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn bytes_response(status: StatusCode, content_type: &'static str, body: Bytes) -> Response {
    let mut r = (status, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

/// `GET /v1/verified/pins` — the offline-signed pin bundle, if one is published.
pub(crate) async fn pins() -> ApiResult {
    let Some(path) = std::env::var("VERIFIED_PINS_FILE").ok().filter(|p| !p.is_empty()) else {
        return Err(err(StatusCode::NOT_FOUND, "no signed pin set is published"));
    };
    let raw = tokio::fs::read(&path).await.map_err(|e| err(StatusCode::NOT_FOUND, format!("pin set: {e}")))?;
    let bundle: Value = serde_json::from_slice(&raw).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("pin set: {e}")))?;
    if !(bundle["pins"].is_string() && bundle["sig"].is_string() && bundle["key"].is_string()) {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "pin set file is not a signed bundle"));
    }
    Ok(Json(bundle).into_response())
}

/// `GET /v1/verified/tinfoil/attestation` — the router enclave's attestation
/// document, cached for a minute.
pub(crate) async fn tinfoil_attestation(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    require_linked(&app, &auth).await?;
    let mut cache = attestation_cache().lock().await;
    if let Some((at, body)) = cache.as_ref() {
        if at.elapsed() < ATTESTATION_TTL {
            return Ok(bytes_response(StatusCode::OK, "application/json", body.clone()));
        }
    }
    let resp = app.http.get(format!("{}/.well-known/tinfoil-attestation", tinfoil_base())).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("attestation: {e}")))?;
    if !resp.status().is_success() {
        return Err(err(StatusCode::BAD_GATEWAY, format!("attestation: HTTP {}", resp.status())));
    }
    let body = resp.bytes().await.map_err(|e| err(StatusCode::BAD_GATEWAY, format!("attestation: {e}")))?;
    *cache = Some((Instant::now(), body.clone()));
    Ok(bytes_response(StatusCode::OK, "application/json", body))
}

/// `GET /v1/verified/amd/vcek/{product}/{chip}?blSPL=…` — AMD KDS, cached.
/// A VCEK is immutable for a chip at a TCB, and KDS rate-limits, so phones
/// share one fetch. Only well-formed KDS queries are relayed.
pub(crate) async fn amd_vcek(
    State(app): State<Shared>,
    Extension(auth): Extension<Auth>,
    Path((product, chip)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> ApiResult {
    require_linked(&app, &auth).await?;
    let chip_len = match product.as_str() {
        "Genoa" => 128,
        "Turin" => 16,
        _ => return Err(err(StatusCode::BAD_REQUEST, "product must be Genoa or Turin")),
    };
    if chip.len() != chip_len || !chip.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(err(StatusCode::BAD_REQUEST, "chip id is not the expected hex length"));
    }
    let mut spl: HashMap<&str, u8> = HashMap::new();
    for pair in query.as_deref().unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').ok_or_else(|| err(StatusCode::BAD_REQUEST, "malformed query"))?;
        let k = ["fmcSPL", "blSPL", "teeSPL", "snpSPL", "ucodeSPL"].into_iter().find(|n| *n == k)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("unexpected parameter {k}")))?;
        spl.insert(k, v.parse().map_err(|_| err(StatusCode::BAD_REQUEST, format!("{k} is not a patch level")))?);
    }
    for k in ["blSPL", "teeSPL", "snpSPL", "ucodeSPL"] {
        if !spl.contains_key(k) {
            return Err(err(StatusCode::BAD_REQUEST, format!("missing {k}")));
        }
    }
    // Canonical order, so equal requests share a cache entry.
    let q = ["fmcSPL", "blSPL", "teeSPL", "snpSPL", "ucodeSPL"].iter()
        .filter_map(|k| spl.get(k).map(|v| format!("{k}={v}")))
        .collect::<Vec<_>>().join("&");
    let key = format!("{product}/{}?{q}", chip.to_ascii_lowercase());
    if let Some(hit) = vcek_cache().lock().await.get(&key).cloned() {
        return Ok(bytes_response(StatusCode::OK, "application/pkix-cert", hit));
    }
    let url = format!("{}/vcek/v1/{key}", env_or("AMD_KDS_URL", "https://kdsintf.amd.com").trim_end_matches('/'));
    let body = kds_fetch(&app.http, &url).await?;
    let mut cache = vcek_cache().lock().await;
    if cache.len() >= VCEK_CACHE_MAX {
        cache.clear();
    }
    cache.insert(key, body.clone());
    Ok(bytes_response(StatusCode::OK, "application/pkix-cert", body))
}

/// AMD's KDS rate-limits hard (a second request inside seconds gets 429
/// with `Retry-After: 10`) and sometimes stalls instead. One bounded retry
/// after the advertised wait, and a timeout shorter than the phone's, so the
/// phone gets a clean error rather than hanging.
async fn kds_fetch(http: &reqwest::Client, url: &str) -> Result<Bytes, (StatusCode, Json<Value>)> {
    for attempt in 0..2 {
        let resp = http.get(url).timeout(Duration::from_secs(12)).send().await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("AMD KDS: {e}")))?;
        if resp.status().as_u16() == 429 && attempt == 0 {
            let wait = resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(10).min(10);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        if !resp.status().is_success() {
            return Err(err(StatusCode::BAD_GATEWAY, format!("AMD KDS: HTTP {}", resp.status())));
        }
        return resp.bytes().await.map_err(|e| err(StatusCode::BAD_GATEWAY, format!("AMD KDS: {e}")));
    }
    Err(err(StatusCode::BAD_GATEWAY, "AMD KDS: rate limited"))
}

/// `POST /v1/verified/tinfoil/chat/completions` — an EHBP-sealed completion,
/// relayed to Tinfoil with our key and charged like any chat call.
pub(crate) async fn tinfoil_chat(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let key = tinfoil_key().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "verified inference is not configured (TINFOIL_API_KEY)"))?;
    let enc = headers.get(ENC_HEADER).and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "a sealed request needs Ehbp-Encapsulated-Key; plaintext belongs on /v1/chat/completions"))?
        .to_string();
    let (agent, _) = require_linked(&app, &auth).await?;
    let a = charge(&app, &agent, &client_ip(&headers, peer), Kind::Chat).await?;
    // EHBP asks for chunked transfer with no Content-Length.
    let upstream = app.http
        .post(format!("{}/v1/chat/completions", tinfoil_base()))
        .bearer_auth(key)
        .header(header::CONTENT_TYPE, "application/json")
        .header(ENC_HEADER, enc)
        .body(reqwest::Body::wrap_stream(futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) })))
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let nonce = upstream.headers().get(NONCE_HEADER).cloned();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let payload = upstream.bytes().await.map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream body: {e}")))?;
    if payload.len() > MAX_RESPONSE {
        return Err(err(StatusCode::BAD_GATEWAY, "upstream response exceeds the size cap"));
    }
    let mut r = (status, payload).into_response();
    if let Some(n) = nonce {
        r.headers_mut().insert(NONCE_HEADER, n);
    }
    if let Some(ct) = content_type {
        r.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    Ok(with_allowance(r, &a))
}
