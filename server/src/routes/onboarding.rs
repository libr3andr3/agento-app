//! Registration and the onboarding interview.

use super::*;

// --------------------------------------------------------------- onboarding

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OnboardReq {
    business_name: String,
    industry: String,
    owner_phone: String,
    /// Proof minted by /api/verify/check. Required when the server runs with
    /// REQUIRE_PHONE_VERIFICATION; tolerated and ignored otherwise so older
    /// APKs keep working while the flag is off.
    #[serde(default)]
    verification_token: Option<String>,
    /// ISO 3166-1 alpha-2 from the registration picker. Roots the business's
    /// locale (language, currency, timezone, payment rails). Older APKs
    /// don't send it; they were all Peruvian.
    #[serde(default)]
    country: Option<String>,
    /// Business category from the app's fixed picker (agente/docs/CREDITS.md § 4).
    #[serde(default)]
    category: Option<String>,
    /// Closed-loop credit terms the owner accepted on the registration screen.
    #[serde(default)]
    terms_version: Option<String>,
    #[serde(default)]
    terms_accepted_at: Option<String>,
    /// Peru RUC, optional and owner-typed (11 digits, validated client-side).
    #[serde(default)]
    ruc: Option<String>,
    /// Play Store install-referrer attribution (utm_source/medium/campaign,
    /// plus the raw string). Absent on direct-channel installs and on older
    /// APKs that predate the field — all optional, all client-reported.
    #[serde(default)]
    referral_source: Option<String>,
    #[serde(default)]
    referral_medium: Option<String>,
    #[serde(default)]
    referral_campaign: Option<String>,
    #[serde(default)]
    install_referrer: Option<String>,
    /// Custom (tenant) builds: the vertical this business runs on from the
    /// first message, and its website. Both best-effort — an unknown bundle
    /// is `generic`, a bad domain is simply not set.
    #[serde(default)]
    bundle: Option<String>,
    #[serde(default)]
    website: Option<String>,
}

