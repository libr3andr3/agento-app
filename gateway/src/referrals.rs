//! Comunidad settlement: a referral that ended in a registered booking is a
//! lead the receiving business got from a peer. The receiver reports it
//! (signed — nobody can claim a referral on someone else's behalf), pays the
//! network lead price from its credits, and the referring business earns
//! the same amount. Never into the red, never between devices of one
//! account, at most one paid referral per pair per 30 days (the rest are
//! recorded, unpaid). The relay already noted the interaction, so the two
//! businesses may review each other afterwards.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::{json, Value};
use yaya_wire::AgentId;

use crate::{accounts, auth::require_linked, credits, err, internal, verify_envelope, ApiResult, Auth, Shared};

/// `POST /v1/referrals` — envelope signed by the RECEIVING business, payload
/// `{id, from, status}` where `from` is the referring agent and `status` is
/// `registered` (a booking/order was created) or `declined`.
pub async fn post(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(env): Json<Value>) -> ApiResult {
    let (bearer, _) = require_linked(&app, &auth).await?;
    let to = verify_envelope(&bearer, &env)?;
    let p = &env["payload"];
    let id: String = p["id"].as_str().unwrap_or("").chars().take(64).collect();
    let from = p["from"].as_str().unwrap_or("").to_string();
    let status = match p["status"].as_str() {
        Some("registered") => "registered",
        Some("declined") => "declined",
        _ => return Err(err(StatusCode::BAD_REQUEST, "status must be registered|declined")),
    };
    if id.is_empty() || !id.starts_with("ref_") {
        return Err(err(StatusCode::BAD_REQUEST, "bad referral id"));
    }
    if !AgentId::looks_valid(&from) || from == to {
        return Err(err(StatusCode::BAD_REQUEST, "bad 'from' agent"));
    }

    // Idempotent per id: the receiver may retry after a flaky network. The
    // row is claimed first and the money moves in the same transaction, so
    // racing retries cannot both pay.
    let (payer, payee) = if status == "registered" {
        (accounts::account_of_agent(&app, &to).await?, accounts::account_of_agent(&app, &from).await?)
    } else {
        (None, None)
    };
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let claimed = sqlx::query("INSERT OR IGNORE INTO referrals (id, from_agent, to_agent, status, charged) VALUES ($1,$2,$3,$4,0)")
        .bind(&id).bind(&from).bind(&to).bind(status)
        .execute(&mut *tx).await.map_err(internal)?.rows_affected();
    if claimed == 0 {
        drop(tx);
        let (charged,): (i64,) = sqlx::query_as("SELECT charged FROM referrals WHERE id = $1").bind(&id).fetch_one(&app.db).await.map_err(internal)?;
        return Ok(Json(json!({"ok": true, "id": id, "charged": charged, "duplicate": true})).into_response());
    }
    let charged = match (payer, payee) {
        (Some(payer), Some(payee)) => settle_in(&mut tx, &id, &from, &to, &payer, &payee, credits::lead_price(&app)).await?,
        _ => 0,
    };
    if charged > 0 {
        sqlx::query("UPDATE referrals SET charged = $2 WHERE id = $1").bind(&id).bind(charged).execute(&mut *tx).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    tracing::info!(referral = %id, %from, %to, status, charged, "referral recorded");
    Ok(Json(json!({"ok": true, "id": id, "status": status, "charged": charged})).into_response())
}

/// Moves one lead price from the receiver's account to the sender's, when
/// both are distinct, the receiver can afford it, and this pair has not
/// settled a referral in the last 30 days. Returns the amount moved.
async fn settle_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: &str, from: &str, to: &str, payer: &str, payee: &str, price: i64,
) -> Result<i64, (StatusCode, Json<Value>)> {
    if payer == payee || price <= 0 {
        return Ok(0);
    }
    let recent: Option<(String,)> = sqlx::query_as(
        "SELECT at FROM referrals WHERE from_agent = $1 AND to_agent = $2 AND charged > 0 AND id <> $3 \
         AND at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days')",
    ).bind(from).bind(to).bind(id).fetch_optional(&mut **tx).await.map_err(internal)?;
    if recent.is_some() {
        return Ok(0);
    }
    let moved = crate::wallet::move_credits_in(tx, payer, payee, price, 0, "referral", "referral", Some(id),
        "customer referred by a peer business", "you referred a customer to a peer business").await?;
    Ok(moved.map_or(0, |m| m.charged))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, as_agent, Keypair};

    fn report(kp: &Keypair, id: &str, from: &Keypair, status: &str) -> Value {
        kp.envelope(json!({"id": id, "from": from.id().to_string(), "status": status}))
    }

    async fn two_businesses(app: &Shared, funds: i64) -> (Keypair, Keypair) {
        let (from, to) = (Keypair::generate(), Keypair::generate());
        account_with_agent(app, "sender", "51900000001", &from).await;
        account_with_agent(app, "receiver", "51900000002", &to).await;
        credits::add(app, "receiver", funds, "topup", None, None).await.unwrap();
        (from, to)
    }

    #[tokio::test]
    async fn a_retried_report_pays_once_even_when_the_retries_race() {
        let app = testkit::app().await;
        let price = credits::lead_price(&app);
        let (from, to) = two_businesses(&app, price * 5).await;
        let body = report(&to, "ref_1", &from, "registered");
        let (a, b) = tokio::join!(
            as_agent(&app, &to, "POST", "/v1/referrals", Some(body.clone())),
            as_agent(&app, &to, "POST", "/v1/referrals", Some(body.clone())));
        assert_eq!((a.0, b.0), (200, 200), "{a:?} {b:?}");
        assert_eq!(credits::balance(&app, "sender").await.unwrap(), price, "paid once");
        assert_eq!(credits::balance(&app, "receiver").await.unwrap(), price * 4);
        let (_, again) = as_agent(&app, &to, "POST", "/v1/referrals", Some(body)).await;
        assert_eq!((again["duplicate"].clone(), again["charged"].clone()), (json!(true), json!(price)));
    }

    #[tokio::test]
    async fn one_paid_referral_per_pair_per_month_and_never_into_the_red() {
        let app = testkit::app().await;
        let price = credits::lead_price(&app);
        let (from, to) = two_businesses(&app, price + price / 2).await;
        let post = |id: &str, status: &str| as_agent(&app, &to, "POST", "/v1/referrals", Some(report(&to, id, &from, status)));
        assert_eq!(post("ref_a", "registered").await.1["charged"], price);
        assert_eq!(post("ref_b", "registered").await.1["charged"], 0, "same pair within 30 days is recorded unpaid");
        assert_eq!(post("ref_c", "declined").await.1["charged"], 0);
        sqlx::query("UPDATE referrals SET at = '2000-01-01T00:00:00Z'").execute(&app.db).await.unwrap();
        assert_eq!(post("ref_d", "registered").await.1["charged"], 0, "half a lead price left: not charged");
        assert_eq!(credits::balance(&app, "receiver").await.unwrap(), price / 2);
        for (id, st) in [("ref_e", "maybe"), ("bad", "registered"), ("", "registered")] {
            assert_eq!(post(id, st).await.0, 400, "{id} {st}");
        }
        let self_ref = to.envelope(json!({"id": "ref_s", "from": to.id().to_string(), "status": "registered"}));
        assert_eq!(as_agent(&app, &to, "POST", "/v1/referrals", Some(self_ref)).await.0, 400);
        // Signed by someone other than the bearer: refused.
        let forged = from.envelope(json!({"id": "ref_f", "from": from.id().to_string(), "status": "registered"}));
        assert_ne!(as_agent(&app, &to, "POST", "/v1/referrals", Some(forged)).await.0, 200);
    }

    #[tokio::test]
    async fn devices_of_one_account_do_not_pay_each_other() {
        let app = testkit::app().await;
        let (a, b) = (Keypair::generate(), Keypair::generate());
        account_with_agent(&app, "one", "51900000001", &a).await;
        sqlx::query("INSERT INTO account_agents (agent, account) VALUES ($1, 'one')").bind(b.id().to_string()).execute(&app.db).await.unwrap();
        credits::add(&app, "one", 10_000, "topup", None, None).await.unwrap();
        let (_, v) = as_agent(&app, &b, "POST", "/v1/referrals", Some(report(&b, "ref_1", &a, "registered"))).await;
        assert_eq!(v["charged"], 0);
        let unlinked = Keypair::generate();
        assert_eq!(as_agent(&app, &unlinked, "POST", "/v1/referrals", Some(report(&unlinked, "ref_2", &a, "registered"))).await.0, 401);
    }
}

