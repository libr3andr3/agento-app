//! Founder ops dashboard (`agente.ceo/admin`): businesses, revenue, usage.
//!
//! Ported from the pre-split `routes.rs` when the two product lines
//! (legacy consumer app on `main`, business-agent on `ship/business-agent`)
//! were consolidated (2026-09-08) — rewritten for SQLite (this server's `Db`
//! is `SqlitePool`; the original was written against `main`'s Postgres
//! backend and used `to_char`/`AT TIME ZONE`/`FILTER`/`interval`, none of
//! which exist here). Two sections from the original `admin_metrics` were
//! dropped rather than carried over, because their backing data doesn't
//! exist on this line yet:
//!   - `leads`: the email-gated APK download flow (`leads` table,
//!     `capture_lead`) is a `main`-only landing-page feature; this line's
//!     landing sends visitors straight to WhatsApp. Stubbed to an empty
//!     section so the existing frontend (which still renders a Leads panel)
//!     doesn't break; add a `leads` migration + `capture_lead` if that flow
//!     is wanted here too.
//!   - `web` (site-traffic stats from Caddy's JSON access log, `crate::admin`
//!     module on `main`): not ported. Add `server/src/admin.rs` back
//!     (`git show main:server/src/admin.rs`) if that's wanted.
//! Both gaps predate this merge — `admin_metrics`/`capture_lead`/curator
//! endpoints were dropped wholesale in ff3e04a's routes.rs split and never
//! restored; this only restores the businesses/revenue/usage core, which is
//! what `landing/admin/index.html` actually reads today.

use super::*;

