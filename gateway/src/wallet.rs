//! The network ledger, priced in soles.
//!
//! Credits (`credits.rs`) started as "half your plan back, spent on leads".
//! This module grows them into the wallet every kind of value on the
//! network settles in — one unit, the céntimo of a sol:
//!
//! * **recargas** — bought by Yape/Plin (a human confirms the transfer until
//!   yaya.cash is live); a launch bonus adds a percentage on top for a while;
//! * **asks** — a question to an agent that prices its answers
//!   (`offer.askPrice` on its card) moves that price from the asker to the
//!   answerer, minus the network's cut;
//! * **the market** — files, bundles and notes (`listings.rs`) sell for
//!   credits, seller earns the price minus the market fee;
//! * **plans** — a monthly plan can be paid straight from the balance;
//! * **data** — an account that shares redacted turns with the orchestrator
//!   earns the cost of that message back: the free tier is data for service;
//! * **transfers** — account to account. Built and wired, switched off by
//!   `TRANSFERS_ENABLED` until the regulatory side is settled.
//!
//! Every movement is a pair of `credit_ledger` rows written in one SQLite
//! transaction (`BEGIN IMMEDIATE`, so two spends of the same balance
//! serialise), never into the red, with the fee as a third row on the
//! platform account. The ledger is the audit trail; the tables `asks`,
//! `purchases` and `transfers` are the receipts that explain it.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{accounts, credits, err, internal, plans, ApiResult, App, Auth, Shared};

/// The account the network's fees land on.
pub fn platform_account() -> String {
    crate::env_or("PLATFORM_ACCOUNT", "yaya")
}

fn env_pct(k: &str, d: i64) -> i64 {
    std::env::var(k).ok().and_then(|v| v.parse::<i64>().ok()).map(|p| p.clamp(0, 100)).unwrap_or(d)
}
fn env_i64(k: &str, d: i64) -> i64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Network cut on a paid consultation (`ASK_FEE_PERCENT`, default 10).
pub fn ask_fee_percent() -> i64 {
    env_pct("ASK_FEE_PERCENT", 10)
}
/// Network cut on a market sale (`MARKET_FEE_PERCENT`, default 20).
pub fn market_fee_percent() -> i64 {
    env_pct("MARKET_FEE_PERCENT", 20)
}
/// Suggested price for a consultation when a business turns them on
/// (`ASK_DEFAULT_PRICE_MINOR`, default S/ 1.00).
pub fn ask_default_price() -> i64 {
    env_i64("ASK_DEFAULT_PRICE_MINOR", 100).max(1)
}
/// Highest consultation price a card may declare (`ASK_MAX_PRICE_MINOR`, S/ 500).
pub fn ask_max_price() -> i64 {
    env_i64("ASK_MAX_PRICE_MINOR", 50_000).max(1)
}
/// Peer-to-peer transfers: the regulatory switch.
pub fn transfers_enabled() -> bool {
    std::env::var("TRANSFERS_ENABLED").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}
