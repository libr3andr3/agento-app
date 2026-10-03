//! WhatsApp OTP delivery — two shapes.
//!
//! `Bridge` talks directly to the wa-otp bridge (../wa-otp): the Meta Cloud
//! API integration this replaces needed a published app, pre-approved
//! templates and a business-verified number; the bridge instead drives a
//! paired WhatsApp account over the Web protocol and sends the code as an
//! ordinary text message. Only a deployment that actually holds a bridge
//! session (the central node) can run this — set WA_BRIDGE_URL/WA_BRIDGE_KEY.
//!
//! `Relay` is for a deployment with no bridge session of its own (every
//! on-device core — see AgenteCore.kt's "the phone IS the server": a fresh
//! install can't have a paired WhatsApp session, so it can't send its own
//! verification code). It signs the send request with this install's own
//! identity (yaya_wire::reqsig) and forwards it to a `Bridge`-mode
//! deployment's `/api/wa/relay/send_otp` — no shared secret ships in the
//! app, unlike a raw WA_BRIDGE_KEY would. Set WA_RELAY_BASE_URL (a public
//! URL, not a secret) to use this mode.
//!
//! Spatially gated like the reminders plugin: mounts only when one of the
//! two is configured, and every route that depends on it answers 503
//! otherwise.

use anyhow::Context;
use serde_json::Value;

use crate::identity::Identity;

pub enum WhatsApp {
    Bridge { http: reqwest::Client, bridge_url: String, bridge_key: String },
    Relay { http: reqwest::Client, base: String, identity: Identity, app_key: String },
}

impl WhatsApp {
    /// WA_BRIDGE_URL set → Bridge (direct, needs a real paired session).
    /// Else WA_RELAY_BASE_URL set → Relay (signed, no bridge needed here).
    /// Neither → None, feature not deployed.
    pub fn from_env(identity: &Identity) -> anyhow::Result<Option<Self>> {
        if let Ok(v) = std::env::var("WA_BRIDGE_URL") {
            if !v.trim().is_empty() {
                let bridge_url = v.trim_end_matches('/').to_string();
                let bridge_key = std::env::var("WA_BRIDGE_KEY")
                    .context("WA_BRIDGE_KEY must be set when WA_BRIDGE_URL is")?;
                anyhow::ensure!(
                    bridge_key.len() >= 32,
                    "WA_BRIDGE_KEY is too short; use the same 32+ char value as the bridge's WA_OTP_KEY"
                );
                return Ok(Some(Self::Bridge {
                    http: crate::net::client(std::time::Duration::from_secs(30)),
                    bridge_url,
                    bridge_key,
                }));
            }
        }
        if let Ok(v) = std::env::var("WA_RELAY_BASE_URL") {
            if !v.trim().is_empty() {
                // Not a secret in the sense bridge_key is (it grants no send
                // capability by itself — the signature does the real work) —
                // see require_app_key's doc comment — but the central
                // deployment's router still checks it on every /api/* route,
                // this one included, so it has to be sent.
                let app_key = std::env::var("WA_RELAY_APP_KEY")
                    .context("WA_RELAY_APP_KEY must be set when WA_RELAY_BASE_URL is")?;
                return Ok(Some(Self::Relay {
                    http: crate::net::client(std::time::Duration::from_secs(30)),
                    base: v.trim_end_matches('/').to_string(),
                    identity: identity.clone(),
                    app_key,
                }));
            }
        }
        Ok(None)
    }

    /// Sends the code as a plain text message (`Bridge`) or forwards a
    /// signed relay request that ends up calling this same method on the
    /// central deployment's own `Bridge` (`Relay`). Either way the caller
    /// gets the bridge's message id back, for tracing delivery problems
    /// across both logs.
    pub async fn send_otp(&self, to_e164: &str, code: &str) -> anyhow::Result<String> {
        match self {
            Self::Bridge { http, bridge_url, bridge_key } => {
                // The code leads the message so it survives push-notification
                // truncation — the owner reads it from the shade without
                // opening WhatsApp (and Android's OTP autofill heuristics can
                // pick it up).
                let text = format!("{code} es tu código de agente. Vence en 10 minutos. No lo compartas.");
                let resp = http
                    .post(format!("{bridge_url}/send"))
                    .header("x-bridge-key", bridge_key)
                    .json(&serde_json::json!({"to": to_e164, "text": text}))
                    .send()
                    .await?;
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or_default();
                anyhow::ensure!(status.is_success(), "wa-otp bridge {status}: {body}");
                body["messageId"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("bridge response had no message id: {body}"))
            }
            Self::Relay { http, base, identity, app_key } => {
                let path = "/api/wa/relay/send_otp";
                let body = serde_json::to_vec(&serde_json::json!({"phone": to_e164, "code": code}))?;
                let resp = crate::identity::signed(
                    http.post(format!("{base}{path}"))
                        .body(body.clone())
                        .header("Content-Type", "application/json")
                        .header("x-app-key", app_key),
                    identity,
                    "POST",
                    path,
                    Some(&body),
                )
                .send()
                .await?;
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or_default();
                anyhow::ensure!(status.is_success(), "otp relay {status}: {body}");
                body["messageId"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("relay response had no message id: {body}"))
            }
        }
    }
}

