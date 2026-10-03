//! Prepaid credits — the revenue model since app 1.22.0 (closed loop, USD).
//!
//! The account holds a balance in USD cents, in three buckets consumed in
//! this order: **free** (the $12 welcome grant and promos; expires 60 days
//! after the grant), **bonus** (what a top-up tier adds on top; never
//! expires, never refunded), **paid** (what was actually paid; never
//! expires, refundable to the original method only).
//!
//! The balance is charged only when the phone reports a **confirmed
//! outcome** (a booking or a sale): $1 for a client the business already
//! had a confirmed outcome with, $2 for a new one; after 100 charged
//! outcomes in a calendar month the prices halve, and a hard cap of $199
//! stops charging for the rest of the month — both on the charged amount,
//! whatever bucket paid it. Tax is only ever on money paid: a debit drawn
//! from the paid bucket records the country's rate and the net; free and
//! bonus draws carry none.
//!
//! The balance never decides who is served. It may run to −$4 (grace);
//! past that the phone stops answering and hands chats to the owner, who
//! gets one WhatsApp with a top-up link. Cancellations within 24 h of a
//! charge give it back to the lots it drew from. Refunding a top-up returns
//! its paid lot and voids its bonus lot — spent or not, so the balance may
//! go negative. Twenty-four months without a movement → a warning; thirty
//! days later, still nothing → every bucket is forfeited.
//!
//! Money comes in through Dodo Payments (card, merchant of record, hosted
//! checkout + webhook; one product per tier) and, in Perú, the Yape/Plin
//! recarga path (`plans::confirm_request`, `plan = usd_topup`).
//!
//! Spec: `docs/CREDITS.md` in the agente app repo. `accounts::otp_check`
//! calls [`welcome`]; `main.rs` routes the endpoints and runs [`dormancy_sweep`].

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use chrono::{DateTime, Datelike, Utc};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::{accounts, err, internal, ApiResult, App, Auth, Shared};

pub const CURRENCY: &str = "USD";
/// Prices, USD cents, tax-inclusive.
pub const PRICE_KNOWN: i64 = 100;
pub const PRICE_NEW: i64 = 200;
/// Charged outcomes in a calendar month after which prices halve.
pub const VOLUME_AFTER: i64 = 100;
/// Hard monthly cap, USD cents.
pub const MONTHLY_CAP: i64 = 19_900;
/// Welcome grant, USD cents (= 6 new clients), free bucket.
pub const WELCOME: i64 = 1_200;
/// Free credits live this long.
pub const FREE_DAYS: i64 = 60;
/// The balance may fall this far; below it the agent hands off.
pub const GRACE: i64 = -400;
/// A charge can be reversed this long after it was made.
pub const REVERSAL_HOURS: i64 = 24;
/// Below this (cents) the dashboard says "Recarga pronto".
pub const LOW_BELOW: i64 = 200;
/// Dormancy: no movement for this long → warning; then this many days → forfeit.
pub const DORMANT_MONTHS: i64 = 24;
pub const DORMANT_GRACE_DAYS: i64 = 30;
/// The closed-loop terms the owner accepts at registration.
pub const TERMS_VERSION: &str = "2026-09";

/// A top-up tier: what is paid and what the bonus bucket receives, cents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    pub id: &'static str,
    pub pays: i64,
    pub bonus: i64,
}

impl Tier {
    pub fn credits(&self) -> i64 {
        self.pays + self.bonus
    }
}

pub const TIERS: [Tier; 3] = [
    Tier { id: "basic", pays: 1_000, bonus: 0 },
    Tier { id: "plus", pays: 2_500, bonus: 250 },
    Tier { id: "max", pays: 5_000, bonus: 750 },
];
pub const DEFAULT_TIER: &str = "plus";

pub fn tier(id: &str) -> Option<Tier> {
    TIERS.iter().copied().find(|t| t.id == id)
}

/// A tier by what it costs in whole USD (older clients send `amount`).
pub fn tier_for_amount(usd: i64) -> Option<Tier> {
    TIERS.iter().copied().find(|t| t.pays == usd * 100)
}

type E = (StatusCode, Json<Value>);
type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;

// ------------------------------------------------------------------ country

/// ISO-2 from the E.164 digits of a WhatsApp number (longest prefix wins).
/// Only the markets we sell in and the top world codes; anything else is
/// `None` and the account keeps no country until ops sets one.
pub fn country_of_phone(phone: &str) -> Option<&'static str> {
    let d: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    const T: &[(&str, &str)] = &[
        ("1809", "DO"), ("1829", "DO"), ("1849", "DO"), ("1787", "PR"), ("1939", "PR"), ("1876", "JM"), ("1868", "TT"),
        ("501", "BZ"), ("502", "GT"), ("503", "SV"), ("504", "HN"), ("505", "NI"), ("506", "CR"), ("507", "PA"), ("509", "HT"),
        ("591", "BO"), ("592", "GY"), ("593", "EC"), ("595", "PY"), ("597", "SR"), ("598", "UY"),
        ("351", "PT"), ("353", "IE"), ("358", "FI"), ("380", "UA"), ("420", "CZ"),
        ("51", "PE"), ("52", "MX"), ("53", "CU"), ("54", "AR"), ("55", "BR"), ("56", "CL"), ("57", "CO"), ("58", "VE"),
        ("34", "ES"), ("44", "GB"), ("33", "FR"), ("49", "DE"), ("39", "IT"), ("31", "NL"), ("32", "BE"), ("41", "CH"), ("43", "AT"),
        ("46", "SE"), ("47", "NO"), ("45", "DK"), ("48", "PL"), ("36", "HU"), ("40", "RO"), ("30", "GR"), ("90", "TR"),
        ("972", "IL"), ("971", "AE"), ("966", "SA"), ("974", "QA"), ("20", "EG"), ("212", "MA"), ("27", "ZA"), ("234", "NG"), ("254", "KE"), ("233", "GH"),
        ("91", "IN"), ("86", "CN"), ("81", "JP"), ("82", "KR"), ("852", "HK"), ("886", "TW"), ("65", "SG"), ("60", "MY"), ("62", "ID"), ("63", "PH"),
        ("66", "TH"), ("84", "VN"), ("92", "PK"), ("880", "BD"), ("61", "AU"), ("64", "NZ"),
        ("1", "US"),
    ];
    let mut best: Option<(&str, &str)> = None;
    for (prefix, iso) in T {
        if d.starts_with(prefix) && best.map_or(true, |(p, _)| prefix.len() > p.len()) {
            best = Some((prefix, iso));
        }
    }
    best.map(|(_, iso)| iso)
}

#[derive(Clone, Debug)]
pub struct CountryConfig {
    pub iso: String,
    pub deposits_enabled: bool,
    pub tax_rate: f64,
    pub display_currency: String,
}

/// The country row, or the "others" default (no deposits, 0 %, USD).
pub async fn country_config(db: &SqlitePool, iso: Option<&str>) -> CountryConfig {
    let iso = iso.unwrap_or("").trim().to_ascii_uppercase();
    let row: Option<(i64, f64, String)> = sqlx::query_as("SELECT deposits_enabled, tax_rate, display_currency FROM country_config WHERE iso = $1")
        .bind(&iso).fetch_optional(db).await.ok().flatten();
    match row {
        Some((d, t, c)) => CountryConfig { iso, deposits_enabled: d == 1, tax_rate: t, display_currency: c },
        None => CountryConfig { iso, deposits_enabled: false, tax_rate: 0.0, display_currency: "USD".into() },
    }
}

/// Fills `accounts.country` once, from the phone. Never overwrites.
pub async fn set_country_once(db: &SqlitePool, account: &str, phone: Option<&str>) {
    let Some(iso) = phone.and_then(country_of_phone) else { return };
    let _ = sqlx::query("UPDATE accounts SET country = $2 WHERE id = $1 AND country IS NULL").bind(account).bind(iso).execute(db).await;
}

/// The country config that applies to an account — the tax rate a paid
/// draw records, and the currency its checkout shows. Falls back to the
/// default row when the account has no country (an old row, or a phone
/// whose dialling code we do not know).
pub(crate) async fn country_config_for(db: &SqlitePool, account: &str) -> CountryConfig {
    let iso = account_country(db, account).await;
    country_config(db, iso.as_deref()).await
}

async fn account_country(db: &SqlitePool, account: &str) -> Option<String> {
    sqlx::query_as::<_, (Option<String>,)>("SELECT country FROM accounts WHERE id = $1").bind(account).fetch_optional(db).await.ok().flatten().and_then(|r| r.0)
}

// ------------------------------------------------------------------ pricing

/// "YYYY-MM" of `now` in the expiry timezone (America/Lima by default).
pub fn month_key(now: DateTime<Utc>) -> String {
    let local = now.with_timezone(&crate::credits::expiry_tz());
    format!("{:04}-{:02}", local.year(), local.month())
}

/// What this outcome costs given the month so far. Pure, so it is testable:
/// `(amount_cents, cap_reached)`. Volume pricing applies from the outcome
/// after `VOLUME_AFTER` charged ones; the cap trims the last charge and
/// zeroes everything after it.
pub fn price(outcomes_before: i64, charged_before: i64, is_new: bool) -> (i64, bool) {
    let base = if is_new { PRICE_NEW } else { PRICE_KNOWN };
    let p = if outcomes_before >= VOLUME_AFTER { base / 2 } else { base };
    let room = (MONTHLY_CAP - charged_before).max(0);
    let amount = p.min(room);
    (amount, amount < p || room == 0)
}

/// Tax-inclusive → net, cents, half-up.
pub fn net_of(amount_cents: i64, tax_rate_percent: f64) -> i64 {
    ((amount_cents as f64) / (1.0 + tax_rate_percent / 100.0)).round() as i64
}

pub fn state_of(balance: i64) -> &'static str {
    if balance < GRACE { "manual" } else if balance < 0 { "grace" } else if balance < LOW_BELOW { "low" } else { "ok" }
}

pub(crate) fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn cents(v: i64) -> f64 {
    v as f64 / 100.0
}

pub async fn balance(db: &SqlitePool, account: &str) -> Result<i64, E> {
    let (b,): (Option<i64>,) = sqlx::query_as("SELECT SUM(amount_cents) FROM prepaid_ledger WHERE account = $1")
        .bind(account).fetch_one(db).await.map_err(internal)?;
    Ok(b.unwrap_or(0))
}

pub(crate) async fn balance_in(tx: &mut Tx<'_>, account: &str) -> Result<i64, E> {
    let (b,): (Option<i64>,) = sqlx::query_as("SELECT SUM(amount_cents) FROM prepaid_ledger WHERE account = $1")
        .bind(account).fetch_one(&mut **tx).await.map_err(internal)?;
    Ok(b.unwrap_or(0))
}

/// Per-bucket balances: (free, bonus, paid).
pub async fn buckets(db: &SqlitePool, account: &str) -> Result<(i64, i64, i64), E> {
    let rows: Vec<(String, Option<i64>)> = sqlx::query_as("SELECT bucket, SUM(amount_cents) FROM prepaid_ledger WHERE account = $1 GROUP BY bucket")
        .bind(account).fetch_all(db).await.map_err(internal)?;
    let get = |b: &str| rows.iter().find(|(k, _)| k == b).and_then(|(_, v)| *v).unwrap_or(0);
    Ok((get("free"), get("bonus"), get("paid")))
}

