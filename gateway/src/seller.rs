//! The seller (D14): agente sells itself. The founder's own business phone
//! runs the same core as every customer's; when its Yaya account is listed in
//! `SELLER_ACCOUNTS` the gateway lets that agent ask "who is this number?"
//! and "activate Pro for them" — so a lead who Yapes the sales number gets
//! their plan turned on by the agent that took the payment, with no human in
//! the loop. Only accounts in the allowlist, only from a signed request.

use axum::{extract::{Query, State}, http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::{json, Value};

use crate::{accounts, err, internal, plans, ApiResult, Auth, Shared};

/// The Yaya accounts whose agents may sell (`SELLER_ACCOUNTS`, comma-separated).
/// Prefer verified identities: a phone with country code (proven by the
/// WhatsApp OTP) or `acct:<id>`. Plain emails still match for existing
/// deployments, but sign-up emails are never verified — an email entry that
/// no account holds yet can be claimed by whoever signs up with it first.
pub fn sellers() -> Vec<String> {
    std::env::var("SELLER_ACCOUNTS").unwrap_or_default()
        .split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
}

pub fn is_seller(email: &str) -> bool {
    let e = email.trim().to_lowercase();
    !e.is_empty() && e.contains('@') && sellers().contains(&e)
}

/// Whether this account sells: by id, by its verified phone, or (legacy) by email.
pub fn is_seller_account(id: &str, email: &str, phone: Option<&str>) -> bool {
    let list = sellers();
    let by_phone = phone.and_then(crate::otp::normalize_phone)
        .is_some_and(|p| list.iter().any(|e| !e.contains('@') && crate::otp::normalize_phone(e).as_deref() == Some(p.as_str())));
    list.iter().any(|e| e.strip_prefix("acct:") == Some(id)) || by_phone || is_seller(email)
}

/// The signed agent behind a seller account, or 401/403.
async fn seller_agent(app: &crate::App, auth: &Auth) -> Result<String, (StatusCode, Json<Value>)> {
    let Auth::Proven(agent) = auth else {
        return Err(err(StatusCode::UNAUTHORIZED, "a signed agent request is required"));
    };
    let agent = agent.to_string();
    let Some(acct) = accounts::account_of_agent(app, &agent).await? else {
        return Err(err(StatusCode::FORBIDDEN, "this agent is not linked to a seller account"));
    };
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT email, phone FROM accounts WHERE id = $1").bind(&acct).fetch_optional(&app.db).await.map_err(internal)?;
    if !row.is_some_and(|(email, phone)| is_seller_account(&acct, &email, phone.as_deref())) {
        return Err(err(StatusCode::FORBIDDEN, "this account does not sell agente"));
    }
    Ok(agent)
}

struct Found {
    id: String,
    email: String,
    name: Option<String>,
    phone: Option<String>,
    created_at: String,
}

async fn find_account(app: &crate::App, phone: Option<&str>, email: Option<&str>) -> Result<Option<Found>, (StatusCode, Json<Value>)> {
    let phone = phone.and_then(crate::otp::normalize_phone);
    let email = email.map(|e| e.trim().to_lowercase()).filter(|e| e.contains('@'));
    if phone.is_none() && email.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "a phone with country code or an email is required"));
    }
    // The phone is the proven identity, so it wins; the (unverified) email
    // is only a fallback when no phone was given.
    let row: Option<(String, String, Option<String>, Option<String>, String)> = match &phone {
        Some(p) => sqlx::query_as("SELECT id, email, name, phone, created_at FROM accounts WHERE phone = $1 ORDER BY created_at DESC LIMIT 1")
            .bind(p).fetch_optional(&app.db).await.map_err(internal)?,
        None => sqlx::query_as("SELECT id, email, name, phone, created_at FROM accounts WHERE email = $1 ORDER BY created_at DESC LIMIT 1")
            .bind(&email).fetch_optional(&app.db).await.map_err(internal)?,
    };
    Ok(row.map(|(id, email, name, phone, created_at)| Found { id, email, name, phone, created_at }))
}

#[derive(serde::Deserialize)]
pub struct LookupQ {
    #[serde(default)] pub phone: Option<String>,
    #[serde(default)] pub email: Option<String>,
}

/// `GET /v1/seller/lookup?phone=+51…` — is this number a customer (and on
/// which plan) or a lead?
pub async fn lookup(State(app): State<Shared>, Extension(auth): Extension<Auth>, Query(q): Query<LookupQ>) -> ApiResult {
    seller_agent(&app, &auth).await?;
    let Some(f) = find_account(&app, q.phone.as_deref(), q.email.as_deref()).await? else {
        return Ok(Json(json!({"found": false, "lead": true, "plan": "none", "state": "lead",
            "note": "no agente account with that phone — a lead. To activate a plan they first sign in to the app (Cuenta) with this number."})).into_response());
    };
    let plan = plans::effective_for(&app, &accounts::subject(&f.id)).await?;
    let agents: (i64,) = sqlx::query_as("SELECT count(*) FROM account_agents WHERE account = $1").bind(&f.id).fetch_one(&app.db).await.map_err(internal)?;
    let t = plans::tier(&plan.plan);
    Ok(Json(json!({
        "found": true, "lead": plan.plan == "free" && plan.source == "none",
        "plan": plan.plan, "state": plans::state_of(&plan.plan), "expiresAt": plan.expires_at, "source": plan.source,
        "caps": {"conversationsPerMonth": t.conversations, "agents": t.agents},
        "account": {"name": f.name, "email": f.email, "phone": f.phone, "since": f.created_at, "agents": agents.0},
    })).into_response())
}