pub fn transfer_max() -> i64 {
    env_i64("TRANSFER_MAX_MINOR", 50_000).max(1)
}
/// What one shared, redacted turn earns (`DATA_REWARD_MINOR`, default = the
/// price of a message, so sharing covers the service) and the daily cap.
pub fn data_reward() -> i64 {
    env_i64("DATA_REWARD_MINOR", credits::call_price() * credits::calls_per_message()).max(0)
}
pub fn data_reward_daily_cap() -> i64 {
    env_i64("DATA_REWARD_PER_DAY_MINOR", 2_000).max(0)
}
/// The date prices change, shown on the screens (`PRICES_RISE_ON`, YYYY-MM-DD).
pub fn prices_rise_on() -> Option<String> {
    std::env::var("PRICES_RISE_ON").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Launch bonus on recargas: `(percent, until)` while `LAUNCH_BONUS_UNTIL`
/// (RFC3339) is in the future and `LAUNCH_BONUS_PERCENT` > 0.
pub fn launch_bonus() -> Option<(i64, String)> {
    let pct = env_pct("LAUNCH_BONUS_PERCENT", 0);
    let until = std::env::var("LAUNCH_BONUS_UNTIL").ok()?;
    let t = chrono::DateTime::parse_from_rfc3339(until.trim()).ok()?.with_timezone(&chrono::Utc);
    (pct > 0 && chrono::Utc::now() < t).then(|| (pct, t.to_rfc3339()))
}
/// Extra credits a recarga of `amount` earns right now.
pub fn bonus_for(amount_minor: i64) -> i64 {
    match launch_bonus() {
        Some((pct, _)) => bonus_with(amount_minor, pct),
        None => 0,
    }
}
pub fn bonus_with(amount_minor: i64, pct: i64) -> i64 {
    (amount_minor.max(0) * pct.clamp(0, 100)) / 100
}
pub fn fee_of(amount_minor: i64, pct: i64) -> i64 {
    (amount_minor.max(0) * pct.clamp(0, 100)) / 100
}

/// The expiry rule, stated wherever a balance or a price is shown (Ley 29571
/// art. 47: informed before contracting; Cal. Civ. Code 1749.45: disclosed).
pub fn expiry_terms() -> Value {
    json!({
        "kinds": credits::expiring_kinds(),
        "rule": "end of the calendar month they were granted for",
        "tz": credits::expiry_tz().name(),
        "es": "Los créditos incluidos en tu plan vencen el último día del mes calendario (hora de Lima) para el que se otorgan; se gastan antes que tu saldo recargado. Las recargas, bonos y lo que ganas en la red nunca vencen.",
        "en": "Plan-included credits expire on the last day of the calendar month (Lima time) they are granted for and are spent before your purchased balance. Top-ups, bonuses and network earnings never expire.",
    })
}

/// The whole price list, in soles, for screens, the landing and `yaya economics`.
pub fn economics(app: &App) -> Value {
    let msg = credits::call_price() * credits::calls_per_message();
    let t = |tier: plans::Tier| json!({
        "name": tier.name, "price": plans::price_minor(tier.name),
        "priceYear": plans::amount_minor(tier.name, 12),
        "messagesPerDay": tier.messages, "customersPerDay": tier.customers, "callsPerDay": tier.calls,
        "creditsPerMonth": credits::grant_for(plans::price_minor(tier.name)),
        "privateData": tier.name != "free",
    });
    json!({
        "currency": credits::CURRENCY,
        "unit": "céntimo",
        "coin": {"name": "Yaya coins", "symbol": "YAYA", "note": "the network's cash: soles, blind-signed by the Yaya exchange; the on-chain capacity token is $YAYA"},
        "message": msg, "call": credits::call_price(), "callsPerMessage": credits::calls_per_message(),
        "lead": app.lead_price_minor, "referral": app.lead_price_minor,
        "ask": {"default": ask_default_price(), "max": ask_max_price(), "feePercent": ask_fee_percent()},
        "market": {"feePercent": market_fee_percent()},
        "recargas": plans::recarga_options(),
        "launchBonus": launch_bonus().map(|(p, until)| json!({"percent": p, "until": until})),
        "pricesRiseOn": prices_rise_on(),
        "data": {"perSample": data_reward(), "dailyCap": data_reward_daily_cap()},
        "transfers": {"enabled": transfers_enabled(), "max": transfer_max()},
        "expiry": expiry_terms(),
        "starter": app.starter_credits_minor,
        "tiers": [t(plans::FREE), t(plans::PRO), t(plans::MAX)],
    })
}

#[derive(Debug)]
pub struct Moved {
    pub charged: i64,
    pub net: i64,
    pub fee: i64,
}

/// `402` with everything the payer's agent needs to fix it: price, balance,
/// the recarga sizes. `type: "payment"` is what the core maps to its own
/// payment_required outcome.
pub fn payment_required(what: &str, price: i64, balance: i64) -> (StatusCode, Json<Value>) {
    (StatusCode::PAYMENT_REQUIRED, Json(json!({"error": {
        "message": format!("{what}: costs {} {:.2}, balance {} {:.2}", credits::CURRENCY, price as f64 / 100.0, credits::CURRENCY, balance as f64 / 100.0),
        "type": "payment", "priceMinor": price, "balanceMinor": balance, "currency": credits::CURRENCY,
        "recargas": plans::recarga_options(),
    }})))
}

/// What of the balance was **bought or earned** (never expires): the whole
/// balance minus what still sits in expiring lots. Plan-included credits are
/// the baseline you use or lose; only this part may leave the account as
/// coins or as a transfer — the same split as "included usage" vs "extra
/// usage" on a subscription.
pub async fn bought_balance(app: &App, account: &str) -> Result<i64, (StatusCode, Json<Value>)> {
    let total = credits::balance(app, account).await?;
    Ok((total - expiring_remaining(&app.db, account).await?).max(0))
}

async fn expiring_remaining<'e, E: sqlx::Executor<'e, Database = sqlx::Sqlite>>(db: E, account: &str) -> Result<i64, (StatusCode, Json<Value>)> {
    let (expiring,): (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(r) FROM (SELECT l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id), 0) AS r \
           FROM credit_ledger l WHERE l.account = $1 AND l.delta > 0 AND l.expires_at IS NOT NULL \
             AND l.expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')) WHERE r > 0",
    ).bind(account).fetch_one(db).await.map_err(internal)?;
    Ok(expiring.unwrap_or(0))
}

