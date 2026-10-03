//! `GET /admin/books?period=YYYYMM` — the Peruvian entity's month, closed from
//! the ledger. Derived, not asserted: every number here is a query over
//! `credit_ledger`, `plan_requests`, `withdrawals` and `spent_coins`, and the
//! consolidator (`yaya-books`) translates it into the group's USD statements.
//!
//! Recognition (see yaya-books/POLICIES.md — ASC 606):
//! * plan sales are a contract liability recognised ratably over the plan term;
//! * balance is a customer liability until *consumed* (calls, leads) — that is
//!   revenue; asks and market sales are user-to-user, the platform's revenue is
//!   the fee row;
//! * plan-included credits that expire are breakage revenue;
//! * coins in circulation are a liability of the exchange, at par.
//! Amounts are PEN minor units (céntimos) unless the key says otherwise.

use axum::{extract::{Query, State}, http::{HeaderMap, StatusCode}, response::IntoResponse, Json};
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use crate::{credits, err, internal, wallet, ApiResult, Shared};

type E = (StatusCode, Json<Value>);

#[derive(serde::Deserialize)]
pub struct BooksQuery { pub period: Option<String> }

/// `(start, end)` of the calendar month in the expiry timezone, as ledger stamps.
pub fn period_bounds(period: &str) -> Option<(String, String)> {
    if period.len() != 6 || !period.bytes().all(|b| b.is_ascii_digit()) { return None; }
    let (y, m) = (period[..4].parse::<i32>().ok()?, period[4..].parse::<u32>().ok()?);
    if !(1..=12).contains(&m) { return None; }
    let tz = credits::expiry_tz();
    let start = tz.with_ymd_and_hms(y, m, 1, 0, 0, 0).earliest()?.with_timezone(&Utc);
    Some((credits::stamp(start), credits::stamp(credits::month_boundary_in(tz, start, 0))))
}

fn plan_days(months: i64) -> i64 { if months >= 12 { 365 } else { 30 * months.max(1) } }

/// Fraction of a plan bought at `paid_at` (RFC3339-ish) that falls inside [start, end).
fn ratable(paid_at: &str, months: i64, start: &str, end: &str) -> f64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).map(|t| t.with_timezone(&Utc))
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00")).map(|t| t.with_timezone(&Utc)));
    let (Ok(p), Ok(s), Ok(e)) = (parse(paid_at), parse(start), parse(end)) else { return 0.0 };
    let term = chrono::Duration::days(plan_days(months));
    let (a, b) = (p.max(s), (p + term).min(e));
    if b <= a { return 0.0; }
    (b - a).num_seconds() as f64 / term.num_seconds() as f64
}

fn minor(soles: f64) -> i64 { (soles * 100.0).round() as i64 }

/// Months a `plan` ledger row was for ("… (3 mes(es))"), default 1.
fn months_in_note(note: &str) -> i64 {
    note.split('(').nth(1).and_then(|s| s.split_whitespace().next()).and_then(|n| n.parse().ok()).unwrap_or(1)
}

