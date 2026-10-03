//! Credits — the network's currency for new customers.
//!
//! Every paid plan returns half its price as credits. A credit is spent
//! when a *new* customer reaches a business through the agentic network:
//! the client's search (`/v1/match`) listed the business, and within a day
//! the client's agent sent it a first message. One lead per client per
//! business per 30 days; a business whose balance cannot cover a lead
//! simply stops being listed, so nobody is ever charged into the red.

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use crate::{accounts, internal, App};

pub const CURRENCY: &str = "PEN";

type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;
type E = (axum::http::StatusCode, axum::Json<Value>);

// ------------------------------------------------------------------ expiry
//
// Plan-included credits are "use it or lose it": they vanish at the end of
// the calendar month (Lima time) they were granted for. Purchased balance,
// bonuses and earnings never expire. See migration 021 for the legal basis.

/// Timezone the month ends in (`EXPIRY_TZ`, default America/Lima).
pub fn expiry_tz() -> Tz {
    std::env::var("EXPIRY_TZ").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(chrono_tz::America::Lima)
}

/// Ledger kinds whose lots expire (`EXPIRING_KINDS`, default `grant`).
pub fn expiring_kinds() -> Vec<String> {
    std::env::var("EXPIRING_KINDS").ok()
        .map(|s| s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["grant".to_string()])
}

pub fn kind_expires(kind: &str) -> bool {
    expiring_kinds().iter().any(|k| k == kind)
}

/// First instant (UTC) of the calendar month `months_ahead` months after the
/// one containing `now`, in the expiry timezone. A lot whose `expires_at` is
/// this instant is usable through the last second of its month.
pub fn month_boundary(now: DateTime<Utc>, months_ahead: u32) -> DateTime<Utc> {
    month_boundary_in(expiry_tz(), now, months_ahead)
}

pub fn month_boundary_in(tz: Tz, now: DateTime<Utc>, months_ahead: u32) -> DateTime<Utc> {
    let local = now.with_timezone(&tz);
    let total = local.year() * 12 + local.month0() as i32 + months_ahead as i32 + 1;
    let (y, m0) = (total.div_euclid(12), total.rem_euclid(12));
    tz.with_ymd_and_hms(y, (m0 + 1) as u32, 1, 0, 0, 0).earliest().map(|t| t.with_timezone(&Utc)).unwrap_or(now)
}

/// The ledger's timestamp format (UTC, milliseconds, lexically sortable and
/// comparable with `created_at`, which SQLite writes as `%Y-%m-%dT%H:%M:%fZ`).
pub fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

/// "31/08/2026" — the last day a lot with this boundary can be used.
pub fn last_day(boundary: DateTime<Utc>) -> String {
    (boundary - chrono::Duration::seconds(1)).with_timezone(&expiry_tz()).format("%d/%m/%Y").to_string()
}

/// Splits a grant into `n` monthly lots; the last lot takes the remainder.
pub fn split_lots(total: i64, n: i64) -> Vec<i64> {
    let n = n.max(1);
    let base = total / n;
    let mut v = vec![base; n as usize];
    if let Some(last) = v.last_mut() { *last += total - base * n; }
    v
}

/// Writes `expire` rows for every due lot of `account` that still has
/// remainder. Idempotent; cheap when nothing is due.
pub async fn expire_due_in(tx: &mut Tx<'_>, account: &str) -> Result<i64, E> {
    let due: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT l.id, l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id), 0), l.expires_at \
         FROM credit_ledger l WHERE l.account = $1 AND l.delta > 0 AND l.expires_at IS NOT NULL \
           AND l.expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(account).fetch_all(&mut **tx).await.map_err(internal)?;
    let mut total = 0i64;
    for (id, remaining, at) in due {
        if remaining <= 0 { continue; }
        let day = chrono::DateTime::parse_from_rfc3339(&at).map(|t| last_day(t.with_timezone(&Utc))).unwrap_or(at.clone());
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note, lot) VALUES ($1,$2,$3,$4,'expire',NULL,$5,$6)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(-remaining).bind(CURRENCY)
            .bind(format!("créditos del plan no usados al {day} — vencidos")).bind(&id)
            .execute(&mut **tx).await.map_err(internal)?;
        total += remaining;
    }
    if total > 0 {
        tracing::info!(%account, expired = total, "plan credits expired (use it or lose it)");
    }
    Ok(total)
}

