//! Tinfoil as a verified backend.
//!
//! Before any prompt leaves the phone, the core fetches the router enclave's
//! SEV-SNP report, verifies it against AMD's roots, requires its measurement
//! to be pinned, and takes the HPKE key the report commits to
//! (`REPORT_DATA[32..64]`). Every request body is then sealed to that key
//! with EHBP, so whatever carries it — the yaya gateway by default — sees
//! headers and ciphertext only.
//!
//! Why boot-time evidence is enough here: the report is not bound to a
//! nonce, so an attacker on the path can replay an old one. That cannot
//! break confidentiality: a body sealed to a key only a genuine, pinned
//! enclave ever held is unreadable to anyone else, and a replayed report's
//! key belongs to an enclave that has since restarted, so the call fails
//! (and is retried after re-attesting). Replay is a denial of service, not a
//! disclosure. For the same reason the TLS-key half of `REPORT_DATA` is not
//! checked: TLS may terminate at the gateway, and nothing depends on it.
//!
//! Routes: through the gateway (`/v1/verified/…`, signed by the agent's
//! identity and metered per call like `/v1/chat/completions`), or — with
//! `TINFOIL_API_KEY` set — straight to Tinfoil and AMD's KDS.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use super::{ehbp, pins, snp, BoxFut, Caps, Evidence, Reply, VerifiedBackend};
use crate::upstream::Upstream;

const FORMAT_V2: &str = "https://tinfoil.sh/predicate/sev-snp-guest/v2";
/// Re-attest at least this often, so a rotated router is picked up promptly.
const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

enum Route {
    /// Through the yaya gateway, as this agent.
    Gateway(Upstream),
    /// Straight to Tinfoil with our own key, and to AMD for VCEKs.
    Direct { key: String, kds: String },
}

struct Session {
    hpke: [u8; 32],
    evidence: Evidence,
    until: Instant,
}

pub struct Tinfoil {
    http: reqwest::Client,
    route: Route,
    pins: RwLock<pins::PinSet>,
    session: Mutex<Option<Session>>,
    /// VCEKs by KDS path. A VCEK certifies one chip at one TCB, so it only
    /// changes when the platform is patched — no need to ask AMD (which
    /// rate-limits hard) on every re-attestation.
    vceks: Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl Tinfoil {
    pub fn from_env(identity: &crate::identity::Identity) -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let route = match var("TINFOIL_API_KEY") {
            Some(key) => Route::Direct { key, kds: var("AMD_KDS_URL").unwrap_or_else(|| "https://kdsintf.amd.com".into()) },
            None => Route::Gateway(Upstream::Gateway {
                base: var("VERIFIED_BASE_URL")
                    .unwrap_or_else(|| format!("{}/v1", crate::network::registry_url()))
                    .trim_end_matches('/')
                    .to_string(),
                identity: identity.clone(),
            }),
        };
        Ok(Self {
            http: crate::net::client(Duration::from_secs(90)),
            route,
            pins: RwLock::new(pins::baked()),
            session: Mutex::new(None),
            vceks: Mutex::new(Default::default()),
        })
    }

    async fn host(&self) -> String {
        self.pins.read().await.tinfoil.host.clone()
    }

    async fn fetch_attestation(&self) -> Result<Value> {
        let req = match &self.route {
            Route::Gateway(up) => up.get(&self.http, "/verified/tinfoil/attestation"),
            Route::Direct { .. } => self.http.get(format!("https://{}/.well-known/tinfoil-attestation", self.host().await)),
        };
        let resp = req.timeout(Duration::from_secs(20)).send().await.context("attestation fetch")?;
        ensure!(resp.status().is_success(), "attestation fetch: HTTP {}", resp.status());
        Ok(resp.json().await?)
    }

    async fn fetch_vcek(&self, kds_path: &str) -> Result<Vec<u8>> {
        if let Some(hit) = self.vceks.lock().await.get(kds_path) {
            return Ok(hit.clone());
        }
        let req = match &self.route {
            Route::Gateway(up) => up.get(&self.http, &format!("/verified/amd/vcek/{kds_path}")),
            Route::Direct { kds, .. } => self.http.get(format!("{kds}/vcek/v1/{kds_path}")),
        };
        let resp = req.timeout(Duration::from_secs(40)).send().await.context("VCEK fetch")?;
        ensure!(resp.status().is_success(), "VCEK fetch: HTTP {}", resp.status());
        let der = resp.bytes().await?.to_vec();
        // Cached before verification on purpose: an unverifiable VCEK fails
        // `snp::verify` every time, it can never make a report pass.
        self.vceks.lock().await.insert(kds_path.to_string(), der.clone());
        Ok(der)
    }

    /// Picks up a newer signed pin set from the gateway. Best effort: a
    /// missing, unsigned, stale or forged bundle leaves the current set.
    async fn refresh_pins(&self) {
        let keys = pins::trusted_keys();
        let Route::Gateway(up) = &self.route else { return };
        if keys.is_empty() {
            return;
        }
        let current = self.pins.read().await.seq;
        let bundle = match up.get(&self.http, "/verified/pins").timeout(Duration::from_secs(10)).send().await {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
            _ => None,
        };
        let Some(bundle) = bundle else { return };
        match pins::verify_update(&bundle, &keys, current) {
            Ok(set) => {
                tracing::info!(seq = set.seq, issued = %set.issued, "verified: pin set updated");
                *self.pins.write().await = set;
            }
            Err(e) => tracing::debug!(error = %e, "verified: pin update not applied"),
        }
    }

