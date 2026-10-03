//! Yaya ID on the phone: sign in / create account / sign out, and the
//! encrypted backups that ride on a paid plan. Loopback app key only: the
//! account comes before any business exists.

use super::*;

pub(super) async fn account_status(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(crate::account::status(&state).await.map_err(internal)?))
}

#[derive(Deserialize)]
pub(super) struct OtpStartReq {
    #[serde(default)] email: Option<String>,
    #[serde(default)] phone: Option<String>,
    #[serde(default)] name: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct OtpCheckReq {
    #[serde(default)] email: Option<String>,
    #[serde(default)] phone: Option<String>,
    code: String,
    #[serde(default)] name: Option<String>,
}

pub(super) fn account_err(e: anyhow::Error) -> (StatusCode, Json<Value>) {
    let m = e.to_string();
    // The gateway's status rides in the message ("registry 409: …"): keep
    // the user-facing part and the right class of status.
    let code = if m.contains("registry 401") { StatusCode::UNAUTHORIZED }
        else if m.contains("registry 409") { StatusCode::CONFLICT }
        else if m.contains("registry 410") { StatusCode::GONE }
        else if m.contains("registry 429") { StatusCode::TOO_MANY_REQUESTS }
        else if m.contains("registry 400") { StatusCode::BAD_REQUEST }
        else if m.contains("registry 503") { StatusCode::SERVICE_UNAVAILABLE }
        else { StatusCode::BAD_GATEWAY };
    err(code, m.splitn(2, ": ").nth(1).unwrap_or(&m))
}

pub(super) async fn account_otp_start(State(state): State<SharedState>, Json(req): Json<OtpStartReq>) -> ApiResult {
    Ok(Json(crate::account::otp_start(&state, req.email.as_deref(), req.phone.as_deref(), req.name.as_deref()).await.map_err(account_err)?))
}

pub(super) async fn account_otp_check(State(state): State<SharedState>, Json(req): Json<OtpCheckReq>) -> ApiResult {
    crate::account::otp_check(&state, req.email.as_deref(), req.phone.as_deref(), req.code.trim(), req.name.as_deref()).await.map_err(account_err)?;
    Ok(Json(crate::account::status(&state).await.map_err(internal)?))
}

#[derive(Deserialize)]
pub(super) struct ShareReq {
    #[serde(default = "share_default")]
    share: bool,
}
fn share_default() -> bool { true }

/// "Continuar sin cuenta": declares a guest (sharing on unless told otherwise).
pub(super) async fn account_guest(State(state): State<SharedState>, Json(req): Json<ShareReq>) -> ApiResult {
    crate::account::declare_guest(&state, req.share).await.map_err(account_err)?;
    Ok(Json(crate::account::status(&state).await.map_err(internal)?))
}

pub(super) async fn account_share(State(state): State<SharedState>, Json(req): Json<ShareReq>) -> ApiResult {
    crate::account::set_share(&state, req.share).await.map_err(internal)?;
    Ok(Json(crate::account::status(&state).await.map_err(internal)?))
}

#[derive(Deserialize)]
pub(super) struct PlanRequestReq { plan: String, #[serde(default)] months: Option<i64> }

/// Buys a plan from the phone: Yape/Plin against a reference, for the
/// whole account. Guests are sent to sign in first.
pub(super) async fn account_plan_request(State(state): State<SharedState>, Json(req): Json<PlanRequestReq>) -> ApiResult {
    let months = req.months.unwrap_or(1).clamp(1, 12);
    Ok(Json(crate::account::plan_request(&state, &req.plan, months).await.map_err(account_err)?))
}

pub(super) async fn account_plan_request_status(State(state): State<SharedState>, axum::extract::Path(reference): axum::extract::Path<String>) -> ApiResult {
    Ok(Json(crate::account::plan_request_status(&state, &reference).await.map_err(account_err)?))
}

#[derive(Deserialize)]
pub(super) struct AdoptReq { token: String }

/// `POST /api/account/adopt` — a node joins an account with a one-time link
/// token the owner minted in the console (`POST /v1/account/link-tokens`).
pub(super) async fn account_adopt(State(state): State<SharedState>, Json(req): Json<AdoptReq>) -> ApiResult {
    let token = req.token.trim();
    if token.len() < 8 {
        return Err(err(StatusCode::BAD_REQUEST, "a link token is required"));
    }
    crate::account::adopt(&state, token).await.map_err(account_err)?;
    Ok(Json(crate::account::status(&state).await.map_err(internal)?))
}

pub(super) async fn account_logout(State(state): State<SharedState>) -> ApiResult {
    crate::account::logout(&state).await.map_err(internal)?;
    Ok(Json(json!({"signedIn": false})))
}

pub(super) async fn backup_now(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(crate::backup::run(&state).await.map_err(account_err)?))
}