pub async fn close(db: &sqlx::SqlitePool, period: &str) -> Result<Value, E> {
    let (start, end) = period_bounds(period).ok_or_else(|| err(StatusCode::BAD_REQUEST, "period must be YYYYMM"))?;
    let platform = wallet::platform_account();
    let coins = crate::exchange::COINS_ACCOUNT;

    // ---- the period's movements, by kind
    let by_kind: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT kind, COALESCE(SUM(CASE WHEN delta > 0 THEN delta END),0), COALESCE(SUM(CASE WHEN delta < 0 THEN -delta END),0), count(*) \
         FROM credit_ledger WHERE created_at >= $1 AND created_at < $2 GROUP BY kind ORDER BY kind",
    ).bind(&start).bind(&end).fetch_all(db).await.map_err(internal)?;
    let out_of = |k: &str| by_kind.iter().find(|r| r.0 == k).map(|r| r.2).unwrap_or(0);
    let into = |k: &str| by_kind.iter().find(|r| r.0 == k).map(|r| r.1).unwrap_or(0);

    let (fees,): (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(delta) FROM credit_ledger WHERE account = $1 AND kind = 'fee' AND created_at >= $2 AND created_at < $3",
    ).bind(&platform).bind(&start).bind(&end).fetch_one(db).await.map_err(internal)?;
    let (data_rewards,): (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(delta) FROM credit_ledger WHERE kind = 'data' AND delta > 0 AND created_at >= $1 AND created_at < $2",
    ).bind(&start).bind(&end).fetch_one(db).await.map_err(internal)?;

    // ---- plan sales: cash now, revenue ratably (ASC 606)
    let paid: Vec<(String, f64, i64, String)> = sqlx::query_as(
        "SELECT plan, amount, months, paid_at FROM plan_requests WHERE status = 'paid' AND paid_at IS NOT NULL AND paid_at < $1 AND paid_at > datetime($2, '-400 days')",
    ).bind(&end).bind(&start).fetch_all(db).await.map_err(internal)?;
    let paid_with_balance: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT -delta, created_at, COALESCE(note,'') FROM credit_ledger WHERE kind = 'plan' AND delta < 0 AND created_at < $1 AND created_at > datetime($2, '-400 days')",
    ).bind(&end).bind(&start).fetch_all(db).await.map_err(internal)?;
    let mut plan_cash = 0i64; let mut plan_cash_n = 0i64; let mut plan_rev = 0f64; let mut deferred = 0f64;
    let mut by_tier: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
    for (plan, amount, months, at) in &paid {
        let m = minor(*amount);
        if at.as_str() >= start.as_str() && at.as_str() < end.as_str() {
            plan_cash += m; plan_cash_n += 1;
            let e = by_tier.entry(plan.clone()).or_default(); e.0 += 1; e.1 += m;
        }
        plan_rev += m as f64 * ratable(at, *months, &start, &end);
        deferred += m as f64 * ratable(at, *months, &end, "2999-01-01T00:00:00Z");
    }
    for (m, at, note) in &paid_with_balance {
        let months = months_in_note(note);
        plan_rev += *m as f64 * ratable(at, months, &start, &end);
        deferred += *m as f64 * ratable(at, months, &end, "2999-01-01T00:00:00Z");
    }

    // ---- liabilities at period end
    async fn bal(db: &sqlx::SqlitePool, sql: &'static str, end: &str, platform: &str, coins: &str) -> Result<i64, E> {
        sqlx::query_as::<_, (Option<i64>,)>(sql).bind(end).bind(platform).bind(coins).fetch_one(db).await.map(|r| r.0.unwrap_or(0)).map_err(internal)
    }
    let customer_balances = bal(db, "SELECT SUM(delta) FROM credit_ledger WHERE created_at < $1 AND account != $2 AND account != $3", &end, &platform, coins).await?;
    let platform_balance = bal(db, "SELECT SUM(delta) FROM credit_ledger WHERE created_at < $1 AND account = $2 AND $3 = $3", &end, &platform, coins).await?;
    let circulation = bal(db, "SELECT SUM(delta) FROM credit_ledger WHERE created_at < $1 AND $2 = $2 AND account = $3", &end, &platform, coins).await?;
    let by_origin: Vec<(String, i64)> = sqlx::query_as(
        "SELECT l.kind, SUM(l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id AND s.created_at < $1), 0)) \
         FROM credit_ledger l WHERE l.delta > 0 AND l.created_at < $1 AND l.account != $2 AND l.account != $3 GROUP BY l.kind",
    ).bind(&end).bind(&platform).bind(coins).fetch_all(db).await.map_err(internal)?;
    let origin = |ks: &[&str]| by_origin.iter().filter(|(k, _)| ks.contains(&k.as_str())).map(|(_, v)| v).sum::<i64>();
    let (unexpired_grants,): (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id AND s.created_at < $1), 0)) \
         FROM credit_ledger l WHERE l.kind = 'grant' AND l.delta > 0 AND l.created_at < $1 AND (l.expires_at IS NULL OR l.expires_at > $1)",
    ).bind(&end).fetch_one(db).await.map_err(internal)?;

    // ---- exchange
    let (withdrawn,): (Option<i64>,) = sqlx::query_as("SELECT SUM(amount) FROM withdrawals WHERE at >= $1 AND at < $2").bind(&start).bind(&end).fetch_one(db).await.map_err(internal)?;
    let spent: Vec<(String, i64)> = sqlx::query_as("SELECT kind, count(*) FROM spent_coins WHERE spent_at >= $1 AND spent_at < $2 GROUP BY kind").bind(&start).bind(&end).fetch_all(db).await.map_err(internal)?;

    // ---- counts
    let (accounts_new,): (i64,) = sqlx::query_as("SELECT count(*) FROM accounts WHERE created_at >= $1 AND created_at < $2").bind(&start).bind(&end).fetch_one(db).await.map_err(internal)?;
    let (accounts_total,): (i64,) = sqlx::query_as("SELECT count(*) FROM accounts WHERE created_at < $1").bind(&end).fetch_one(db).await.map_err(internal)?;

    let purchased = origin(&["topup"]);
    let promotional = origin(&["grant", "bonus", "starter"]);
    let earned = origin(&["earn", "sale", "referral", "data", "transfer", "deposit"]);
    Ok(json!({
        "entity": "yaya-peru", "functionalCurrency": credits::CURRENCY, "unit": "minor (céntimos) unless stated",
        "period": period, "periodStart": start, "periodEnd": end, "tz": credits::expiry_tz().name(),
        "generatedAt": Utc::now().to_rfc3339(), "basis": "derived from the ledger; unaudited",
        "cash": {
            "planSales": plan_cash, "planSalesCount": plan_cash_n,
            "planSalesByTier": by_tier.iter().map(|(t, (n, a))| json!({"plan": t, "n": n, "amount": a})).collect::<Vec<_>>(),
            "topups": into("topup"),
            "totalIn": plan_cash + into("topup"),
        },
        "revenue": {
            "planRatable": plan_rev.round() as i64,
            "consumption": {"calls": out_of("call"), "leads": out_of("lead"), "total": out_of("call") + out_of("lead")},
            "platformFees": fees.unwrap_or(0),
            "breakage": out_of("expire"),
            "total": plan_rev.round() as i64 + out_of("call") + out_of("lead") + fees.unwrap_or(0) + out_of("expire"),
        },
        "expenses": {"dataRewards": data_rewards.unwrap_or(0), "launchBonus": into("bonus"), "starterCredits": into("starter")},
        "liabilities": {
            "customerBalances": customer_balances,
            "byOrigin": {"purchased": purchased, "promotional": promotional, "earned": earned,
                          "unattributedSpend": (purchased + promotional + earned) - customer_balances},
            "unexpiredPlanCredits": unexpired_grants.unwrap_or(0),
            "deferredPlanRevenue": deferred.round() as i64,
            "coinsInCirculation": circulation,
        },
        "platformBalance": platform_balance,
        "exchange": {"withdrawn": withdrawn.unwrap_or(0), "coinsSpent": spent.iter().map(|(k, n)| json!({"kind": k, "n": n})).collect::<Vec<_>>(), "circulationEnd": circulation},
        "ledgerByKind": by_kind.iter().map(|(k, i, o, n)| json!({"kind": k, "in": i, "out": o, "rows": n})).collect::<Vec<_>>(),
        "accounts": {"new": accounts_new, "total": accounts_total},
    }))
}

