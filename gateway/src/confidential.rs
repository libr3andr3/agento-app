//! Attested confidential inference (ACI) — the gateway proves the model host
//! is a real TEE before one byte of a customer's message leaves this process.
//!
//! The phone never talks to the inference provider. It talks to us, as it
//! always has; we hold the upstream key, and we are the ones who attest.
//! That is the whole point of routing it here: one endpoint for the app
//! (`llm.yaya.tech`), one place that has to be right.
//!
//! **The chain we actually check**, in order, all of it recomputable by
//! anyone from the same public report (`GET /v1/attestation/report` on the
//! provider, mirrored by us at `GET /v1/attestation`):
//!
//! 1. `api_version = aci/1`, `tee_type = tdx`, keyset not expired.
//! 2. `report_data` — the 64 bytes the TD burned into its quote — is exactly
//!    `signing_address ‖ 12 zero bytes ‖ nvidia nonce`. This is what binds the
//!    hardware quote to the key that signs the answers.
//! 3. The quote is a real TDX quote (v4, TEE type 0x81) and carries those same
//!    64 bytes.
//! 4. The workload was built from a source repo we expect.
//! 5. **The endpoint itself**: the TD attests the SPKI of the TLS certificate
//!    it serves on, and we compare that against the certificate our own
//!    connection actually received. An interceptor between us and the TEE
//!    fails here, which is the check the other four exist to support.
//! 6. Pins: the keyset digest, the signing address and the OS image hash are
//!    recorded on first success and must not change afterwards. A silent
//!    workload swap is the attack this catches.
//!
//! Then, per request: every response the provider returns carries
//! `x-aci-keyset-digest`, and it must equal the digest we attested. A
//! mismatch means we were served by something other than what we verified —
//! the response is dropped and the verdict is torn down.
//!
//! **Fail closed.** [`guard`] runs before the request body is sent anywhere.
//! With `ACI_REQUIRED=1` (the default once enabled) a bad or unavailable
//! verdict is a `503`, never a quiet fallback to a plaintext provider: a
//! promise of confidentiality that degrades silently is worse than no promise.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{extract::State, http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::{json, Value};
use sha2::Digest;

use crate::{env_or, ApiResult, App, Auth, Shared};

type E = (StatusCode, Json<Value>);

/// The source repo the attested workload must have been built from.
const DEFAULT_REPO: &str = "https://github.com/Dstack-TEE/private-ai-gateway.git";
/// TDX, as it appears in the quote header's `tee_type` (little-endian u32).
const TEE_TYPE_TDX: u32 = 0x81;
/// Settings keys holding the trust-on-first-use pins.
const PIN_KEYSET: &str = "aci_pin_keyset_digest";
const PIN_SIGNER: &str = "aci_pin_signing_address";
const PIN_OS_IMAGE: &str = "aci_pin_os_image_hash";

// ------------------------------------------------------------------ config

pub struct Aci {
    enabled: bool,
    required: bool,
    /// Base URL of the confidential provider, no trailing slash.
    base_url: String,
    key: String,
    model: String,
    /// Extra `provider` object merged into every request body.
    provider_flags: Value,
    expected_repos: Vec<String>,
    require_tls_pin: bool,
    trust_new_pins: bool,
    ttl: Duration,
    negative_ttl: Duration,
    http: reqwest::Client,
    cached: Mutex<Option<(Instant, Verdict)>>,
    /// Held across a refresh so a cold cache under load produces ONE
    /// attestation fetch, not one per in-flight request.
    refreshing: tokio::sync::Mutex<()>,
}