#[derive(Deserialize, Default)]
pub(super) struct RestoreReq {
    #[serde(default)]
    force: bool,
}

/// Restores the latest snapshot. Refuses to overwrite an existing business
/// unless `force` — a fresh phone is the intended case.
pub(super) async fn restore_latest(State(state): State<SharedState>, body: Option<Json<RestoreReq>>) -> ApiResult {
    let force = body.map(|b| b.0.force).unwrap_or(false);
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM businesses").fetch_one(&state.db).await.map_err(internal)?;
    if n > 0 && !force {
        return Err(err(StatusCode::CONFLICT, "this phone already has a business — pass force to replace it"));
    }
    let mut out = crate::backup::restore(&state).await.map_err(|e| {
        if e.to_string().contains("registry 404") { err(StatusCode::NOT_FOUND, "no backup on this account") } else { account_err(e) }
    })?;
    // The old phone's device token stayed on the old phone: mint one here so
    // the shell can pair with the restored business right away.
    let biz: Option<(Uuid, String)> = sqlx::query_as("SELECT id, country FROM businesses ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(&state.db).await.map_err(internal)?;
    if let Some((bid, country)) = biz {
        let token: String = rand::thread_rng().sample_iter(&Alphanumeric).take(40).map(char::from).collect();
        sqlx::query("INSERT INTO devices (id, business_id, token_hash) VALUES ($1, $2, $3)")
            .bind(Uuid::new_v4()).bind(bid).bind(crate::db::sha256_hex(token.as_bytes()))
            .execute(&state.db).await.map_err(internal)?;
        let profile = crate::locale::profile(&country);
        out["businessId"] = json!(bid);
        out["deviceToken"] = json!(token);
        out["locale"] = json!({"country": profile.iso, "language": profile.language, "currency": profile.currency,
                               "currencySymbol": profile.symbol, "timezone": profile.timezone});
        crate::backup::backup_if_due(state.clone());
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, api, Mock};

    #[test]
    fn gateway_errors_keep_their_class_and_message() {
        let e = |m: &str| account_err(anyhow::anyhow!(m.to_string()));
        assert_eq!(e("registry 401 Unauthorized: sign in").0, StatusCode::UNAUTHORIZED);
        assert_eq!(e("registry 409 Conflict: taken").1 .0["error"], "taken");
        assert_eq!(e("registry 410 Gone: expired").0, StatusCode::GONE);
        assert_eq!(e("registry 429 Too Many Requests: slow").0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e("registry 400 Bad Request: x").0, StatusCode::BAD_REQUEST);
        assert_eq!(e("registry 503 Service Unavailable: x").0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e("connection refused").0, StatusCode::BAD_GATEWAY);
        assert_eq!(e("connection refused").1 .0["error"], "connection refused");
    }

    #[tokio::test]
    async fn sign_in_status_and_sign_out() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        assert_eq!(api(&s, "GET", "/api/account", None, None).await.1["signedIn"], false);
        m.on("/v1/accounts/otp/start", json!({"sent": true}));
        assert_eq!(api(&s, "POST", "/api/account/otp/start", None, Some(json!({"email": "t@x.pe"}))).await.1["sent"], true);
        testkit::script_sign_in(&m);
        let (st, v) = api(&s, "POST", "/api/account/otp/check", None, Some(json!({"email": "t@x.pe", "code": " 123456 "}))).await;
        assert_eq!((st, v["signedIn"].clone(), v["email"].clone()), (200, json!(true), json!("tito@x.pe")));
        assert_eq!(m.seen_path("/v1/accounts/otp/check")[0].body["code"], "123456");
        m.on("/v1/accounts/logout", json!({}));
        assert_eq!(api(&s, "POST", "/api/account/logout", None, None).await.1, json!({"signedIn": false}));
    }

    #[tokio::test]
    async fn wrong_codes_and_bad_adoptions() {
        let m = Mock::start().await;
        m.on_status("/v1/accounts/otp/check", 401, json!({"error": {"message": "wrong code"}}));
        let s = testkit::state_on(&m).await;
        let (st, v) = api(&s, "POST", "/api/account/otp/check", None, Some(json!({"phone": "+51999", "code": "1"}))).await;
        assert_eq!((st, v["error"].clone()), (401, json!("wrong code")));
        assert_eq!(api(&s, "POST", "/api/account/adopt", None, Some(json!({"token": " short "}))).await.0, 400);
    }

    #[tokio::test]
    async fn guest_share_plan_requests_and_backup() {
        let m = Mock::start().await;
        m.on("/v1/agents/guest", json!({})).on("/v1/agents/share", json!({}));
        let s = testkit::state_on(&m).await;
        assert_eq!(api(&s, "POST", "/api/account/guest", None, Some(json!({}))).await.1["shareTraining"], true, "sharing defaults on for guests");
        assert_eq!(api(&s, "POST", "/api/account/share", None, Some(json!({"share": false}))).await.1["shareTraining"], false);
        // Not signed in: plan purchase and backups say so.
        assert_eq!(api(&s, "POST", "/api/account/plan/request", None, Some(json!({"plan": "pro"}))).await.0, 502);
        assert_eq!(api(&s, "POST", "/api/backup", None, None).await.0, 502);
        testkit::sign_in(&s, &m).await;
        m.on("/v1/account/plan/request", json!({"ref": "AG-9"}));
        api(&s, "POST", "/api/account/plan/request", None, Some(json!({"plan": "pro", "months": 99}))).await;
        assert_eq!(m.seen_path("/v1/account/plan/request")[0].body["months"], 12, "months are clamped to a year");
        m.on("/v1/account/plan/request/AG-9", json!({"status": "pending"}));
        assert_eq!(api(&s, "GET", "/api/account/plan/request/AG-9", None, None).await.1["status"], "pending");
    }

    #[tokio::test]
    async fn restore_refuses_to_overwrite_and_mints_a_device_token() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::sign_in(&s, &m).await;
        testkit::business(&s.db).await;
        assert_eq!(api(&s, "POST", "/api/restore", None, None).await.0, 409);
        m.on_status("/v1/backup/latest", 404, json!({"error": {"message": "none"}}));
        assert_eq!(api(&s, "POST", "/api/restore", None, Some(json!({"force": true}))).await.0, 404);
        // A real snapshot restores and hands the shell a fresh token.
        let snap = crate::backup::export(&s.db).await.unwrap();
        let blob = crate::backup::encrypt(&[1u8; 32], snap.to_string().as_bytes()).unwrap();
        let m2 = Mock::start().await;
        m2.on_bytes("/v1/backup/latest", blob, &[("x-backup-meta", "{}")]);
        let fresh = testkit::state_on(&m2).await;
        testkit::sign_in(&fresh, &m2).await;
        let (st, v) = api(&fresh, "POST", "/api/restore", None, None).await;
        assert_eq!(st, 200, "{v}");
        let token = v["deviceToken"].as_str().unwrap().to_string();
        assert_eq!(v["locale"]["country"], "PE");
        assert_eq!(api(&fresh, "GET", "/api/ui", Some(&token), None).await.0, 200, "the new token works");
    }
}