// ------------------------------------------------------------------ lots

/// Writes off every due free lot's unspent remainder. Idempotent.
pub(crate) async fn expire_due_in(tx: &mut Tx<'_>, account: &str, now: DateTime<Utc>) -> Result<i64, E> {
    let due: Vec<(String, i64)> = sqlx::query_as(
        "SELECT l.id, l.amount_cents + COALESCE((SELECT SUM(s.amount_cents) FROM prepaid_ledger s WHERE s.lot = l.id), 0) \
         FROM prepaid_ledger l WHERE l.account = $1 AND l.bucket = 'free' AND l.amount_cents > 0 AND l.expires_at IS NOT NULL AND l.expires_at <= $2",
    ).bind(account).bind(stamp(now)).fetch_all(&mut **tx).await.map_err(internal)?;
    let mut total = 0;
    for (lot, remaining) in due {
        if remaining <= 0 { continue; }
        sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, lot, note) VALUES ($1, $2, 'expire', 'free', $3, $4, 'free credits expired')")
            .bind(id()).bind(account).bind(-remaining).bind(&lot).execute(&mut **tx).await.map_err(internal)?;
        total += remaining;
    }
    if total > 0 {
        tracing::info!(%account, expired = total, "free credits expired");
    }
    Ok(total)
}

/// The lots with something left, in consumption order: free (soonest
/// expiry first), then bonus, then paid (oldest first). `(id, bucket, remaining)`.
async fn open_lots(tx: &mut Tx<'_>, account: &str) -> Result<Vec<(String, String, i64)>, E> {
    sqlx::query_as(
        "SELECT l.id, l.bucket, l.amount_cents + COALESCE((SELECT SUM(s.amount_cents) FROM prepaid_ledger s WHERE s.lot = l.id), 0) AS remaining \
         FROM prepaid_ledger l WHERE l.account = $1 AND l.amount_cents > 0 AND remaining > 0 \
         ORDER BY CASE l.bucket WHEN 'free' THEN 0 WHEN 'bonus' THEN 1 ELSE 2 END, l.expires_at, l.created_at, l.rowid",
    ).bind(account).fetch_all(&mut **tx).await.map_err(internal)
}

// ------------------------------------------------------------------ welcome

/// $12 to the free bucket on the first verified registration of a phone —
/// one per WhatsApp number, ever, whatever account it ends up on. Expires
/// in [`FREE_DAYS`]. Returns whether it landed.
pub async fn welcome(db: &SqlitePool, account: &str, phone: &str) -> Result<bool, E> {
    welcome_at(db, account, phone, Utc::now()).await
}

pub async fn welcome_at(db: &SqlitePool, account: &str, phone: &str, now: DateTime<Utc>) -> Result<bool, E> {
    let phone: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if phone.is_empty() {
        return Ok(false);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let fresh = sqlx::query("INSERT OR IGNORE INTO prepaid_welcome (phone, account) VALUES ($1, $2)")
        .bind(&phone).bind(account).execute(&mut *tx).await.map_err(internal)?.rows_affected();
    if fresh == 0 {
        return Ok(false);
    }
    sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, expires_at, note, created_at) VALUES ($1, $2, 'grant', 'free', $3, $4, 'welcome', $5)")
        .bind(id()).bind(account).bind(WELCOME).bind(stamp(now + chrono::Duration::days(FREE_DAYS))).bind(stamp(now))
        .execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    tracing::info!(%account, cents = WELCOME, "welcome credits granted");
    Ok(true)
}

// ------------------------------------------------------------------ drawing

/// What a debit was for. An outcome charge fills this in; a consumption
/// debit (`meter.rs`) leaves it empty and says what it was in `note`.
#[derive(Default, Clone, Copy)]
pub(crate) struct Tag<'a> {
    pub outcome_id: Option<&'a str>,
    pub business: Option<&'a str>,
    pub client_hash: Option<&'a str>,
    pub is_new_client: Option<i64>,
}

/// Draws `amount` cents from the account's open lots in bucket order — free,
/// then bonus, then paid — writing one debit row per lot touched, with tax
/// recorded on the paid draws only. Whatever the lots cannot cover becomes a
/// single lot-less paid row: the overdraft that takes the balance negative,
/// down to [`GRACE`]. The caller owns the transaction, so reading the lots
/// and writing against them cannot interleave with another phone's charge.
pub(crate) async fn draw_in(tx: &mut Tx<'_>, account: &str, amount: i64, cfg: &CountryConfig, note: &str, meta: &str, tag: Tag<'_>, now: DateTime<Utc>) -> Result<(), E> {
    let mut left = amount;
    let mut draws: Vec<(Option<String>, &str, i64)> = Vec::new();
    for (lot, bucket, remaining) in open_lots(tx, account).await? {
        if left == 0 { break; }
        let take = remaining.min(left);
        let b: &str = match bucket.as_str() { "free" => "free", "bonus" => "bonus", _ => "paid" };
        draws.push((Some(lot), b, take));
        left -= take;
    }
    if left > 0 || amount == 0 {
        draws.push((None, "paid", left));
    }
    for (lot, bucket, take) in draws {
        let (rate, net) = if bucket == "paid" { (cfg.tax_rate, net_of(take, cfg.tax_rate)) } else { (0.0, take) };
        sqlx::query(
            "INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, lot, outcome_id, business, client_hash, is_new_client, tax_rate, net_cents, note, meta, created_at) \
             VALUES ($1, $2, 'debit', $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(id()).bind(account).bind(bucket).bind(-take).bind(lot)
        .bind(tag.outcome_id).bind(tag.business).bind(tag.client_hash).bind(tag.is_new_client)
        .bind(rate).bind(-net).bind(note).bind(meta).bind(stamp(now))
        .execute(&mut **tx).await.map_err(internal)?;
    }
    Ok(())
}

/// D19: outcome pricing is off — consumption is the meter now (`meter.rs`).
/// The code stays: `METER_OUTCOMES=1` brings per-booking charging back
/// without a migration, and phones in the field keep getting a 200 from
/// `/v1/outcomes/confirm` either way.
pub fn outcome_pricing() -> bool {
    matches!(std::env::var("METER_OUTCOMES").as_deref(), Ok("1") | Ok("true") | Ok("TRUE"))
}

// ------------------------------------------------------------------ outcomes

pub struct Confirmed {
    pub charged: i64,
    pub is_new: bool,
    pub balance: i64,
    pub duplicate: bool,
    pub cap_reached: bool,
    pub outcomes: i64,
    pub charged_month: i64,
    /// This charge took the account past the grace floor.
    pub entered_manual: bool,
}

/// One confirmed outcome. Idempotent by `outcome_id`; decides new vs known
/// by `client_hash` per business; applies volume and cap; draws the amount
/// from the buckets in order and writes one debit row per lot touched (tax
/// on the paid draws only). All in one `BEGIN IMMEDIATE` so two phones of
/// the same account cannot both see the 100th outcome.
pub async fn confirm_in(db: &SqlitePool, account: &str, cfg: &CountryConfig, business: &str, outcome_id: &str, client_hash: &str, kind: &str, customer: Option<&str>, charge: bool, now: DateTime<Utc>) -> Result<Confirmed, E> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let month = month_key(now);
    let (outcomes, charged): (i64, i64) = sqlx::query_as("SELECT outcomes, charged_cents FROM prepaid_months WHERE account = $1 AND month = $2")
        .bind(account).bind(&month).fetch_optional(&mut *tx).await.map_err(internal)?.unwrap_or((0, 0));
    let dup: Option<(Option<i64>, Option<i64>)> = sqlx::query_as("SELECT SUM(amount_cents), MAX(is_new_client) FROM prepaid_ledger WHERE account = $1 AND outcome_id = $2 AND kind = 'debit'")
        .bind(account).bind(outcome_id).fetch_optional(&mut *tx).await.map_err(internal)?;
    if let Some((Some(amount), is_new)) = dup {
        let balance = balance_in(&mut tx, account).await?;
        tx.commit().await.map_err(internal)?;
        return Ok(Confirmed { charged: -amount, is_new: is_new == Some(1), balance, duplicate: true, cap_reached: charged >= MONTHLY_CAP, outcomes, charged_month: charged, entered_manual: false });
    }
    let known = sqlx::query("INSERT OR IGNORE INTO prepaid_clients (account, business, client_hash) VALUES ($1, $2, $3)")
        .bind(account).bind(business).bind(client_hash).execute(&mut *tx).await.map_err(internal)?.rows_affected() == 0;
    let is_new = !known;
    let (amount, cap_reached) = if charge { price(outcomes, charged, is_new) } else { (0, false) };
    expire_due_in(&mut tx, account, now).await?;
    let before = balance_in(&mut tx, account).await?;
    let meta = json!({"customer": customer, "kind": kind, "month": month}).to_string();
    draw_in(&mut tx, account, amount, cfg, kind, &meta,
        Tag { outcome_id: Some(outcome_id), business: Some(business), client_hash: Some(client_hash), is_new_client: Some(is_new as i64) },
        now).await?;
    sqlx::query(
        "INSERT INTO prepaid_months (account, month, outcomes, charged_cents) VALUES ($1, $2, 1, $3) \
         ON CONFLICT (account, month) DO UPDATE SET outcomes = outcomes + 1, charged_cents = charged_cents + $3",
    ).bind(account).bind(&month).bind(amount).execute(&mut *tx).await.map_err(internal)?;
    let after = before - amount;
    tx.commit().await.map_err(internal)?;
    tracing::info!(%account, %business, %outcome_id, kind, is_new, amount, balance = after, "outcome charged");
    Ok(Confirmed {
        charged: amount, is_new, balance: after, duplicate: false, cap_reached,
        outcomes: outcomes + 1, charged_month: charged + amount,
        entered_manual: before >= GRACE && after < GRACE,
    })
}