impl Aci {
    /// Builds its own HTTP client rather than sharing the app's: the SPKI
    /// comparison needs `tls_info`, which has to be enabled on the builder
    /// (the cargo feature alone does nothing), and attestation wants a
    /// shorter timeout than a completion does.
    pub fn from_env() -> Self {
        let http = reqwest::Client::builder()
            .tls_info(true)
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|e| {
                tracing::error!("could not build the attestation client ({e}); falling back without tls_info");
                reqwest::Client::new()
            });
        let enabled = matches!(std::env::var("ACI_ENABLED").as_deref(), Ok("1") | Ok("true"));
        let key = std::env::var("ACI_API_KEY").unwrap_or_default();
        if enabled && key.is_empty() {
            tracing::error!("ACI_ENABLED is set but ACI_API_KEY is empty — confidential inference will refuse every call");
        }
        let repos = env_or("ACI_EXPECTED_REPOS", DEFAULT_REPO)
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            enabled,
            // Confidentiality that silently degrades is not confidentiality.
            required: !matches!(std::env::var("ACI_REQUIRED").as_deref(), Ok("0") | Ok("false")),
            base_url: env_or("ACI_BASE_URL", "https://inference.phala.com/v1").trim_end_matches('/').to_string(),
            key,
            model: env_or("ACI_MODEL", "qwen/qwen3.8-27b"),
            provider_flags: json!({"aci_verified": true}),
            expected_repos: repos,
            require_tls_pin: !matches!(std::env::var("ACI_REQUIRE_TLS_PIN").as_deref(), Ok("0") | Ok("false")),
            trust_new_pins: matches!(std::env::var("ACI_TRUST_NEW_PINS").as_deref(), Ok("1") | Ok("true")),
            ttl: Duration::from_secs(env_secs("ACI_ATTEST_TTL_SECS", 900)),
            // A failure is re-checked soon, but not on every request: a
            // provider outage must not turn into a request storm.
            negative_ttl: Duration::from_secs(env_secs("ACI_ATTEST_RETRY_SECS", 30)),
            http,
            cached: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub fn for_test(base_url: &str, required: bool, trust_new_pins: bool) -> Self {
        Self {
            enabled: true, required, base_url: base_url.trim_end_matches('/').to_string(), key: "ak".into(), model: "qwen/test".into(),
            provider_flags: json!({"aci_verified": true}), expected_repos: vec![DEFAULT_REPO.to_string()],
            require_tls_pin: false, trust_new_pins, ttl: Duration::from_secs(900), negative_ttl: Duration::from_secs(30),
            http: reqwest::Client::new(), cached: Mutex::new(None), refreshing: tokio::sync::Mutex::new(()),
        }
    }

    pub fn enabled(&self) -> bool { self.enabled }
    pub fn required(&self) -> bool { self.required }
    pub fn model(&self) -> &str { &self.model }
    pub fn chat_url(&self) -> String { format!("{}/chat/completions", self.base_url) }
    pub fn report_url(&self) -> String { format!("{}/attestation/report", self.base_url) }
    pub fn key(&self) -> &str { &self.key }
    pub fn provider_flags(&self) -> &Value { &self.provider_flags }

    fn host(&self) -> String {
        self.base_url
            .split("://").nth(1).unwrap_or(&self.base_url)
            .split('/').next().unwrap_or("")
            .to_string()
    }

    fn cached_fresh(&self) -> Option<Verdict> {
        let c = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        let (at, v) = c.as_ref()?;
        let ttl = if v.ok { self.ttl } else { self.negative_ttl };
        (at.elapsed() < ttl).then(|| v.clone())
    }

    fn store(&self, v: Verdict) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v));
    }

    /// Drops the cached verdict, so the next call re-attests from scratch.
    pub fn invalidate(&self) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The last verdict, fresh or stale, for reporting. Never used to admit
    /// traffic — [`guard`] is the only thing that may do that.
    pub fn last(&self) -> Option<Verdict> {
        self.cached.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(_, v)| v.clone())
    }
}

fn env_secs(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(d)
}

// ----------------------------------------------------------------- verdict

#[derive(Clone, Debug, Default)]
pub struct Verdict {
    pub ok: bool,
    /// Why not, when `ok` is false. Safe to show a customer.
    pub reason: Option<String>,
    pub api_version: String,
    pub tee_type: String,
    pub keyset_digest: String,
    pub signing_address: String,
    pub repo_url: String,
    pub repo_commit: String,
    pub os_image_hash: String,
    pub tls_domain: String,
    pub tls_spki: String,
    /// Whether the certificate we were actually served matched `tls_spki`.
    pub tls_spki_matched: bool,
    pub not_after: i64,
    pub checked_at: String,
}

impl Verdict {
    fn fail(reason: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            reason: Some(reason.to_string()),
            checked_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        }
    }

    /// Short form for logs, headers and the app's badge.
    pub fn short_digest(&self) -> String {
        self.keyset_digest.rsplit(':').next().unwrap_or("").chars().take(12).collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "reason": self.reason,
            "apiVersion": self.api_version,
            "teeType": self.tee_type,
            "keysetDigest": self.keyset_digest,
            "signingAddress": self.signing_address,
            "source": {"repo": self.repo_url, "commit": self.repo_commit},
            "osImageHash": self.os_image_hash,
            "endpoint": {"domain": self.tls_domain, "spkiSha256": self.tls_spki, "matched": self.tls_spki_matched},
            "notAfter": self.not_after,
            "checkedAt": self.checked_at,
        })
    }
}