/// Note the coupling: this extractor requires `main.rs` to serve with
/// `into_make_service_with_connect_info::<SocketAddr>()`. Without it the
/// extension is absent and this handler rejects.
pub(super) async fn onboard_business(
    State(state): State<SharedState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<OnboardReq>,
) -> ApiResult {
    // The app key ships inside the APK, so it proves nothing about who is
    // calling. Until owner phone verification exists, throttling is what stands
    // between this route and unlimited business rows, device tokens, and LLM
    // spend. Checked first: a rejected call must cost nothing.
    let ip = crate::limits::client_ip(&headers, Some(peer));
    if !state.limits.allow_registration(&ip, &req.owner_phone) {
        tracing::warn!(%ip, phone = %req.owner_phone, "registration rate-limited");
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many registrations — try again later",
        ));
    }

    // With the flag on, the phone must have completed the OTP dance and the
    // proof is burned here — one verification, one registration. The proof is
    // matched on the canonical phone, so "+51 999..." and "51999..." are the
    // same identity.
    if state.require_phone_verification {
        let canonical = crate::whatsapp::normalize_phone(&req.owner_phone)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "owner phone must include country code"))?;
        let proof = req.verification_token.as_deref().unwrap_or("");
        let consumed: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE phone_verifications SET consumed_at = $3 \
             WHERE phone = $1 \
               AND proof_hash = $2 \
               AND consumed_at IS NULL AND proof_expires_at > $3 \
             RETURNING id",
        )
        .bind(&canonical)
        .bind(crate::db::sha256_hex(proof.as_bytes()))
        .bind(crate::db::now())
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
        if consumed.is_none() {
            return Err(err(
                StatusCode::FORBIDDEN,
                "owner phone is not verified — complete /api/verify first",
            ));
        }
    }

    // The prohibited-business gate, before any row exists: the gateway's
    // list when it answered once, the bundled default otherwise.
    let category = req.category.as_deref().map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).map(|c| c.chars().take(40).collect::<String>());
    if let Some(c) = category.as_deref() {
        if crate::outcomes::is_prohibited(&state, c).await {
            return Err(err(StatusCode::FORBIDDEN, "prohibited_category"));
        }
    }
    let country = req
        .country
        .as_deref()
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()))
        .unwrap_or_else(|| "PE".to_string());
    let terms_version = req.terms_version.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(|v| v.chars().take(20).collect::<String>());
    let terms_at = req.terms_accepted_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&chrono::Utc).to_rfc3339());
    // Play's own contract caps the referrer string at 1024 bytes; this is
    // just matching that in case a client sends something else entirely.
    let cap = |s: &Option<String>| s.as_deref().map(|v| v.chars().take(1024).collect::<String>());
    let referral_source = cap(&req.referral_source);
    let referral_medium = cap(&req.referral_medium);
    let referral_campaign = cap(&req.referral_campaign);
    let install_referrer = cap(&req.install_referrer);
    // 11 digits or nothing: the client validates the same rule, and a
    // malformed RUC is worse than no RUC when SUNAT sees it.
    let ruc = req
        .ruc
        .as_deref()
        .map(str::trim)
        .filter(|v| v.len() == 11 && v.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string);
    let biz: (Uuid,) = sqlx::query_as(
        "INSERT INTO businesses (id, name, industry, owner_phone, trial_ends_at, country, category, terms_version, terms_accepted_at, \
                                 ruc, referral_source, referral_medium, referral_campaign, install_referrer) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&req.business_name)
    .bind(&req.industry)
    .bind(&req.owner_phone)
    .bind(crate::db::hence(chrono::Duration::days(trial_days() as i64)))
    .bind(&country)
    .bind(&category)
    .bind(&terms_version)
    .bind(&terms_at)
    .bind(&ruc)
    .bind(&referral_source)
    .bind(&referral_medium)
    .bind(&referral_campaign)
    .bind(&install_referrer)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;
    // The account learns the category and the terms acceptance (best effort;
    // the gateway keeps its own prohibited list and may still say no).
    {
        let bg = state.clone();
        let (c, v, at) = (category.clone(), terms_version.clone(), terms_at.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::outcomes::send_profile(&bg, c.as_deref(), v.as_deref(), at.as_deref()).await {
                tracing::warn!(error = %e, "account profile not sent");
            }
        });
    }
    // The owner is the first row of the CRM: name/email from the Yaya account.
    let acct = crate::account::load(&state).await.ok().flatten();
    crate::contacts::owner(&state.db, biz.0, acct.as_ref().and_then(|a| a.name.as_deref()), acct.as_ref().map(|a| a.email.as_str()), Some(&req.owner_phone)).await;

    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    // Only the hash is persisted; this response is the one and only time the
    // token itself exists server-side.
    sqlx::query(
        "INSERT INTO devices (id, business_id, token_hash) VALUES ($1, $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(biz.0)
    .bind(crate::db::sha256_hex(token.as_bytes()))
    .execute(&state.db)
    .await
    .map_err(internal)?;

    if let Some(b) = req.bundle.as_deref().filter(|b| !b.trim().is_empty()) {
        if let Err(e) = crate::learning::pin_bundle(&state.db, &state.schemas_dir, biz.0, b).await {
            tracing::warn!(error = %e, "tenant bundle not pinned");
        }
    }
    if let Some(w) = req.website.as_deref().filter(|w| !w.trim().is_empty()) {
        if let Err(e) = crate::ops::configure(&state.db, biz.0, &json!({"domain": w})).await {
            tracing::warn!(error = %e, "tenant website not set");
        }
    }
    let outcome = match agents::run_onboarding_agent(&state, biz.0, None, &json!([])).await {
        Ok(o) => o,
        Err(e) => {
            // The app never got the token: undo the registration, or the
            // retry's business would not be the phone's first (see owner.rs).
            let _ = sqlx::query("DELETE FROM businesses WHERE id = $1").bind(biz.0).execute(&state.db).await;
            return Err(agent_err(e));
        }
    };
    append_message(&state, biz.0, "onboarding", &req.owner_phone,
                   "assistant", &outcome.reply).await?;

    let profile = crate::locale::profile(&country);
    Ok(Json(json!({
        "businessId": biz.0,
        "deviceToken": token,
        "locale": {
            "country": profile.iso, "language": profile.language,
            "currency": profile.currency, "currencySymbol": profile.symbol,
            "timezone": profile.timezone
        },
        "conversationStarterMessage": outcome.reply,
        "audioBase64": tts_b64(&state, biz.0, &outcome.reply).await,
        "audioFormat": "wav"
    })))
}