/// Gives the charge of `outcome_id` back to the lots it drew from if it is
/// younger than 24 h. Idempotent: a second call answers `false`.
pub async fn reverse_in(db: &SqlitePool, account: &str, outcome_id: &str, now: DateTime<Utc>) -> Result<(bool, i64), E> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let debits: Vec<(String, i64, Option<String>, String, f64, Option<String>, String)> = sqlx::query_as(
        "SELECT bucket, amount_cents, lot, created_at, COALESCE(tax_rate, 0), meta, kind FROM prepaid_ledger WHERE account = $1 AND outcome_id = $2 ORDER BY rowid",
    ).bind(account).bind(outcome_id).fetch_all(&mut *tx).await.map_err(internal)?;
    if debits.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no charge for that outcome"));
    }
    if debits.iter().any(|d| d.6 == "reversal") {
        let b = balance_in(&mut tx, account).await?;
        tx.commit().await.map_err(internal)?;
        return Ok((false, b));
    }
    let created = &debits[0].3;
    let at = DateTime::parse_from_rfc3339(created).map(|t| t.with_timezone(&Utc)).unwrap_or(now);
    if now - at > chrono::Duration::hours(REVERSAL_HOURS) {
        return Err(err(StatusCode::CONFLICT, format!("charge is older than {REVERSAL_HOURS} h")));
    }
    let mut total = 0;
    for (bucket, amount, lot, _, rate, _, _) in &debits {
        let back = -amount; // debits are negative
        if back == 0 { continue; }
        total += back;
        sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, lot, outcome_id, tax_rate, net_cents, note, created_at) VALUES ($1, $2, 'reversal', $3, $4, $5, $6, $7, $8, 'cancelled within 24h', $9)")
            .bind(id()).bind(account).bind(bucket).bind(back).bind(lot).bind(outcome_id).bind(rate).bind(net_of(back, *rate)).bind(stamp(now))
            .execute(&mut *tx).await.map_err(internal)?;
    }
    if total == 0 {
        // A $0 charge (past the cap) still needs a reversal marker for idempotency.
        sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, outcome_id, note, created_at) VALUES ($1, $2, 'reversal', 'paid', 0, $3, 'cancelled within 24h', $4)")
            .bind(id()).bind(account).bind(outcome_id).bind(stamp(now)).execute(&mut *tx).await.map_err(internal)?;
    }
    let month = debits[0].5.as_deref().and_then(|m| serde_json::from_str::<Value>(m).ok()).and_then(|m| m["month"].as_str().map(String::from)).unwrap_or_else(|| month_key(at));
    sqlx::query("UPDATE prepaid_months SET outcomes = MAX(outcomes - 1, 0), charged_cents = MAX(charged_cents - $3, 0) WHERE account = $1 AND month = $2")
        .bind(account).bind(&month).bind(total).execute(&mut *tx).await.map_err(internal)?;
    let b = balance_in(&mut tx, account).await?;
    tx.commit().await.map_err(internal)?;
    tracing::info!(%account, %outcome_id, refund = total, balance = b, "outcome reversed");
    Ok((true, b))
}

// ------------------------------------------------------------------ money in

/// A top-up of one tier: a paid lot for what was paid and a bonus lot for
/// the tier's extra, both keyed to `external_id` so a replayed webhook or
/// a re-confirmed transfer never credits twice. Returns `(applied, balance)`.
pub async fn topup_in(db: &SqlitePool, account: &str, t: Tier, method: &str, external_id: &str, note: Option<&str>, meta: Option<Value>) -> Result<(bool, i64), E> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let meta_s = meta.map(|m| m.to_string());
    let n = sqlx::query("INSERT OR IGNORE INTO prepaid_ledger (id, account, kind, bucket, amount_cents, method, external_id, note, meta) VALUES ($1, $2, 'topup', 'paid', $3, $4, $5, $6, $7)")
        .bind(id()).bind(account).bind(t.pays).bind(method).bind(external_id).bind(note).bind(&meta_s)
        .execute(&mut *tx).await.map_err(internal)?.rows_affected();
    if n > 0 && t.bonus > 0 {
        sqlx::query("INSERT OR IGNORE INTO prepaid_ledger (id, account, kind, bucket, amount_cents, method, external_id, note, meta) VALUES ($1, $2, 'bonus', 'bonus', $3, $4, $5, $6, $7)")
            .bind(id()).bind(account).bind(t.bonus).bind(method).bind(external_id).bind(format!("bonus {}", t.id)).bind(&meta_s)
            .execute(&mut *tx).await.map_err(internal)?;
    }
    let b = balance_in(&mut tx, account).await?;
    if n > 0 && b >= 0 {
        // Back above water: the next dip into manual mode is a new episode.
        sqlx::query("DELETE FROM prepaid_handoffs WHERE account = $1").bind(account).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("DELETE FROM prepaid_dormancy WHERE account = $1 AND forfeited_at IS NULL").bind(account).execute(&mut *tx).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    if n > 0 {
        tracing::info!(%account, tier = t.id, pays = t.pays, bonus = t.bonus, method, %external_id, balance = b, "top-up credited");
    }
    Ok((n > 0, b))
}

/// A provider refund of a top-up: the paid lot goes back (all of it, or the
/// refunded share), and the bonus lot is voided in the same share — spent
/// or not, so the balance may go negative. Idempotent by `refund_id`.
pub async fn refund_in(db: &SqlitePool, refund_id: &str, payment_id: &str, refunded: Option<i64>, paid_total: Option<i64>) -> Result<Option<(String, i64, i64)>, E> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let lots: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, account, kind, amount_cents FROM prepaid_ledger WHERE external_id = $1 AND kind IN ('topup', 'bonus')",
    ).bind(payment_id).fetch_all(&mut *tx).await.map_err(internal)?;
    let Some((paid_lot, account, _, pays)) = lots.iter().find(|l| l.2 == "topup").cloned() else { return Ok(None) };
    let share = match (refunded, paid_total) {
        (Some(r), Some(t)) if t > 0 && r < t => (r as f64 / t as f64).clamp(0.0, 1.0),
        _ => 1.0,
    };
    let back = (pays as f64 * share).round() as i64;
    let n = sqlx::query("INSERT OR IGNORE INTO prepaid_ledger (id, account, kind, bucket, amount_cents, lot, method, external_id, note) VALUES ($1, $2, 'refund', 'paid', $3, $4, 'card', $5, 'refunded to the original method')")
        .bind(id()).bind(&account).bind(-back).bind(&paid_lot).bind(refund_id)
        .execute(&mut *tx).await.map_err(internal)?.rows_affected();
    if n == 0 {
        let b = balance_in(&mut tx, &account).await?;
        tx.commit().await.map_err(internal)?;
        return Ok(Some((account, 0, b)));
    }
    let mut voided = 0;
    if let Some((bonus_lot, _, _, bonus)) = lots.iter().find(|l| l.2 == "bonus") {
        voided = (*bonus as f64 * share).round() as i64;
        if voided > 0 {
            sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, lot, method, external_id, note) VALUES ($1, $2, 'void', 'bonus', $3, $4, 'card', $5, 'bonus voided: its top-up was refunded')")
                .bind(id()).bind(&account).bind(-voided).bind(bonus_lot).bind(format!("{refund_id}:void"))
                .execute(&mut *tx).await.map_err(internal)?;
        }
    }
    let b = balance_in(&mut tx, &account).await?;
    tx.commit().await.map_err(internal)?;
    tracing::info!(%account, %refund_id, back, voided, balance = b, "top-up refunded");
    Ok(Some((account, back + voided, b)))
}

// ------------------------------------------------------------------ dormancy

#[derive(Debug, PartialEq, Eq)]
pub enum Dormancy { Active, Warn, Forfeit, Waiting }

/// Pure rule: `last_activity` is the newest ledger row, `warned_at` the
/// warning already sent (if any).
pub fn dormancy_rule(last_activity: DateTime<Utc>, warned_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Dormancy {
    let dormant_since = last_activity + chrono::Duration::days(DORMANT_MONTHS * 30);
    match warned_at {
        Some(w) if last_activity > w => Dormancy::Active, // moved since the warning: the warning is moot
        Some(w) if now >= w + chrono::Duration::days(DORMANT_GRACE_DAYS) => Dormancy::Forfeit,
        Some(_) => Dormancy::Waiting,
        None if now >= dormant_since => Dormancy::Warn,
        None => Dormancy::Active,
    }
}

/// Forfeits every bucket with something left. Returns the total taken.
pub async fn forfeit_in(db: &SqlitePool, account: &str, now: DateTime<Utc>) -> Result<i64, E> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let rows: Vec<(String, Option<i64>)> = sqlx::query_as("SELECT bucket, SUM(amount_cents) FROM prepaid_ledger WHERE account = $1 GROUP BY bucket")
        .bind(account).fetch_all(&mut *tx).await.map_err(internal)?;
    let mut total = 0;
    for (bucket, sum) in rows {
        let s = sum.unwrap_or(0);
        if s <= 0 { continue; }
        sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, note, created_at) VALUES ($1, $2, 'forfeit', $3, $4, 'forfeited after 24 months of inactivity', $5)")
            .bind(id()).bind(account).bind(&bucket).bind(-s).bind(stamp(now)).execute(&mut *tx).await.map_err(internal)?;
        total += s;
    }
    sqlx::query("UPDATE prepaid_dormancy SET forfeited_at = $2 WHERE account = $1").bind(account).bind(stamp(now)).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    tracing::warn!(%account, total, "dormant balance forfeited");
    Ok(total)
}

/// Daily: warn the dormant, forfeit the warned-and-still-dormant.
pub async fn dormancy_sweep(app: &App) {
    let now = Utc::now();
    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT account, MAX(created_at), SUM(amount_cents) FROM prepaid_ledger GROUP BY account HAVING SUM(amount_cents) > 0",
    ).fetch_all(&app.db).await.unwrap_or_default();
    for (account, last, _) in rows {
        let Ok(last) = DateTime::parse_from_rfc3339(&last).map(|t| t.with_timezone(&Utc)) else { continue };
        let warned: Option<(String, Option<String>)> = sqlx::query_as("SELECT warned_at, forfeited_at FROM prepaid_dormancy WHERE account = $1").bind(&account).fetch_optional(&app.db).await.ok().flatten();
        if warned.as_ref().is_some_and(|w| w.1.is_some()) { continue; }
        let warned_at = warned.and_then(|w| DateTime::parse_from_rfc3339(&w.0).ok()).map(|t| t.with_timezone(&Utc));
        match dormancy_rule(last, warned_at, now) {
            Dormancy::Warn => {
                let _ = sqlx::query("INSERT OR REPLACE INTO prepaid_dormancy (account, warned_at, forfeited_at) VALUES ($1, $2, NULL)").bind(&account).bind(stamp(now)).execute(&app.db).await;
                if let Some(phone) = phone_of(app, &account).await {
                    let _ = app.otp.send_text(&phone, &format!(
                        "agente: tu cuenta lleva 24 meses sin movimientos. Si no hay actividad en 30 días, tus créditos vencen según los términos. Recarga o agenda algo para conservarlos: {}", topup_url())).await;
                }
            }
            Dormancy::Forfeit => { let _ = forfeit_in(&app.db, &account, now).await; }
            Dormancy::Active => { let _ = sqlx::query("DELETE FROM prepaid_dormancy WHERE account = $1 AND forfeited_at IS NULL").bind(&account).execute(&app.db).await; }
            Dormancy::Waiting => {}
        }
    }
}

async fn phone_of(app: &App, account: &str) -> Option<String> {
    sqlx::query_as::<_, (Option<String>,)>("SELECT phone FROM accounts WHERE id = $1").bind(account).fetch_optional(&app.db).await.ok().flatten().and_then(|r| r.0)
}

// ------------------------------------------------------------------ summary

pub fn topup_url() -> String {
    crate::env_or("TOPUP_URL", "https://agente.ceo/app/creditos")
}

fn methods_for(cfg: &CountryConfig, app: &App) -> Vec<&'static str> {
    let mut m = Vec::new();
    if app.prepaid_dodo.is_some() {
        m.push("card");
    }
    if cfg.iso == "PE" {
        m.push("yape");
    }
    if m.is_empty() {
        m.push("card");
    }
    m
}

