//! Plans: the one place both apps learn what they are allowed to do.
//!
//! Tiers are the same three words everywhere — free, pro, max — and the
//! gateway is the source of truth. A plan belongs to a *subject*: the Yaya
//! account (`acct:<id>`, the normal case — every device of the account runs
//! under it) or, for unlinked legacy agents and admin overrides, the agent
//! id itself. Plans are bought on the web only (Yape/Plin through
//! yaya.cash, or a reference sales confirms); phones read the result from
//! `/v1/me`.

use axum::{extract::Query, http::HeaderMap, http::StatusCode, response::IntoResponse, Json};
use serde_json::{json, Value};

use crate::{err, internal, ApiResult, App};

#[derive(Clone, Copy)]
pub struct Tier {
    pub name: &'static str,
    /// Gateway calls per UTC day (LLM/vision/audio). 0 = unlimited.
    pub calls: i64,
    /// Business receptionist: customer messages per local day. 0 = unlimited.
    pub messages: i64,
    /// Business receptionist: NEW customers per local day. 0 = unlimited.
    pub customers: i64,
    /// D14: customer conversations per calendar month — one peer the agent
    /// answered counts once a month. 0 = unlimited. This is THE metric.
    pub conversations: i64,
    /// Agents (phones/nodes) the account may run at once. 0 = unlimited.
    pub agents: i64,
}

/// D14 (2026-08-30): three words. `free` is what an account is after its
/// trial: the agent keeps answering, 30 conversations a month, one phone.
pub const FREE: Tier = Tier { name: "free", calls: 600, messages: 0, customers: 0, conversations: 30, agents: 1 };
/// Every new account starts here: Pro for `TRIAL_DAYS` (14).
pub const TRIAL: Tier = Tier { name: "trial", calls: 4000, messages: 0, customers: 0, conversations: 1000, agents: 1 };
/// Pro, S/150: 1 000 conversations a month, one phone.
pub const PRO: Tier = Tier { name: "pro", calls: 4000, messages: 0, customers: 0, conversations: 1000, agents: 1 };
/// Max, S/300: 3 000 conversations a month, up to three phones on one account.
pub const MAX: Tier = Tier { name: "max", calls: 20000, messages: 0, customers: 0, conversations: 3000, agents: 3 };

/// No tier is quoted any more (D14); kept so older call sites read naturally.
pub fn quoted(_name: &str) -> bool {
    false
}

impl Tier {
    /// Media (STT/TTS/vision) calls per day: a quarter of the chat cap, with
    /// the free tier's set explicitly. 0 = unlimited.
    pub fn media_cap(&self, app: &App) -> i64 {
        match self.name {
            "free" => app.free_media_cap,
            _ => self.calls / 4,
        }
    }
}

pub fn tier(name: &str) -> Tier {
    match name {
        "trial" => TRIAL,
        "pro" => PRO,
        // Legacy rows sold before D14 keep Max's entitlements.
        "max" | "custom" | "enterprise" => MAX,
        _ => FREE,
    }
}

/// Length of the welcome trial (`TRIAL_DAYS`, default 14).
pub fn trial_days() -> i64 {
    std::env::var("TRIAL_DAYS").ok().and_then(|v| v.parse().ok()).filter(|d| (1..=90).contains(d)).unwrap_or(14)
}

/// Starts the trial for a brand-new account. Never overwrites a plan.
pub async fn start_trial(app: &App, subject: &str) {
    let expires = (chrono::Utc::now() + chrono::Duration::days(trial_days())).to_rfc3339();
    let _ = sqlx::query(
        "INSERT INTO plans (agent, plan, cap, note, expires_at, source) VALUES ($1, 'trial', $2, 'welcome trial', $3, 'trial') \
         ON CONFLICT (agent) DO NOTHING",
    ).bind(subject).bind(TRIAL.calls).bind(&expires).execute(&app.db).await;
    tracing::info!(%subject, %expires, "trial started");
}

/// Where the account stands, in one word the screens can switch on:
/// `trial` (days left), `active` (paid), `free` (the trial is over; the
/// agent still answers within the free tier's monthly conversations).
pub fn state_of(plan: &str) -> &'static str {
    match plan {
        "trial" => "trial",
        "pro" | "max" | "custom" | "enterprise" => "active",
        _ => "free",
    }
}

/// Monthly price in minor units (céntimos). `PLAN_<TIER>_PRICE` is in
/// whole soles: Pro 100, Max 200 (D19, 2026-09-08 — repriced to sit under
/// every WhatsApp-CRM sold into Peru, where the cheapest tiers are quoted
/// per *user*); Enterprise 0 = no list price, quoted in a discovery call.
/// The subscription is capacity; consumption beyond it is metered credits
/// (`meter.rs`), so the headline price no longer has to carry the compute.
pub fn price_minor(name: &str) -> i64 {
    price_minor_in(name, "PEN")
}

/// The price list is per currency, and the currency follows the client's
/// language (D12): Spanish → soles, English → US dollars. `PLAN_<TIER>_PRICE`
/// overrides soles, `PLAN_<TIER>_PRICE_USD` overrides dollars.
pub fn price_minor_in(name: &str, currency: &str) -> i64 {
    let (d, key) = if currency == "USD" {
        (match name { "pro" => 29.0, "max" => 59.0, _ => 0.0 }, format!("PLAN_{}_PRICE_USD", name.to_ascii_uppercase()))
    } else {
        (match name { "pro" => 100.0, "max" => 200.0, _ => 0.0 }, format!("PLAN_{}_PRICE", name.to_ascii_uppercase()))
    };
    (env_f64(&key, d) * 100.0).round() as i64
}

