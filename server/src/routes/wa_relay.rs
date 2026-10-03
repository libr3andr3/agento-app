//! Relays a WhatsApp OTP send for an on-device core that has no WhatsApp
//! bridge session of its own (every business's data lives on the owner's
//! phone — see AgenteCore.kt — but sending the *first* WhatsApp message to
//! a not-yet-verified phone needs an already-paired session, which only this
//! central deployment holds).
//!
//! Deliberately narrow: this relays exactly one thing — a 6-digit code to
//! one phone number — never an arbitrary message, so a compromised or
//! malicious caller can't turn the shared business WhatsApp number into a
//! spam relay. Authenticated by the caller's own per-install identity
//! (yaya_wire::reqsig — the same scheme `identity::signed` uses to call
//! *out* to a gateway, here verified on the way *in*), not a shared secret:
//! nothing this endpoint requires is worth extracting from an APK.

use super::*;

#[derive(Deserialize)]
pub(super) struct RelaySendOtpReq {
    phone: String,
    code: String,
}

pub(super) async fn send_otp_relay(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult {
    let Some(wa) = &state.whatsapp else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "relay not configured on this node"));
    };

    let agent_str = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let agent: yaya_wire::AgentId = agent_str.parse().map_err(|_| err(StatusCode::UNAUTHORIZED, "bad or missing agent id"))?;

    let sig_header = headers.get(yaya_wire::reqsig::HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    let presented = yaya_wire::reqsig::parse(sig_header).map_err(|_| err(StatusCode::UNAUTHORIZED, "bad or missing signature"))?;

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let body_hash = yaya_wire::sha256_hex(&body);
    yaya_wire::reqsig::verify(&agent, &presented, "POST", "/api/wa/relay/send_otp", &body_hash, now, yaya_wire::reqsig::DEFAULT_WINDOW_SECS)
        .map_err(|_| err(StatusCode::UNAUTHORIZED, "signature does not verify"))?;
    state
        .reqsig_nonces
        .claim(&agent, presented.nonce, now)
        .map_err(|_| err(StatusCode::UNAUTHORIZED, "replayed request"))?;

    let req: RelaySendOtpReq = serde_json::from_slice(&body).map_err(|_| err(StatusCode::BAD_REQUEST, "bad json"))?;
    let phone = crate::whatsapp::normalize_phone(&req.phone)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "phone must include country code"))?;
    if req.code.len() != 6 || !req.code.chars().all(|c| c.is_ascii_digit()) {
        return Err(err(StatusCode::BAD_REQUEST, "code must be exactly 6 digits"));
    }

    // Keyed by the caller's own identity (not IP): protects the shared
    // number from any single compromised install, not just any single IP.
    if !state.limits.allow_otp(&agent.to_string(), &phone) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many codes requested — try again later"));
    }

    let msg_id = wa.send_otp(&phone, &req.code).await.map_err(internal)?;
    crate::audit::record(&state, None, "wa_relay_send", &agent.to_string(), "", json!({"phone": phone})).await;
    Ok(Json(json!({"messageId": msg_id})))
}

#[cfg(test)]
mod tests {
    use crate::identity::{signed, Identity};
    use crate::testkit::{self, Mock, APP_KEY};
    use serde_json::{json, Value};

    /// Sends the relay request as a real on-device core would: signed.
    async fn relay(s: &crate::SharedState, id: &Identity, body: &Value, tamper: Option<&Value>) -> (u16, Value) {
        let bytes = serde_json::to_vec(body).unwrap();
        let built = signed(reqwest::Client::new().post("http://x/api/wa/relay/send_otp"), id, "POST", "/api/wa/relay/send_otp", Some(&bytes)).build().unwrap();
        let auth = built.headers()["authorization"].to_str().unwrap().to_string();
        let sig = built.headers()[yaya_wire::reqsig::HEADER].to_str().unwrap().to_string();
        let sent = tamper.unwrap_or(body).clone();
        testkit::call(s, "POST", "/api/wa/relay/send_otp", &[("x-app-key", APP_KEY), ("authorization", &auth), (yaya_wire::reqsig::HEADER, &sig)], Some(sent)).await
    }

    #[tokio::test]
    async fn a_signed_six_digit_code_is_relayed_and_audited() {
        let m = Mock::start().await;
        let s = testkit::state_with_whatsapp(&m).await;
        let id = Identity::ephemeral();
        let (st, v) = relay(&s, &id, &json!({"phone": "+51 999 000 111", "code": "012345"}), None).await;
        assert_eq!((st, v["messageId"].clone()), (200, json!("wamid.test")), "{v}");
        assert_eq!(m.seen_path("/send")[0].body["to"], "51999000111");
        let (actor,): (String,) = sqlx::query_as("SELECT actor FROM audit_log WHERE kind = 'wa_relay_send'").fetch_one(&s.db).await.unwrap();
        assert_eq!(actor, id.id());
    }

    #[tokio::test]
    async fn only_codes_never_messages() {
        let m = Mock::start().await;
        let s = testkit::state_with_whatsapp(&m).await;
        let id = Identity::ephemeral();
        for code in ["12345", "1234567", "12a456", "compra ya en spam.com"] {
            assert_eq!(relay(&s, &id, &json!({"phone": "+51999000111", "code": code}), None).await.0, 400, "{code}");
        }
        assert_eq!(relay(&s, &id, &json!({"phone": "12", "code": "123456"}), None).await.0, 400);
        assert!(m.seen_path("/send").is_empty());
    }

    #[tokio::test]
    async fn unsigned_tampered_or_replayed_requests_are_refused() {
        let m = Mock::start().await;
        let s = testkit::state_with_whatsapp(&m).await;
        let id = Identity::ephemeral();
        let body = json!({"phone": "+51999000111", "code": "123456"});
        // Body changed after signing: the signature no longer covers it.
        assert_eq!(relay(&s, &id, &body, Some(&json!({"phone": "+51988777666", "code": "123456"}))).await.0, 401);
        // No identity at all.
        assert_eq!(testkit::api(&s, "POST", "/api/wa/relay/send_otp", None, Some(body.clone())).await.0, 401);
        // Replay of the very same signed request.
        let bytes = serde_json::to_vec(&body).unwrap();
        let built = signed(reqwest::Client::new().post("http://x/"), &id, "POST", "/api/wa/relay/send_otp", Some(&bytes)).build().unwrap();
        let (auth, sig) = (built.headers()["authorization"].to_str().unwrap().to_string(), built.headers()[yaya_wire::reqsig::HEADER].to_str().unwrap().to_string());
        let h = [("x-app-key", APP_KEY), ("authorization", auth.as_str()), (yaya_wire::reqsig::HEADER, sig.as_str())];
        assert_eq!(testkit::call(&s, "POST", "/api/wa/relay/send_otp", &h, Some(body.clone())).await.0, 200);
        assert_eq!(testkit::call(&s, "POST", "/api/wa/relay/send_otp", &h, Some(body)).await.0, 401);
        assert_eq!(m.seen_path("/send").len(), 1);
    }

    #[tokio::test]
    async fn unavailable_without_a_bridge() {
        let s = testkit::state().await;
        let (st, _) = relay(&s, &Identity::ephemeral(), &json!({"phone": "+51999000111", "code": "123456"}), None).await;
        assert_eq!(st, 503);
    }
}