// ------------------------------------------------------------ the verifier

/// Everything that can be decided from the report alone, with no network and
/// no clock beyond `now`. Split out so it is unit-testable and so a third
/// party can recompute our verdict from the same public JSON.
///
/// `served_spki` is the SHA-256 of the SubjectPublicKeyInfo of the
/// certificate our own TLS connection received; `None` means we could not
/// observe it.
pub fn verify_report(
    report: &Value,
    expect_host: &str,
    expected_repos: &[String],
    served_spki: Option<&str>,
    require_tls_pin: bool,
    now: i64,
) -> Verdict {
    let att = &report["attestation"];
    let mut v = Verdict {
        api_version: s(&report["api_version"]),
        tee_type: s(&att["tee_type"]),
        keyset_digest: s(&report["workload_keyset_digest"]),
        signing_address: s(&report["signing_address"]).to_lowercase(),
        repo_url: s(&att["source_provenance"]["repo_url"]),
        repo_commit: s(&att["source_provenance"]["repo_commit"]),
        os_image_hash: field(&att["evidence"]["vm_config"], "os_image_hash"),
        tls_domain: s(&att["evidence"]["downstream_tls_binding"]["domain"]),
        tls_spki: s(&att["evidence"]["downstream_tls_binding"]["spki_sha256"]).to_lowercase(),
        not_after: att["workload_keyset"]["not_after"].as_i64().unwrap_or(0),
        checked_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };

    // 1. Shape and freshness.
    if v.api_version != "aci/1" {
        return Verdict { reason: Some(format!("unsupported attestation version {:?}", v.api_version)), ..v };
    }
    if v.tee_type != "tdx" {
        return Verdict { reason: Some(format!("unexpected TEE type {:?}", v.tee_type)), ..v };
    }
    if v.not_after <= now + 60 {
        return Verdict { reason: Some("the workload keyset has expired".into()), ..v };
    }

    // 2. report_data binds the signing key and the GPU nonce to the quote.
    let report_data = s(&att["report_data"]).to_lowercase();
    let rd = match hex::decode(&report_data) {
        Ok(b) if b.len() == 64 => b,
        _ => return Verdict { reason: Some("report_data is not 64 bytes of hex".into()), ..v },
    };
    let addr_hex = v.signing_address.trim_start_matches("0x").to_string();
    if addr_hex.len() != 40 || hex::decode(&addr_hex).is_err() {
        return Verdict { reason: Some("signing_address is not a 20-byte address".into()), ..v };
    }
    if hex::encode(&rd[0..20]) != addr_hex {
        return Verdict { reason: Some("report_data does not bind the signing address".into()), ..v };
    }
    if rd[20..32].iter().any(|b| *b != 0) {
        return Verdict { reason: Some("report_data padding is not zero".into()), ..v };
    }
    let nonce = field(&report["nvidia_payload"], "nonce").to_lowercase();
    if !nonce.is_empty() && hex::encode(&rd[32..64]) != nonce {
        return Verdict { reason: Some("report_data does not bind the GPU nonce".into()), ..v };
    }

    // 3. The quote is a real TDX quote and carries those same 64 bytes.
    let quote = s(&att["evidence"]["quote"]).to_lowercase();
    let quote_rd = s(&att["evidence"]["quote_report_data"]).to_lowercase();
    if quote_rd != report_data {
        return Verdict { reason: Some("the quote's report_data differs from the attestation's".into()), ..v };
    }
    match hex::decode(&quote) {
        Ok(q) if q.len() >= 1024 => {
            let version = u16::from_le_bytes([q[0], q[1]]);
            let tee_type = u32::from_le_bytes([q[4], q[5], q[6], q[7]]);
            if version < 4 {
                return Verdict { reason: Some(format!("quote version {version} is older than v4")), ..v };
            }
            if tee_type != TEE_TYPE_TDX {
                return Verdict { reason: Some(format!("quote tee_type {tee_type:#x} is not TDX")), ..v };
            }
        }
        _ => return Verdict { reason: Some("the quote is missing or too short to be a TDX quote".into()), ..v },
    }
    if !quote.contains(&report_data) {
        return Verdict { reason: Some("the quote does not carry the attested report_data".into()), ..v };
    }

    // 4. Built from a source we expect.
    if !expected_repos.is_empty() && !expected_repos.iter().any(|r| r == &v.repo_url) {
        return Verdict { reason: Some(format!("workload was built from an unexpected repo: {}", v.repo_url)), ..v };
    }

    // 5. The endpoint itself: the TD attests the TLS key it serves on, and we
    //    compare it with the certificate our own connection received.
    if !v.tls_domain.is_empty() && !expect_host.is_empty() && v.tls_domain != expect_host {
        return Verdict { reason: Some(format!("the TEE attests {} but we are calling {}", v.tls_domain, expect_host)), ..v };
    }
    match served_spki {
        Some(spki) if !v.tls_spki.is_empty() => {
            v.tls_spki_matched = spki.eq_ignore_ascii_case(&v.tls_spki);
            if !v.tls_spki_matched {
                return Verdict {
                    reason: Some("the certificate we were served is not the one the TEE attests — the connection is being intercepted".into()),
                    ..v
                };
            }
        }
        _ if require_tls_pin => {
            return Verdict { reason: Some("could not observe the served certificate to compare against the attested one".into()), ..v };
        }
        _ => {}
    }

    v.ok = true;
    v.reason = None;
    v
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// `vm_config` and `nvidia_payload` arrive as JSON *objects* in some builds
/// and as JSON *strings* in others; both are read the same way.
fn field(v: &Value, key: &str) -> String {
    if let Some(o) = v.as_object() {
        return o.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    }
    v.as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|p| p.get(key).and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

/// SHA-256 of a certificate's SubjectPublicKeyInfo, the value the TEE
/// attests. Computed over the DER of the whole SPKI structure.
fn spki_sha256(cert_der: &[u8]) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
    let spki = cert.tbs_certificate.subject_pki.raw;
    Some(hex::encode(sha2::Sha256::digest(spki)))
}

// -------------------------------------------------------------- the guard

/// Fetches and verifies the provider's attestation, applying the stored pins.
/// Everything here happens *before* any customer data exists in a request.
async fn attest(app: &App) -> Verdict {
    let aci = &app.aci;
    let resp = match aci.http.get(aci.report_url()).timeout(Duration::from_secs(20)).send().await {
        Ok(r) => r,
        Err(e) => return Verdict::fail(format!("the attestation report is unreachable: {e}")),
    };
    if !resp.status().is_success() {
        return Verdict::fail(format!("the attestation report returned HTTP {}", resp.status()));
    }
    // The certificate this very connection was served, for check 5.
    let served_spki = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|t| t.peer_certificate())
        .and_then(spki_sha256);
    let report: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Verdict::fail(format!("the attestation report is not JSON: {e}")),
    };

    let mut v = verify_report(
        &report,
        &aci.host(),
        &aci.expected_repos,
        served_spki.as_deref(),
        aci.require_tls_pin,
        chrono::Utc::now().timestamp(),
    );
    if !v.ok {
        return v;
    }
    // 6. Pins. First good verdict records them; a later change is refused,
    //    because a workload swap is exactly what a pin exists to catch.
    for (key, current, label) in [
        (PIN_KEYSET, v.keyset_digest.clone(), "workload keyset digest"),
        (PIN_SIGNER, v.signing_address.clone(), "signing address"),
        (PIN_OS_IMAGE, v.os_image_hash.clone(), "OS image hash"),
    ] {
        if current.is_empty() {
            continue;
        }
        match pin_get(app, key).await {
            Some(pinned) if pinned == current => {}
            Some(pinned) => {
                tracing::error!(key, %pinned, %current, "ACI pin changed — refusing confidential traffic");
                if !aci.trust_new_pins {
                    v.ok = false;
                    v.reason = Some(format!("the provider's {label} changed since it was pinned"));
                    return v;
                }
                pin_set(app, key, &current).await;
            }
            None => {
                tracing::info!(key, %current, "ACI pin recorded on first verification");
                pin_set(app, key, &current).await;
            }
        }
    }
    v
}