/// Spends `amount` from the non-expiring lots only (topups, bonuses,
/// earnings, deposits), oldest first. `Ok(false)` when the bought balance
/// cannot cover it — the baseline is never touched here.
pub(crate) async fn spend_bought_in(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, account: &str, amount: i64, kind: &str, reference: Option<&str>, note: &str) -> Result<bool, (StatusCode, Json<Value>)> {
    credits::expire_due_in(tx, account).await?;
    let (total,): (Option<i64>,) = sqlx::query_as("SELECT SUM(delta) FROM credit_ledger WHERE account = $1").bind(account).fetch_one(&mut **tx).await.map_err(internal)?;
    let bought = (total.unwrap_or(0) - expiring_remaining(&mut **tx, account).await?).max(0);
    if bought < amount {
        return Ok(false);
    }
    let lots: Vec<(String, i64)> = sqlx::query_as(
        "SELECT l.id, l.delta + COALESCE((SELECT SUM(s.delta) FROM credit_ledger s WHERE s.lot = l.id), 0) AS remaining \
         FROM credit_ledger l WHERE l.account = $1 AND l.delta > 0 AND l.expires_at IS NULL AND remaining > 0 ORDER BY l.created_at",
    ).bind(account).fetch_all(&mut **tx).await.map_err(internal)?;
    let mut left = amount;
    for (id, remaining) in lots {
        if left == 0 { break; }
        let take = remaining.min(left);
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note, lot) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(-take).bind(credits::CURRENCY).bind(kind).bind(reference).bind(note).bind(&id)
            .execute(&mut **tx).await.map_err(internal)?;
        left -= take;
    }
    if left > 0 {
        // Pre-021 lots were spent unattributed; the bought check above is the truth.
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(-left).bind(credits::CURRENCY).bind(kind).bind(reference).bind(note)
            .execute(&mut **tx).await.map_err(internal)?;
    }
    Ok(true)
}

/// Like [`move_credits`], but only bought/earned balance may leave: coins
/// (bearer value that never expires) and transfers to other accounts.
#[allow(clippy::too_many_arguments)]
pub async fn move_bought(
    app: &App, payer: &str, payee: &str, amount: i64, fee_pct: i64,
    kind_payer: &str, kind_payee: &str, reference: Option<&str>, note_payer: &str, note_payee: &str,
) -> Result<Option<Moved>, (StatusCode, Json<Value>)> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let moved = move_bought_in(&mut tx, payer, payee, amount, fee_pct, kind_payer, kind_payee, reference, note_payer, note_payee).await?;
    if moved.is_some() {
        tx.commit().await.map_err(internal)?;
    }
    Ok(moved)
}

/// [`move_bought`] inside a caller-owned transaction (the caller commits).
#[allow(clippy::too_many_arguments)]
pub async fn move_bought_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, payer: &str, payee: &str, amount: i64, fee_pct: i64,
    kind_payer: &str, kind_payee: &str, reference: Option<&str>, note_payer: &str, note_payee: &str,
) -> Result<Option<Moved>, (StatusCode, Json<Value>)> {
    if amount <= 0 {
        return Ok(Some(Moved { charged: 0, net: 0, fee: 0 }));
    }
    if payer == payee {
        return Err(err(StatusCode::BAD_REQUEST, "payer and payee are the same account"));
    }
    let fee = fee_of(amount, fee_pct);
    let net = amount - fee;
    if !spend_bought_in(tx, payer, amount, kind_payer, reference, note_payer).await? {
        return Ok(None);
    }
    let row = |account: &str, delta: i64, kind: &str, note: &str| {
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(account.to_string()).bind(delta).bind(credits::CURRENCY)
            .bind(kind.to_string()).bind(reference.map(String::from)).bind(note.to_string())
    };
    row(payee, net, kind_payee, note_payee).execute(&mut **tx).await.map_err(internal)?;
    if fee > 0 {
        row(&platform_account(), fee, "fee", &format!("{kind_payee} fee {fee_pct}%")).execute(&mut **tx).await.map_err(internal)?;
    }
    tracing::info!(%payer, %payee, amount, fee, kind = kind_payee, "bought credits moved");
    Ok(Some(Moved { charged: amount, net, fee }))
}

/// Moves `amount` from `payer` to `payee` minus `fee_pct` for the platform,
/// atomically, only if the payer can cover it. `Ok(None)` = cannot.
#[allow(clippy::too_many_arguments)]
pub async fn move_credits(
    app: &App, payer: &str, payee: &str, amount: i64, fee_pct: i64,
    kind_payer: &str, kind_payee: &str, reference: Option<&str>, note_payer: &str, note_payee: &str,
) -> Result<Option<Moved>, (StatusCode, Json<Value>)> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let moved = move_credits_in(&mut tx, payer, payee, amount, fee_pct, kind_payer, kind_payee, reference, note_payer, note_payee).await?;
    if moved.is_some() {
        tx.commit().await.map_err(internal)?;
    }
    Ok(moved)
}