/// Same, outside a transaction. Only opens one when something is due.
pub async fn expire_due(app: &App, account: &str) -> Result<i64, E> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM credit_ledger WHERE account = $1 AND delta > 0 AND expires_at IS NOT NULL AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') \
           AND NOT EXISTS (SELECT 1 FROM credit_ledger e WHERE e.lot = credit_ledger.id AND e.kind = 'expire')",
    ).bind(account).fetch_one(&app.db).await.map_err(internal)?;
    if n == 0 { return Ok(0); }
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let t = expire_due_in(&mut tx, account).await?;
    tx.commit().await.map_err(internal)?;
    Ok(t)
}

/// What is about to expire: (amount still unspent in expiring lots, the earliest boundary).
pub async fn expiring(app: &App, account: &str) -> Result<Option<(i64, String)>, E> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id), 0), l.expires_at \
         FROM credit_ledger l WHERE l.account = $1 AND l.delta > 0 AND l.expires_at IS NOT NULL \
           AND l.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') ORDER BY l.expires_at",
    ).bind(account).fetch_all(&app.db).await.map_err(internal)?;
    let first = rows.iter().find(|(r, _)| *r > 0).map(|(_, at)| at.clone());
    let Some(at) = first else { return Ok(None) };
    let amount: i64 = rows.iter().filter(|(r, a)| *r > 0 && *a == at).map(|(r, _)| r).sum();
    Ok(Some((amount, at)))
}

/// Spends `amount` from `account` inside `tx`, expiring lots first, one
/// negative row per lot consumed. `Ok(false)` = the balance cannot cover it.
pub async fn spend_in(tx: &mut Tx<'_>, account: &str, amount: i64, kind: &str, reference: Option<&str>, note: Option<&str>) -> Result<bool, E> {
    if amount <= 0 { return Ok(true); }
    expire_due_in(tx, account).await?;
    let (b,): (Option<i64>,) = sqlx::query_as("SELECT SUM(delta) FROM credit_ledger WHERE account = $1")
        .bind(account).fetch_one(&mut **tx).await.map_err(internal)?;
    if b.unwrap_or(0) < amount { return Ok(false); }
    let lots: Vec<(String, i64)> = sqlx::query_as(
        "SELECT l.id, l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id), 0) AS remaining \
         FROM credit_ledger l WHERE l.account = $1 AND l.delta > 0 AND remaining > 0 \
         ORDER BY (l.expires_at IS NULL), l.expires_at, l.created_at",
    ).bind(account).fetch_all(&mut **tx).await.map_err(internal)?;
    let row = |delta: i64, lot: Option<&str>| {
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note, lot) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(account.to_string()).bind(delta).bind(CURRENCY)
            .bind(kind.to_string()).bind(reference.map(String::from)).bind(note.map(String::from)).bind(lot.map(String::from))
    };
    let mut left = amount;
    for (id, remaining) in lots {
        if left == 0 { break; }
        let take = remaining.min(left);
        row(-take, Some(&id)).execute(&mut **tx).await.map_err(internal)?;
        left -= take;
    }
    if left > 0 {
        // Pre-021 spends were never attributed to a lot, so old lots can look
        // fuller than the balance is; the balance check above is the truth.
        row(-left, None).execute(&mut **tx).await.map_err(internal)?;
    }
    Ok(true)
}

/// `spend_in` in its own transaction. `Ok(Some(balance))` when charged.
pub async fn spend(app: &App, account: &str, amount: i64, kind: &str, reference: Option<&str>, note: Option<&str>) -> Result<Option<i64>, E> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    if !spend_in(&mut tx, account, amount, kind, reference, note).await? {
        return Ok(None);
    }
    let (b,): (Option<i64>,) = sqlx::query_as("SELECT SUM(delta) FROM credit_ledger WHERE account = $1")
        .bind(account).fetch_one(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Some(b.unwrap_or(0)))
}