    /// Attests the router enclave (or reuses a fresh attestation) and returns
    /// the HPKE key it committed to.
    async fn session(&self, force: bool) -> Result<([u8; 32], Evidence)> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref().filter(|s| !force && s.until > Instant::now()) {
            return Ok((s.hpke, s.evidence.clone()));
        }
        *guard = None;
        self.refresh_pins().await;

        let doc = self.fetch_attestation().await?;
        let format = doc["format"].as_str().unwrap_or_default();
        ensure!(format == FORMAT_V2, "attestation format {format:?} is not {FORMAT_V2}; it would not commit to an HPKE key");
        let gz = base64::engine::general_purpose::STANDARD
            .decode(doc["body"].as_str().ok_or_else(|| anyhow!("attestation has no body"))?)
            .context("attestation body is not base64")?;
        let raw = super::gunzip(&gz)?;
        let kds_path = snp::Report::parse(&raw)?.kds_path()?;
        let vcek = self.fetch_vcek(&kds_path).await?;
        let v = snp::verify(&raw, &vcek, chrono::Utc::now().timestamp())?;

        let pins = self.pins.read().await;
        let release = pins.tinfoil.release_of(&v.measurement).map(String::from).ok_or_else(|| {
            anyhow!(
                "Tinfoil router measurement {} is not pinned (pin set seq {}); refusing to send",
                hex::encode(v.measurement),
                pins.seq
            )
        })?;
        if let Some(floor) = &pins.tinfoil.min_tcb {
            ensure!(v.tcb.at_least(floor), "platform TCB {:?} is below the pinned floor {floor:?}", v.tcb);
        }
        let hpke: [u8; 32] = v.report_data[32..64].try_into().expect("32 bytes");
        ensure!(hpke != [0u8; 32], "report commits to no HPKE key");
        let evidence = Evidence {
            backend: "tinfoil",
            host: pins.tinfoil.host.clone(),
            platform: "sev-snp",
            product: v.product.name().to_string(),
            measurement: hex::encode(v.measurement),
            release,
            tcb: v.tcb,
            verified_at: chrono::Utc::now(),
        };
        drop(pins);
        tracing::info!(release = %evidence.release, product = %evidence.product, "verified: Tinfoil router attested");
        *guard = Some(Session { hpke, evidence: evidence.clone(), until: Instant::now() + SESSION_TTL });
        Ok((hpke, evidence))
    }

    async fn send(&self, sealed: &ehbp::Sealed) -> Result<(u16, Option<String>, Vec<u8>)> {
        let req = match &self.route {
            // The gateway signs over exact body bytes, so this hop is buffered.
            Route::Gateway(up) => up.post_bytes(&self.http, "/verified/tinfoil/chat/completions", sealed.body.clone(), "application/json"),
            // EHBP asks for chunked transfer with no Content-Length.
            Route::Direct { key, .. } => {
                let body = sealed.body.clone();
                self.http
                    .post(format!("https://{}/v1/chat/completions", self.host().await))
                    .bearer_auth(key)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(reqwest::Body::wrap_stream(futures_util::stream::once(async move {
                        Ok::<_, std::io::Error>(body)
                    })))
            }
        };
        let resp = req.header(ehbp::ENC_HEADER, sealed.enc_header()).send().await?;
        let status = resp.status().as_u16();
        let nonce = resp.headers().get(ehbp::NONCE_HEADER).and_then(|v| v.to_str().ok()).map(String::from);
        let bytes = resp.bytes().await?;
        ensure!(bytes.len() <= MAX_RESPONSE, "response larger than {MAX_RESPONSE} bytes");
        Ok((status, nonce, bytes.to_vec()))
    }

    async fn round_trip(&self, body: &Value) -> Result<Reply> {
        let plaintext = serde_json::to_vec(body)?;
        for attempt in 0..2 {
            let (hpke, _) = self.session(attempt > 0).await?;
            let sealed = ehbp::seal(&hpke, &plaintext)?;
            let (status, nonce, bytes) = self.send(&sealed).await?;
            match nonce {
                Some(n) => match sealed.open_response(&n, &bytes) {
                    Ok(pt) => {
                        let body = serde_json::from_slice(&pt).context("decrypted response is not JSON")?;
                        return Ok(Reply { status, body, authenticated: true });
                    }
                    // The enclave may have restarted under a new key.
                    Err(e) if attempt == 0 => tracing::warn!(error = %e, "verified: response did not open — re-attesting"),
                    Err(e) => return Err(e),
                },
                None if (200..300).contains(&status) => {
                    bail!("HTTP {status} without {}: refusing an unauthenticated success", ehbp::NONCE_HEADER)
                }
                // Key-configuration mismatch: the server rejected the request
                // before processing it, so re-attesting and resending is safe.
                None if status == 422 && attempt == 0 => tracing::warn!("verified: key-config mismatch — re-attesting"),
                // Plaintext errors come from the path (the gateway's plan and
                // auth checks, rate limits), never from the enclave. They are
                // surfaced as-is but marked unauthenticated.
                None => {
                    let body = serde_json::from_slice(&bytes)
                        .unwrap_or_else(|_| serde_json::json!({"error": String::from_utf8_lossy(&bytes).chars().take(500).collect::<String>()}));
                    return Ok(Reply { status, body, authenticated: false });
                }
            }
        }
        bail!("verified request failed after re-attesting")
    }
}