/// Same, inside a caller-owned transaction, so a ledger move can be atomic
/// with whatever else must happen with it (e.g. marking coins spent). The
/// caller commits; on `Ok(None)` it must roll back.
#[allow(clippy::too_many_arguments)]
pub async fn move_credits_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, payer: &str, payee: &str, amount: i64, fee_pct: i64,
    kind_payer: &str, kind_payee: &str, reference: Option<&str>, note_payer: &str, note_payee: &str,
) -> Result<Option<Moved>, (StatusCode, Json<Value>)> {
    if amount <= 0 {
        return Ok(Some(Moved { charged: 0, net: 0, fee: 0 }));
    }
    if payer == payee {
        return Err(err(StatusCode::BAD_REQUEST, "payer and payee are the same account"));
    }
    let fee = fee_of(amount, fee_pct);
    let net = amount - fee;
    // The payer's side: expiring lots first, never into the red.
    if !credits::spend_in(tx, payer, amount, kind_payer, reference, Some(note_payer)).await? {
        return Ok(None);
    }
    let row = |account: &str, delta: i64, kind: &str, note: &str| {
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(account.to_string()).bind(delta).bind(credits::CURRENCY)
            .bind(kind.to_string()).bind(reference.map(String::from)).bind(note.to_string())
    };
    row(payee, net, kind_payee, note_payee).execute(&mut **tx).await.map_err(internal)?;
    if fee > 0 {
        row(&platform_account(), fee, "fee", &format!("{kind_payee} fee {fee_pct}%")).execute(&mut **tx).await.map_err(internal)?;
    }
    tracing::info!(%payer, %payee, amount, fee, kind = kind_payee, reference = reference.unwrap_or("-"), "credits moved");
    Ok(Some(Moved { charged: amount, net, fee }))
}

// ------------------------------------------------------------------ asks

/// The consultation price an agent's published card declares
/// (`payload.offer.askPrice`, céntimos). 0 = answers for free.
pub fn ask_price_of_payload(p: &Value) -> i64 {
    let v = &p["offer"]["askPrice"];
    let n = v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)).or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok())).unwrap_or(0);
    n.clamp(0, ask_max_price())
}

pub async fn ask_price_of(app: &App, agent: &str) -> i64 {
    let row: Option<(String,)> = sqlx::query_as("SELECT card FROM agents WHERE agent = $1 AND revoked_at IS NULL")
        .bind(agent).fetch_optional(&app.db).await.ok().flatten();
    row.and_then(|(c,)| serde_json::from_str::<Value>(&c).ok()).map(|v| ask_price_of_payload(&v["payload"])).unwrap_or(0)
}

/// A business on the network: it has published a card with an offer. Peers
/// reach each other (referrals, community) without paying consultations.
async fn is_business(app: &App, agent: &str) -> bool {
    let row: Option<(String,)> = sqlx::query_as("SELECT card FROM agents WHERE agent = $1 AND name IS NOT NULL AND revoked_at IS NULL")
        .bind(agent).fetch_optional(&app.db).await.ok().flatten();
    row.and_then(|(c,)| serde_json::from_str::<Value>(&c).ok()).map(|v| v["payload"]["offer"].is_object()).unwrap_or(false)
}

/// Called by the relay for every message `from` → `to`. When `to` prices
/// its answers and the sender is not the owner: a declared `pay` settles
/// the price (402 if it cannot); no `pay` from a business peer passes
/// unpaid; no `pay` from anyone else is refused with the price, so the
/// asker's agent can confirm with the person and come back paying.
/// Returns what was paid for this message.
pub async fn settle_ask(app: &App, from: &str, to: &str, pay: &Value, same_account: bool) -> Result<Option<i64>, (StatusCode, Json<Value>)> {
    let price = ask_price_of(app, to).await;
    if price <= 0 || same_account {
        return Ok(None);
    }
    let declared = pay["amountMinor"].as_i64().unwrap_or(0);
    if declared <= 0 {
        if is_business(app, from).await {
            return Ok(None);
        }
        let balance = match accounts::account_of_agent(app, from).await? {
            Some(a) => credits::balance(app, &a).await?,
            None => 0,
        };
        return Err(payment_required("this agent charges per consultation", price, balance));
    }
    if declared < price {
        return Err(payment_required("the declared payment is below this agent's price", price, declared));
    }
    let Some(payer) = accounts::account_of_agent(app, from).await? else {
        return Err(err(StatusCode::UNAUTHORIZED, "sign in with agente to pay for consultations"));
    };
    // An answering agent nobody has linked yet (grace window) answers unpaid.
    let Some(payee) = accounts::account_of_agent(app, to).await? else {
        return Ok(None);
    };
    let note_payer = format!("consulta a {}", short(to));
    let note_payee = format!("consulta pagada por {}", short(from));
    match move_credits(app, &payer, &payee, price, ask_fee_percent(), "ask", "earn", Some(to), &note_payer, &note_payee).await? {
        Some(_) => Ok(Some(price)),
        None => Err(payment_required("not enough balance for this consultation", price, credits::balance(app, &payer).await?)),
    }
}