/// Founder ops dashboard: businesses (with install-attribution), revenue,
/// conversation volume. Admin-key gated; day buckets follow Lima because
/// that is where the businesses (and the founder) live.
pub(super) async fn admin_metrics(State(state): State<SharedState>, headers: HeaderMap) -> ApiResult {
    require_admin(&state, &headers)?;
    let db = &state.db;
    let tz = chrono_tz::America::Lima;
    // Lima has no DST; a fixed offset computed once covers the whole window.
    let offset = {
        use chrono::{Offset, TimeZone};
        let now = chrono::Utc::now();
        format!("{} seconds", tz.offset_from_utc_datetime(&now.naive_utc()).fix().local_minus_utc())
    };
    let since = crate::db::ago(chrono::Duration::days(30));
    let since_date = (chrono::Utc::now() - chrono::Duration::days(30)).with_timezone(&tz).date_naive();

    // -- businesses --------------------------------------------------------
    let (biz_total, biz_onboarded, biz_active_trials): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), \
                SUM(CASE WHEN onboarded THEN 1 ELSE 0 END), \
                SUM(CASE WHEN plan <> 'free' THEN 1 ELSE 0 END) \
         FROM businesses",
    )
    .fetch_one(db)
    .await
    .map_err(internal)?;
    let biz_rows: Vec<(String, String, bool, String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT name, industry, onboarded, \
                strftime('%Y-%m-%d', created_at, $1), \
                plan, \
                CASE WHEN plan <> 'free' THEN 1 ELSE 0 END, \
                referral_source \
         FROM businesses ORDER BY created_at DESC LIMIT 500",
    )
    .bind(&offset)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let biz_daily: Vec<(String, i64)> = sqlx::query_as(
        "SELECT strftime('%Y-%m-%d', created_at, $1), count(*) \
         FROM businesses WHERE created_at > $2 GROUP BY 1 ORDER BY 1",
    )
    .bind(&offset)
    .bind(since)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    // Installs arrive from Play campaigns (utm_source on the store link) and
    // direct/sideloaded builds (no referrer at all) — grouped like leads so
    // the dashboard can tell a founder which channel is actually converting.
    let biz_by_source: Vec<(String, i64)> = sqlx::query_as(
        "SELECT coalesce(referral_source, 'direct'), count(*) \
         FROM businesses GROUP BY 1 ORDER BY 2 DESC",
    )
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let biz_daily_source: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT strftime('%Y-%m-%d', created_at, $1), coalesce(referral_source, 'direct'), count(*) \
         FROM businesses WHERE created_at > $2 GROUP BY 1, 2 ORDER BY 1",
    )
    .bind(&offset)
    .bind(since)
    .fetch_all(db)
    .await
    .map_err(internal)?;

    // -- revenue (money actually seen: Yape events the app forwarded) ----
    let (rev_total, pay_count): (Option<f64>, i64) =
        sqlx::query_as("SELECT sum(amount), count(*) FROM payments")
            .fetch_one(db)
            .await
            .map_err(internal)?;
    let (rev_30d,): (Option<f64>,) = sqlx::query_as("SELECT sum(amount) FROM payments WHERE received_at > $1")
        .bind(since)
        .fetch_one(db)
        .await
        .map_err(internal)?;
    let rev_daily: Vec<(String, f64, i64)> = sqlx::query_as(
        "SELECT strftime('%Y-%m-%d', received_at, $1), coalesce(sum(amount), 0), count(*) \
         FROM payments WHERE received_at > $2 GROUP BY 1 ORDER BY 1",
    )
    .bind(&offset)
    .bind(since)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let rev_by_biz: Vec<(String, f64, i64)> = sqlx::query_as(
        "SELECT b.name, coalesce(sum(p.amount), 0), count(p.id) \
         FROM payments p JOIN businesses b ON b.id = p.business_id \
         GROUP BY b.name ORDER BY 2 DESC LIMIT 20",
    )
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let (paid_appts, paid_orders): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM appointments WHERE paid), \
                (SELECT count(*) FROM orders WHERE paid)",
    )
    .fetch_one(db)
    .await
    .map_err(internal)?;

    // -- usage ---------------------------------------------------------------
    let (msg_total,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM messages").fetch_one(db).await.map_err(internal)?;
    let msg_daily: Vec<(String, i64)> = sqlx::query_as(
        "SELECT strftime('%Y-%m-%d', created_at, $1), count(*) \
         FROM messages WHERE created_at > $2 GROUP BY 1 ORDER BY 1",
    )
    .bind(&offset)
    .bind(since)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    // `usage_counters.day` is already a Lima-local calendar date (limits::charge
    // writes it with chrono_tz::America::Lima), so no offset conversion here.
    let meters: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT day, kind, sum(n) FROM usage_counters WHERE day > $1 GROUP BY 1, 2 ORDER BY 1",
    )
    .bind(since_date)
    .fetch_all(db)
    .await
    .map_err(internal)?;

    Ok(Json(json!({
        "generatedAt": chrono::Utc::now().to_rfc3339(),
        "timezone": "America/Lima",
        // Stub: no `leads` table on this product line yet (see module doc).
        "leads": {
            "total": 0, "bySource": [], "dailyBySource": [], "daily": [], "rows": [],
        },
        "businesses": {
            "total": biz_total,
            "onboarded": biz_onboarded,
            "paid": biz_active_trials,
            "daily": biz_daily.iter().map(|(d, n)| json!({"day": d, "count": n})).collect::<Vec<_>>(),
            "bySource": biz_by_source.iter().map(|(s, n)| json!({"source": s, "total": n})).collect::<Vec<_>>(),
            "dailyBySource": biz_daily_source.iter().map(|(d, s, n)| json!({"day": d, "source": s, "count": n})).collect::<Vec<_>>(),
            "rows": biz_rows.iter().map(|(name, ind, ob, created, trial_ends, active, source)| json!({
                "name": name, "industry": ind, "onboarded": ob,
                "createdAt": created, "plan": trial_ends, "paid": active,
                "referralSource": source.clone().unwrap_or_else(|| "direct".to_string()),
            })).collect::<Vec<_>>(),
        },
        "revenue": {
            "currency": "PEN",
            "total": rev_total.unwrap_or(0.0),
            "last30d": rev_30d.unwrap_or(0.0),
            "payments": pay_count,
            "paidAppointments": paid_appts,
            "paidOrders": paid_orders,
            "daily": rev_daily.iter().map(|(d, amt, n)| json!({"day": d, "amount": amt, "count": n})).collect::<Vec<_>>(),
            "byBusiness": rev_by_biz.iter().map(|(name, amt, n)| json!({"name": name, "amount": amt, "payments": n})).collect::<Vec<_>>(),
        },
        "usage": {
            "messagesTotal": msg_total,
            "messagesDaily": msg_daily.iter().map(|(d, n)| json!({"day": d, "count": n})).collect::<Vec<_>>(),
            "meters": meters.iter().map(|(d, k, n)| json!({"day": d, "kind": k, "n": n})).collect::<Vec<_>>(),
        },
        // Stub: site-traffic stats need `server/src/admin.rs` (Caddy JSON
        // access-log parsing) ported back too; see module doc.
        "web": Value::Null,
        "paymentSources": crate::sources::local(&state.db).await,
    })))
}

#[derive(Deserialize)]
pub(super) struct SetPlanReq {
    #[serde(default, rename = "businessId")]
    business_id: Option<Uuid>,
    #[serde(default, rename = "ownerPhone")]
    owner_phone: Option<String>,
    plan: String,
    #[serde(default, rename = "msgCap")]
    msg_cap: Option<i32>,
    #[serde(default, rename = "customerCap")]
    customer_cap: Option<i32>,
}