/// Canonical wire format: bare E.164 digits, no `+`.
///
/// The one convenience applied is the market we operate in: a 9-digit number
/// starting with 9 is a Peruvian mobile and gets the 51 prefix. Everything
/// else must arrive with its country code — guessing one would send someone
/// else's phone a code.
pub fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    match digits.len() {
        9 if digits.starts_with('9') => Some(format!("51{digits}")),
        10..=15 => Some(digits),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_normalization() {
        assert_eq!(normalize_phone("+51 999 000 111").as_deref(), Some("51999000111"));
        assert_eq!(normalize_phone("999000111").as_deref(), Some("51999000111"));
        assert_eq!(normalize_phone("+1 (786) 393-4208").as_deref(), Some("17863934208"));
        assert_eq!(normalize_phone("12345").as_deref(), None, "too short to be routable");
        assert_eq!(normalize_phone("no digits").as_deref(), None);
    }
    use crate::testkit::Mock;
    use serde_json::json;

    #[test]
    fn phone_normalization_bounds() {
        assert_eq!(normalize_phone("899000111"), None, "9 digits not starting with 9 has no country");
        assert_eq!(normalize_phone("1234567890").as_deref(), Some("1234567890"));
        assert_eq!(normalize_phone(&"1".repeat(15)).as_deref(), Some("111111111111111"));
        assert_eq!(normalize_phone(&"1".repeat(16)), None);
    }

    #[tokio::test]
    async fn bridge_sends_the_code_first_with_the_key() {
        let m = Mock::start().await;
        m.on("/send", json!({"messageId": "wamid.1"}));
        let wa = WhatsApp::Bridge { http: reqwest::Client::new(), bridge_url: m.base.clone(), bridge_key: "k".repeat(32) };
        assert_eq!(wa.send_otp("51999000111", "012345").await.unwrap(), "wamid.1");
        let r = &m.seen()[0];
        assert_eq!(r.headers["x-bridge-key"], "k".repeat(32).as_str());
        assert_eq!(r.body["to"], "51999000111");
        assert!(r.body["text"].as_str().unwrap().starts_with("012345 "), "the code survives notification truncation");
    }

    #[tokio::test]
    async fn bridge_errors() {
        let m = Mock::start().await;
        m.on_status("/send", 502, json!({"error": "not paired"}));
        let wa = WhatsApp::Bridge { http: reqwest::Client::new(), bridge_url: m.base.clone(), bridge_key: "k".into() };
        assert!(wa.send_otp("1", "1").await.unwrap_err().to_string().contains("502"));
        m.set("/send", json!({"ok": true}));
        assert!(wa.send_otp("1", "1").await.unwrap_err().to_string().contains("no message id"));
    }

    #[tokio::test]
    async fn relay_signs_as_this_install() {
        let m = Mock::start().await;
        m.on("/api/wa/relay/send_otp", json!({"messageId": "r1"}));
        let id = Identity::ephemeral();
        let wa = WhatsApp::Relay { http: reqwest::Client::new(), base: m.base.clone(), identity: id.clone(), app_key: "appk".into() };
        assert_eq!(wa.send_otp("51999000111", "999999").await.unwrap(), "r1");
        let r = &m.seen()[0];
        assert_eq!(r.headers["x-app-key"], "appk");
        assert_eq!(r.headers["authorization"], format!("Bearer {}", id.id()).as_str());
        assert_eq!(r.body, json!({"phone": "51999000111", "code": "999999"}));
        let p = yaya_wire::reqsig::parse(r.headers[yaya_wire::reqsig::HEADER].to_str().unwrap()).unwrap();
        let agent: yaya_wire::AgentId = id.id().parse().unwrap();
        let hash = yaya_wire::sha256_hex(&serde_json::to_vec(&r.body).unwrap());
        assert!(yaya_wire::reqsig::verify(&agent, &p, "POST", "/api/wa/relay/send_otp", &hash, p.ts, 30).is_ok());
        m.on_status("/api/wa/relay/send_otp", 429, json!({}));
        m.set("/api/wa/relay/send_otp", json!({}));
        assert!(wa.send_otp("1", "1").await.unwrap_err().to_string().contains("no message id"));
    }

    #[test]
    fn not_configured_by_default() {
        // Tests never set WA_BRIDGE_URL / WA_RELAY_BASE_URL.
        assert!(WhatsApp::from_env(&Identity::ephemeral()).unwrap().is_none());
    }
}