#[derive(Deserialize)]
pub(super) struct OnboardMsgReq {
    message: String,
}

pub(super) async fn onboarding_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<OnboardMsgReq>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let peer: (String,) =
        sqlx::query_as("SELECT owner_phone FROM businesses WHERE id = $1")
            .bind(business_id)
            .fetch_one(&state.db)
            .await
            .map_err(internal)?;
    let history = load_history(&state, business_id, "onboarding", &peer.0).await?;
    let outcome = agents::run_onboarding_agent(&state, business_id, Some(&req.message), &history)
        .await
        .map_err(agent_err)?;
    append_message(&state, business_id, "onboarding", &peer.0, "user", &req.message).await?;
    append_message(&state, business_id, "onboarding", &peer.0, "assistant", &outcome.reply).await?;
    crate::audit::record(&state, Some(business_id), crate::audit::kind::OWNER_TURN, "owner", &outcome.session, json!({
        "in": crate::audit::digest_of(&req.message),
        "reply": outcome.reply,
        "action": outcome.action.as_ref().map(|(a, _)| a.clone()),
    })).await;
    crate::audit::anchor_if_due(state.clone());
    let (action, action_data) = outcome
        .action
        .map(|(a, d)| (json!(a), d))
        .unwrap_or((Value::Null, Value::Null));
    if action == "finish_onboarding" {
        // The business now has a name worth announcing: refresh the card.
        let bg = state.clone();
        tokio::spawn(async move { crate::network::publish(&bg, true).await; });
    }
    Ok(Json(json!({
        "agentResponse": outcome.reply,
        "action": action,
        "actionData": action_data,
        "audioBase64": tts_b64(&state, business_id, &outcome.reply).await,
        "audioFormat": "wav"
    })))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::{json, Value};

    fn req(extra: Value) -> Value {
        let mut b = json!({"businessName": "Tito", "industry": "barbería", "ownerPhone": "+51 999 000 111"});
        for (k, v) in extra.as_object().unwrap() { b[k] = v.clone(); }
        b
    }

    #[tokio::test]
    async fn registers_a_business_and_hands_out_a_device_token() {
        let m = Mock::start().await;
        m.say("¡Hola Tito!");
        let s = testkit::state_on(&m).await;
        let (st, v) = api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"country": " br ", "ruc": "20123456789", "category": " Barberia ", "termsVersion": "v2", "termsAcceptedAt": "2026-09-01T10:00:00-05:00"})))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["conversationStarterMessage"], "¡Hola Tito!");
        assert_eq!(v["deviceToken"].as_str().unwrap().len(), 40);
        assert_eq!(v["locale"]["currency"], "BRL");
        let (country, ruc, category, terms_at): (String, Option<String>, Option<String>, Option<String>) = sqlx::query_as("SELECT country, ruc, category, terms_accepted_at FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!((country.as_str(), ruc.as_deref(), category.as_deref()), ("BR", Some("20123456789"), Some("barberia")));
        assert_eq!(terms_at.as_deref(), Some("2026-09-01T15:00:00+00:00"));
        // Only the hash of the token is stored.
        let (hash,): (String,) = sqlx::query_as("SELECT token_hash FROM devices").fetch_one(&s.db).await.unwrap();
        assert_eq!(hash, crate::db::sha256_hex(v["deviceToken"].as_str().unwrap().as_bytes()));
        // The owner is the CRM's first row.
        assert_eq!(crate::contacts::list(&s.db, serde_json::from_value(v["businessId"].clone()).unwrap(), None).await.unwrap()[0]["kind"], "owner");
    }

    #[tokio::test]
    async fn bad_optional_fields_are_dropped_not_fatal() {
        let m = Mock::start().await;
        m.say("hola");
        let s = testkit::state_on(&m).await;
        let (st, _) = api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"country": "Perú", "ruc": "123", "termsAcceptedAt": "yesterday"})))).await;
        assert_eq!(st, 200);
        let (country, ruc, terms_at): (String, Option<String>, Option<String>) = sqlx::query_as("SELECT country, ruc, terms_accepted_at FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!((country.as_str(), ruc, terms_at), ("PE", None, None));
    }

    #[tokio::test]
    async fn needs_the_app_key_and_a_body() {
        let s = testkit::state().await;
        assert_eq!(testkit::call(&s, "POST", "/api/onboard_business", &[], Some(req(json!({})))).await.0, 401);
        assert_eq!(testkit::call(&s, "POST", "/api/onboard_business", &[("x-app-key", "wrong")], Some(req(json!({})))).await.0, 401);
        assert_eq!(api(&s, "POST", "/api/onboard_business", None, Some(json!({"businessName": "x"}))).await.0, 422);
    }

    #[tokio::test]
    async fn prohibited_categories_are_refused_before_any_row() {
        let s = testkit::state().await;
        let (st, v) = api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"category": "ARMAS"})))).await;
        assert_eq!((st, v["error"].as_str()), (403, Some("prohibited_category")));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn registrations_are_rate_limited_per_phone() {
        let m = Mock::start().await;
        m.say("hola");
        let s = testkit::state_on(&m).await;
        let mut codes = vec![];
        for _ in 0..4 {
            codes.push(api(&s, "POST", "/api/onboard_business", None, Some(req(json!({})))).await.0);
        }
        assert_eq!(codes, vec![200, 200, 200, 429], "3 per phone per day by default");
    }

    #[tokio::test]
    async fn a_failed_first_turn_leaves_no_orphan_business() {
        // The LLM is down: registration fails. Nothing may be left behind, or
        // the retry's business is no longer "the" business on this phone.
        let m = Mock::start().await;
        m.on_status("/chat/completions", 400, json!({"error": "down"}));
        let s = testkit::state_on(&m).await;
        let (st, _) = api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"businessName": "Orphan"})))).await;
        assert_eq!(st, 500);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 0, "a failed registration must not leave a business row");
        let d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices").fetch_one(&s.db).await.unwrap();
        assert_eq!(d, 0);
    }

    #[tokio::test]
    async fn verification_is_required_when_configured() {
        let m = Mock::start().await;
        m.say("hola");
        let base = testkit::state_on(&m).await;
        let s = std::sync::Arc::new(crate::AppState { require_phone_verification: true, ..std::sync::Arc::try_unwrap(base).ok().unwrap() });
        let (st, v) = api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"verificationToken": "nope"})))).await;
        assert_eq!(st, 403, "{v}");
        let (st, _) = api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"ownerPhone": "123"})))).await;
        assert_eq!(st, 400);
        // A proof minted for this phone is burned on use.
        sqlx::query("INSERT INTO phone_verifications (id, phone, code_hash, expires_at, proof_hash, proof_expires_at, verified_at) VALUES ($1, '51999000111', 'x', $2, $3, $2, $4)")
            .bind(uuid::Uuid::new_v4()).bind(crate::db::hence(chrono::Duration::minutes(10))).bind(crate::db::sha256_hex(b"proof-1")).bind(crate::db::now())
            .execute(&s.db).await.unwrap();
        assert_eq!(api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"verificationToken": "proof-1"})))).await.0, 200);
        assert_eq!(api(&s, "POST", "/api/onboard_business", None, Some(req(json!({"verificationToken": "proof-1"})))).await.0, 403, "single use");
    }

    #[tokio::test]
    async fn onboarding_turns_need_the_device_token_and_are_kept() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (token, bid) = testkit::onboard(&s, &m).await;
        assert_eq!(api(&s, "POST", "/api/onboarding_message", None, Some(json!({"message": "hola"}))).await.0, 401);
        assert_eq!(api(&s, "POST", "/api/onboarding_message", Some("bogus"), Some(json!({"message": "hola"}))).await.0, 401);
        m.say("Anotado: cortes a S/ 25.");
        let (st, v) = api(&s, "POST", "/api/onboarding_message", Some(&token), Some(json!({"message": "cortes a 25"}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["agentResponse"], "Anotado: cortes a S/ 25.");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE business_id = $1 AND agent_type = 'onboarding'").bind(bid).fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 3, "starter + user + reply");
        let audited: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE kind = 'owner_turn'").fetch_one(&s.db).await.unwrap();
        assert_eq!(audited, 1);
        // A revoked device is out.
        sqlx::query("UPDATE devices SET revoked_at = $1").bind(crate::db::now()).execute(&s.db).await.unwrap();
        assert_eq!(api(&s, "POST", "/api/onboarding_message", Some(&token), Some(json!({"message": "x"}))).await.0, 401);
    }

    #[tokio::test]
    async fn gateway_account_and_allowance_errors_reach_the_app() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (token, _) = testkit::onboard(&s, &m).await;
        m.set("/chat/completions", json!({}));
        m.on_status("/chat/completions", 401, json!({"error": {"type": "account"}}));
        m.set("/chat/completions", json!({"error": {"type": "account"}}));
        let m2 = Mock::start().await;
        m2.on_status("/chat/completions", 401, json!({"error": {"type": "account"}}));
        let s2 = crate::AppState { llm: std::sync::Arc::new(crate::llm::Llm::with_upstream(crate::upstream::Upstream::Direct { base: format!("{}/v1", m2.base), key: "k".into() }, "m")), ..std::sync::Arc::try_unwrap(s).ok().unwrap() };
        let s2 = std::sync::Arc::new(s2);
        let (st, v) = api(&s2, "POST", "/api/onboarding_message", Some(&token), Some(json!({"message": "x"}))).await;
        assert_eq!((st, v["error"].as_str()), (401, Some("account")));
    }

    /// A custom (tenant) build registers its business already on its
    /// vertical and website: the interview starts from the right bundle.
    #[tokio::test]
    async fn a_tenant_build_arrives_with_its_bundle_and_website() {
        let m = crate::testkit::Mock::start().await;
        let s = crate::testkit::state_on(&m).await;
        m.say("¡Hola! Cuéntame de tu red de productores.");
        let (st, v) = crate::testkit::api(&s, "POST", "/api/onboard_business", None, Some(serde_json::json!({
            "businessName": "Agro Andes", "industry": "mercado agrícola", "ownerPhone": "+51 999 000 222", "country": "pe",
            "bundle": "agro", "website": "https://Example.com/",
        }))).await;
        assert_eq!(st, 200, "{v}");
        let b: uuid::Uuid = serde_json::from_value(v["businessId"].clone()).unwrap();
        let (pin,): (Option<String>,) = sqlx::query_as("SELECT bundle FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_eq!(pin.as_deref(), Some("agro@1"));
        assert_eq!(crate::ops::config(&s.db, b).await.domain.as_deref(), Some("example.com"));
        let sys = m.seen_path("/chat/completions").last().unwrap().body["messages"][0]["content"].to_string();
        assert!(sys.contains("Mercado agrícola"), "the interview already knows the vertical");

        // Unknown bundles and bad domains never block a registration.
        m.say("¡Hola!");
        let (st, v) = crate::testkit::api(&s, "POST", "/api/onboard_business", None, Some(serde_json::json!({
            "businessName": "X", "industry": "x", "ownerPhone": "+51 999 000 333", "bundle": "../../etc", "website": "nope",
        }))).await;
        assert_eq!(st, 200, "{v}");
        let b: uuid::Uuid = serde_json::from_value(v["businessId"].clone()).unwrap();
        let (pin,): (Option<String>,) = sqlx::query_as("SELECT bundle FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_ne!(pin.as_deref(), Some("../../etc@1"));
        assert!(crate::ops::config(&s.db, b).await.domain.is_none());
    }
}