/// Price of one network lead, minor units (S/ 5.00 by default).
pub fn lead_price(app: &App) -> i64 {
    app.lead_price_minor
}

/// The balance, after writing off anything that expired.
pub async fn balance(app: &App, account: &str) -> Result<i64, E> {
    expire_due(app, account).await?;
    let (b,): (Option<i64>,) = sqlx::query_as("SELECT SUM(delta) FROM credit_ledger WHERE account = $1")
        .bind(account).fetch_one(&app.db).await.map_err(internal)?;
    Ok(b.unwrap_or(0))
}

/// Adds a lot. Positive rows of an expiring kind get this month's boundary;
/// everything else lives forever. Negative deltas are written unattributed
/// (admin adjustments) — spend paths use `spend`/`spend_in` instead.
pub async fn add(app: &App, account: &str, delta: i64, kind: &str, reference: Option<&str>, note: Option<&str>) -> Result<i64, E> {
    let exp = (delta > 0 && kind_expires(kind)).then(|| stamp(month_boundary(Utc::now(), 0)));
    add_lot(app, account, delta, kind, reference, note, exp.as_deref()).await
}

pub async fn add_lot(app: &App, account: &str, delta: i64, kind: &str, reference: Option<&str>, note: Option<&str>, expires_at: Option<&str>) -> Result<i64, E> {
    sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note, expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(delta).bind(CURRENCY).bind(kind).bind(reference).bind(note).bind(expires_at)
        .execute(&app.db).await.map_err(internal)?;
    let b = balance(app, account).await?;
    tracing::info!(%account, delta, kind, expires = ?expires_at, balance = b, "credits");
    Ok(b)
}

/// Welcome credits so a new business can be found a couple of times
/// before it buys a plan (`FREE_STARTER_CREDITS_MINOR`).
pub async fn starter(app: &App, account: &str) {
    if app.starter_credits_minor > 0 {
        let _ = add(app, account, app.starter_credits_minor, "starter", None, Some("welcome credits")).await;
    }
}

/// Half of what was paid comes back as credits.
pub fn grant_for(amount_minor: i64) -> i64 {
    amount_minor / 2
}

/// Price of one gateway call paid from credits, minor units
/// (`CREDIT_CALL_PRICE_MINOR`, default 5 = S/ 0.05; a customer message is
/// about two calls, so S/ 20 ≈ 200 messages).
pub fn call_price() -> i64 {
    std::env::var("CREDIT_CALL_PRICE_MINOR").ok().and_then(|v| v.parse().ok()).filter(|p| *p > 0).unwrap_or(5)
}

/// Calls per message the screens assume when turning a balance into
/// "≈ N mensajes" (`CREDIT_CALLS_PER_MESSAGE`, default 2).
pub fn calls_per_message() -> i64 {
    std::env::var("CREDIT_CALLS_PER_MESSAGE").ok().and_then(|v| v.parse().ok()).filter(|p| *p > 0).unwrap_or(2)
}

/// How many customer messages the balance still covers.
pub fn messages_for(balance_minor: i64) -> i64 {
    (balance_minor / call_price() / calls_per_message()).max(0)
}

/// Pays one call from the account's credits, if it can. `Ok(Some(balance))`
/// when charged, `Ok(None)` when the balance does not cover a call. Never
/// into the red; one row per call so the ledger stays an audit trail
/// (`kind = call`).
pub async fn spend_call(app: &App, account: &str) -> Result<Option<i64>, E> {
    spend(app, account, call_price(), "call", None, None).await
}

/// Credits spent on calls today and over the last 30 days, minor units.
pub async fn spent_on_calls(app: &App, account: &str) -> (i64, i64) {
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT SUM(CASE WHEN created_at >= strftime('%Y-%m-%dT00:00:00Z','now') THEN -delta END), SUM(-delta) \
         FROM credit_ledger WHERE account = $1 AND kind = 'call' AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days')",
    ).bind(account).fetch_optional(&app.db).await.ok().flatten();
    row.map(|(t, m)| (t.unwrap_or(0), m.unwrap_or(0))).unwrap_or((0, 0))
}