/// "en", "en-US", "en_GB" → USD; anything else (Spanish, unknown) → PEN.
pub fn currency_for_lang(lang: Option<&str>) -> &'static str {
    let l = lang.unwrap_or("").trim().to_ascii_lowercase();
    if l.starts_with("en") { "USD" } else { "PEN" }
}

/// The currency a request wants: `?lang=` / `?currency=` first, then the
/// first `Accept-Language` tag. Missing or Spanish → soles.
pub fn currency_of(headers: &axum::http::HeaderMap, query_lang: Option<&str>) -> &'static str {
    if let Some(q) = query_lang {
        let q = q.trim().to_ascii_uppercase();
        if q == "USD" || q == "PEN" { return if q == "USD" { "USD" } else { "PEN" }; }
        return currency_for_lang(Some(&q));
    }
    let al = headers.get("accept-language").and_then(|v| v.to_str().ok()).unwrap_or("");
    let first = al.split(',').next().unwrap_or("").split(';').next().unwrap_or("").trim();
    currency_for_lang(if first.is_empty() { None } else { Some(first) })
}

/// Soles per US dollar for the credits ledger (`USD_PEN_RATE`, default 3.5):
/// the half-price credit grant on a dollar plan is booked in soles.
pub fn usd_pen_rate() -> f64 {
    env_f64("USD_PEN_RATE", 3.5)
}

/// Minor units in `currency` → ledger minor units (soles).
pub fn to_ledger_minor(amount_minor: i64, currency: &str) -> i64 {
    if currency == "USD" { (amount_minor as f64 * usd_pen_rate()).round() as i64 } else { amount_minor }
}

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// A year costs this many months (`PLAN_ANNUAL_MONTHS`, default 10: pay
/// ten, get twelve — the same shape Peruvian SaaS buyers already know).
pub fn annual_months() -> i64 {
    std::env::var("PLAN_ANNUAL_MONTHS").ok().and_then(|v| v.parse().ok()).filter(|m| (1..=12).contains(m)).unwrap_or(10)
}

/// What `months` of a plan cost, in minor units. Twelve months is the
/// annual price; anything shorter is monthly × months.
pub fn amount_minor(name: &str, months: i64) -> i64 {
    amount_minor_in(name, months, "PEN")
}

pub fn amount_minor_in(name: &str, months: i64, currency: &str) -> i64 {
    let p = price_minor_in(name, currency);
    if months >= 12 { p * annual_months() } else { p * months.max(1) }
}

/// Prices in whole currency units, for the plan screens: `pro`/`max`/
/// `enterprise` per month, `annual.<tier>` per year.
pub fn business_prices() -> Value {
    business_prices_in("PEN")
}

pub fn business_prices_in(currency: &str) -> Value {
    let m = |t: &str| price_minor_in(t, currency) as f64 / 100.0;
    let y = |t: &str| amount_minor_in(t, 12, currency) as f64 / 100.0;
    json!({
        "currency": currency,
        "pro": m("pro"), "max": m("max"),
        "annual": {"pro": y("pro"), "max": y("max")},
        "annualMonthsCharged": annual_months(),
        "trialDays": trial_days(),
    })
}

/// What every plan screen renders. 0 = unlimited.
pub fn tiers_json() -> Value {
    tiers_json_in("PEN")
}

pub fn tiers_json_in(currency: &str) -> Value {
    let t = |t: Tier| json!({
        "name": t.name, "calls": t.calls, "messagesPerDay": t.messages, "customersPerDay": t.customers,
        "conversationsPerMonth": t.conversations, "agents": t.agents, "multiAgent": t.agents != 1,
        // Every paid plan may connect SUNAT boleta electrónica (optional).
        "boleta": t.name != "free",
        "currency": currency,
        "price": price_minor_in(t.name, currency) as f64 / 100.0,
        "priceYear": amount_minor_in(t.name, 12, currency) as f64 / 100.0,
        "backups": t.name != "free", "quoted": false,
    });
    json!([t(FREE), t(TRIAL), t(PRO), t(MAX)])
}

/// `GET /v1/plans` — the price list, with no session. This is what
/// `agente.ceo/checkout` renders before the buyer has verified anything;
/// the currency follows `Accept-Language` exactly as everywhere else.
/// `free` and `trial` are in the list because the page explains them, but
/// only `pro` and `max` can be bought (`billing::checkout` refuses the rest).
pub async fn public_tiers(headers: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>) -> ApiResult {
    // `?lang=` / `?currency=` first, so the page's own ES/EN toggle decides
    // the price list rather than whatever the browser happens to send.
    let cur = currency_of(&headers, q.get("lang").or_else(|| q.get("currency")).map(String::as_str));
    Ok(Json(json!({
        "currency": cur,
        "trialDays": trial_days(),
        "annualMonthsCharged": annual_months(),
        "tiers": tiers_json_in(cur),
        "channels": payment_channels(),
        "recarga": {"options": recarga_options(), "minMinor": recarga_min(), "maxMinor": recarga_max()},
    })).into_response())
}

/// Recarga sizes offered on the screens, minor units (`RECARGA_OPTIONS_MINOR`,
/// default S/ 20 · 50 · 100).
pub fn recarga_options() -> Vec<i64> {
    std::env::var("RECARGA_OPTIONS_MINOR").ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse::<i64>().ok()).filter(|x| *x > 0).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![2000, 5000, 10000])
}

/// Smallest recarga, minor units (`RECARGA_MIN_MINOR`, default S/ 20).
pub fn recarga_min() -> i64 {
    std::env::var("RECARGA_MIN_MINOR").ok().and_then(|s| s.trim().parse().ok()).filter(|x: &i64| *x > 0).unwrap_or(2000)
}