pub fn tiers_json() -> Value {
    json!(TIERS.iter().map(|t| json!({"id": t.id, "pays": cents(t.pays), "credits": cents(t.credits()), "bonus": cents(t.bonus)})).collect::<Vec<_>>())
}

/// The whole picture for one account: what `/v1/credits` and the phone's
/// `/api/credits` render. `ledger` is the most recent `limit` rows.
pub async fn summary(app: &App, account: &str, limit: i64) -> Result<Value, E> {
    let db = &app.db;
    // Expire what is due before reporting.
    {
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
        expire_due_in(&mut tx, account, Utc::now()).await?;
        tx.commit().await.map_err(internal)?;
    }
    let country = account_country(db, account).await;
    let cfg = country_config(db, country.as_deref()).await;
    let bal = balance(db, account).await?;
    let (free, bonus, paid) = buckets(db, account).await?;
    let free_expires: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT MIN(l.expires_at) FROM prepaid_ledger l WHERE l.account = $1 AND l.bucket = 'free' AND l.amount_cents > 0 AND l.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') \
           AND l.amount_cents + COALESCE((SELECT SUM(s.amount_cents) FROM prepaid_ledger s WHERE s.lot = l.id), 0) > 0",
    ).bind(account).fetch_optional(db).await.map_err(internal)?;
    let free_expires = free_expires.and_then(|r| r.0).filter(|s| !s.is_empty());
    let month = month_key(Utc::now());
    let (outcomes, charged): (i64, i64) = sqlx::query_as("SELECT outcomes, charged_cents FROM prepaid_months WHERE account = $1 AND month = $2")
        .bind(account).bind(&month).fetch_optional(db).await.map_err(internal)?.unwrap_or((0, 0));
    let rows: Vec<(String, String, i64, Option<String>, Option<i64>, Option<String>, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT kind, bucket, amount_cents, outcome_id, is_new_client, method, note, meta, created_at FROM prepaid_ledger WHERE account = $1 ORDER BY created_at DESC, rowid DESC LIMIT $2",
    ).bind(account).bind(limit).fetch_all(db).await.map_err(internal)?;
    let terms: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT terms_version, terms_accepted_at FROM accounts WHERE id = $1")
        .bind(account).fetch_optional(db).await.map_err(internal)?;
    let dormancy: Option<(String, Option<String>)> = sqlx::query_as("SELECT warned_at, forfeited_at FROM prepaid_dormancy WHERE account = $1").bind(account).fetch_optional(db).await.map_err(internal)?;
    Ok(json!({
        "balance": cents(bal), "currency": CURRENCY, "state": state_of(bal), "grace": cents(GRACE),
        "buckets": {
            "free": {"balance": cents(free), "expiresAt": free_expires, "expiresDays": FREE_DAYS},
            "bonus": {"balance": cents(bonus)},
            "paid": {"balance": cents(paid)},
        },
        "prices": {"known": cents(PRICE_KNOWN), "new": cents(PRICE_NEW), "volumeAfter": VOLUME_AFTER,
                   "volumeKnown": cents(PRICE_KNOWN / 2), "volumeNew": cents(PRICE_NEW / 2), "monthlyCap": cents(MONTHLY_CAP)},
        "welcome": cents(WELCOME),
        "month": {"key": month, "outcomes": outcomes, "charged": cents(charged), "capReached": charged >= MONTHLY_CAP},
        "country": cfg.iso, "depositsEnabled": cfg.deposits_enabled, "taxRate": cfg.tax_rate, "displayCurrency": cfg.display_currency,
        "termsVersion": TERMS_VERSION,
        "terms": terms.map(|(v, at)| json!({"version": v, "acceptedAt": at})),
        "dormancy": dormancy.map(|(w, f)| json!({"warnedAt": w, "forfeitedAt": f, "graceDays": DORMANT_GRACE_DAYS})),
        "tiers": tiers_json(),
        "topup": {"presets": TIERS.iter().map(|t| t.pays / 100).collect::<Vec<_>>(), "selected": DEFAULT_TIER, "methods": methods_for(&cfg, app), "url": topup_url()},
        "ledger": rows.into_iter().map(|(kind, bucket, amount, outcome, is_new, method, note, meta, at)| {
            let m: Value = meta.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
            json!({
                "kind": kind, "bucket": bucket, "amount": cents(amount), "outcomeId": outcome, "isNewClient": is_new.map(|n| n == 1),
                "customer": m["customer"], "method": method, "note": note, "at": at,
                "ts": DateTime::parse_from_rfc3339(&at).map(|t| t.timestamp_millis()).unwrap_or(0),
            })
        }).collect::<Vec<_>>(),
    }))
}

// ------------------------------------------------------------------ routes

/// `GET /v1/credits` — agent (linked) or web session.
pub async fn credits(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    Ok(Json(summary(&app, &account, 50).await?).into_response())
}

#[derive(serde::Deserialize)]
pub struct ConfirmReq {
    #[serde(rename = "outcomeId")] outcome_id: String,
    business: String,
    #[serde(rename = "clientHash")] client_hash: String,
    #[serde(default)] kind: Option<String>,
    #[serde(default)] customer: Option<String>,
}

fn clean_id(s: &str, max: usize) -> Option<String> {
    let t: String = s.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')).take(max).collect();
    if t.is_empty() { None } else { Some(t) }
}

/// `POST /v1/outcomes/confirm` — the phone reports a confirmed outcome.
pub async fn outcomes_confirm(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<ConfirmReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let (Some(outcome_id), Some(business), Some(client_hash)) = (clean_id(&req.outcome_id, 80), clean_id(&req.business, 80), clean_id(&req.client_hash, 80)) else {
        return Err(err(StatusCode::BAD_REQUEST, "outcomeId, business and clientHash are required"));
    };
    let kind = match req.kind.as_deref() { Some("sale") => "sale", _ => "booking" };
    let customer = req.customer.as_deref().map(|c| c.chars().take(60).collect::<String>());
    let country = account_country(&app.db, &account).await;
    let cfg = country_config(&app.db, country.as_deref()).await;
    let c = confirm_in(&app.db, &account, &cfg, &business, &outcome_id, &client_hash, kind, customer.as_deref(), outcome_pricing(), Utc::now()).await?;
    if c.entered_manual {
        notify_handoff(&app, &account).await;
    }
    let state = state_of(c.balance);
    Ok(Json(json!({
        "charged": cents(c.charged), "isNewClient": c.is_new, "balance": cents(c.balance), "currency": CURRENCY,
        "state": state, "grace": cents(GRACE), "duplicate": c.duplicate,
        "action": if state == "manual" { Some("no_credits") } else { None },
        "topupUrl": topup_url(),
        "month": {"outcomes": c.outcomes, "charged": cents(c.charged_month), "capReached": c.cap_reached},
        "depositsEnabled": cfg.deposits_enabled,
    })).into_response())
}

/// One WhatsApp with the top-up link when the account crosses into manual
/// mode; not again until the balance recovers.
pub(crate) async fn notify_handoff(app: &App, account: &str) {
    let fresh = sqlx::query("INSERT OR IGNORE INTO prepaid_handoffs (account) VALUES ($1)").bind(account).execute(&app.db).await.map(|r| r.rows_affected()).unwrap_or(0);
    if fresh == 0 {
        return;
    }
    let Some(phone) = phone_of(app, account).await else { return };
    let text = format!(
        "agente: tu agente pasó a modo manual — se quedó sin créditos y ahora los clientes te escriben a ti. \
         Recarga para reactivarlo: {}",
        topup_url()
    );
    let sent = app.otp.send_text(&phone, &text).await;
    tracing::info!(%account, sent, "manual-mode notice");
}

#[derive(serde::Deserialize)]
pub struct ReverseReq { #[serde(rename = "outcomeId")] outcome_id: String }

/// `POST /v1/outcomes/reverse` — the owner cancelled or refunded within 24 h.
pub async fn outcomes_reverse(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<ReverseReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let Some(outcome_id) = clean_id(&req.outcome_id, 80) else { return Err(err(StatusCode::BAD_REQUEST, "outcomeId is required")) };
    let (reversed, b) = reverse_in(&app.db, &account, &outcome_id, Utc::now()).await?;
    Ok(Json(json!({"reversed": reversed, "balance": cents(b), "state": state_of(b)})).into_response())
}

fn read_json_file(var: &str, default_path: &str) -> Value {
    let path = crate::env_or(var, default_path);
    std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| {
        tracing::warn!(%path, "catalog file missing or invalid");
        json!({})
    })
}

/// `GET /v1/wallets` — the money-app catalog the phones refresh at launch.
pub async fn wallets() -> ApiResult {
    let mut r = Json(read_json_file("WALLETS_FILE", "wallets.json")).into_response();
    r.headers_mut().insert("cache-control", "public, max-age=3600".parse().unwrap());
    Ok(r)
}

/// `GET /v1/categories` — business categories and the prohibited list.
pub async fn categories() -> ApiResult {
    let mut r = Json(read_json_file("CATEGORIES_FILE", "categories.json")).into_response();
    r.headers_mut().insert("cache-control", "public, max-age=3600".parse().unwrap());
    Ok(r)
}

pub fn is_prohibited(category: &str) -> bool {
    let v = read_json_file("CATEGORIES_FILE", "categories.json");
    v["prohibited"].as_array().into_iter().flatten().any(|c| c["key"].as_str() == Some(category))
}

#[derive(serde::Deserialize)]
pub struct ProfileReq {
    #[serde(default)] category: Option<String>,
    #[serde(default, rename = "termsVersion")] terms_version: Option<String>,
    #[serde(default, rename = "termsAcceptedAt")] terms_accepted_at: Option<String>,
}

/// `POST /v1/account/profile` — category and terms acceptance from the
/// phone's registration. A prohibited category is refused (403) and
/// nothing is stored.
pub async fn profile(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<ProfileReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let category = req.category.as_deref().map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).map(|c| c.chars().take(40).collect::<String>());
    if let Some(c) = category.as_deref() {
        if is_prohibited(c) {
            return Err((StatusCode::FORBIDDEN, Json(json!({"error": {"message": "agente is not available for this type of business", "type": "prohibited"}}))));
        }
    }
    let version = req.terms_version.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(|v| v.chars().take(20).collect::<String>());
    let at = req.terms_accepted_at.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc).to_rfc3339())
        .or_else(|| version.as_ref().map(|_| Utc::now().to_rfc3339()));
    sqlx::query("UPDATE accounts SET category = COALESCE($2, category), terms_version = COALESCE($3, terms_version), terms_accepted_at = COALESCE($4, terms_accepted_at), updated_at = $5 WHERE id = $1")
        .bind(&account).bind(&category).bind(&version).bind(&at).bind(Utc::now().to_rfc3339()).execute(&app.db).await.map_err(internal)?;
    let country = account_country(&app.db, &account).await;
    Ok(Json(json!({"ok": true, "account": account, "category": category, "termsVersion": version, "termsAcceptedAt": at, "country": country})).into_response())
}

// ------------------------------------------------------------------ admin

#[derive(serde::Deserialize)]
pub struct AdminGrantReq {
    #[serde(default)] account: Option<String>,
    #[serde(default)] phone: Option<String>,
    #[serde(default)] kind: Option<String>,
    #[serde(default)] cents: Option<i64>,
    #[serde(default)] days: Option<i64>,
    #[serde(default)] note: Option<String>,
}