/// Half of what was paid comes back as network credits — one lot per month
/// of the plan, each usable through the end of its calendar month and then
/// gone (use it or lose it). Purchased balance never expires; these do.
pub async fn grant_for_plan(app: &App, subject: &str, plan: &str, months: i64, reference: Option<&str>) {
    let Some(account) = subject.strip_prefix("acct:") else { return };
    // A dollar plan (English buyer) grants half its price too — booked in
    // soles at USD_PEN_RATE, since the ledger has one currency.
    let currency: String = match reference {
        Some(r) => sqlx::query_as::<_, (String,)>("SELECT currency FROM plan_requests WHERE ref = $1 UNION ALL SELECT currency FROM checkouts WHERE id = $1 LIMIT 1")
            .bind(r).fetch_optional(&app.db).await.ok().flatten().map(|c| c.0).unwrap_or_else(|| "PEN".into()),
        None => "PEN".into(),
    };
    let amount = crate::plans::to_ledger_minor(crate::plans::amount_minor_in(plan, months, &currency), &currency);
    if amount <= 0 {
        return;
    }
    let months = months.clamp(1, 12);
    let now = Utc::now();
    for (k, lot) in split_lots(grant_for(amount), months).into_iter().enumerate() {
        if lot <= 0 { continue; }
        let boundary = month_boundary(now, k as u32);
        let note = if months > 1 {
            format!("{plan} plan, mes {}/{months} · vence el {} (úsalo o piérdelo)", k + 1, last_day(boundary))
        } else {
            format!("{plan} plan · vence el {} (úsalo o piérdelo)", last_day(boundary))
        };
        let _ = add_lot(app, account, lot, "grant", reference, Some(&note), Some(&stamp(boundary))).await;
    }
}

/// The client saw these businesses in a search result.
pub async fn note_match(app: &App, client_agent: &str, businesses: &[String]) {
    for b in businesses {
        let _ = sqlx::query(
            "INSERT INTO match_hits (client_agent, business_agent) VALUES ($1, $2) \
             ON CONFLICT (client_agent, business_agent) DO UPDATE SET at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
        ).bind(client_agent).bind(b).execute(&app.db).await;
    }
}

/// Can this business still be listed? Its account must cover one lead.
pub async fn listable(app: &App, business_agent: &str) -> Result<bool, (axum::http::StatusCode, axum::Json<Value>)> {
    match accounts::account_of_agent(app, business_agent).await? {
        Some(acct) => Ok(balance(app, &acct).await? >= lead_price(app)),
        // Unlinked (previous build): listed only while the grace window lets it operate at all.
        None => Ok(app.grace_open()),
    }
}