/// Largest recarga in one payment, minor units (`RECARGA_MAX_MINOR`, default S/ 5 000).
pub fn recarga_max() -> i64 {
    std::env::var("RECARGA_MAX_MINOR").ok().and_then(|s| s.trim().parse().ok()).filter(|x: &i64| *x > 0).unwrap_or(500_000)
}

/// A recarga is any whole amount of soles between the minimum and the
/// maximum; the offered sizes are only suggestions on the screens. Whole
/// soles keep the céntimos free for the processor to tag the transfer.
pub fn recarga_check(amount_minor: i64) -> Result<(), (StatusCode, Json<Value>)> {
    let (lo, hi) = (recarga_min(), recarga_max());
    if amount_minor < lo || amount_minor > hi || amount_minor % 100 != 0 {
        return Err(err(StatusCode::BAD_REQUEST, format!("recarga must be whole soles between {} and {} (céntimos)", lo, hi)));
    }
    Ok(())
}

pub struct Effective {
    pub plan: String,
    pub cap: i64,
    pub expires_at: Option<String>,
    pub source: String,
}

/// The subject an agent's plan lives under: its account when linked.
pub async fn subject_of(app: &App, agent: &str) -> Result<String, (StatusCode, Json<Value>)> {
    Ok(match crate::accounts::account_of_agent(app, agent).await? {
        Some(acct) => crate::accounts::subject(&acct),
        None => agent.to_string(),
    })
}

/// The plan that counts right now for an agent.
pub async fn effective(app: &App, agent: &str) -> Result<Effective, (StatusCode, Json<Value>)> {
    let subject = subject_of(app, agent).await?;
    effective_for(app, &subject).await
}

/// Backups ride on paid plans (or everywhere when BACKUPS_FREE=1).
pub fn backups_allowed(app: &App, plan: &str) -> bool {
    app.backups_free || plan != "free"
}

/// The plan that counts right now: a lapsed subscription is `free` again.
pub async fn effective_for(app: &App, subject: &str) -> Result<Effective, (StatusCode, Json<Value>)> {
    let row: Option<(String, Option<i64>, Option<String>, String)> =
        sqlx::query_as("SELECT plan, cap, expires_at, source FROM plans WHERE agent = $1")
            .bind(subject)
            .fetch_optional(&app.db)
            .await
            .map_err(internal)?;
    let now = chrono::Utc::now().to_rfc3339();
    Ok(match row {
        Some((plan, cap, exp, source)) if exp.as_deref().is_none_or(|e| e > now.as_str()) => {
            let t = tier(&plan);
            Effective { plan: t.name.into(), cap: cap.unwrap_or(t.calls), expires_at: exp, source }
        }
        Some((_, _, exp, _)) => Effective { plan: "free".into(), cap: app.free_cap, expires_at: exp, source: "expired".into() },
        None => Effective { plan: "free".into(), cap: app.free_cap, expires_at: None, source: "none".into() },
    })
}

/// Writes the plan row. `free` never expires; paid plans do, `months` from
/// now (a full year for twelve).
pub async fn set(app: &App, agent: &str, plan: &str, months: i64, source: &str, product: Option<&str>) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let t = tier(plan);
    let days = if months >= 12 { 365 } else { 30 * months.max(1) };
    let expires = match t.name {
        "free" => None,
        _ => Some((chrono::Utc::now() + chrono::Duration::days(days)).to_rfc3339()),
    };
    sqlx::query(
        "INSERT INTO plans (agent, plan, cap, note, expires_at, source, product) VALUES ($1,$2,$3,$4,$5,$6,$7) \
         ON CONFLICT (agent) DO UPDATE SET plan = excluded.plan, cap = excluded.cap, note = excluded.note, \
           expires_at = excluded.expires_at, source = excluded.source, product = excluded.product, \
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(agent).bind(t.name).bind(t.calls).bind(source).bind(&expires).bind(source).bind(product)
    .execute(&app.db).await.map_err(internal)?;
    tracing::info!(%agent, plan = t.name, source, expires = ?expires, "plan set");
    Ok(expires)
}

// ------------------------------------------------------ business: Yape/Plin

/// Where the money goes: Yape/Plin numbers and the sales WhatsApp.
pub fn payment_channels() -> Value {
    let v = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
    json!({
        "yape": v("PAY_YAPE_NUMBER"),
        "plin": v("PAY_PLIN_NUMBER"),
        "payee": v("PAY_PAYEE_NAME"),
        "salesWhatsapp": v("SALES_PHONE"),
    })
}