/// `GET /admin/books?period=YYYYMM` (default: the current month so far).
pub async fn admin_books(State(app): State<Shared>, headers: HeaderMap, Query(q): Query<BooksQuery>) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    let period = q.period.unwrap_or_else(|| Utc::now().with_timezone(&credits::expiry_tz()).format("%Y%m").to_string());
    Ok(Json(close(&app.db, &period).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_proration() {
        assert!(period_bounds("20€6").is_none() && period_bounds("2026-8").is_none());
        let (s, e) = period_bounds("202608").unwrap();
        assert_eq!((s.as_str(), e.as_str()), ("2026-08-01T05:00:00.000Z", "2026-09-01T05:00:00.000Z"));
        assert!(period_bounds("202613").is_none());
        // A 30-day plan paid Aug 16 05:00Z: 16 of 30 days fall in August.
        let f = ratable("2026-08-16T05:00:00Z", 1, &s, &e);
        assert!((f - 16.0 / 30.0).abs() < 1e-9, "{f}");
        assert_eq!(ratable("2026-06-01T00:00:00Z", 1, &s, &e), 0.0);
        assert_eq!(months_in_note("plan pro pagado con saldo (3 mes(es))"), 3);
        assert_eq!(months_in_note("x"), 1);
    }

    #[tokio::test]
    async fn closes_a_month_from_the_ledger() {
        let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let row = |id: &str, acct: &str, delta: i64, kind: &str, at: &str, lot: Option<&str>, exp: Option<&str>| {
            let (id, acct, kind, at) = (id.to_string(), acct.to_string(), kind.to_string(), at.to_string());
            let (lot, exp) = (lot.map(String::from), exp.map(String::from));
            async move {
                sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, created_at, lot, expires_at) VALUES ($1,$2,$3,'PEN',$4,$5,$6,$7)")
                    .bind(id).bind(acct).bind(delta).bind(kind).bind(at).bind(lot).bind(exp)
            }
        };
        // August (Lima): a recarga, a plan grant, calls, a paid ask with fee, an expiry, coins out.
        row("t", "c1", 5_000, "topup", "2026-08-03T12:00:00.000Z", None, None).await.execute(&db).await.unwrap();
        row("g", "c1", 5_000, "grant", "2026-08-03T12:00:00.000Z", None, Some("2026-09-01T05:00:00.000Z")).await.execute(&db).await.unwrap();
        row("s1", "c1", -1_000, "call", "2026-08-10T12:00:00.000Z", Some("g"), None).await.execute(&db).await.unwrap();
        row("s2", "c1", -200, "ask", "2026-08-11T12:00:00.000Z", Some("g"), None).await.execute(&db).await.unwrap();
        row("e1", "c2", 180, "earn", "2026-08-11T12:00:00.000Z", None, None).await.execute(&db).await.unwrap();
        row("f1", "yaya", 20, "fee", "2026-08-11T12:00:00.000Z", None, None).await.execute(&db).await.unwrap();
        row("x", "c1", -3_800, "expire", "2026-09-01T05:00:00.000Z", Some("g"), None).await.execute(&db).await.unwrap(); // September's row
        row("w", "c1", -1_000, "withdraw", "2026-08-20T12:00:00.000Z", Some("t"), None).await.execute(&db).await.unwrap();
        row("wc", "coins", 1_000, "withdraw", "2026-08-20T12:00:00.000Z", None, None).await.execute(&db).await.unwrap();
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, status, paid_at) VALUES ('r1','acct:c1','pro',100.0,'PEN',1,'paid','2026-08-16T05:00:00Z')").execute(&db).await.unwrap();

        let b = close(&db, "202608").await.unwrap();
        assert_eq!(b["cash"]["planSales"], 10_000);
        assert_eq!(b["cash"]["topups"], 5_000);
        assert_eq!(b["revenue"]["consumption"]["total"], 1_000);
        assert_eq!(b["revenue"]["platformFees"], 20);
        assert_eq!(b["revenue"]["breakage"], 0);            // the write-off lands in September
        assert_eq!(b["revenue"]["planRatable"], 5_333);      // 16/30 of S/100
        assert_eq!(b["liabilities"]["deferredPlanRevenue"], 4_667);
        assert_eq!(b["liabilities"]["customerBalances"], 5_000 + 5_000 - 1_000 - 200 + 180 - 1_000);
        assert_eq!(b["liabilities"]["byOrigin"]["purchased"], 4_000);
        assert_eq!(b["liabilities"]["byOrigin"]["promotional"], 3_800);
        assert_eq!(b["liabilities"]["unexpiredPlanCredits"], 0); // boundary == periodEnd: not unexpired
        assert_eq!(b["liabilities"]["coinsInCirculation"], 1_000);
        assert_eq!(b["platformBalance"], 20);

        let s = close(&db, "202609").await.unwrap();
        assert_eq!(s["revenue"]["breakage"], 3_800);
        assert_eq!(s["revenue"]["planRatable"], 4_667);
        assert_eq!(s["cash"]["planSales"], 0);
    }
}