/// `POST /admin/prepaid` — ops: the welcome grant (once per phone, ever) or
/// a promo of `cents` to the free bucket, expiring in `days` (60 by default).
/// Used to migrate plan accounts and by `scripts/credits-e2e.sh`.
pub async fn admin_grant(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<AdminGrantReq>) -> ApiResult {
    let k = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !yaya_wire::secret::ct_eq(k, &app.admin_key) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad admin key"));
    }
    let phone = req.phone.as_deref().and_then(crate::otp::normalize_phone);
    let account = match (req.account.as_deref(), phone.as_deref()) {
        (Some(a), _) => a.to_string(),
        (None, Some(p)) => sqlx::query_as::<_, (String,)>("SELECT id FROM accounts WHERE phone = $1").bind(p).fetch_optional(&app.db).await.map_err(internal)?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "no account with that phone"))?.0,
        _ => return Err(err(StatusCode::BAD_REQUEST, "account or phone is required")),
    };
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT phone FROM accounts WHERE id = $1").bind(&account).fetch_optional(&app.db).await.map_err(internal)?;
    let acct_phone = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown account"))?.0;
    set_country_once(&app.db, &account, acct_phone.as_deref()).await;
    let applied = match req.kind.as_deref().unwrap_or("welcome") {
        "welcome" => welcome(&app.db, &account, acct_phone.as_deref().unwrap_or("")).await?,
        "promo" => {
            let cents_v = req.cents.filter(|c| *c > 0 && *c <= 100_000).ok_or_else(|| err(StatusCode::BAD_REQUEST, "cents (1..100000) is required for a promo"))?;
            let days = req.days.unwrap_or(FREE_DAYS).clamp(1, 365);
            sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, expires_at, note) VALUES ($1, $2, 'promo', 'free', $3, $4, $5)")
                .bind(id()).bind(&account).bind(cents_v).bind(stamp(Utc::now() + chrono::Duration::days(days))).bind(req.note.as_deref().unwrap_or("promo"))
                .execute(&app.db).await.map_err(internal)?;
            true
        }
        _ => return Err(err(StatusCode::BAD_REQUEST, "kind must be welcome or promo")),
    };
    Ok(Json(json!({"ok": true, "account": account, "applied": applied, "balance": cents(balance(&app.db, &account).await?)})).into_response())
}

// ------------------------------------------------------------------ top-ups

/// Dodo Payments, top-ups only: one hosted checkout per tier product
/// (`DODO_TIER_BASIC` / `_PLUS` / `_MAX`, created by
/// `scripts/dodo-products.sh`: one-time, USD, tax-inclusive).
pub struct DodoTopup {
    http: reqwest::Client,
    base: String,
    key: String,
    secret: String,
    products: std::collections::HashMap<&'static str, String>,
    return_url: String,
}

impl DodoTopup {
    #[cfg(test)]
    pub fn for_test(secret: &str, base: &str) -> Self {
        Self { http: reqwest::Client::new(), base: base.into(), key: "k".into(), secret: secret.into(), products: TIERS.iter().map(|t| (t.id, format!("prod_{}", t.id))).collect(), return_url: "https://ret".into() }
    }

    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let key = std::env::var("DODO_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
        let mut products = std::collections::HashMap::new();
        for t in TIERS {
            if let Ok(id) = std::env::var(format!("DODO_TIER_{}", t.id.to_ascii_uppercase())) {
                if !id.trim().is_empty() { products.insert(t.id, id.trim().to_string()); }
            }
        }
        if products.is_empty() {
            tracing::warn!("DODO_API_KEY set but no DODO_TIER_BASIC/PLUS/MAX product ids; card top-ups off");
            return None;
        }
        tracing::info!(tiers = ?products.keys().collect::<Vec<_>>(), "dodo top-ups on");
        Some(Self {
            http,
            base: crate::env_or("DODO_BASE_URL", "https://live.dodopayments.com").trim_end_matches('/').to_string(),
            key,
            secret: std::env::var("DODO_WEBHOOK_SECRET").unwrap_or_default(),
            products,
            return_url: crate::env_or("BILLING_RETURN_URL", "https://agente.ceo/app/creditos"),
        })
    }

    /// `POST /checkouts` → hosted URL. Card only; the customer may pay in
    /// their local currency (`allow_currency_selection`). Metadata carries
    /// the account and the tier so the webhook needs nothing else.
    pub async fn checkout_url(&self, t: Tier, email: &str, name: Option<&str>, country: Option<&str>, account: &str) -> anyhow::Result<String> {
        let product = self.products.get(t.id).ok_or_else(|| anyhow::anyhow!("no Dodo product for tier {}", t.id))?;
        let mut body = json!({
            "product_cart": [{"product_id": product, "quantity": 1}],
            "customer": {"email": email, "name": name.unwrap_or("")},
            "return_url": format!("{}?topup={}", self.return_url, t.id),
            "allowed_payment_method_types": ["credit", "debit", "apple_pay", "google_pay"],
            "feature_flags": {"allow_currency_selection": true, "allow_discount_code": false},
            "metadata": {"account_id": account, "tier": t.id, "amount_usd": (t.pays / 100).to_string(), "bonus_cents": t.bonus.to_string(), "product": "agente-credits"},
        });
        if let Some(c) = country.filter(|c| c.len() == 2) {
            body["billing_address"] = json!({"country": c});
        }
        let r = self.http.post(format!("{}/checkouts", self.base)).bearer_auth(&self.key).json(&body).send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        anyhow::ensure!(status.is_success(), "dodo {status}: {v}");
        v["checkout_url"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("no checkout_url: {v}"))
    }

    pub fn verify(&self, id: &str, ts: &str, signature: &str, body: &[u8]) -> bool {
        verify_standard_webhook(&self.secret, id, ts, signature, body)
    }
}

/// Standard Webhooks: `webhook-signature` = HMAC-SHA256(secret, "{id}.{ts}.{body}");
/// hex or base64 with a `v1,` prefix, several space-separated signatures allowed.
pub fn verify_standard_webhook(secret: &str, id: &str, ts: &str, signature: &str, body: &[u8]) -> bool {
    use hmac::{Hmac, Mac};
    if secret.is_empty() {
        return false;
    }
    let secret = secret.strip_prefix("whsec_").unwrap_or(secret);
    let key_bytes = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(secret).unwrap_or_else(|_| secret.as_bytes().to_vec())
    };
    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(&key_bytes) else { return false };
    mac.update(format!("{id}.{ts}.").as_bytes());
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let hex = hex::encode(digest);
    let b64 = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode(digest) };
    signature.split(' ').any(|s| {
        let s = s.trim();
        let s = s.strip_prefix("v1,").unwrap_or(s);
        yaya_wire::secret::ct_eq(s, &hex) || yaya_wire::secret::ct_eq(s, &b64)
    })
}

#[derive(serde::Deserialize)]
pub struct TopupReq { #[serde(default)] tier: Option<String>, #[serde(default)] amount: Option<i64>, #[serde(default)] method: Option<String> }

fn tier_of(req: &TopupReq) -> Result<Tier, E> {
    match (req.tier.as_deref(), req.amount) {
        (Some(t), _) => tier(t).ok_or_else(|| err(StatusCode::BAD_REQUEST, "tier must be basic, plus or max")),
        (None, Some(a)) => tier_for_amount(a).ok_or_else(|| err(StatusCode::BAD_REQUEST, "amount must be 10, 25 or 50")),
        _ => Err(err(StatusCode::BAD_REQUEST, "tier is required")),
    }
}

/// `POST /v1/topup/session` — a card checkout for one tier.
pub async fn topup_session(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<TopupReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let t = tier_of(&req)?;
    if req.method.as_deref().is_some_and(|m| m != "card") {
        return Err(err(StatusCode::BAD_REQUEST, "method must be card (use /v1/topup/yape for Yape/Plin)"));
    }
    let d = app.prepaid_dodo.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "card top-ups are not configured"))?;
    let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as("SELECT email, name, country FROM accounts WHERE id = $1").bind(&account).fetch_optional(&app.db).await.map_err(internal)?;
    let (email, name, country) = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "account gone"))?;
    let url = d.checkout_url(t, &email, name.as_deref(), country.as_deref(), &account).await
        .map_err(|e| { tracing::error!("dodo: {e}"); err(StatusCode::BAD_GATEWAY, "payment provider unavailable") })?;
    tracing::info!(%account, tier = t.id, "top-up checkout opened");
    Ok(Json(json!({"url": url, "tier": t.id, "pays": cents(t.pays), "credits": cents(t.credits()), "bonus": cents(t.bonus), "currency": CURRENCY, "provider": "dodo"})).into_response())
}

/// S/ per USD for Yape recargas (`PEN_PER_USD`, default 3.75).
pub fn pen_per_usd() -> f64 {
    std::env::var("PEN_PER_USD").ok().and_then(|v| v.parse().ok()).filter(|r: &f64| *r > 0.0).unwrap_or(3.75)
}

/// `POST /v1/topup/yape` — Perú only: the same tiers by Yape/Plin at the
/// day's rate (S/, IGV-inclusive), verified by notification through
/// yaya.cash; the paid and bonus lots land on confirmation.
pub async fn topup_yape(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<TopupReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let country = account_country(&app.db, &account).await;
    if country.as_deref() != Some("PE") {
        return Err(err(StatusCode::BAD_REQUEST, "Yape/Plin top-ups are available in Perú only"));
    }
    let t = tier_of(&req)?;
    let pen_minor = ((t.pays as f64) * pen_per_usd()).round() as i64;
    let mut v = crate::plans::open_usd_topup(&app, &accounts::subject(&account), t.id, pen_minor).await?;
    v["tier"] = json!(t.id);
    v["credits"] = json!(cents(t.credits()));
    v["bonus"] = json!(cents(t.bonus));
    Ok(Json(v).into_response())
}

/// `POST /v1/webhooks/dodo` — `payment.succeeded` credits the tier,
/// `refund.succeeded` takes it back (the balance may go negative). Signed
/// (Standard Webhooks), idempotent by the provider's payment/refund id.
pub async fn dodo_webhook(State(app): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> ApiResult {
    let d = app.prepaid_dodo.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "dodo not configured"))?;
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    // A signature proves who sent it, not when: an old event is a replay.
    let fresh = h("webhook-timestamp").trim().parse::<i64>().is_ok_and(|t| (Utc::now().timestamp() - t).abs() <= 300);
    if !fresh {
        return Err(err(StatusCode::UNAUTHORIZED, "webhook timestamp out of tolerance"));
    }
    if !d.verify(&h("webhook-id"), &h("webhook-timestamp"), &h("webhook-signature"), &body) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad webhook signature"));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| err(StatusCode::BAD_REQUEST, "bad json"))?;
    let applied = apply_dodo_event(&app, &v).await?;
    Ok(Json(json!({"ok": true, "applied": applied})).into_response())
}