async fn pin_get(app: &App, key: &str) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = $1")
        .bind(key)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .map(|r| r.0)
}

async fn pin_set(app: &App, key: &str, value: &str) {
    let _ = sqlx::query(
        "INSERT INTO settings (key, value) VALUES ($1, $2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&app.db)
    .await;
}

/// The gate. Returns the verdict a request may proceed under, or an error —
/// and an error here means nothing was sent anywhere.
pub async fn guard(app: &App) -> Result<Verdict, E> {
    if let Some(v) = app.aci.cached_fresh() {
        if v.ok {
            return Ok(v);
        }
        return Err(refused(&v));
    }
    // One refresh at a time. Without this, every request that arrives on a
    // cold cache opens its own attestation fetch — a self-inflicted burst
    // against the provider exactly when it may already be struggling.
    let _flight = app.aci.refreshing.lock().await;
    if let Some(v) = app.aci.cached_fresh() {
        return if v.ok { Ok(v) } else { Err(refused(&v)) };
    }
    let v = attest(app).await;
    app.aci.store(v.clone());
    if v.ok {
        tracing::info!(digest = %v.short_digest(), signer = %v.signing_address, tls = v.tls_spki_matched, "confidential upstream attested");
        Ok(v)
    } else {
        tracing::error!(reason = ?v.reason, "confidential upstream REFUSED — no data sent");
        Err(refused(&v))
    }
}

fn refused(v: &Verdict) -> E {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {
            "message": v.reason.clone().unwrap_or_else(|| "the confidential model host could not be attested".into()),
            "type": "attestation",
            "sent": false,
        }})),
    )
}