/// Receipt for a settled ask, keyed by the mailbox id.
pub async fn record_ask(app: &App, mailbox_id: &str, from: &str, to: &str, price: i64) {
    let (payer, payee) = (
        accounts::account_of_agent(app, from).await.ok().flatten().unwrap_or_default(),
        accounts::account_of_agent(app, to).await.ok().flatten().unwrap_or_default(),
    );
    let _ = sqlx::query("INSERT OR IGNORE INTO asks (id, from_agent, to_agent, payer, payee, price, fee) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(mailbox_id).bind(from).bind(to).bind(payer).bind(payee).bind(price).bind(fee_of(price, ask_fee_percent()))
        .execute(&app.db).await;
}

fn short(agent: &str) -> String {
    let h = agent.strip_prefix("agent:").unwrap_or(agent);
    format!("agent:{}…", h.chars().take(8).collect::<String>())
}

// ------------------------------------------------------------------ data

/// A shared, redacted turn earns its message back — for accounts, up to the
/// daily cap. Unlinked agents share for free (there is nobody to pay).
pub async fn reward_data(app: &App, agent: &str) -> Option<i64> {
    let reward = data_reward();
    if reward <= 0 {
        return None;
    }
    let account = accounts::account_of_agent(app, agent).await.ok().flatten()?;
    let (today,): (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(delta) FROM credit_ledger WHERE account = $1 AND kind = 'data' AND created_at >= strftime('%Y-%m-%dT00:00:00Z','now')",
    ).bind(&account).fetch_one(&app.db).await.ok()?;
    if today.unwrap_or(0) + reward > data_reward_daily_cap() {
        return None;
    }
    credits::add(app, &account, reward, "data", Some(agent), Some("muestra compartida con la red")).await.ok()?;
    Some(reward)
}

// -------------------------------------------------------------- handlers

async fn resolve_account(app: &App, who: &str) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let who = who.trim();
    if who.is_empty() {
        return Ok(None);
    }
    if who.starts_with("agent:") {
        return accounts::account_of_agent(app, who).await;
    }
    if who.contains('@') {
        let row: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE email = $1").bind(who.to_lowercase()).fetch_optional(&app.db).await.map_err(internal)?;
        return Ok(row.map(|r| r.0));
    }
    let id = who.strip_prefix("acct:").unwrap_or(who);
    let row: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE id = $1").bind(id).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(row.map(|r| r.0))
}

#[derive(serde::Deserialize)]
pub struct TransferReq {
    to: String,
    #[serde(rename = "amountMinor")] amount_minor: i64,
    #[serde(default)] note: Option<String>,
}

/// `POST /v1/wallet/transfer` — credits from my account to another (email,
/// account id or agent id). No fee. Off until `TRANSFERS_ENABLED=1`.
pub async fn transfer(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<TransferReq>) -> ApiResult {
    let from = accounts::account_of_auth(&app, &auth).await?;
    if !transfers_enabled() {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": {
            "message": "transfers between accounts are not enabled yet (pending regulatory approval); credits can be spent on the network today",
            "type": "regulatory"}}))));
    }
    if req.amount_minor <= 0 || req.amount_minor > transfer_max() {
        return Err(err(StatusCode::BAD_REQUEST, format!("amountMinor must be 1..={}", transfer_max())));
    }
    let Some(to) = resolve_account(&app, &req.to).await? else { return Err(err(StatusCode::NOT_FOUND, "no such account")) };
    if to == from {
        return Err(err(StatusCode::BAD_REQUEST, "that is your own account"));
    }
    let note: String = req.note.as_deref().unwrap_or("").chars().take(120).collect();
    let id = format!("tr_{}", uuid::Uuid::new_v4().simple());
    let moved = move_bought(&app, &from, &to, req.amount_minor, 0, "transfer", "transfer", Some(&id),
        &format!("enviado{}", if note.is_empty() { String::new() } else { format!(": {note}") }),
        &format!("recibido{}", if note.is_empty() { String::new() } else { format!(": {note}") })).await?;
    let Some(m) = moved else { return Err(payment_required("transfer (only bought/earned balance can be sent; plan credits stay)", req.amount_minor, bought_balance(&app, &from).await?)) };
    sqlx::query("INSERT INTO transfers (id, from_account, to_account, amount, note) VALUES ($1,$2,$3,$4,$5)")
        .bind(&id).bind(&from).bind(&to).bind(m.charged).bind(&note).execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true, "id": id, "amountMinor": m.charged, "balance": credits::balance(&app, &from).await?})).into_response())
}

#[derive(serde::Deserialize)]
pub struct PlanCreditsReq {
    plan: String,
    #[serde(default)] months: Option<i64>,
}

/// `POST /v1/account/plan/credits` — a plan paid from the balance. No
/// credits come back on this one (that would be a discount loop).
pub async fn plan_with_credits(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<PlanCreditsReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let t = plans::tier(&req.plan);
    if t.name == "free" || t.name == "trial" {
        return Err(err(StatusCode::BAD_REQUEST, "choose pro, max or custom"));
    }
    let months = req.months.unwrap_or(1).clamp(1, 12);
    let amount = plans::amount_minor(t.name, months);
    let reference = format!("YAYA-SALDO-{}", uuid::Uuid::new_v4().simple().to_string()[..8].to_uppercase());
    if credits::spend(&app, &account, amount, "plan", Some(&reference), Some(&format!("plan {} pagado con saldo ({months} mes(es))", t.name))).await?.is_none() {
        return Err(payment_required(&format!("plan {} × {months}", t.name), amount, credits::balance(&app, &account).await?));
    }
    let expires = plans::set(&app, &accounts::subject(&account), t.name, months, "credits", Some(&reference)).await?;
    Ok(Json(json!({"ok": true, "plan": t.name, "months": months, "amountMinor": amount, "expiresAt": expires, "balance": credits::balance(&app, &account).await?})).into_response())
}