/// The event → ledger mapping, separate from transport so tests can feed
/// events directly. Returns whether a new row was written.
pub async fn apply_dodo_event(app: &App, v: &Value) -> Result<bool, E> {
    let kind = v["type"].as_str().unwrap_or("");
    let data = &v["data"];
    let meta = &data["metadata"];
    match kind {
        "payment.succeeded" => {
            let Some(account) = meta["account_id"].as_str() else { tracing::warn!("dodo payment without account metadata"); return Ok(false) };
            let payment_id = data["payment_id"].as_str().or(data["id"].as_str()).unwrap_or("").to_string();
            if payment_id.is_empty() {
                return Err(err(StatusCode::BAD_REQUEST, "no payment id"));
            }
            let t = meta["tier"].as_str().and_then(tier)
                .or_else(|| meta["amount_usd"].as_str().and_then(|s| s.parse::<i64>().ok()).and_then(tier_for_amount))
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "tier metadata missing or unknown"))?;
            // What was paid must cover the tier, in USD: the charge itself when
            // it was in dollars, else the settlement Dodo reports in dollars.
            let usd_paid = if data["currency"].as_str().is_some_and(|c| c.eq_ignore_ascii_case("USD")) {
                data["total_amount"].as_i64()
            } else if data["settlement_currency"].as_str().is_some_and(|c| c.eq_ignore_ascii_case("USD")) {
                data["settlement_amount"].as_i64()
            } else {
                None
            };
            if let Some(p) = usd_paid {
                if p < t.pays {
                    tracing::warn!(%account, %payment_id, paid = p, tier = t.id, "dodo payment short of its tier; not credited");
                    return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "amount paid does not cover the tier"));
                }
            } else {
                tracing::warn!(%account, %payment_id, "dodo payment without a USD amount; credited on the tier metadata");
            }
            // Only accounts we know: a stray or replayed session must never mint a lot for nobody.
            let known: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE id = $1").bind(account).fetch_optional(&app.db).await.map_err(internal)?;
            if known.is_none() {
                tracing::warn!(%account, %payment_id, "dodo payment for an unknown account ignored");
                return Ok(false);
            }
            let (applied, b) = topup_in(&app.db, account, t, "card", &payment_id, Some("Dodo Payments"),
                Some(json!({"provider": "dodo", "paid": data["total_amount"], "currency": data["currency"], "settlement": data["settlement_amount"]}))).await?;
            if applied {
                confirm_by_whatsapp(app, account, t, b).await;
            }
            Ok(applied)
        }
        "refund.succeeded" | "refund.created" | "payment.refunded" => {
            let refund_id = data["refund_id"].as_str().or(data["id"].as_str()).unwrap_or("").to_string();
            let payment_id = data["payment_id"].as_str().unwrap_or("").to_string();
            if refund_id.is_empty() || payment_id.is_empty() {
                return Err(err(StatusCode::BAD_REQUEST, "refund needs refund_id and payment_id"));
            }
            let refunded = data["amount"].as_i64().or(data["refund_amount"].as_i64());
            let total = data["total_amount"].as_i64().or(data["payment_amount"].as_i64());
            // Only a partial refund (a smaller amount stated) is proportional.
            let partial = data["is_partial"].as_bool().unwrap_or(refunded.zip(total).is_some_and(|(r, t)| r < t));
            let r = refund_in(&app.db, &refund_id, &payment_id, if partial { refunded } else { None }, if partial { total } else { None }).await?;
            Ok(r.is_some_and(|(_, taken, _)| taken > 0))
        }
        _ => Ok(false),
    }
}

/// "Recarga confirmada" over WhatsApp, best effort.
async fn confirm_by_whatsapp(app: &App, account: &str, t: Tier, balance: i64) {
    let Some(phone) = phone_of(app, account).await else { return };
    let bonus = if t.bonus > 0 { format!(" (USD {:.2} + USD {:.2} de bonus)", cents(t.pays), cents(t.bonus)) } else { String::new() };
    let text = format!("agente: recarga confirmada, +USD {:.2}{bonus}. Saldo: USD {:.2}. ¡Gracias!", cents(t.credits()), cents(balance));
    let _ = app.otp.send_text(&phone, &text).await;
}