/// Opens a plan purchase for `subject`: the charge on yaya.cash (céntimo-
/// tagged amount) and how to pay. The plan activates when yaya.cash confirms
/// the transfer (`settle` / `sweep`), or through `/admin/plan {ref}` as a
/// manual fallback. One open request per subject: asking again returns it.
pub async fn open_request(app: &App, subject: &str, plan: &str, months: i64) -> Result<Value, (StatusCode, Json<Value>)> {
    // Yape and Plin move soles and nothing else: the price is the PEN one
    // whatever language the buyer's browser speaks (a USD figure here asked
    // for "29.00" by Yape for a S/ 100 plan).
    let currency = "PEN";
    let t = tier(plan);
    if quoted(plan) {
        return Err(err(StatusCode::BAD_REQUEST, "enterprise is quoted in a discovery call — write to sales"));
    }
    if t.name == "free" || t.name == "trial" {
        return Err(err(StatusCode::BAD_REQUEST, "choose pro or max"));
    }
    let months = months.clamp(1, 12);
    expire_stale(app).await?;
    let open: Option<(String, String, f64, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT ref, plan, amount, currency, months, expires_at FROM plan_requests WHERE agent = $1 AND status = 'pending' \
         AND (expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') OR (expires_at IS NULL AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day'))) ORDER BY created_at DESC LIMIT 1",
    ).bind(subject).fetch_optional(&app.db).await.map_err(internal)?;
    if let Some((reference, plan, amount, currency, m, exp)) = open {
        return Ok(json!({
            "ref": reference, "plan": plan, "months": m, "amount": amount, "currency": currency, "expiresAt": exp,
            "pay": payment_channels(), "status": "pending", "reused": true,
            "processor": if app.yayacash.is_some() { "yaya.cash" } else { "manual" },
        }));
    }
    let prices = business_prices_in(currency);
    let amount = amount_minor_in(t.name, months, currency) as f64 / 100.0;
    // Short, unambiguous reference the owner types into the Yape message.
    let code: String = {
        use rand_core::RngCore;
        let mut b = [0u8; 4];
        rand_core::OsRng.fill_bytes(&mut b);
        const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        b.iter().map(|x| A[(*x as usize) % A.len()] as char).collect()
    };
    let reference = format!("YAYA-{}-{code}", t.name.to_uppercase());
    let currency = prices["currency"].as_str().unwrap_or("PEN").to_string();
    let mut expires: Option<String> = None;
    let (payment_id, amount_minor, exact) = match app.yayacash.as_ref() {
        Some(yc) => {
            let (pid, minor) = yc.open_charge(&reference, (amount * 100.0).round() as i64, &currency,
                json!({"subject": subject, "plan": t.name, "months": months, "product": "agente"})).await
                .map_err(|e| { tracing::error!("yaya.cash open charge: {e}"); err(StatusCode::BAD_GATEWAY, "payment processor unavailable") })?;
            (Some(pid), Some(minor), minor as f64 / 100.0)
        }
        None => {
            let (minor, exp) = insert_tagged(app, &NewRequest {
                reference: &reference, subject, plan: t.name, currency: &currency, months,
                product: None, base_minor: (amount * 100.0).round() as i64,
            }).await?;
            expires = Some(exp);
            (None, Some(minor), minor as f64 / 100.0)
        }
    };
    if payment_id.is_some() {
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, payment_id, amount_minor, base_minor) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(&reference).bind(subject).bind(t.name).bind(exact).bind(&currency).bind(months).bind(&payment_id).bind(amount_minor)
            .bind((amount * 100.0).round() as i64)
            .execute(&app.db).await.map_err(internal)?;
    }
    tracing::info!(%subject, plan = t.name, %reference, amount = exact, processor = payment_id.is_some(), "plan requested");
    Ok(json!({
        "ref": reference, "plan": t.name, "months": months,
        "amount": exact, "currency": currency,
        "pay": payment_channels(),
        "status": "pending", "expiresAt": expires,
        "processor": if payment_id.is_some() { "yaya.cash" } else { "manual" },
        "note": "Send EXACTLY this amount (céntimos included) by Yape or Plin to the number shown, today (see expiresAt); the plan activates automatically when the transfer is seen (usually within a minute).",
    }))
}

/// Opens a credits purchase ("recarga") for `subject`: same rails, same
/// reference flow as a plan, but what arrives is balance, not a tier. Any
/// whole amount of soles within `recarga_check` is accepted.
pub async fn open_credits_request(app: &App, subject: &str, amount_minor: i64) -> Result<Value, (StatusCode, Json<Value>)> {
    recarga_check(amount_minor)?;
    expire_stale(app).await?;
    let open: Option<(String, f64, String, Option<String>)> = sqlx::query_as(
        "SELECT ref, amount, currency, expires_at FROM plan_requests WHERE agent = $1 AND plan = 'credits' AND status = 'pending' \
         AND months = $2 AND (expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') OR (expires_at IS NULL AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day'))) ORDER BY created_at DESC LIMIT 1",
    ).bind(subject).bind(amount_minor).fetch_optional(&app.db).await.map_err(internal)?;
    if let Some((reference, amount, currency, exp)) = open {
        return Ok(json!({"ref": reference, "plan": "credits", "months": 0, "amount": amount, "currency": currency, "expiresAt": exp,
                         "creditsMinor": amount_minor, "pay": payment_channels(), "status": "pending", "reused": true,
                         "processor": if app.yayacash.is_some() { "yaya.cash" } else { "manual" }}));
    }
    let code: String = {
        use rand_core::RngCore;
        let mut b = [0u8; 4];
        rand_core::OsRng.fill_bytes(&mut b);
        const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        b.iter().map(|x| A[(*x as usize) % A.len()] as char).collect()
    };
    let reference = format!("YAYA-RECARGA-{code}");
    let currency = business_prices()["currency"].as_str().unwrap_or("PEN").to_string();
    let mut expires: Option<String> = None;
    let (payment_id, exact_minor) = match app.yayacash.as_ref() {
        Some(yc) => {
            let (pid, minor) = yc.open_charge(&reference, amount_minor, &currency,
                json!({"subject": subject, "plan": "credits", "creditsMinor": amount_minor, "product": "agente"})).await
                .map_err(|e| { tracing::error!("yaya.cash open charge: {e}"); err(StatusCode::BAD_GATEWAY, "payment processor unavailable") })?;
            (Some(pid), minor)
        }
        // `months` doubles as the credit amount for recargas: the ledger
        // grant reads it back on confirmation, so a céntimo-tagged charge
        // still credits exactly what was bought.
        None => {
            let (minor, exp) = insert_tagged(app, &NewRequest {
                reference: &reference, subject, plan: "credits", currency: &currency, months: amount_minor,
                product: None, base_minor: amount_minor,
            }).await?;
            expires = Some(exp);
            (None, minor)
        }
    };
    let exact = exact_minor as f64 / 100.0;
    if payment_id.is_some() {
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, payment_id, amount_minor, base_minor) VALUES ($1,$2,'credits',$3,$4,$5,$6,$7,$8)")
            .bind(&reference).bind(subject).bind(exact).bind(&currency).bind(amount_minor).bind(&payment_id).bind(exact_minor).bind(amount_minor)
            .execute(&app.db).await.map_err(internal)?;
    }
    tracing::info!(%subject, %reference, amount = exact, processor = payment_id.is_some(), "recarga requested");
    Ok(json!({
        "ref": reference, "plan": "credits", "months": 0, "amount": exact, "currency": currency, "creditsMinor": amount_minor,
        "pay": payment_channels(), "status": "pending", "expiresAt": expires,
        "processor": if payment_id.is_some() { "yaya.cash" } else { "manual" },
        "note": "Send EXACTLY this amount (céntimos included) by Yape or Plin to the number shown, today (see expiresAt); the credits land automatically when the transfer is seen.",
    }))
}