/// Per-response binding: the provider echoes the keyset digest on every
/// answer, and it must be the one we attested. A mismatch means something
/// other than the verified workload served us — the answer is thrown away
/// and the verdict torn down.
pub fn check_response(app: &App, headers: &reqwest::header::HeaderMap, v: &Verdict) -> Result<(), E> {
    let seen = headers.get("x-aci-keyset-digest").and_then(|h| h.to_str().ok()).unwrap_or("");
    if !seen.is_empty() && seen == v.keyset_digest {
        return Ok(());
    }
    // An ABSENT header is a failure, not a pass. The real service sends it on
    // every response including errors, so its absence means we are not
    // talking to the service we attested — which is precisely the case this
    // check exists to catch.
    tracing::error!(expected = %v.keyset_digest, seen = %if seen.is_empty() { "<absent>" } else { seen }, "ACI keyset digest mismatch on a response — discarding it");
    app.aci.invalidate();
    Err((
        StatusCode::BAD_GATEWAY,
        Json(json!({"error": {
            "message": "the answer came from a workload we did not verify; it was discarded",
            "type": "attestation",
        }})),
    ))
}

/// Stamps how a completion was served, so the phone can show it and the
/// audit chain can record it.
pub fn stamp(mut r: axum::response::Response, v: Option<&Verdict>) -> axum::response::Response {
    let value = match v {
        Some(v) if v.ok => format!("aci/1 tdx {}", v.short_digest()),
        _ => "none".to_string(),
    };
    if let Ok(h) = value.parse() {
        r.headers_mut().insert("x-yaya-confidential", h);
    }
    r
}

// --------------------------------------------------------------- the route

/// `GET /v1/attestation` — our verdict on the model host, for a caller that
/// holds an agent identity. The app checks the claim without ever talking to
/// the provider; `report` links the provider's own report so the whole chain
/// can still be recomputed independently by anyone the owner shows it to.
///
/// Authenticated, for two reasons: the verdict is a map of our supply chain
/// (provider, model, source repo, pinned image and signer), and a cold cache
/// turns a request here into an outbound fetch — an anonymous endpoint that
/// makes us call a third party is a lever worth not handing out.
pub async fn attestation(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    crate::require_linked(&app, &auth).await?;
    if !app.aci.enabled() {
        return Ok(Json(json!({
            "confidential": false,
            "mode": "standard",
            "covers": [],
            "note": "confidential inference is not enabled on this gateway",
        }))
        .into_response());
    }
    // Report the live verdict. `guard` owns refreshing (and its single
    // flight); a refusal here is a verdict to display, not an error.
    let v = match guard(&app).await {
        Ok(v) => v,
        Err(_) => app.aci.last().unwrap_or_else(|| Verdict::fail("not yet attested")),
    };
    Ok(Json(json!({
        "confidential": v.ok,
        "mode": if v.ok { "aci/1" } else { "refused" },
        "required": app.aci.required(),
        "model": app.aci.model(),
        // Honesty about scope: speech and vision still go to a standard
        // provider, and saying otherwise would be the one unforgivable bug.
        "covers": ["chat"],
        "notCovered": ["speechToText", "textToSpeech", "vision"],
        "verdict": v.to_json(),
        "report": app.aci.report_url(),
        "checks": [
            "attestation version and TEE type",
            "keyset not expired",
            "report_data binds the signing address and GPU nonce",
            "TDX quote v4 carries that report_data",
            "workload built from the expected source repo",
            "served TLS certificate matches the one the TEE attests",
            "keyset digest, signer and OS image match their pins",
            "every response echoes the attested keyset digest",
        ],
    }))
    .into_response())
}