/// Wallet summary for `/v1/account`, `/v1/me` and `GET /v1/wallet`.
pub async fn summary(app: &App, account: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let balance = credits::balance(app, account).await?;
    let by_kind: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, SUM(delta) FROM credit_ledger WHERE account = $1 AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days') GROUP BY kind",
    ).bind(account).fetch_all(&app.db).await.map_err(internal)?;
    let earned: i64 = by_kind.iter().filter(|(k, _)| matches!(k.as_str(), "earn" | "sale" | "referral" | "data" | "transfer")).map(|(_, d)| d.max(&0)).sum();
    let spent: i64 = by_kind.iter().map(|(_, d)| (-d).max(0)).sum();
    let (listings,): (i64,) = sqlx::query_as("SELECT count(*) FROM listings WHERE account = $1 AND status != 'hidden'").bind(account).fetch_one(&app.db).await.map_err(internal)?;
    let (purchases,): (i64,) = sqlx::query_as("SELECT count(*) FROM purchases WHERE buyer = $1").bind(account).fetch_one(&app.db).await.map_err(internal)?;
    let (asks,): (i64,) = sqlx::query_as("SELECT count(*) FROM asks WHERE payee = $1").bind(account).fetch_one(&app.db).await.map_err(internal)?;
    // The consultation price this account's agents declare (highest wins the display).
    let cards: Vec<(String,)> = sqlx::query_as("SELECT a.card FROM agents a JOIN account_agents aa ON aa.agent = a.agent WHERE aa.account = $1 AND a.revoked_at IS NULL")
        .bind(account).fetch_all(&app.db).await.map_err(internal)?;
    let ask_price = cards.iter().filter_map(|(c,)| serde_json::from_str::<Value>(c).ok()).map(|v| ask_price_of_payload(&v["payload"])).max().unwrap_or(0);
    let expiring = credits::expiring(app, account).await?;
    Ok(json!({
        "balance": balance, "currency": credits::CURRENCY,
        // Claude-style: the plan's included credits are the baseline (use it or lose it);
        // what you bought or earned is extra, never expires, and is the only part that can leave.
        "bought": bought_balance(app, account).await?, "baseline": balance - bought_balance(app, account).await?,
        "expiring": expiring.map(|(a, at)| json!({"amountMinor": a, "at": at})),
        "expiry": expiry_terms(),
        "earned30d": earned, "spent30d": spent,
        "byKind30d": by_kind.into_iter().map(|(k, d)| json!({"kind": k, "delta": d})).collect::<Vec<_>>(),
        "askPrice": ask_price, "asksAnswered": asks,
        "listings": listings, "purchases": purchases,
        "launchBonus": launch_bonus().map(|(p, until)| json!({"percent": p, "until": until})),
        "pricesRiseOn": prices_rise_on(),
        "transfers": {"enabled": transfers_enabled(), "max": transfer_max()},
        "fees": {"askPercent": ask_fee_percent(), "marketPercent": market_fee_percent()},
    }))
}

/// `GET /v1/wallet` — session or linked agent.
pub async fn wallet(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let mut v = summary(&app, &account).await?;
    v["credits"] = credits::summary(&app, &account, 30).await?;
    v["economics"] = economics(&app);
    Ok(Json(v).into_response())
}