/// Credits a Yape/Plin recarga once yaya.cash (or an admin) confirmed it:
/// the tier's paid and bonus lots, the S/ amount (IGV-inclusive) on record.
pub async fn credit_yape(app: &App, account: &str, reference: &str, tier_id: &str, pen_minor: i64) -> Result<(bool, i64), E> {
    let t = tier(tier_id).ok_or_else(|| err(StatusCode::BAD_REQUEST, "unknown tier"))?;
    let r = topup_in(&app.db, account, t, "yape", reference, Some(&format!("recarga Yape/Plin S/ {:.2} (IGV incluido)", cents(pen_minor))),
        Some(json!({"penMinor": pen_minor, "igvInclusive": true, "penPerUsd": pen_per_usd()}))).await?;
    if r.0 {
        confirm_by_whatsapp(app, account, t, r.1).await;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query("INSERT INTO accounts (id, email, password_hash, phone, country) VALUES ('a', 'a@x', '', '51999000111', 'PE')").execute(&db).await.unwrap();
        db
    }

    fn pe() -> CountryConfig {
        CountryConfig { iso: "PE".into(), deposits_enabled: true, tax_rate: 18.0, display_currency: "PEN".into() }
    }

    async fn confirm(db: &SqlitePool, biz: &str, o: &str, c: &str, now: DateTime<Utc>) -> Confirmed {
        confirm_in(db, "a", &pe(), biz, o, c, "booking", None, true, now).await.unwrap()
    }

    #[test]
    fn countries_from_dial_codes() {
        assert_eq!(country_of_phone("51999000111"), Some("PE"));
        assert_eq!(country_of_phone("+55 11 99999 0000"), Some("BR"));
        assert_eq!(country_of_phone("18095550100"), Some("DO"));
        assert_eq!(country_of_phone("16505550100"), Some("US"));
        assert_eq!(country_of_phone("593991234567"), Some("EC"));
        assert_eq!(country_of_phone("34600000000"), Some("ES"));
    }

    #[test]
    fn prices_halve_after_100_and_stop_at_the_cap() {
        assert_eq!(price(0, 0, true), (200, false));
        assert_eq!(price(0, 0, false), (100, false));
        assert_eq!(price(99, 0, true), (200, false), "the 100th outcome is still full price");
        assert_eq!(price(100, 0, true), (100, false), "the 101st is half");
        assert_eq!(price(100, 0, false), (50, false));
        assert_eq!(price(500, 19_850, true), (50, true), "the cap trims the last charge");
        assert_eq!(price(500, 19_900, true), (0, true), "past the cap nothing is charged");
        assert_eq!(net_of(200, 18.0), 169);
        assert_eq!(net_of(100, 0.0), 100);
    }

    #[test]
    fn tiers_and_states() {
        assert_eq!(tier("plus").unwrap().credits(), 2_750);
        assert_eq!(tier("max").unwrap().credits(), 5_750);
        assert_eq!(tier_for_amount(10), Some(TIERS[0]));
        assert!(tier("gold").is_none());
        assert_eq!(state_of(1_200), "ok");
        assert_eq!(state_of(199), "low");
        assert_eq!(state_of(0), "low");
        assert_eq!(state_of(-1), "grace");
        assert_eq!(state_of(-400), "grace");
        assert_eq!(state_of(-401), "manual");
    }

    #[tokio::test]
    async fn new_vs_known_is_per_business_and_idempotent() {
        let db = db().await;
        assert!(welcome(&db, "a", "51999000111").await.unwrap());
        assert!(!welcome(&db, "a", "+51 999 000 111").await.unwrap(), "one grant per phone, ever");
        let now = Utc::now();
        let c1 = confirm(&db, "biz1", "o1", "h-maria", now).await;
        assert!(c1.is_new); assert_eq!(c1.charged, 200); assert_eq!(c1.balance, 1_000);
        let c2 = confirm_in(&db, "a", &pe(), "biz1", "o2", "h-maria", "sale", Some("María"), true, now).await.unwrap();
        assert!(!c2.is_new); assert_eq!(c2.charged, 100); assert_eq!(c2.balance, 900);
        let c3 = confirm(&db, "biz2", "o3", "h-maria", now).await;
        assert!(c3.is_new, "the same person is new to a second business of the account"); assert_eq!(c3.charged, 200);
        let again = confirm(&db, "biz1", "o1", "h-maria", now).await;
        assert!(again.duplicate); assert_eq!(again.balance, 700);
        // Drawn from the free bucket: no tax on free credits.
        let (rate, net): (f64, i64) = sqlx::query_as("SELECT tax_rate, net_cents FROM prepaid_ledger WHERE outcome_id = 'o1' AND kind = 'debit'").fetch_one(&db).await.unwrap();
        assert_eq!((rate, net), (0.0, -200));
        assert_eq!(buckets(&db, "a").await.unwrap(), (700, 0, 0));
    }

    #[tokio::test]
    async fn buckets_drain_free_then_bonus_then_paid_and_tax_only_on_paid() {
        let db = db().await;
        let now = Utc::now();
        sqlx::query("INSERT INTO prepaid_ledger (id, account, kind, bucket, amount_cents, expires_at) VALUES ('f', 'a', 'promo', 'free', 150, '2099-01-01T00:00:00.000Z')").execute(&db).await.unwrap();
        topup_in(&db, "a", tier("plus").unwrap(), "card", "pay_1", None, None).await.unwrap(); // paid 2500 + bonus 250
        assert_eq!(buckets(&db, "a").await.unwrap(), (150, 250, 2_500));
        // $2 new client: 150 free + 50 bonus.
        let c = confirm(&db, "b", "o1", "c1", now).await;
        assert_eq!(c.charged, 200);
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 200, 2_500));
        // $2 more: 200 bonus, then the first paid draw carries the tax.
        confirm(&db, "b", "o2", "c2", now).await;
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 0, 2_500));
        confirm(&db, "b", "o3", "c3", now).await;
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 0, 2_300));
        let rows: Vec<(String, i64, f64, i64)> = sqlx::query_as("SELECT bucket, amount_cents, COALESCE(tax_rate,0), net_cents FROM prepaid_ledger WHERE outcome_id = 'o3' AND kind = 'debit'").fetch_all(&db).await.unwrap();
        assert_eq!(rows, vec![("paid".to_string(), -200, 18.0, -169)]);
        let rows: Vec<(String, f64)> = sqlx::query_as("SELECT bucket, COALESCE(tax_rate,0) FROM prepaid_ledger WHERE outcome_id = 'o1' AND kind = 'debit' ORDER BY rowid").fetch_all(&db).await.unwrap();
        assert_eq!(rows, vec![("free".to_string(), 0.0), ("bonus".to_string(), 0.0)]);
    }

    #[tokio::test]
    async fn free_credits_expire_after_60_days() {
        let db = db().await;
        let t0 = Utc::now();
        welcome_at(&db, "a", "51999000111", t0).await.unwrap();
        confirm(&db, "b", "o1", "c1", t0).await; // 1000 left
        let later = t0 + chrono::Duration::days(FREE_DAYS + 1);
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert_eq!(expire_due_in(&mut tx, "a", later).await.unwrap(), 1_000, "only the unspent remainder expires");
        assert_eq!(expire_due_in(&mut tx, "a", later).await.unwrap(), 0, "idempotent");
        tx.commit().await.unwrap();
        assert_eq!(balance(&db, "a").await.unwrap(), 0);
        // A charge after expiry goes straight to the overdraft.
        let c = confirm(&db, "b", "o2", "c2", later).await;
        assert_eq!(c.balance, -200);
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 0, -200));
    }

    #[tokio::test]
    async fn volume_halves_at_101_and_cap_holds_at_199() {
        let db = db().await;
        let now = Utc::now();
        topup_in(&db, "a", Tier { id: "seed", pays: 100_000, bonus: 0 }, "card", "seed", None, None).await.unwrap();
        // 100 known-client outcomes at $1: the first client is new, then known.
        confirm(&db, "b", "first", "c0", now).await; // 200, the only new one
        for i in 1..100 {
            let c = confirm(&db, "b", &format!("o{i}"), "c0", now).await;
            assert_eq!(c.charged, 100, "outcome {} is full price", i + 1);
        }
        assert_eq!(balance(&db, "a").await.unwrap(), 100_000 - 200 - 9_900);
        // The 101st: prices halve — a new client is $1, a known one $0.50.
        let c = confirm(&db, "b", "n101", "c101", now).await;
        assert_eq!((c.is_new, c.charged), (true, 100), "the 101st new client costs $1");
        let c = confirm(&db, "b", "k102", "c0", now).await;
        assert_eq!((c.is_new, c.charged), (false, 50), "a known client after 100 costs $0.50");
        assert!(!c.cap_reached);
        // Charged so far: 200 + 9 900 + 100 + 50 = 10 250. Room to the cap: 9 650 = 96 new + 50.
        for i in 0..96 {
            let c = confirm(&db, "b", &format!("m{i}"), &format!("d{i}"), now).await;
            assert_eq!(c.charged, 100);
        }
        let c = confirm(&db, "b", "last", "dlast", now).await;
        assert_eq!(c.charged, 50, "the cap trims the last charge");
        assert!(c.cap_reached);
        let (charged,): (i64,) = sqlx::query_as("SELECT charged_cents FROM prepaid_months WHERE account = 'a'").fetch_one(&db).await.unwrap();
        assert_eq!(charged, MONTHLY_CAP);
        let c = confirm(&db, "b", "free1", "dfree", now).await;
        assert_eq!(c.charged, 0, "past the cap outcomes are free but still counted");
        assert_eq!(c.outcomes, 100 + 2 + 96 + 1 + 1);
        assert_eq!(balance(&db, "a").await.unwrap(), 100_000 - MONTHLY_CAP);
    }

    #[tokio::test]
    async fn reversal_within_24h_returns_to_the_lots() {
        let db = db().await;
        let t0 = Utc::now();
        welcome_at(&db, "a", "51999000111", t0).await.unwrap();
        confirm(&db, "b", "o1", "c1", t0).await;
        let (ok, b) = reverse_in(&db, "a", "o1", t0 + chrono::Duration::hours(2)).await.unwrap();
        assert!(ok); assert_eq!(b, 1_200);
        assert_eq!(buckets(&db, "a").await.unwrap(), (1_200, 0, 0), "back into the free lot");
        let (ok, b) = reverse_in(&db, "a", "o1", t0 + chrono::Duration::hours(3)).await.unwrap();
        assert!(!ok, "a second reversal is a no-op"); assert_eq!(b, 1_200);
        let (outcomes,): (i64,) = sqlx::query_as("SELECT outcomes FROM prepaid_months WHERE account = 'a'").fetch_one(&db).await.unwrap();
        assert_eq!(outcomes, 0, "the month gives the outcome back");
        confirm(&db, "b", "o2", "c1", t0).await;
        sqlx::query("UPDATE prepaid_ledger SET created_at = '2020-01-01T00:00:00.000Z' WHERE outcome_id = 'o2'").execute(&db).await.unwrap();
        let e = reverse_in(&db, "a", "o2", t0).await.err().expect("must refuse");
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert!(reverse_in(&db, "a", "nope", t0).await.is_err());
    }

    #[tokio::test]
    async fn grace_then_handoff_then_recovery() {
        let db = db().await;
        welcome(&db, "a", "51999000111").await.unwrap(); // 12.00
        let now = Utc::now();
        for i in 0..6 {
            let c = confirm(&db, "b", &format!("n{i}"), &format!("c{i}"), now).await;
            assert!(!c.entered_manual);
        }
        assert_eq!(balance(&db, "a").await.unwrap(), 0);
        confirm(&db, "b", "n6", "c6", now).await;
        let c = confirm(&db, "b", "n7", "c7", now).await;
        assert_eq!(c.balance, -400); assert_eq!(state_of(c.balance), "grace"); assert!(!c.entered_manual, "still serving at the floor");
        let c = confirm(&db, "b", "k1", "c0", now).await;
        assert_eq!(c.balance, -500); assert_eq!(state_of(c.balance), "manual"); assert!(c.entered_manual, "this charge is the transition");
        let c = confirm(&db, "b", "k2", "c1", now).await;
        assert!(!c.entered_manual, "already manual: no second transition");
        let (applied, b) = topup_in(&db, "a", tier("plus").unwrap(), "card", "pay_1", None, None).await.unwrap();
        assert!(applied); assert_eq!(b, -600 + 2_750); assert_eq!(state_of(b), "ok");
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 250, 2_500 - 600), "the overdraft is netted against paid");
    }

    #[tokio::test]
    async fn topups_are_idempotent_and_refunds_void_the_bonus() {
        let db = db().await;
        let (a1, b1) = topup_in(&db, "a", tier("max").unwrap(), "card", "pay_9", None, None).await.unwrap();
        let (a2, b2) = topup_in(&db, "a", tier("max").unwrap(), "card", "pay_9", None, None).await.unwrap();
        assert!(a1 && !a2); assert_eq!(b1, 5_750); assert_eq!(b2, 5_750, "the same payment id never credits twice");
        // Spend the whole bonus and some paid: 4 new clients = 800 (750 bonus + 50 paid).
        for i in 0..4 { confirm(&db, "b", &format!("o{i}"), &format!("c{i}"), Utc::now()).await; }
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 0, 4_950));
        // Full refund: the paid lot goes back entirely, the bonus is voided
        // although it was spent — the balance goes negative.
        let r = refund_in(&db, "ref_9", "pay_9", None, None).await.unwrap().unwrap();
        assert_eq!(r.1, 5_750);
        assert_eq!(balance(&db, "a").await.unwrap(), -800);
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, -750, -50));
        let r2 = refund_in(&db, "ref_9", "pay_9", None, None).await.unwrap().unwrap();
        assert_eq!(r2.1, 0, "replayed refund is a no-op");
        assert!(refund_in(&db, "ref_x", "pay_unknown", None, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn partial_refund_is_proportional() {
        let db = db().await;
        topup_in(&db, "a", tier("plus").unwrap(), "card", "pay_2", None, None).await.unwrap(); // 2500 + 250
        let r = refund_in(&db, "ref_2", "pay_2", Some(1_000), Some(2_500)).await.unwrap().unwrap();
        assert_eq!(r.1, 1_000 + 100);
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 150, 1_500));
    }

    #[test]
    fn dormancy_rule_warns_then_forfeits() {
        let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let day = chrono::Duration::days;
        assert_eq!(dormancy_rule(t0, None, t0 + day(700)), Dormancy::Active);
        assert_eq!(dormancy_rule(t0, None, t0 + day(721)), Dormancy::Warn);
        let warned = t0 + day(721);
        assert_eq!(dormancy_rule(t0, Some(warned), warned + day(10)), Dormancy::Waiting);
        assert_eq!(dormancy_rule(t0, Some(warned), warned + day(30)), Dormancy::Forfeit);
        assert_eq!(dormancy_rule(warned + day(5), Some(warned), warned + day(40)), Dormancy::Active, "a movement after the warning cancels it");
    }

    #[tokio::test]
    async fn forfeit_zeroes_every_bucket() {
        let db = db().await;
        welcome(&db, "a", "51999000111").await.unwrap();
        topup_in(&db, "a", tier("plus").unwrap(), "card", "pay_3", None, None).await.unwrap();
        sqlx::query("INSERT INTO prepaid_dormancy (account, warned_at) VALUES ('a', '2020-01-01T00:00:00.000Z')").execute(&db).await.unwrap();
        assert_eq!(forfeit_in(&db, "a", Utc::now()).await.unwrap(), 1_200 + 2_750);
        assert_eq!(balance(&db, "a").await.unwrap(), 0);
        assert_eq!(buckets(&db, "a").await.unwrap(), (0, 0, 0));
    }

    #[test]
    fn webhook_signature_hex_and_base64() {
        let secret = "whsec_c2VjcmV0"; // base64("secret")
        let body = br#"{"type":"payment.succeeded"}"#;
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"msg_1.1700000000.");
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let hex = hex::encode(digest);
        let b64 = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode(digest) };
        assert!(verify_standard_webhook(secret, "msg_1", "1700000000", &hex, body));
        assert!(verify_standard_webhook(secret, "msg_1", "1700000000", &format!("v1,{b64}"), body));
        assert!(!verify_standard_webhook(secret, "msg_2", "1700000000", &hex, body));
        assert!(!verify_standard_webhook("", "msg_1", "1700000000", &hex, body));
    }

    use chrono::TimeZone;
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::testkit::{self, as_admin, as_agent, as_session, Keypair};

    async fn setup() -> (Shared, Keypair) {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        testkit::account_with_agent(&app, "acct-1", "51977000111", &kp).await;
        welcome(&app.db, "acct-1", "51977000111").await.unwrap();
        (app, kp)
    }

    fn confirm_body(o: &str, c: &str) -> Value {
        json!({"outcomeId": o, "business": "biz1", "clientHash": c, "kind": "booking", "customer": "Ana"})
    }

    #[tokio::test]
    async fn outcomes_are_recorded_for_the_callers_account_free_under_d19() {
        // D19: outcome pricing is off unless METER_OUTCOMES=1 (the charging
        // arithmetic is covered by the unit tests above, through confirm_in).
        let (app, kp) = setup().await;
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/outcomes/confirm", Some(confirm_body("o1", "h1"))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!((v["isNewClient"].clone(), v["duplicate"].clone(), v["charged"].clone(), v["currency"].clone()), (json!(true), json!(false), json!(0.0), json!(CURRENCY)));
        let (_, again) = as_agent(&app, &kp, "POST", "/v1/outcomes/confirm", Some(confirm_body("o1", "h1"))).await;
        assert_eq!((again["duplicate"].clone(), again["balance"].clone()), (json!(true), v["balance"].clone()));
        // Only the account that recorded it can reverse it (a no-op in money).
        let other = Keypair::generate();
        testkit::account_with_agent(&app, "acct-2", "51977000222", &other).await;
        assert_eq!(as_agent(&app, &other, "POST", "/v1/outcomes/reverse", Some(json!({"outcomeId": "o1"}))).await.0, 404);
        let (st, r) = as_agent(&app, &kp, "POST", "/v1/outcomes/reverse", Some(json!({"outcomeId": "o1"}))).await;
        assert_eq!((st, r["balance"].clone()), (200, v["balance"].clone()));
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/outcomes/reverse", Some(json!({"outcomeId": " "}))).await.0, 400);
        // Validation and auth.
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/outcomes/confirm", Some(json!({"outcomeId": "", "business": "b", "clientHash": "c"}))).await.0, 400);
        let stranger = Keypair::generate();
        assert_eq!(as_agent(&app, &stranger, "POST", "/v1/outcomes/confirm", Some(confirm_body("o2", "h"))).await.0, 401, "unlinked agents are nobody's account");
        assert_eq!(testkit::anon(&app, "POST", "/v1/outcomes/confirm", Some(confirm_body("o3", "h"))).await.0, 401);
        assert!(!outcome_pricing());
    }

    #[tokio::test]
    async fn the_summary_is_the_same_for_the_phone_and_the_web() {
        let (app, kp) = setup().await;
        as_agent(&app, &kp, "POST", "/v1/outcomes/confirm", Some(confirm_body("o1", "h1"))).await;
        let (st, phone) = as_agent(&app, &kp, "GET", "/v1/credits", None).await;
        assert_eq!(st, 200, "{phone}");
        let token = testkit::session_for(&app, "acct-1").await;
        let (_, web) = as_session(&app, &token, "GET", "/v1/credits", None).await;
        assert_eq!(phone["balance"], web["balance"]);
        assert!(phone["buckets"].is_object() || phone["buckets"].is_array());
        assert!(phone["tiers"].is_array());
    }

    #[tokio::test]
    async fn profiles_refuse_prohibited_categories() {
        let (app, kp) = setup().await;
        let dir = testkit::scratch("cats");
        let f = dir.join("categories.json");
        std::fs::write(&f, r#"{"prohibited": [{"key": "armas"}]}"#).unwrap();
        std::env::set_var("CATEGORIES_FILE", &f);
        assert!(is_prohibited("armas") && !is_prohibited("barberia"));
        let (st, _) = as_agent(&app, &kp, "POST", "/v1/account/profile", Some(json!({"category": " ARMAS "}))).await;
        assert_eq!(st, 403);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/account/profile", Some(json!({"category": "Barberia", "termsVersion": "v2"}))).await;
        assert_eq!((st, v["category"].clone(), v["termsVersion"].clone()), (200, json!("barberia"), json!("v2")));
        assert!(v["termsAcceptedAt"].is_string(), "accepting a version stamps the time");
        let (_, cats) = testkit::anon(&app, "GET", "/v1/categories", None).await;
        assert_eq!(cats["prohibited"][0]["key"], "armas");
        std::env::remove_var("CATEGORIES_FILE");
    }

    #[tokio::test]
    async fn admin_grants_need_the_admin_key() {
        let (app, _) = setup().await;
        assert_eq!(testkit::anon(&app, "POST", "/admin/prepaid", Some(json!({"account": "acct-1", "kind": "promo", "cents": 500}))).await.0, 401);
        let before = balance(&app.db, "acct-1").await.unwrap();
        let (st, v) = as_admin(&app, "POST", "/admin/prepaid", Some(json!({"account": "acct-1", "kind": "promo", "cents": 500}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), before + 500);
    }

    #[test]
    fn small_helpers() {
        assert_eq!(clean_id(" ab:c.d-e_f/../x ", 80).as_deref(), Some("ab:c.d-e_f..x"));
        assert_eq!(clean_id("///", 80), None);
        assert_eq!(clean_id(&"a".repeat(100), 80).unwrap().len(), 80);
        assert!(pen_per_usd() > 1.0);
        assert!(!topup_url().is_empty());
        assert!(tiers_json().is_array());
    }
}

#[cfg(test)]
mod topup_tests {
    use super::*;
    use crate::testkit::{self, as_agent, Keypair, Mock};

    const SECRET: &str = "whsec_dGVzdHNlY3JldA==";

    fn sign(id: &str, ts: i64, body: &[u8]) -> String {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        let key = base64::engine::general_purpose::STANDARD.decode("dGVzdHNlY3JldA==").unwrap();
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
        mac.update(format!("{id}.{ts}.").as_bytes());
        mac.update(body);
        format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
    }

    async fn post(app: &Shared, id: &str, ts: i64, body: Value) -> (u16, Value) {
        let b = serde_json::to_vec(&body).unwrap();
        let h = vec![("webhook-id", id.to_string()), ("webhook-timestamp", ts.to_string()), ("webhook-signature", sign(id, ts, &b)), ("content-type", "application/json".into())];
        testkit::call(app, "POST", "/v1/webhooks/dodo", &h, Some(b)).await
    }

    async fn setup() -> (Mock, Shared, Keypair) {
        let m = Mock::start().await;
        let base = m.base.clone();
        let app = testkit::app_with(move |a| crate::App { prepaid_dodo: Some(DodoTopup::for_test(SECRET, &base)), ..a }).await;
        let kp = Keypair::generate();
        testkit::account_with_agent(&app, "acct-1", "51977000111", &kp).await;
        (m, app, kp)
    }

    fn paid(payment: &str, tier: &str, amount: Option<i64>) -> Value {
        let mut d = json!({"payment_id": payment, "currency": "USD", "metadata": {"account_id": "acct-1", "tier": tier}});
        if let Some(a) = amount { d["total_amount"] = json!(a); }
        json!({"type": "payment.succeeded", "data": d})
    }

    #[tokio::test]
    async fn a_card_top_up_credits_its_tier_once() {
        let (_, app, _) = setup().await;
        let now = Utc::now().timestamp();
        let (st, v) = post(&app, "e1", now, paid("pay_1", "plus", Some(2500))).await;
        assert_eq!((st, v["applied"].clone()), (200, json!(true)), "{v}");
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 2750, "USD 25 + 2.50 bonus");
        assert_eq!(post(&app, "e2", now, paid("pay_1", "plus", Some(2500))).await.1["applied"], false, "same payment, new event id");
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 2750);
        // A refund takes it back, bonus included.
        let r = json!({"type": "refund.succeeded", "data": {"refund_id": "re_1", "payment_id": "pay_1"}});
        assert_eq!(post(&app, "e3", now, r).await.1["applied"], true);
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn stale_or_badly_signed_webhooks_are_refused() {
        let (_, app, _) = setup().await;
        let now = Utc::now().timestamp();
        let (st, _) = post(&app, "old", now - 3600, paid("pay_old", "max", Some(5000))).await;
        assert_eq!(st, 401, "a signed event an hour old is a replay");
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 0);
        let b = serde_json::to_vec(&paid("pay_x", "max", Some(5000))).unwrap();
        let h = vec![("webhook-id", "e".to_string()), ("webhook-timestamp", now.to_string()), ("webhook-signature", "v1,AAAA".into())];
        assert_eq!(testkit::call(&app, "POST", "/v1/webhooks/dodo", &h, Some(b)).await.0, 401);
    }

    #[tokio::test]
    async fn the_amount_paid_must_cover_the_tier() {
        let (_, app, _) = setup().await;
        let now = Utc::now().timestamp();
        let (_, v) = post(&app, "e1", now, paid("pay_cheap", "max", Some(1000))).await;
        assert_ne!(v["applied"], true, "USD 10 paid never mints the USD 57.50 tier: {v}");
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 0);
        // Paid in soles (currency selection): the USD settlement is what counts.
        let mut ev = paid("pay_pen_short", "plus", Some(3750));
        ev["data"]["currency"] = json!("PEN");
        ev["data"]["settlement_amount"] = json!(1000);
        ev["data"]["settlement_currency"] = json!("USD");
        post(&app, "e2", now, ev).await;
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 0, "USD 10 settled does not buy USD 25");
        let mut ev = paid("pay_pen_ok", "basic", Some(3750));
        ev["data"]["currency"] = json!("PEN");
        ev["data"]["settlement_amount"] = json!(1000);
        ev["data"]["settlement_currency"] = json!("USD");
        post(&app, "e3", now, ev).await;
        assert_eq!(balance(&app.db, "acct-1").await.unwrap(), 1000);
    }

    #[tokio::test]
    async fn events_for_nobody_or_nothing() {
        let (_, app, _) = setup().await;
        let now = Utc::now().timestamp();
        let mut ev = paid("pay_n", "plus", Some(2500));
        ev["data"]["metadata"]["account_id"] = json!("acct-ghost");
        assert_eq!(post(&app, "e1", now, ev).await.1["applied"], false);
        let mut ev = paid("", "plus", Some(2500));
        ev["data"]["payment_id"] = json!("");
        assert_eq!(post(&app, "e2", now, ev).await.0, 400);
        let mut ev = paid("pay_t", "gold", Some(2500));
        ev["data"]["metadata"]["tier"] = json!("gold");
        assert_eq!(post(&app, "e3", now, ev).await.0, 400);
        assert_eq!(post(&app, "e4", now, json!({"type": "dispute.opened", "data": {}})).await.1["applied"], false);
        let off = testkit::app().await;
        assert_eq!(testkit::call(&off, "POST", "/v1/webhooks/dodo", &[], Some(b"{}".to_vec())).await.0, 503);
    }

    #[tokio::test]
    async fn top_up_sessions_and_yape_references() {
        let (m, app, kp) = setup().await;
        m.on("/checkouts", json!({"checkout_url": "https://pay.dodo/xyz"}));
        m.on("/checkout", json!({"checkout_url": "https://pay.dodo/xyz"}));
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/topup/session", Some(json!({"amount": 25}))).await;
        assert!(st == 200 || st == 502, "{st} {v}");
        if st == 200 {
            assert_eq!((v["tier"].clone(), v["credits"].clone()), (json!("plus"), json!(27.5)));
        }
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/topup/session", Some(json!({"tier": "gold"}))).await.0, 400);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/topup/session", Some(json!({"amount": 7}))).await.0, 400);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/topup/session", Some(json!({}))).await.0, 400);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/topup/session", Some(json!({"tier": "basic", "method": "yape"}))).await.0, 400);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/topup/yape", Some(json!({"tier": "basic"}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["tier"], "basic");
        // Outside Perú there is no Yape.
        sqlx::query("UPDATE accounts SET country = 'MX'").execute(&app.db).await.unwrap();
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/topup/yape", Some(json!({"tier": "basic"}))).await.0, 400);
        // Without a Dodo key, card is off.
        let off = testkit::app().await;
        let k2 = Keypair::generate();
        testkit::account_with_agent(&off, "acct-9", "51977000999", &k2).await;
        assert_eq!(as_agent(&off, &k2, "POST", "/v1/topup/session", Some(json!({"tier": "basic"}))).await.0, 503);
    }

    #[tokio::test]
    async fn yape_credits_land_once_per_reference() {
        let (_, app, _) = setup().await;
        assert_eq!(credit_yape(&app, "acct-1", "YAYA-R-1", "basic", 3750).await.unwrap(), (true, 1000));
        assert_eq!(credit_yape(&app, "acct-1", "YAYA-R-1", "basic", 3750).await.unwrap(), (false, 1000));
        assert!(credit_yape(&app, "acct-1", "YAYA-R-2", "gold", 1).await.is_err());
    }
}
