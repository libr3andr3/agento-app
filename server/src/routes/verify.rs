//! WhatsApp OTP phone verification.

use super::*;

// ------------------------------------------- phone verification (WhatsApp OTP)

#[derive(Deserialize)]
pub(super) struct VerifyStartReq {
    phone: String,
}

/// Sends a 6-digit code to the phone over WhatsApp. Costs a paid template
/// message per call, hence its own rate buckets — checked before any work,
/// like registration.
pub(super) async fn verify_start(
    State(state): State<SharedState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<VerifyStartReq>,
) -> ApiResult {
    let Some(wa) = &state.whatsapp else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "phone verification is not configured"));
    };
    let phone = crate::whatsapp::normalize_phone(&req.phone)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "phone must include country code"))?;
    let ip = crate::limits::client_ip(&headers, Some(peer));
    if !state.limits.allow_otp(&ip, &phone) {
        tracing::warn!(%ip, %phone, "otp send rate-limited");
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many codes requested — try again later"));
    }

    // Leading zeros are legitimate codes; format keeps all six digits.
    let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32));
    // A resend kills every code still outstanding for this phone: only the
    // latest one can win, so retrying never widens the guessing surface.
    sqlx::query(
        "UPDATE phone_verifications SET expires_at = $2 \
         WHERE phone = $1 AND verified_at IS NULL AND expires_at > $2",
    )
    .bind(&phone)
    .bind(crate::db::now())
    .execute(&state.db)
    .await
    .map_err(internal)?;
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO phone_verifications (id, phone, code_hash, expires_at) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&phone)
    .bind(crate::db::sha256_hex(code.as_bytes()))
    .bind(crate::db::hence(chrono::Duration::minutes(10)))
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    let msg_id = wa.send_otp(&phone, &code).await.map_err(internal)?;
    sqlx::query("UPDATE phone_verifications SET wa_message_id = $1 WHERE id = $2")
        .bind(&msg_id)
        .bind(row.0)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    Ok(Json(json!({"status": "sent", "expiresInSeconds": 600})))
}

#[derive(Deserialize)]
pub(super) struct VerifyCheckReq {
    phone: String,
    code: String,
}

/// Checks the code and, on success, mints the one-time registration proof.
/// The attempt is counted in the same statement that fetches the row, so
/// parallel guesses can't share a free try; five wrong answers kill the code.
pub(super) async fn verify_check(
    State(state): State<SharedState>,
    Json(req): Json<VerifyCheckReq>,
) -> ApiResult {
    if state.whatsapp.is_none() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "phone verification is not configured"));
    }
    let phone = crate::whatsapp::normalize_phone(&req.phone)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "phone must include country code"))?;

    let row: Option<(Uuid, i32, bool)> = sqlx::query_as(
        "UPDATE phone_verifications SET attempts = attempts + 1 \
         WHERE id = (SELECT id FROM phone_verifications \
                     WHERE phone = $1 AND verified_at IS NULL AND expires_at > $3 \
                     ORDER BY created_at DESC LIMIT 1) \
         RETURNING id, attempts, code_hash = $2",
    )
    .bind(&phone)
    .bind(crate::db::sha256_hex(req.code.trim().as_bytes()))
    .bind(crate::db::now())
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;
    let Some((id, attempts, code_ok)) = row else {
        return Err(err(StatusCode::GONE, "no active code for this phone — request a new one"));
    };
    if attempts > 5 || !code_ok {
        return Err(err(StatusCode::UNAUTHORIZED, "wrong code"));
    }

    // Same shape and rules as a device token: 40 chars of CSPRNG, hash-only
    // at rest, single use, expires before it can be hoarded.
    let proof: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    sqlx::query(
        "UPDATE phone_verifications \
         SET verified_at = $3, proof_hash = $1, proof_expires_at = $4 \
         WHERE id = $2",
    )
    .bind(crate::db::sha256_hex(proof.as_bytes()))
    .bind(id)
    .bind(crate::db::now())
    .bind(crate::db::hence(chrono::Duration::minutes(30)))
    .execute(&state.db)
    .await
    .map_err(internal)?;

    Ok(Json(json!({"verified": true, "verificationToken": proof})))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    fn code_from(m: &Mock) -> String {
        m.seen_path("/send").last().unwrap().body["text"].as_str().unwrap()[..6].to_string()
    }

    #[tokio::test]
    async fn unavailable_without_whatsapp() {
        let s = testkit::state().await;
        assert_eq!(api(&s, "POST", "/api/verify/start", None, Some(json!({"phone": "+51999000111"}))).await.0, 503);
        assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "+51999000111", "code": "1"}))).await.0, 503);
    }

    #[tokio::test]
    async fn code_round_trip_mints_a_single_proof() {
        let m = Mock::start().await;
        let s = testkit::state_with_whatsapp(&m).await;
        let (st, v) = api(&s, "POST", "/api/verify/start", None, Some(json!({"phone": "999 000 111"}))).await;
        assert_eq!((st, v["status"].as_str()), (200, Some("sent")));
        assert_eq!(m.seen_path("/send")[0].body["to"], "51999000111", "a 9-digit Peruvian mobile gets 51");
        let code = code_from(&m);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        let (st, v) = api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "+51 999 000 111", "code": format!(" {code} ")}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["verificationToken"].as_str().unwrap().len(), 40);
        // Used: the code cannot be checked again.
        assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "51999000111", "code": code}))).await.0, 410);
    }

    #[tokio::test]
    async fn five_wrong_guesses_kill_the_code_and_resends_replace_it() {
        let m = Mock::start().await;
        let s = testkit::state_with_whatsapp(&m).await;
        api(&s, "POST", "/api/verify/start", None, Some(json!({"phone": "+51999000111"}))).await;
        let good = code_from(&m);
        let wrong = if good == "000000" { "111111" } else { "000000" };
        for _ in 0..5 {
            assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "+51999000111", "code": wrong}))).await.0, 401);
        }
        assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "+51999000111", "code": good}))).await.0, 401, "the right code is dead after five misses");
        // A resend kills the old code: only the latest can win.
        api(&s, "POST", "/api/verify/start", None, Some(json!({"phone": "+51999000111"}))).await;
        let old = good;
        let new = code_from(&m);
        if old != new {
            assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "+51999000111", "code": old}))).await.0, 401);
        }
        assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "+51999000111", "code": new}))).await.0, 200);
    }

    #[tokio::test]
    async fn bad_phones_and_rate_limits() {
        let m = Mock::start().await;
        let s = testkit::state_with_whatsapp(&m).await;
        assert_eq!(api(&s, "POST", "/api/verify/start", None, Some(json!({"phone": "123"}))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/verify/check", None, Some(json!({"phone": "123", "code": "1"}))).await.0, 400);
        let codes: Vec<u16> = futures_codes(&s).await;
        assert_eq!(codes, vec![200, 200, 200, 200, 200, 429], "5 codes per phone per day");
    }

    async fn futures_codes(s: &crate::SharedState) -> Vec<u16> {
        let mut v = vec![];
        for _ in 0..6 {
            v.push(api(s, "POST", "/api/verify/start", None, Some(json!({"phone": "+51988000222"}))).await.0);
        }
        v
    }
}