/// Opens a prepaid-credits recarga by Yape/Plin (Perú): `pen_minor`
/// céntimos, IGV-inclusive, credited in USD on confirmation
/// (`confirm_request`, plan = `usd_topup`). Same reference flow as a plan.
pub async fn open_usd_topup(app: &App, subject: &str, tier: &str, pen_minor: i64) -> Result<Value, (StatusCode, Json<Value>)> {
    if !(500..=500_000).contains(&pen_minor) {
        return Err(err(StatusCode::BAD_REQUEST, "amount must be between S/ 5 and S/ 5 000"));
    }
    expire_stale(app).await?;
    let open: Option<(String, f64, Option<String>)> = sqlx::query_as(
        "SELECT ref, amount, expires_at FROM plan_requests WHERE agent = $1 AND plan = 'usd_topup' AND status = 'pending' \
         AND months = $2 AND (expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') OR (expires_at IS NULL AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day'))) ORDER BY created_at DESC LIMIT 1",
    ).bind(subject).bind(pen_minor).fetch_optional(&app.db).await.map_err(internal)?;
    let usd_cents = ((pen_minor as f64) / crate::prepaid::pen_per_usd()).round() as i64;
    if let Some((reference, amount, exp)) = open {
        return Ok(json!({"ref": reference, "plan": "usd_topup", "amount": amount, "currency": "PEN", "usdCents": usd_cents, "expiresAt": exp,
                         "igvInclusive": true, "pay": payment_channels(), "status": "pending", "reused": true,
                         "processor": if app.yayacash.is_some() { "yaya.cash" } else { "manual" }}));
    }
    let code: String = {
        use rand_core::RngCore;
        let mut b = [0u8; 4];
        rand_core::OsRng.fill_bytes(&mut b);
        const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        b.iter().map(|x| A[(*x as usize) % A.len()] as char).collect()
    };
    let reference = format!("YAYA-CREDITOS-{code}");
    let mut expires: Option<String> = None;
    let (payment_id, exact_minor) = match app.yayacash.as_ref() {
        Some(yc) => {
            let (pid, minor) = yc.open_charge(&reference, pen_minor, "PEN",
                json!({"subject": subject, "plan": "usd_topup", "penMinor": pen_minor, "product": "agente-credits"})).await
                .map_err(|e| { tracing::error!("yaya.cash open charge: {e}"); err(StatusCode::BAD_GATEWAY, "payment processor unavailable") })?;
            (Some(pid), minor)
        }
        None => {
            let (minor, exp) = insert_tagged(app, &NewRequest {
                reference: &reference, subject, plan: "usd_topup", currency: "PEN", months: pen_minor,
                product: Some(tier), base_minor: pen_minor,
            }).await?;
            expires = Some(exp);
            (None, minor)
        }
    };
    let exact = exact_minor as f64 / 100.0;
    if payment_id.is_some() {
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, payment_id, amount_minor, product, base_minor) VALUES ($1,$2,'usd_topup',$3,'PEN',$4,$5,$6,$7,$8)")
            .bind(&reference).bind(subject).bind(exact).bind(pen_minor).bind(&payment_id).bind(exact_minor).bind(tier).bind(pen_minor)
            .execute(&app.db).await.map_err(internal)?;
    }
    tracing::info!(%subject, %reference, amount = exact, usd_cents, "prepaid recarga requested");
    Ok(json!({
        "ref": reference, "plan": "usd_topup", "amount": exact, "currency": "PEN", "usdCents": usd_cents, "igvInclusive": true,
        "pay": payment_channels(), "status": "pending", "expiresAt": expires,
        "processor": if payment_id.is_some() { "yaya.cash" } else { "manual" },
        "note": "Send EXACTLY this amount (céntimos included) by Yape or Plin to the number shown, today (see expiresAt); the credits land automatically when the transfer is seen.",
    }))
}

/// An unpaid Yape/Plin request lives until midnight in Lima the day it
/// was opened — never less than [`MIN_LIFE_MINUTES`], so a buyer at 23:50
/// still has time to pay. Short lives keep the céntimos moving: most
/// buyers pay the exact price because nobody else holds it.
pub const MIN_LIFE_MINUTES: i64 = 60;

/// When a request opened at `now` stops holding its amount.
pub fn expires_at(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    let tz = chrono_tz::America::Lima;
    let tomorrow = now.with_timezone(&tz).date_naive() + chrono::Duration::days(1);
    let midnight = tomorrow.and_hms_opt(0, 0, 0).and_then(|t| tz.from_local_datetime(&t).single())
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or(now + chrono::Duration::hours(24));
    midnight.max(now + chrono::Duration::minutes(MIN_LIFE_MINUTES))
}