/// `curl -X POST https://agente.ceo/api/admin/plan -H "X-Admin-Key: …" \
///   -d '{"ownerPhone":"+51…","plan":"pro"}'`
pub(super) async fn admin_set_plan(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<SetPlanReq>,
) -> ApiResult {
    require_admin(&state, &headers)?;
    let id: Uuid = match (req.business_id, req.owner_phone.as_deref()) {
        (Some(id), _) => id,
        (None, Some(phone)) => {
            let canon = crate::whatsapp::normalize_phone(phone)
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "ownerPhone needs a country code"))?;
            // owner_phone is stored as the owner typed it at registration,
            // so compare canonical forms, newest business first.
            let rows: Vec<(Uuid, String)> = sqlx::query_as(
                "SELECT id, owner_phone FROM businesses ORDER BY created_at DESC",
            )
            .fetch_all(&state.db)
            .await
            .map_err(internal)?;
            rows.into_iter()
                .find(|(_, p)| crate::whatsapp::normalize_phone(p).as_deref() == Some(canon.as_str()))
                .ok_or_else(|| err(StatusCode::NOT_FOUND, "no business with that owner phone"))?.0
        }
        _ => return Err(err(StatusCode::BAD_REQUEST, "businessId or ownerPhone required")),
    };
    let plan = req.plan.trim().to_lowercase();
    if plan.is_empty() || plan.len() > 20 {
        return Err(err(StatusCode::BAD_REQUEST, "bad plan name"));
    }
    let row: (String,) = sqlx::query_as(
        "UPDATE businesses SET plan = $2, msg_cap = $3, customer_cap = $4 WHERE id = $1 RETURNING name",
    )
    .bind(id)
    .bind(&plan)
    .bind(req.msg_cap)
    .bind(req.customer_cap)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;
    tracing::info!(%id, business = %row.0, %plan, msg_cap = ?req.msg_cap, customer_cap = ?req.customer_cap, "plan changed");
    Ok(Json(json!({"businessId": id, "business": row.0, "plan": plan,
                   "msgCap": req.msg_cap, "customerCap": req.customer_cap})))
}

fn require_admin(state: &SharedState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let admin = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct_eq(admin, &state.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, Mock, ADMIN_KEY, APP_KEY};
    use serde_json::json;

    async fn admin(s: &crate::SharedState, method: &str, path: &str, body: Option<serde_json::Value>) -> (u16, serde_json::Value) {
        testkit::call(s, method, path, &[("x-admin-key", ADMIN_KEY)], body).await
    }

    #[tokio::test]
    async fn metrics_need_the_admin_key_not_the_app_key() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await;
        assert_eq!(testkit::call(&s, "GET", "/api/admin/metrics", &[("x-app-key", APP_KEY)], None).await.0, 401);
        assert_eq!(testkit::call(&s, "GET", "/api/admin/metrics", &[("x-admin-key", "nope")], None).await.0, 401);
        let (st, v) = admin(&s, "GET", "/api/admin/metrics", None).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["businesses"]["total"], 1);
        assert_eq!(v["businesses"]["rows"][0]["referralSource"], "direct");
        assert_eq!(v["revenue"]["total"], 0.0);
        assert_eq!(v["leads"]["total"], 0);
        assert!(v["usage"]["messagesTotal"].as_i64().unwrap() >= 1);
        // The admin key opens only admin routes, never tenant ones.
        assert_eq!(testkit::call(&s, "GET", "/api/dashboard", &[("x-admin-key", ADMIN_KEY)], None).await.0, 401);
    }

    #[tokio::test]
    async fn set_plan_by_id_or_by_the_owners_phone_however_typed() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (_, b) = testkit::onboard(&s, &m).await; // owner "+51 999 000 111"
        let (st, v) = admin(&s, "POST", "/api/admin/plan", Some(json!({"businessId": b, "plan": " PRO ", "msgCap": 0}))).await;
        assert_eq!((st, v["plan"].as_str(), v["msgCap"].as_i64()), (200, Some("pro"), Some(0)));
        let (st, v) = admin(&s, "POST", "/api/admin/plan", Some(json!({"ownerPhone": "51999000111", "plan": "max"}))).await;
        assert_eq!(st, 200, "the phone typed at registration must be found: {v}");
        let (plan,): (String,) = sqlx::query_as("SELECT plan FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!(plan, "max");
        assert_eq!(admin(&s, "POST", "/api/admin/plan", Some(json!({"ownerPhone": "+1 555 000 0000", "plan": "x"}))).await.0, 404);
        assert_eq!(admin(&s, "POST", "/api/admin/plan", Some(json!({"ownerPhone": "12", "plan": "x"}))).await.0, 400);
        assert_eq!(admin(&s, "POST", "/api/admin/plan", Some(json!({"plan": "x"}))).await.0, 400);
        assert_eq!(admin(&s, "POST", "/api/admin/plan", Some(json!({"businessId": b, "plan": " "}))).await.0, 400);
        assert_eq!(admin(&s, "POST", "/api/admin/plan", Some(json!({"businessId": b, "plan": "x".repeat(21)}))).await.0, 400);
        assert_eq!(testkit::call(&s, "POST", "/api/admin/plan", &[], Some(json!({"businessId": b, "plan": "pro"}))).await.0, 401);
    }
}