/// A relay message from `client` to `business` just went through: charge a
/// lead if the client found the business through a search in the last day
/// and has not been counted in the last 30. Returns the amount charged.
pub async fn charge_lead_if_sourced(app: &App, client: &str, business: &str) -> Option<i64> {
    let acct = accounts::account_of_agent(app, business).await.ok().flatten()?;
    // Same account talking to itself (owner testing) is never a lead.
    if accounts::account_of_agent(app, client).await.ok().flatten().as_deref() == Some(acct.as_str()) {
        return None;
    }
    let sourced: Option<(String,)> = sqlx::query_as(
        "SELECT at FROM match_hits WHERE client_agent = $1 AND business_agent = $2 AND at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day')",
    ).bind(client).bind(business).fetch_optional(&app.db).await.ok()?;
    sourced?;
    let recent: Option<(String,)> = sqlx::query_as(
        "SELECT at FROM leads WHERE business_agent = $1 AND client_agent = $2 AND at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days')",
    ).bind(business).bind(client).fetch_optional(&app.db).await.ok()?;
    if recent.is_some() {
        return None;
    }
    let price = lead_price(app);
    spend(app, &acct, price, "lead", Some(client), Some("new customer via the network")).await.ok()??;
    let _ = sqlx::query(
        "INSERT INTO leads (business_agent, client_agent, charged) VALUES ($1, $2, $3) \
         ON CONFLICT (business_agent, client_agent) DO UPDATE SET charged = excluded.charged, at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    ).bind(business).bind(client).bind(price).execute(&app.db).await;
    Some(price)
}

/// Balance + recent movements, for the account page and `/v1/me`.
pub async fn summary(app: &App, account: &str, limit: i64) -> Result<Value, (axum::http::StatusCode, axum::Json<Value>)> {
    let rows: Vec<(i64, String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT delta, kind, ref, note, created_at FROM credit_ledger WHERE account = $1 ORDER BY created_at DESC LIMIT $2",
    ).bind(account).bind(limit).fetch_all(&app.db).await.map_err(internal)?;
    let (leads,): (i64,) = sqlx::query_as("SELECT count(*) FROM leads WHERE business_agent IN (SELECT agent FROM account_agents WHERE account = $1)")
        .bind(account).fetch_one(&app.db).await.map_err(internal)?;
    let bal = balance(app, account).await?;
    let (spent_today, spent_30d) = spent_on_calls(app, account).await;
    let exp = expiring(app, account).await?;
    Ok(json!({
        "balance": bal, "currency": CURRENCY, "leadPrice": lead_price(app), "leads": leads,
        "expiring": exp.as_ref().map(|(a, at)| json!({"amountMinor": a, "at": at, "lastDay": chrono::DateTime::parse_from_rfc3339(at).map(|t| last_day(t.with_timezone(&Utc))).unwrap_or_default(),
            "rule": "los créditos incluidos en el plan vencen al fin de cada mes calendario (hora de Lima) y se gastan antes que tu saldo recargado; las recargas nunca vencen"})),
        "callPrice": call_price(), "callsPerMessage": calls_per_message(), "messagesLeft": messages_for(bal),
        "spentToday": spent_today, "spent30d": spent_30d,
        "recargas": crate::plans::recarga_options(),
        "ledger": rows.into_iter().map(|(d, k, r, n, at)| json!({"delta": d, "kind": k, "ref": r, "note": n, "at": at})).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_boundary_is_lima_midnight_of_next_month() {
        let tz = chrono_tz::America::Lima;
        let now = Utc.with_ymd_and_hms(2026, 8, 25, 20, 0, 0).unwrap(); // 15:00 Lima, Aug 25
        assert_eq!(stamp(month_boundary_in(tz, now, 0)), "2026-09-01T05:00:00.000Z"); // 00:00 Lima, Sep 1
        assert_eq!(stamp(month_boundary_in(tz, now, 1)), "2026-10-01T05:00:00.000Z");
        assert_eq!(stamp(month_boundary_in(tz, now, 4)), "2027-01-01T05:00:00.000Z"); // wraps the year
        // 02:00Z on Sep 1 is still Aug 31 in Lima, so the lot lives through this month.
        let late = Utc.with_ymd_and_hms(2026, 9, 1, 2, 0, 0).unwrap();
        assert_eq!(stamp(month_boundary_in(tz, late, 0)), "2026-09-01T05:00:00.000Z");
        assert_eq!((month_boundary_in(tz, now, 0) - chrono::Duration::seconds(1)).with_timezone(&tz).format("%d/%m/%Y").to_string(), "31/08/2026");
    }

    #[test]
    fn lots_split_exactly() {
        assert_eq!(split_lots(5_000, 12).iter().sum::<i64>(), 5_000);
        assert_eq!(split_lots(5_000, 12)[11], 5_000 - 416 * 11);
        assert_eq!(split_lots(100, 1), vec![100]);
        assert_eq!(split_lots(7, 3), vec![2, 2, 3]);
    }

    #[test]
    fn only_grants_expire_by_default() {
        assert!(kind_expires("grant"));
        for k in ["topup", "bonus", "earn", "sale", "starter", "referral", "data", "transfer", "deposit"] {
            assert!(!kind_expires(k), "{k} must never expire");
        }
    }

    /// The real migrations on an in-memory ledger: expiring lots are spent
    /// first, purchased balance is untouched until they run out, and what
    /// is left of a lot at month end is written off — nothing else is.
    #[tokio::test]
    async fn spends_expiring_first_and_writes_off_only_the_remainder() {
        let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        async fn ins(db: &sqlx::SqlitePool, id: &str, delta: i64, kind: &str, exp: Option<&str>) {
            sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, expires_at) VALUES ($1,'a',$2,'PEN',$3,$4)")
                .bind(id).bind(delta).bind(kind).bind(exp).execute(db).await.unwrap();
        }
        async fn sum(db: &sqlx::SqlitePool) -> i64 {
            let (b,): (Option<i64>,) = sqlx::query_as("SELECT SUM(delta) FROM credit_ledger WHERE account = 'a'").fetch_one(db).await.unwrap();
            b.unwrap_or(0)
        }
        ins(&db, "g1", 1_000, "grant", Some("2099-01-01T00:00:00Z")).await; // this month's plan credits
        ins(&db, "t1", 5_000, "topup", None).await;                        // bought balance

        // Spend 700: all of it comes out of the expiring lot.
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(spend_in(&mut tx, "a", 700, "call", None, None).await.unwrap());
        tx.commit().await.unwrap();
        let (g1_rem,): (i64,) = sqlx::query_as("SELECT 1000 + COALESCE(SUM(delta),0) FROM credit_ledger WHERE lot = 'g1'").fetch_one(&db).await.unwrap();
        let (t1_rem,): (i64,) = sqlx::query_as("SELECT 5000 + COALESCE(SUM(delta),0) FROM credit_ledger WHERE lot = 't1'").fetch_one(&db).await.unwrap();
        assert_eq!((g1_rem, t1_rem), (300, 5_000));

        // Spend 500: finishes the lot (300) then dips into the topup (200).
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(spend_in(&mut tx, "a", 500, "ask", Some("q1"), None).await.unwrap());
        tx.commit().await.unwrap();
        let (t1_rem,): (i64,) = sqlx::query_as("SELECT 5000 + COALESCE(SUM(delta),0) FROM credit_ledger WHERE lot = 't1'").fetch_one(&db).await.unwrap();
        assert_eq!(t1_rem, 4_800);
        assert_eq!(sum(&db).await, 4_800);

        // Cannot go into the red; nothing is written on refusal.
        let (rows_before,): (i64,) = sqlx::query_as("SELECT count(*) FROM credit_ledger").fetch_one(&db).await.unwrap();
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(!spend_in(&mut tx, "a", 4_801, "call", None, None).await.unwrap());
        drop(tx);
        let (rows_after,): (i64,) = sqlx::query_as("SELECT count(*) FROM credit_ledger").fetch_one(&db).await.unwrap();
        assert_eq!(rows_before, rows_after);

        // A new month's lot, then the month ends with 250 of it unused: exactly 250 expires.
        ins(&db, "g2", 400, "grant", Some("2000-01-01T00:00:00Z")).await; // already past
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        // it is due, so a spend expires it first and then pays from the topup
        assert_eq!(expire_due_in(&mut tx, "a").await.unwrap(), 400);
        assert_eq!(expire_due_in(&mut tx, "a").await.unwrap(), 0); // idempotent
        tx.commit().await.unwrap();
        ins(&db, "g3", 400, "grant", Some("2000-02-01T00:00:00Z")).await;
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, lot) VALUES ('s3','a',-150,'PEN','call','g3')").execute(&db).await.unwrap();
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert_eq!(expire_due_in(&mut tx, "a").await.unwrap(), 250);
        tx.commit().await.unwrap();
        let (expired,): (Option<i64>,) = sqlx::query_as("SELECT SUM(-delta) FROM credit_ledger WHERE kind = 'expire'").fetch_one(&db).await.unwrap();
        assert_eq!(expired, Some(650));
        assert_eq!(sum(&db).await, 4_800); // the topup was never touched by expiry
    }

    #[test]
    fn half_of_what_was_paid() {
        assert_eq!(super::grant_for(10_000), 5_000);
        assert_eq!(super::grant_for(0), 0);
        // A year of Pro is ten months' money, so ten months' credits.
        assert_eq!(super::grant_for(crate::plans::amount_minor("pro", 12)), crate::plans::price_minor("pro") * 10 / 2);
        assert_eq!(crate::plans::amount_minor("pro", 3), crate::plans::price_minor("pro") * 3);
    }
}