fn ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A Yape/Plin request about to be written without a processor.
pub(crate) struct NewRequest<'a> {
    pub reference: &'a str,
    pub subject: &'a str,
    pub plan: &'a str,
    pub currency: &'a str,
    pub months: i64,
    pub product: Option<&'a str>,
    /// The price, minor units.
    pub base_minor: i64,
}

/// Unpaid requests past their life let go of their amount. Rows from before
/// `expires_at` existed get one day.
pub async fn expire_stale(app: &App) -> Result<u64, (StatusCode, Json<Value>)> {
    Ok(sqlx::query(
        "UPDATE plan_requests SET status = 'expired' WHERE status = 'pending' AND payment_id IS NULL \
         AND (expires_at <= $1 OR (expires_at IS NULL AND created_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day')))")
        .bind(ts(chrono::Utc::now()))
        .execute(&app.db).await.map_err(internal)?.rows_affected())
}

/// The order amounts are handed out in, as céntimo offsets from the price:
/// the price itself, then +1, −1, +2, −2, … ±99. Whoever asks first pays
/// exactly the price; a céntimo is used only while another pending payment
/// holds the amount, and half the time the buyer pays less, not more.
pub fn offsets() -> impl Iterator<Item = i64> {
    std::iter::once(0).chain((1..=99).flat_map(|n| [n, -n]))
}