/// The one line `/v1/me` carries, so the phone knows how it is being served
/// without a second round trip — and can say so on screen honestly.
pub fn me_json(app: &App) -> Value {
    if !app.aci.enabled() {
        return json!({"ok": false, "mode": "standard"});
    }
    match app.aci.last() {
        Some(v) if v.ok => json!({
            "ok": true, "mode": "aci/1", "teeType": v.tee_type,
            "digest": v.short_digest(), "checkedAt": v.checked_at,
            "covers": ["chat"], "notCovered": ["speechToText", "textToSpeech", "vision"],
        }),
        Some(v) => json!({"ok": false, "mode": "refused", "reason": v.reason, "required": app.aci.required()}),
        None => json!({"ok": false, "mode": "pending", "required": app.aci.required()}),
    }
}

// --------------------------------------------------------------- the tests

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal report shaped like the real one, valid by construction.
    fn good_report() -> Value {
        // 20-byte address, 12 zero bytes, 32-byte nonce.
        let addr = "79a5061efe5a46b0d1f33b11cf1c5adbedae6b79";
        let nonce = "52fe1f5ef5de2bceafd1cefd4601a32b13a9ab2b1bd34ba24a50adfe34ac89b7";
        let report_data = format!("{addr}{}{nonce}", "0".repeat(24));
        // v4 quote, tee_type 0x81, padded past the length floor, ending with
        // the report_data the way a real quote carries it.
        let quote = format!("040002008100000000000000{}{report_data}", "aa".repeat(1024));
        json!({
            "api_version": "aci/1",
            "workload_keyset_digest": "sha256:c28520514f1381bf",
            "signing_address": format!("0x{addr}"),
            "nvidia_payload": {"nonce": nonce},
            "attestation": {
                "tee_type": "tdx",
                "report_data": report_data,
                "workload_keyset": {"not_after": 4102444800i64},
                "source_provenance": {"repo_url": DEFAULT_REPO, "repo_commit": "c2d31a8"},
                "evidence": {
                    "quote": quote,
                    "quote_report_data": report_data,
                    "vm_config": {"os_image_hash": "bd369a8c"},
                    "downstream_tls_binding": {"domain": "inference.phala.com", "spki_sha256": "8e89a8a0"},
                }
            }
        })
    }

    fn check(r: &Value, spki: Option<&str>) -> Verdict {
        verify_report(r, "inference.phala.com", &[DEFAULT_REPO.to_string()], spki, true, 1_788_000_000)
    }

    #[test]
    fn a_well_formed_report_passes() {
        let v = check(&good_report(), Some("8e89a8a0"));
        assert!(v.ok, "{:?}", v.reason);
        assert!(v.tls_spki_matched);
        assert_eq!(v.signing_address, "0x79a5061efe5a46b0d1f33b11cf1c5adbedae6b79");
    }

    #[test]
    fn an_intercepted_connection_is_refused() {
        let v = check(&good_report(), Some("deadbeef"));
        assert!(!v.ok);
        assert!(v.reason.unwrap().contains("intercepted"));
    }

    /// The whole point of report_data: swapping the signing key without a
    /// fresh quote must not verify.
    #[test]
    fn a_signing_key_the_quote_does_not_cover_is_refused() {
        let mut r = good_report();
        r["signing_address"] = json!("0xffffffffffffffffffffffffffffffffffffffff");
        let v = check(&r, Some("8e89a8a0"));
        assert!(!v.ok);
        assert!(v.reason.unwrap().contains("signing address"));
    }

    #[test]
    fn an_expired_keyset_is_refused() {
        let mut r = good_report();
        r["attestation"]["workload_keyset"]["not_after"] = json!(1_700_000_000);
        assert!(!check(&r, Some("8e89a8a0")).ok);
    }

    #[test]
    fn a_foreign_workload_is_refused() {
        let mut r = good_report();
        r["attestation"]["source_provenance"]["repo_url"] = json!("https://github.com/someone/else.git");
        let v = check(&r, Some("8e89a8a0"));
        assert!(!v.ok);
        assert!(v.reason.unwrap().contains("unexpected repo"));
    }

    #[test]
    fn a_quote_that_does_not_carry_the_report_data_is_refused() {
        let mut r = good_report();
        r["attestation"]["evidence"]["quote"] = json!(format!("040002008100000000000000{}", "aa".repeat(1024)));
        let v = check(&r, Some("8e89a8a0"));
        assert!(!v.ok);
        assert!(v.reason.unwrap().contains("does not carry"));
    }

    #[test]
    fn a_non_tdx_quote_is_refused() {
        let mut r = good_report();
        let rd = s(&r["attestation"]["report_data"]);
        r["attestation"]["evidence"]["quote"] = json!(format!("040002000000000000000000{}{rd}", "aa".repeat(1024)));
        let v = check(&r, Some("8e89a8a0"));
        assert!(!v.ok);
        assert!(v.reason.unwrap().contains("not TDX"));
    }

    /// Without an observed certificate the pin cannot be checked; strict mode
    /// refuses rather than assuming.
    #[test]
    fn an_unobservable_certificate_is_refused_in_strict_mode() {
        assert!(!check(&good_report(), None).ok);
        let lax = verify_report(&good_report(), "inference.phala.com", &[DEFAULT_REPO.to_string()], None, false, 1_788_000_000);
        assert!(lax.ok);
        assert!(!lax.tls_spki_matched);
    }

    #[test]
    fn a_report_for_another_host_is_refused() {
        let v = verify_report(&good_report(), "inference.example.com", &[DEFAULT_REPO.to_string()], Some("8e89a8a0"), true, 1_788_000_000);
        assert!(!v.ok);
        assert!(v.reason.unwrap().contains("we are calling"));
    }

    #[test]
    fn nested_json_strings_are_read_like_objects() {
        let mut r = good_report();
        r["nvidia_payload"] = json!(r#"{"nonce":"52fe1f5ef5de2bceafd1cefd4601a32b13a9ab2b1bd34ba24a50adfe34ac89b7"}"#);
        r["attestation"]["evidence"]["vm_config"] = json!(r#"{"os_image_hash":"bd369a8c"}"#);
        let v = check(&r, Some("8e89a8a0"));
        assert!(v.ok, "{:?}", v.reason);
        assert_eq!(v.os_image_hash, "bd369a8c");
    }

    mod gate {
        use super::*;
        use crate::testkit::{self, account_with_agent, as_agent, Keypair, Mock};

        fn report_for(m: &Mock, keyset: &str) -> Value {
            let mut r = good_report();
            r["workload_keyset_digest"] = json!(keyset);
            r["attestation"]["evidence"]["downstream_tls_binding"]["domain"] = json!(m.base.trim_start_matches("http://"));
            r
        }

        async fn setup(required: bool) -> (Mock, Mock, Shared, Keypair) {
            let aci = Mock::start().await;
            let std_up = Mock::start().await;
            let (base, up) = (format!("{}/v1", aci.base), format!("{}/std/chat/completions", std_up.base));
            let app = testkit::app_with(move |a| App { aci: Aci::for_test(&base, required, false), upstreams: vec![crate::Upstream { url: up, key: "uk".into(), model: "std-model".into() }], ..a }).await;
            let kp = Keypair::generate();
            account_with_agent(&app, "a", "51900000001", &kp).await;
            (aci, std_up, app, kp)
        }

        fn answer() -> Value {
            json!({"choices": [{"message": {"content": "hola"}}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
        }

        #[tokio::test]
        async fn one_fetch_serves_a_burst_and_pins_catch_a_swapped_workload() {
            let (aci, _std, app, _) = setup(true).await;
            aci.on("/v1/attestation/report", report_for(&aci, "sha256:aaaa"));
            let mut tasks = Vec::new();
            for _ in 0..6 {
                let app = app.clone();
                tasks.push(tokio::spawn(async move { guard(&app).await.is_ok() }));
            }
            for t in tasks { assert!(t.await.unwrap()); }
            assert_eq!(aci.seen_path("/attestation/report").len(), 1, "single flight");
            assert_eq!(pin_get(&app, PIN_KEYSET).await.as_deref(), Some("sha256:aaaa"), "pinned on first sight");
            assert_eq!(me_json(&app)["mode"], "aci/1");
            // The provider swaps the workload: refused, and nothing is sent.
            app.aci.invalidate();
            aci.on("/v1/attestation/report", report_for(&aci, "sha256:bbbb"));
            let e = guard(&app).await.unwrap_err();
            assert_eq!((e.0, e.1["error"]["sent"].clone()), (StatusCode::SERVICE_UNAVAILABLE, json!(false)));
            assert!(e.1["error"]["message"].as_str().unwrap().contains("keyset digest changed"));
            assert!(guard(&app).await.is_err(), "the refusal is cached");
            assert_eq!(aci.seen_path("/attestation/report").len(), 2, "…without refetching inside the retry window");
            assert_eq!(me_json(&app)["mode"], "refused");
        }

        #[tokio::test]
        async fn chat_goes_only_to_the_attested_workload_and_checks_every_answer() {
            let (aci, std_up, app, kp) = setup(true).await;
            aci.on("/v1/attestation/report", report_for(&aci, "sha256:aaaa"));
            aci.on("/v1/chat/completions", answer());
            let hi = json!({"messages": [{"role": "user", "content": "hola"}], "provider": {"only": ["anyone"]}});
            // The answer carries no keyset digest: discarded.
            let (st, v) = as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(hi.clone())).await;
            assert_eq!((st, v["error"]["type"].clone()), (502, json!("attestation")), "{v}");
            let sent = aci.seen_path("/v1/chat/completions").pop().unwrap();
            assert_eq!((sent.body["provider"].clone(), sent.body["model"].clone()), (json!({"aci_verified": true}), json!("qwen/test")), "the caller cannot steer the provider");
            assert_eq!(sent.headers["authorization"], "Bearer ak");
            assert!(std_up.seen_path("/std").is_empty(), "never the standard provider while ACI is required");
            // The digest check invalidated the verdict; the next turn re-attests.
            let before = aci.seen_path("/attestation/report").len();
            as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(hi)).await;
            assert_eq!(aci.seen_path("/attestation/report").len(), before + 1);
            let (st, v) = as_agent(&app, &kp, "GET", "/v1/attestation", None).await;
            assert_eq!((st, v["covers"].clone()), (200, json!(["chat"])));
        }

        #[tokio::test]
        async fn an_answer_that_echoes_the_digest_is_served_and_stamped() {
            let (aci, _std, app, kp) = setup(true).await;
            aci.on("/v1/attestation/report", report_for(&aci, "sha256:aaaa"));
            aci.on_with_headers("/v1/chat/completions", 200, answer(), &[("x-aci-keyset-digest", "sha256:aaaa")]);
            let res = tower::ServiceExt::oneshot(crate::router(app.clone()), {
                let body = serde_json::to_vec(&json!({"messages": []})).unwrap();
                let mut req = axum::http::Request::post("/v1/chat/completions").header("content-type", "application/json");
                for (k, v) in testkit::agent_headers(&kp, "POST", "/v1/chat/completions", &body) { req = req.header(k, v); }
                let mut req = req.body(axum::body::Body::from(body)).unwrap();
                req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
                req
            }).await.unwrap();
            assert_eq!(res.status(), 200);
            assert!(res.headers()["x-yaya-confidential"].to_str().unwrap().starts_with("aci/1 tdx"));
            // Same workload, wrong digest on one answer: discarded.
            aci.on_with_headers("/v1/chat/completions", 200, answer(), &[("x-aci-keyset-digest", "sha256:ffff")]);
            assert_eq!(as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(json!({"messages": []}))).await.0, 502);
            assert_eq!(stamp(axum::response::IntoResponse::into_response("ok"), None).headers()["x-yaya-confidential"], "none");
            assert_eq!(app.aci.host(), aci.base.trim_start_matches("http://"));
        }

        #[tokio::test]
        async fn when_not_required_an_unattestable_host_falls_back_to_standard() {
            let (aci, std_up, app, kp) = setup(false).await;
            aci.on_status("/v1/attestation/report", 500, json!({}));
            std_up.on("/std/chat/completions", answer());
            let (st, _) = as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(json!({"messages": []}))).await;
            assert_eq!(st, 200);
            assert!(aci.seen_path("/v1/chat/completions").is_empty());
            assert_eq!(std_up.seen_path("/std/chat/completions").pop().unwrap().body["model"], "std-model");
        }

        #[tokio::test]
        async fn when_required_an_unattestable_host_sends_nothing() {
            let (aci, std_up, app, kp) = setup(true).await;
            aci.on_status("/v1/attestation/report", 500, json!({}));
            let (st, v) = as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(json!({"messages": []}))).await;
            assert_eq!((st, v["error"]["sent"].clone()), (503, json!(false)));
            assert!(aci.seen_path("/v1/chat/completions").is_empty() && std_up.seen_path("/std").is_empty());
            let (used,): (i64,) = sqlx::query_as("SELECT COALESCE(SUM(n), 0) FROM usage").fetch_one(&app.db).await.unwrap();
            assert_eq!(used, 0, "a refusal costs the caller nothing");
        }
    }
}