#[derive(serde::Deserialize)]
pub struct SetReq {
    #[serde(default)] pub phone: Option<String>,
    #[serde(default)] pub email: Option<String>,
    pub plan: String,
    #[serde(default)] pub months: Option<i64>,
    #[serde(default)] pub note: Option<String>,
}

/// `POST /v1/seller/plan {phone|email, plan: pro|max, months}` — the seller's
/// agent activates a plan it just sold.
pub async fn set_plan(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<SetReq>) -> ApiResult {
    let agent = seller_agent(&app, &auth).await?;
    let plan = req.plan.trim().to_lowercase();
    if !matches!(plan.as_str(), "pro" | "max") {
        return Err(err(StatusCode::BAD_REQUEST, "plan must be pro or max"));
    }
    let Some(f) = find_account(&app, req.phone.as_deref(), req.email.as_deref()).await? else {
        return Err(err(StatusCode::NOT_FOUND, "no agente account with that phone — ask the customer to sign in to the app (Cuenta) with this number, then activate again"));
    };
    let months = req.months.unwrap_or(1).clamp(1, 12);
    let note = format!("sold by {agent}{}", req.note.as_deref().map(|n| format!(" — {n}")).unwrap_or_default());
    let expires = plans::set(&app, &accounts::subject(&f.id), &plan, months, "seller", Some(&note)).await?;
    tracing::info!(seller = %agent, account = %f.id, %plan, months, "plan sold by the seller agent");
    Ok(Json(json!({"ok": true, "plan": plan, "months": months, "expiresAt": expires,
        "account": {"name": f.name, "email": f.email, "phone": f.phone}})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_agent, Keypair};

    /// SELLER_ACCOUNTS is process-wide, so every case lives in one test.
    #[tokio::test]
    async fn only_listed_verified_sellers_can_look_up_and_activate() {
        let app = testkit::app().await;
        let (seller, squatter, other) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
        account_with_agent(&app, "seller", "51900000001", &seller).await;
        account_with_agent(&app, "buyer", "51900000002", &other).await;
        // Someone else holds an email that is on the list but unverified.
        account_with_agent(&app, "squat", "51900000009", &squatter).await;
        sqlx::query("UPDATE accounts SET email = 'ventas@yaya.tech' WHERE id = 'squat'").execute(&app.db).await.unwrap();
        std::env::set_var("SELLER_ACCOUNTS", "+51 900 000 001, acct:nobody");
        assert!(is_seller_account("seller", "seller@test.pe", Some("51900000001")));
        assert!(is_seller_account("nobody", "x@y.pe", None));
        assert!(!is_seller_account("squat", "ventas@yaya.tech", Some("51900000009")));
        assert!(!is_seller(""));

        let buy = |b: Value| b;
        assert_eq!(anon(&app, "GET", "/v1/seller/lookup?phone=51900000002", None).await.0, 401);
        assert_eq!(as_agent(&app, &Keypair::generate(), "GET", "/v1/seller/lookup?phone=51900000002", None).await.0, 403, "unlinked");
        assert_eq!(as_agent(&app, &squatter, "POST", "/v1/seller/plan", Some(buy(json!({"phone": "51900000002", "plan": "max"})))).await.0, 403);
        let (st, v) = as_agent(&app, &seller, "GET", "/v1/seller/lookup?phone=51900000002", None).await;
        assert_eq!((st, v["found"].clone()), (200, json!(true)), "{v}");
        let (_, lead) = as_agent(&app, &seller, "GET", "/v1/seller/lookup?phone=51911111111", None).await;
        assert_eq!(lead["lead"], true);
        assert_eq!(as_agent(&app, &seller, "GET", "/v1/seller/lookup", None).await.0, 400);
        assert_eq!(as_agent(&app, &seller, "POST", "/v1/seller/plan", Some(json!({"phone": "51900000002", "plan": "free"}))).await.0, 400);
        assert_eq!(as_agent(&app, &seller, "POST", "/v1/seller/plan", Some(json!({"phone": "51911111111", "plan": "pro"}))).await.0, 404);
        // A phone and an email that point at different accounts: the phone wins.
        let (st, v) = as_agent(&app, &seller, "POST", "/v1/seller/plan", Some(json!({"phone": "51900000002", "email": "ventas@yaya.tech", "plan": "PRO", "months": 99}))).await;
        assert_eq!((st, v["months"].clone(), v["account"]["phone"].clone()), (200, json!(12), json!("51900000002")), "{v}");
        let (plan,): (String,) = sqlx::query_as("SELECT plan FROM plans WHERE agent = 'acct:buyer'").fetch_one(&app.db).await.unwrap();
        assert_eq!(plan, "pro");
        assert!(sqlx::query_as::<_, (String,)>("SELECT plan FROM plans WHERE agent = 'acct:squat'").fetch_optional(&app.db).await.unwrap().is_none());

        // Legacy email entries still work for the account that holds them.
        std::env::set_var("SELLER_ACCOUNTS", "seller@test.pe");
        assert_eq!(as_agent(&app, &seller, "GET", "/v1/seller/lookup?email=buyer@test.pe", None).await.0, 200);
        std::env::remove_var("SELLER_ACCOUNTS");
        assert_eq!(as_agent(&app, &seller, "GET", "/v1/seller/lookup?phone=51900000002", None).await.0, 403);
    }
}