impl VerifiedBackend for Tinfoil {
    fn id(&self) -> &'static str {
        "tinfoil"
    }

    fn caps(&self) -> Caps {
        Caps { streaming: true, sealed_bodies: true, audio: true, tools: true, embeddings: true }
    }

    fn model_for<'a>(&'a self, alias: &'a str) -> BoxFut<'a, Option<String>> {
        Box::pin(async move { self.pins.read().await.model(alias, "tinfoil").map(String::from) })
    }

    fn verify(&self) -> BoxFut<'_, Result<Evidence>> {
        Box::pin(async move { self.session(false).await.map(|(_, e)| e) })
    }

    fn chat<'a>(&'a self, body: &'a Value) -> BoxFut<'a, Result<Reply>> {
        Box::pin(self.round_trip(body))
    }
}

#[cfg(test)]
mod live {
    //! Against production. `cargo test --lib verified::tinfoil::live -- --ignored --nocapture`

    use super::*;

    fn direct(key: &str) -> Tinfoil {
        Tinfoil {
            http: crate::net::client(Duration::from_secs(60)),
            route: Route::Direct { key: key.into(), kds: "https://kdsintf.amd.com".into() },
            pins: RwLock::new(pins::baked()),
            session: Mutex::new(None),
            vceks: Mutex::new(Default::default()),
        }
    }

    /// Attests the live router with AMD's KDS and the baked pins.
    #[tokio::test]
    #[ignore]
    async fn attests_the_live_router() {
        let e = direct("unused").verify().await.expect("live router attests against the baked pins");
        println!("{}", serde_json::to_string_pretty(&e).unwrap());
    }

    /// The route the app uses: attestation, VCEK and the sealed call all go
    /// through a yaya gateway (`VERIFIED_E2E_GATEWAY`, e.g. from
    /// `scripts/verified-e2e.sh --local`), which relays to Tinfoil. With a
    /// real `TINFOIL_API_KEY` on the gateway this is a real completion; with
    /// a dummy key the enclave's sealed 401 still proves the relay.
    #[tokio::test]
    #[ignore]
    async fn through_the_gateway() {
        let gw = std::env::var("VERIFIED_E2E_GATEWAY").expect("VERIFIED_E2E_GATEWAY");
        let identity = crate::identity::Identity::ephemeral();
        // A local gateway starts empty: give this throwaway agent a plan, or
        // it is (correctly) refused like any agent with no plan or credits.
        if let Ok(admin) = std::env::var("VERIFIED_E2E_ADMIN_KEY") {
            let r = reqwest::Client::new()
                .post(format!("{}/admin/plan", gw.trim_end_matches('/')))
                .header("x-admin-key", admin)
                .json(&serde_json::json!({"agent": identity.id(), "plan": "pro"}))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success(), "admin/plan: {}", r.text().await.unwrap_or_default());
        }
        let t = Tinfoil {
            http: crate::net::client(Duration::from_secs(90)),
            route: Route::Gateway(Upstream::Gateway {
                base: format!("{}/v1", gw.trim_end_matches('/')),
                identity,
            }),
            pins: RwLock::new(pins::baked()),
            session: Mutex::new(None),
            vceks: Mutex::new(Default::default()),
        };
        let e = t.verify().await.expect("attests through the gateway");
        println!("attested {} {} via gateway", e.backend, e.release);
        let r = t
            .chat(&serde_json::json!({"model": "gpt-oss-120b", "max_tokens": 32,
                "messages": [{"role": "user", "content": "Responde solo: hola"}]}))
            .await
            .expect("sealed round trip through the gateway");
        println!("status={} authenticated={} body={}", r.status, r.authenticated, r.body);
        assert!(r.authenticated, "the enclave's answer must come back sealed");
    }

    /// Seals a request to the live enclave with a key that cannot be valid.
    /// If the enclave's EHBP layer answers, its 401 comes back sealed to this
    /// request — opening it proves this client interoperates with production.
    #[tokio::test]
    #[ignore]
    async fn production_enclave_answers_our_sealed_request() {
        let t = direct("sk-interop-probe-not-a-real-key");
        let reply = t.chat(&serde_json::json!({"model": "gpt-oss-120b", "messages": [{"role": "user", "content": "ping"}]})).await;
        match reply {
            Ok(r) => println!("status={} authenticated={} body={}", r.status, r.authenticated, r.body),
            Err(e) => println!("error: {e:#}"),
        }
    }
}