/// Writes a request for the first amount in [`offsets`] order that no
/// pending request holds, so the Yape notification's amount alone says who
/// paid. A paid, cancelled or expired request holds nothing: its amount is
/// free again at once. The partial unique index on pending amounts makes it
/// race-free — two buyers at the same instant get different amounts, in the
/// order their writes land. Returns (amount to pay, expiry).
pub(crate) async fn insert_tagged(app: &App, r: &NewRequest<'_>) -> Result<(i64, String), (StatusCode, Json<Value>)> {
    expire_stale(app).await?;
    let held: Vec<(i64,)> = sqlx::query_as(
        "SELECT amount_minor FROM plan_requests WHERE status = 'pending' AND yape_tag IS NOT NULL AND amount_minor BETWEEN $1 AND $2")
        .bind(r.base_minor - 99).bind(r.base_minor + 99).fetch_all(&app.db).await.map_err(internal)?;
    let held: std::collections::HashSet<i64> = held.into_iter().map(|t| t.0).collect();
    let expires = ts(expires_at(chrono::Utc::now()));
    for tag in offsets().filter(|t| r.base_minor + t > 0 && !held.contains(&(r.base_minor + t))) {
        let amount_minor = r.base_minor + tag;
        let res = sqlx::query(
            "INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, amount_minor, product, base_minor, yape_tag, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(r.reference).bind(r.subject).bind(r.plan).bind(amount_minor as f64 / 100.0).bind(r.currency).bind(r.months)
            .bind(amount_minor).bind(r.product).bind(r.base_minor).bind(tag).bind(&expires)
            .execute(&app.db).await;
        match res {
            Ok(_) => return Ok((amount_minor, expires)),
            // Someone took this amount between our read and our write.
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() && e.message().contains("amount_minor") => continue,
            Err(e) => return Err(internal(e)),
        }
    }
    tracing::warn!(base = r.base_minor, "every amount within ±99 céntimos of this price is held");
    Err(err(StatusCode::SERVICE_UNAVAILABLE, "too many open payments of this amount right now — try again in a few minutes"))
}

/// Asks yaya.cash whether a pending request was paid; activates it if so.
pub async fn settle(app: &App, reference: &str) -> Result<bool, (StatusCode, Json<Value>)> {
    let Some(yc) = app.yayacash.as_ref() else { return Ok(false) };
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT status, payment_id FROM plan_requests WHERE ref = $1")
        .bind(reference).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((status, payment_id)) = row else { return Ok(false) };
    if status != "pending" || payment_id.is_none() {
        return Ok(status == "paid");
    }
    match yc.charge_status(reference).await {
        Ok(s) if s == "confirmed_service" || s == "applied_subscription" => {
            confirm_request(app, reference).await?;
            tracing::info!(%reference, "plan paid via yaya.cash");
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) => { tracing::warn!(%reference, error = %e, "yaya.cash status check failed"); Ok(false) }
    }
}

/// Pending requests of this subject from the last 7 days, settled if paid.
/// Called from `/v1/me` and the web account page so plans turn on without
/// anyone tapping anything.
pub async fn sweep(app: &App, agent: &str) {
    let refs: Vec<(String,)> = sqlx::query_as(
        "SELECT ref FROM plan_requests WHERE agent = $1 AND status = 'pending' AND payment_id IS NOT NULL \
         AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-7 days')")
        .bind(agent).fetch_all(&app.db).await.unwrap_or_default();
    for (r,) in refs {
        let _ = settle(app, &r).await;
    }
}

/// Status of one request, settling it on the way if yaya.cash saw the money.
pub async fn request_status_for(app: &App, subject: &str, reference: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    settle(app, reference).await?;
    let row: Option<(String, String, f64, String, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT plan, status, amount, currency, months, created_at, paid_at FROM plan_requests WHERE ref = $1 AND agent = $2")
        .bind(reference).bind(subject).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((plan, status, amount, currency, months, created, paid)) = row else {
        return Err(err(StatusCode::NOT_FOUND, "unknown reference"));
    };
    Ok(json!({"ref": reference, "plan": plan, "status": status, "amount": amount, "currency": currency, "months": months,
              "createdAt": created, "paidAt": paid, "pay": payment_channels()}))
}

/// Confirms a pending request (admin): sets the plan for `months` from now.
pub async fn confirm_request(app: &App, reference: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let row: Option<(String, String, i64, String, f64, String, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT agent, plan, months, status, amount, currency, customer_doc, customer_doc_type, customer_email FROM plan_requests WHERE ref = $1")
            .bind(reference).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((agent, plan, months, status, amount, currency, cdoc, cdoc_type, cemail)) = row else {
        return Err(err(StatusCode::NOT_FOUND, "unknown reference"));
    };
    if status == "paid" {
        return Err(err(StatusCode::CONFLICT, "already confirmed"));
    }
    // Claim it before anything moves: the yaya.cash sweep and an operator
    // confirming at the same moment must not both credit (and both issue a
    // comprobante for) one transfer.
    let claimed = sqlx::query("UPDATE plan_requests SET status = 'paid', paid_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE ref = $1 AND status <> 'paid'")
        .bind(reference).execute(&app.db).await.map_err(internal)?.rows_affected();
    if claimed == 0 {
        return Err(err(StatusCode::CONFLICT, "already confirmed"));
    }
    // Money received is money SUNAT wants documented, whatever the channel.
    // A recarga carries its céntimos in `months`; a plan carries soles in
    // `amount`. Failing to issue must never undo a paid plan, so it is
    // logged, not propagated.
    let account = agent.strip_prefix("acct:").unwrap_or(&agent).to_string();
    let comprobante = |minor: i64, desc: String| {
        let (app, account, reference) = (app, account.clone(), reference.to_string());
        let (cdoc, cdoc_type, cemail) = (cdoc.clone(), cdoc_type.clone(), cemail.clone());
        let currency = currency.clone();
        async move {
            if currency != "PEN" {
                return;
            }
            if let Err(e) = crate::billing::issue_comprobante(
                app, &account, &reference, minor, &desc,
                cdoc.as_deref(), cdoc_type.as_deref(), None, cemail.as_deref(),
            ).await {
                tracing::error!(%account, %reference, error = %e, "comprobante not issued");
            }
        }
    };

    if plan == "usd_topup" {
        // A prepaid-credits recarga by Yape/Plin (agente/docs/CREDITS.md § 9):
        // `months` carries the céntimos PEN paid, IGV-inclusive; the ledger
        // is credited in USD at PEN_PER_USD.
        // `product` on the row carries the tier (basic|plus|max).
        let tier: Option<(Option<String>,)> = sqlx::query_as("SELECT product FROM plan_requests WHERE ref = $1").bind(reference).fetch_optional(&app.db).await.map_err(internal)?;
        let tier = tier.and_then(|t| t.0).unwrap_or_else(|| "basic".into());
        if let Some(account) = agent.strip_prefix("acct:") {
            if let Err(e) = crate::prepaid::credit_yape(app, account, reference, &tier, months).await {
                let _ = sqlx::query("UPDATE plan_requests SET status = $2, paid_at = NULL WHERE ref = $1").bind(reference).bind(&status).execute(&app.db).await;
                return Err(e);
            }
        }
        comprobante(months, "Recarga agente — saldo".into()).await;
        return Ok(json!({"ok": true, "agent": agent, "plan": "usd_topup", "tier": tier, "penMinor": months}));
    }
    if plan == "credits" {
        // A recarga: `months` carries the céntimos bought.
        if let Some(account) = agent.strip_prefix("acct:") {
            let _ = crate::credits::add(app, account, months, "topup", Some(reference), Some("recarga")).await;
            // Launch: a percentage on top of every recarga while the window lasts.
            let bonus = crate::wallet::bonus_for(months);
            if bonus > 0 {
                if let Some((pct, _)) = crate::wallet::launch_bonus() {
                    let _ = crate::credits::add(app, account, bonus, "bonus", Some(reference), Some(&format!("lanzamiento: +{pct}% en tu recarga"))).await;
                }
            }
        }
        comprobante(months, "Recarga agente — saldo".into()).await;
        return Ok(json!({"ok": true, "agent": agent, "plan": "credits", "creditsMinor": months}));
    }
    let expires = match set(app, &agent, &plan, months, "yape", Some(reference)).await {
        Ok(e) => e,
        Err(e) => {
            // Nothing was activated: release the claim so it can be retried.
            let _ = sqlx::query("UPDATE plan_requests SET status = $2, paid_at = NULL WHERE ref = $1").bind(reference).bind(&status).execute(&app.db).await;
            return Err(e);
        }
    };
    comprobante((amount * 100.0).round() as i64,
                format!("Plan {} agente — {} mes(es)", crate::billing::capitalize(&plan), months)).await;
    Ok(json!({"ok": true, "agent": agent, "plan": plan, "expiresAt": expires}))
}

#[cfg(test)]
mod confirm_race_tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, Keypair};

    #[tokio::test]
    async fn a_payment_confirmed_twice_at_once_counts_once() {
        let app = testkit::app().await;
        account_with_agent(&app, "a", "51900000001", &Keypair::generate()).await;
        let r = open_credits_request(&app, "acct:a", 2_000).await.unwrap();
        let reference = r["ref"].as_str().unwrap().to_string();
        let (x, y) = tokio::join!(confirm_request(&app, &reference), confirm_request(&app, &reference));
        assert_eq!([x.is_ok(), y.is_ok()].iter().filter(|ok| **ok).count(), 1, "one confirmation wins, the other is a 409");
        assert_eq!(crate::credits::balance(&app, "a").await.unwrap(), 2_000 + crate::wallet::bonus_for(2_000));
        assert_eq!(confirm_request(&app, &reference).await.unwrap_err().0, StatusCode::CONFLICT);
        assert_eq!(confirm_request(&app, "YAYA-NOPE").await.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_confirmed_plan_request_activates_the_plan() {
        let app = testkit::app().await;
        account_with_agent(&app, "a", "51900000001", &Keypair::generate()).await;
        let r = open_request(&app, "acct:a", "max", 3).await.unwrap();
        assert_eq!((r["amount"].clone(), r["currency"].clone()), (json!(600.0), json!("PEN")), "the first buyer pays the exact price");
        let again = open_request(&app, "acct:a", "pro", 1).await.unwrap();
        assert_eq!((again["ref"].clone(), again["reused"].clone()), (r["ref"].clone(), json!(true)), "one open request per subject");
        let v = confirm_request(&app, r["ref"].as_str().unwrap()).await.unwrap();
        assert!(v["expiresAt"].is_string());
        assert_eq!(effective_for(&app, "acct:a").await.unwrap().plan, "max");
        for bad in ["free", "trial"] { assert_eq!(open_request(&app, "acct:a", bad, 1).await.unwrap_err().0, StatusCode::BAD_REQUEST); }
        assert_eq!(open_credits_request(&app, "acct:a", 1_234).await.unwrap_err().0, StatusCode::BAD_REQUEST);
    }
}

#[cfg(test)]
mod currency_tests {
    use super::*;

    #[test]
    fn english_buys_in_dollars_spanish_in_soles() {
        assert_eq!(currency_for_lang(Some("en")), "USD");
        assert_eq!(currency_for_lang(Some("en-US")), "USD");
        assert_eq!(currency_for_lang(Some("es-PE")), "PEN");
        assert_eq!(currency_for_lang(None), "PEN");
        let mut h = axum::http::HeaderMap::new();
        h.insert("accept-language", "en-US,en;q=0.9,es;q=0.8".parse().unwrap());
        assert_eq!(currency_of(&h, None), "USD");
        assert_eq!(currency_of(&h, Some("es")), "PEN");
        assert_eq!(currency_of(&axum::http::HeaderMap::new(), Some("USD")), "USD");
    }

    #[test]
    fn the_two_price_lists() {
        assert_eq!(price_minor_in("pro", "PEN"), 10000);
        assert_eq!(price_minor_in("max", "PEN"), 20000);
        assert_eq!(price_minor_in("custom", "PEN"), 0);
        assert_eq!(price_minor_in("pro", "USD"), 2900);
        assert_eq!(price_minor_in("max", "USD"), 5900);
        assert!(!quoted("enterprise"));
        assert_eq!(tier("enterprise").name, "max");
        assert_eq!(tier("free").conversations, 30);
        assert_eq!(tier("pro").conversations, 1000);
        assert_eq!(tier("max").agents, 3);
        assert_eq!(tier("max").conversations, 3000);
        assert_eq!(amount_minor_in("pro", 12, "USD"), 2900 * annual_months());
        assert_eq!(to_ledger_minor(5000, "USD"), (5000.0 * usd_pen_rate()).round() as i64);
        assert_eq!(tiers_json_in("USD")[2]["currency"], "USD");
        assert_eq!(tiers_json_in("USD").as_array().unwrap().len(), 4);
        assert_eq!(state_of("free"), "free");
    }
}

#[cfg(test)]
mod confirm_tests {
    use super::*;

    /// `confirm_request` reads nine columns now, three of them added by
    /// migration 028. sqlx binds those names at runtime, so getting one
    /// wrong is not a compile error — it is a 500 the first time an owner
    /// pays by Yape, which is how nearly all of them pay. Pin the whole
    /// path: a pending request confirms, activates the plan, and is marked
    /// paid.
    #[tokio::test]
    async fn a_yape_request_confirms_and_activates() {
        let app = crate::test_app().await;
        sqlx::query(
            "INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, status) \
             VALUES ('YAYA-REF-1', 'ag-yape', 'pro', 100.0, 'PEN', 1, 'pending')",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = confirm_request(&app, "YAYA-REF-1").await.expect("a pending request confirms");
        assert_eq!(out["ok"], true);
        assert_eq!(out["plan"], "pro");

        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM plan_requests WHERE ref = 'YAYA-REF-1'")
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(status, "paid");

        // Confirming twice must not sell the same month twice.
        assert!(confirm_request(&app, "YAYA-REF-1").await.is_err());
    }

    /// No NubeFact configured — the tests' case, and a real one whenever the
    /// token is missing. The sale must still stand: the plan turns on and no
    /// comprobante row is invented for a document that was never issued.
    #[tokio::test]
    async fn a_sale_stands_when_nubefact_is_absent() {
        let app = crate::test_app().await;
        assert!(app.billing.nubefact.is_none());
        sqlx::query(
            "INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, status) \
             VALUES ('YAYA-REF-2', 'ag-yape-2', 'max', 200.0, 'PEN', 1, 'pending')",
        )
        .execute(&app.db)
        .await
        .unwrap();

        confirm_request(&app, "YAYA-REF-2").await.expect("the plan activates without a comprobante");

        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM invoices")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 0, "no comprobante was issued, so none may be filed");
    }
}

#[cfg(test)]
mod recarga_tests {
    use super::recarga_check;

    #[test]
    fn any_whole_sol_from_the_minimum() {
        assert!(recarga_check(2000).is_ok());
        assert!(recarga_check(3700).is_ok());
        assert!(recarga_check(500_000).is_ok());
        assert!(recarga_check(1900).is_err());
        assert!(recarga_check(2050).is_err());
        assert!(recarga_check(500_100).is_err());
    }
}