/// `GET /v1/economics` — public price list.
pub async fn economics_public(State(app): State<Shared>) -> ApiResult {
    Ok(Json(economics(&app)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bonus_and_fees_round_down() {
        assert_eq!(bonus_with(2000, 50), 1000);
        assert_eq!(bonus_with(5000, 50), 2500);
        assert_eq!(bonus_with(3, 50), 1);
        assert_eq!(bonus_with(2000, 0), 0);
        assert_eq!(fee_of(100, 10), 10);
        assert_eq!(fee_of(99, 10), 9);
        assert_eq!(fee_of(5000, 20), 1000);
    }

    #[test]
    fn ask_price_reads_the_card_leniently() {
        assert_eq!(ask_price_of_payload(&json!({"offer": {"askPrice": 150}})), 150);
        assert_eq!(ask_price_of_payload(&json!({"offer": {"askPrice": "250"}})), 250);
        assert_eq!(ask_price_of_payload(&json!({"offer": {"askPrice": 99.6}})), 100);
        assert_eq!(ask_price_of_payload(&json!({"offer": {}})), 0);
        assert_eq!(ask_price_of_payload(&json!({"offer": {"askPrice": -5}})), 0);
        assert_eq!(ask_price_of_payload(&json!({"offer": {"askPrice": 10_000_000}})), ask_max_price());
    }

    #[test]
    fn launch_bonus_needs_a_future_date() {
        std::env::set_var("LAUNCH_BONUS_PERCENT", "50");
        std::env::set_var("LAUNCH_BONUS_UNTIL", "2001-01-01T00:00:00Z");
        assert!(launch_bonus().is_none());
        assert_eq!(bonus_for(2000), 0);
        std::env::set_var("LAUNCH_BONUS_UNTIL", "2999-01-01T00:00:00Z");
        assert_eq!(launch_bonus().map(|b| b.0), Some(50));
        assert_eq!(bonus_for(2000), 1000);
        std::env::remove_var("LAUNCH_BONUS_UNTIL");
        std::env::remove_var("LAUNCH_BONUS_PERCENT");
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_agent, as_session, session_for, Keypair};

    async fn funded(app: &Shared, id: &str, phone: &str, bought: i64, plan: i64) -> Keypair {
        let kp = Keypair::generate();
        account_with_agent(app, id, phone, &kp).await;
        if bought > 0 { credits::add(app, id, bought, "topup", None, None).await.unwrap(); }
        if plan > 0 { credits::add(app, id, plan, "grant", None, None).await.unwrap(); }
        kp
    }

    async fn publish(app: &Shared, kp: &Keypair, payload: Value) {
        sqlx::query("INSERT INTO agents (agent, card, name) VALUES ($1,$2,$3)")
            .bind(kp.id().to_string()).bind(json!({"payload": payload}).to_string()).bind(payload["name"].as_str())
            .execute(&app.db).await.unwrap();
    }

    #[tokio::test]
    async fn only_bought_balance_leaves_and_moves_are_all_or_nothing() {
        let app = testkit::app().await;
        funded(&app, "a", "51900000001", 300, 500).await;
        funded(&app, "b", "51900000002", 0, 0).await;
        assert_eq!(bought_balance(&app, "a").await.unwrap(), 300);
        assert!(move_bought(&app, "a", "b", 301, 0, "x", "y", None, "", "").await.unwrap().is_none(), "plan credits never leave");
        assert_eq!(credits::balance(&app, "a").await.unwrap(), 800, "a refused move leaves no trace");
        let m = move_bought(&app, "a", "b", 300, 10, "x", "y", None, "", "").await.unwrap().unwrap();
        assert_eq!((m.charged, m.net, m.fee), (300, 270, 30));
        assert_eq!(credits::balance(&app, "b").await.unwrap(), 270);
        assert_eq!(credits::balance(&app, &platform_account()).await.unwrap(), 30);
        assert_eq!(credits::balance(&app, "a").await.unwrap(), 500);
        // move_credits spends the plan baseline too, expiring lots first.
        let m = move_credits(&app, "a", "b", 500, 0, "x", "y", None, "", "").await.unwrap().unwrap();
        assert_eq!(m.net, 500);
        assert!(move_credits(&app, "a", "b", 1, 0, "x", "y", None, "", "").await.unwrap().is_none());
        assert_eq!(move_credits(&app, "a", "a", 1, 0, "x", "y", None, "", "").await.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(move_bought(&app, "a", "b", 0, 0, "x", "y", None, "", "").await.unwrap().unwrap().charged, 0);
    }

    #[tokio::test]
    async fn transfers_are_off_until_switched_on_then_bounded() {
        let app = testkit::app().await;
        let a = funded(&app, "a", "51900000001", 1_000, 1_000).await;
        funded(&app, "b", "51900000002", 0, 0).await;
        let t = |to: &str, amt: i64| json!({"to": to, "amountMinor": amt, "note": "gracias"});
        std::env::remove_var("TRANSFERS_ENABLED");
        assert_eq!(as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("b", 10))).await.0, 403);
        std::env::set_var("TRANSFERS_ENABLED", "1");
        assert_eq!(anon(&app, "POST", "/v1/wallet/transfer", Some(t("b", 10))).await.0, 401);
        assert_eq!(as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("b", 0))).await.0, 400);
        assert_eq!(as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("b", transfer_max() + 1))).await.0, 400);
        assert_eq!(as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("nobody@x.pe", 10))).await.0, 404);
        assert_eq!(as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("a@test.pe", 10))).await.0, 400, "not to yourself");
        assert_eq!(as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("B@TEST.PE", 1_001))).await.0, 402, "plan credits stay");
        let (st, v) = as_agent(&app, &a, "POST", "/v1/wallet/transfer", Some(t("acct:b", 400))).await;
        assert_eq!((st, v["balance"].clone()), (200, json!(1_600)), "{v}");
        assert_eq!(credits::balance(&app, "b").await.unwrap(), 400);
        let (n,): (i64,) = sqlx::query_as("SELECT amount FROM transfers WHERE from_account='a'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 400);
        std::env::remove_var("TRANSFERS_ENABLED");
    }

    #[tokio::test]
    async fn consultations_charge_strangers_but_not_owners_or_businesses() {
        let app = testkit::app().await;
        let seller = funded(&app, "s", "51900000001", 0, 0).await;
        let buyer = funded(&app, "b", "51900000002", 150, 0).await;
        let (sid, bid) = (seller.id().to_string(), buyer.id().to_string());
        let peer = Keypair::generate();
        let anon_kp = Keypair::generate();
        publish(&app, &seller, json!({"name": "Clínica", "offer": {"askPrice": "100"}})).await;
        publish(&app, &peer, json!({"name": "Bodega", "offer": {}})).await;
        assert_eq!(ask_price_of(&app, &sid).await, 100);
        assert_eq!(settle_ask(&app, &bid, &sid, &Value::Null, true).await.unwrap(), None, "the owner's own agents talk free");
        assert_eq!(settle_ask(&app, &peer.id().to_string(), &sid, &Value::Null, false).await.unwrap(), None, "businesses reach each other free");
        let e = settle_ask(&app, &bid, &sid, &Value::Null, false).await.unwrap_err();
        assert_eq!((e.0, e.1["error"]["priceMinor"].clone(), e.1["error"]["balanceMinor"].clone()), (StatusCode::PAYMENT_REQUIRED, json!(100), json!(150)));
        assert_eq!(settle_ask(&app, &bid, &sid, &json!({"amountMinor": 99}), false).await.unwrap_err().0, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(settle_ask(&app, &anon_kp.id().to_string(), &sid, &json!({"amountMinor": 100}), false).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(settle_ask(&app, &bid, &sid, &json!({"amountMinor": 500}), false).await.unwrap(), Some(100), "charges the price, not the offer");
        assert_eq!(credits::balance(&app, "s").await.unwrap(), 100 - fee_of(100, ask_fee_percent()));
        record_ask(&app, "mb1", &bid, &sid, 100).await;
        record_ask(&app, "mb1", &bid, &sid, 100).await;
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM asks WHERE payee='s'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1, "one receipt per mailbox id");
        assert_eq!(settle_ask(&app, &bid, &sid, &json!({"amountMinor": 100}), false).await.unwrap_err().0, StatusCode::PAYMENT_REQUIRED, "50 left");
        assert_eq!(credits::balance(&app, "b").await.unwrap(), 50);
        // An unlinked answering agent answers unpaid.
        let orphan = Keypair::generate();
        publish(&app, &orphan, json!({"name": "Nuevo", "offer": {"askPrice": 10}})).await;
        assert_eq!(settle_ask(&app, &bid, &orphan.id().to_string(), &json!({"amountMinor": 10}), false).await.unwrap(), None);
        let s = summary(&app, "s").await.unwrap();
        assert_eq!((s["askPrice"].clone(), s["asksAnswered"].clone()), (json!(100), json!(1)));
    }

    #[tokio::test]
    async fn plans_bought_with_balance_charge_the_real_price() {
        let app = testkit::app().await;
        let kp = funded(&app, "a", "51900000001", 10_000, 15_000).await;
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/account/plan/credits", Some(json!({"plan": "free"}))).await.0, 400);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/account/plan/credits", Some(json!({"plan": "whatever"}))).await.0, 400);
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/account/plan/credits", Some(json!({"plan": "custom", "months": 1}))).await;
        assert_eq!((st, v["plan"].clone(), v["amountMinor"].clone()), (200, json!("max"), json!(20_000)), "a legacy name is not a free plan: {v}");
        assert_eq!(v["balance"], 5_000);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/account/plan/credits", Some(json!({"plan": "pro"}))).await.0, 402);
        let (plan,): (String,) = sqlx::query_as("SELECT plan FROM plans WHERE agent='acct:a'").fetch_one(&app.db).await.unwrap();
        assert_eq!(plan, "max");
    }

    #[tokio::test]
    async fn the_wallet_and_price_list_render() {
        let app = testkit::app().await;
        funded(&app, "a", "51900000001", 700, 300).await;
        let token = session_for(&app, "a").await;
        let (st, w) = as_session(&app, &token, "GET", "/v1/wallet", None).await;
        assert_eq!(st, 200, "{w}");
        assert_eq!((w["balance"].clone(), w["bought"].clone(), w["baseline"].clone()), (json!(1000), json!(700), json!(300)));
        assert!(w["economics"].is_object() && w["credits"].is_object());
        assert_eq!(anon(&app, "GET", "/v1/wallet", None).await.0, 401);
        assert_eq!(anon(&app, "GET", "/v1/economics", None).await.0, 200);
    }

    #[tokio::test]
    async fn shared_data_earns_up_to_the_daily_cap() {
        let app = testkit::app().await;
        let kp = funded(&app, "a", "51900000001", 0, 0).await;
        let id = kp.id().to_string();
        assert_eq!(reward_data(&app, &Keypair::generate().id().to_string()).await, None, "unlinked agents share free");
        let r = data_reward();
        let mut total = 0;
        while let Some(got) = reward_data(&app, &id).await { total += got; assert!(total <= data_reward_daily_cap()); if total > 100_000 { break; } }
        assert!(total + r > data_reward_daily_cap());
        assert_eq!(credits::balance(&app, "a").await.unwrap(), total);
    }

    #[test]
    fn short_ids_are_prefixed() {
        assert_eq!(short("agent:abcdef123456"), "agent:abcdef12…");
        assert_eq!(short("xyz"), "agent:xyz…");
        assert_eq!(payment_required("x", 5, 2).0, StatusCode::PAYMENT_REQUIRED);
        assert!(economics_json_has_fees());
    }

    fn economics_json_has_fees() -> bool {
        expiry_terms().is_object()
    }
}

