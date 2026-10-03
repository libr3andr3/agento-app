//! The yaya exchange — GNU Taler's shape, in soles, on our ledger.
//!
//! * A **denomination** is an RSA-2048 key worth a fixed amount (S/ 0.01,
//!   0.10, 1, 5, 10, 20, 50 — céntimos included: prices need them). `GET /v1/exchange/keys` publishes the public halves,
//!   signed by the registry key, so every phone can pin them and verify
//!   coins **offline**.
//! * A **coin** is an ephemeral ed25519 identity (`agent:<hex>`, our usual
//!   envelope format) whose id was **blind-signed** by a denomination key
//!   (RSA-PSS blind signatures, RFC 9474 shape). We sign what we cannot
//!   read, so a withdrawal and a later deposit are unlinkable.
//! * **Withdraw** debits the account's balance and blind-signs; the ledger
//!   account `coins` holds what is in circulation. **Deposit** verifies the
//!   coin, checks it was never spent, credits the depositor. **Refresh**
//!   swaps received coins for fresh blinded ones (same value): the recipient
//!   of a peer-to-peer payment does this the moment it is online, which is
//!   what makes an offline payment final.
//! * Spending is a signed statement by the coin's own key: `{coin, to,
//!   amount, nonce, at}` — the recipient checks it offline, the exchange
//!   checks it at deposit (`to` must be the depositor).
//!
//! Money in/out of coins is `EXCHANGE_ENABLED` (regulatory); keys are
//! always served. Per-account daily withdraw cap `EXCHANGE_WITHDRAW_PER_DAY_MINOR`.

use std::collections::HashMap;

use axum::{extract::{Path, State}, http::StatusCode, response::IntoResponse, Extension, Json};
use base64::Engine;
use blind_rsa_signatures::{BlindSignature, BlindingResult, KeyPair, MessageRandomizer, PublicKey, Randomized, SecretKey, Sha384, Signature, PSS};
use serde_json::{json, Value};

use crate::{accounts, credits, err, internal, wallet, ApiResult, App, Auth, Shared};

type Kp = KeyPair<Sha384, PSS, Randomized>;
type Pk = PublicKey<Sha384, PSS, Randomized>;
type Sk = SecretKey<Sha384, PSS, Randomized>;

pub const COINS_ACCOUNT: &str = "coins";

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn unb64(s: &str) -> Result<Vec<u8>, (StatusCode, Json<Value>)> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|_| err(StatusCode::BAD_REQUEST, "bad base64"))
}

pub fn enabled() -> bool {
    std::env::var("EXCHANGE_ENABLED").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}
fn withdraw_cap() -> i64 {
    std::env::var("EXCHANGE_WITHDRAW_PER_DAY_MINOR").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000)
}
fn denom_values() -> Vec<i64> {
    std::env::var("EXCHANGE_DENOMS_MINOR").ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse::<i64>().ok()).filter(|x| *x > 0).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![1, 10, 100, 500, 1000, 2000, 5000])
}

pub struct Denom {
    pub id: String,
    pub value: i64,
    pub pk: Pk,
    sk: Sk,
    pub pk_pem: String,
    pub expires_at: String,
}

pub struct Exchange {
    pub denoms: HashMap<String, Denom>,
}

impl Exchange {
    /// Loads the denomination keys, minting any that are missing. RSA
    /// keygen is slow: done once per denomination, ever.
    pub async fn load(db: &sqlx::SqlitePool) -> anyhow::Result<Self> {
        let rows: Vec<(String, i64, String, String, String)> = sqlx::query_as("SELECT id, value_minor, sk_pem, pk_pem, expires_at FROM denominations WHERE retired = 0").fetch_all(db).await?;
        let mut denoms = HashMap::new();
        for (id, value, sk_pem, pk_pem, exp) in rows {
            let sk = Sk::from_pem(&sk_pem)?;
            let pk = Pk::from_pem(&pk_pem)?;
            denoms.insert(id.clone(), Denom { id, value, pk, sk, pk_pem, expires_at: exp });
        }
        for v in denom_values() {
            if denoms.values().any(|d| d.value == v) {
                continue;
            }
            let id = format!("d{v}-{}", chrono::Utc::now().format("%Y%m"));
            let kp = tokio::task::spawn_blocking(|| Kp::generate(&mut blind_rsa_signatures::DefaultRng, 2048)).await??;
            let (sk_pem, pk_pem) = (kp.sk.to_pem()?, kp.pk.to_pem()?);
            let exp = (chrono::Utc::now() + chrono::Duration::days(730)).to_rfc3339();
            sqlx::query("INSERT INTO denominations (id, value_minor, sk_pem, pk_pem, expires_at) VALUES ($1,$2,$3,$4,$5)")
                .bind(&id).bind(v).bind(&sk_pem).bind(&pk_pem).bind(&exp).execute(db).await?;
            tracing::info!(%id, value = v, "denomination key minted");
            denoms.insert(id.clone(), Denom { id, value: v, pk: kp.pk, sk: kp.sk, pk_pem, expires_at: exp });
        }
        Ok(Self { denoms })
    }

    fn by_value(&self, v: i64) -> Option<&Denom> {
        self.denoms.values().filter(|d| d.value == v).max_by(|a, b| a.expires_at.cmp(&b.expires_at))
    }

    /// The signed key list phones pin.
    pub fn keys_doc(&self, app: &App) -> Value {
        let mut list: Vec<&Denom> = self.denoms.values().collect();
        list.sort_by_key(|d| (d.value, d.id.clone()));
        let mut doc = json!({
            "exchange": {"id": app.registry.id().to_string(), "did": app.registry.did(), "url": app.public_url, "name": "yaya exchange"},
            "currency": credits::CURRENCY,
            "scheme": "rsabssa-sha384-pss-randomized",
            "coinId": "agent:<hex> — an ed25519 key; the signed message is the id's bytes",
            "denominations": list.iter().map(|d| json!({"id": d.id, "value": d.value, "pk": d.pk_pem, "expiresAt": d.expires_at})).collect::<Vec<_>>(),
            "enabled": enabled(),
            "withdrawPerDay": withdraw_cap(),
            "issuedAt": chrono::Utc::now().to_rfc3339(),
        });
        let sig = app.registry.sign_hex(&yaya_wire::canonical(&doc));
        doc["signature"] = json!({"alg": "ed25519", "signer": app.registry.id().to_string(), "sig": sig});
        doc
    }
}

/// `GET /v1/exchange/keys` — public.
pub async fn keys(State(app): State<Shared>) -> ApiResult {
    Ok(Json(app.exchange.keys_doc(&app)).into_response())
}

fn regulatory() -> (StatusCode, Json<Value>) {
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": {"message": "the exchange is not enabled yet (pending regulatory approval): coins can be verified and passed around, not minted or redeemed here", "type": "regulatory"}})))
}

#[derive(serde::Deserialize)]
pub struct BlindReq {
    denomination: String,
    blinded: String,
}

#[derive(serde::Deserialize)]
pub struct WithdrawReq {
    coins: Vec<BlindReq>,
}

fn blind_sign(d: &Denom, blinded_b64: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let bm = unb64(blinded_b64)?;
    let sig = d.sk.blind_sign(&bm).map_err(|e| err(StatusCode::BAD_REQUEST, format!("blind sign: {e}")))?;
    Ok(b64(&sig.0))
}

/// `POST /v1/exchange/withdraw` — debit the balance, sign blindly.
pub async fn withdraw(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<WithdrawReq>) -> ApiResult {
    if !enabled() { return Err(regulatory()); }
    let account = accounts::account_of_auth(&app, &auth).await?;
    if req.coins.is_empty() || req.coins.len() > 200 {
        return Err(err(StatusCode::BAD_REQUEST, "1..=200 coins per withdrawal"));
    }
    let mut total = 0i64;
    for c in &req.coins {
        let d = app.exchange.denoms.get(&c.denomination).ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("unknown denomination {}", c.denomination)))?;
        total += d.value;
    }
    // Sign first: it is pure, and a malformed blinded value must fail *before*
    // any balance moves. Then cap, debit and record in one transaction.
    let mut sigs = Vec::with_capacity(req.coins.len());
    for c in &req.coins {
        let d = &app.exchange.denoms[&c.denomination];
        sigs.push(blind_sign(d, &c.blinded)?);
    }
    let id = format!("wd_{}", uuid::Uuid::new_v4().simple());
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    // The cap is read inside the write transaction, so racing withdrawals
    // cannot each see the same "today" and together pass it.
    let (today,): (Option<i64>,) = sqlx::query_as("SELECT SUM(amount) FROM withdrawals WHERE account = $1 AND at >= strftime('%Y-%m-%dT00:00:00Z','now')").bind(&account).fetch_one(&mut *tx).await.map_err(internal)?;
    if today.unwrap_or(0) + total > withdraw_cap() {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, format!("daily withdrawal cap {} {:.2}", credits::CURRENCY, withdraw_cap() as f64 / 100.0)));
    }
    // Coins are bearer value that never expires: only bought/earned balance
    // may become coins — the plan's baseline stays in the account.
    let moved = wallet::move_bought_in(&mut tx, &account, COINS_ACCOUNT, total, 0, "withdraw", "withdraw", Some(&id), "monedas retiradas", "coins issued").await?;
    if moved.is_none() {
        drop(tx);
        return Err(wallet::payment_required("withdraw (only bought/earned balance becomes coins; plan credits stay in the account)", total, wallet::bought_balance(&app, &account).await?));
    }
    sqlx::query("INSERT INTO withdrawals (id, account, amount, coins) VALUES ($1,$2,$3,$4)").bind(&id).bind(&account).bind(total).bind(req.coins.len() as i64).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    tracing::info!(%account, total, coins = req.coins.len(), "coins withdrawn");
    Ok(Json(json!({"id": id, "amountMinor": total, "signatures": sigs, "balance": credits::balance(&app, &account).await?})).into_response())
}

/// A coin as presented for deposit/refresh.
#[derive(serde::Deserialize, Clone)]
pub struct CoinIn {
    pub coin: String,
    pub denomination: String,
    pub sig: String,
    pub randomizer: String,
    /// Envelope signed by the coin key: payload {coin, to, amountMinor?, nonce, at}
    pub spend: Value,
}

/// Verifies the exchange signature and the spend statement; returns the value.
pub fn verify_coin(ex: &Exchange, c: &CoinIn, expected_to: &str) -> Result<i64, String> {
    let d = ex.denoms.get(&c.denomination).ok_or_else(|| format!("unknown denomination {}", c.denomination))?;
    if !yaya_wire::AgentId::looks_valid(&c.coin) {
        return Err("bad coin id".into());
    }
    let sig = Signature(base64::engine::general_purpose::STANDARD.decode(c.sig.trim()).map_err(|_| "bad sig")?);
    let r: [u8; 32] = base64::engine::general_purpose::STANDARD.decode(c.randomizer.trim()).map_err(|_| "bad randomizer")?.try_into().map_err(|_| "randomizer must be 32 bytes")?;
    d.pk.verify(&sig, Some(MessageRandomizer(r)), c.coin.as_bytes()).map_err(|_| "exchange signature does not verify".to_string())?;
    let signer = yaya_wire::envelope::verify(&c.spend).map_err(|e| format!("spend statement: {e}"))?;
    if signer.to_string() != c.coin {
        return Err("spend statement not signed by the coin".into());
    }
    let p = &c.spend["payload"];
    if p["coin"].as_str() != Some(c.coin.as_str()) {
        return Err("spend statement is for another coin".into());
    }
    if p["to"].as_str() != Some(expected_to) {
        return Err(format!("coin was spent to {} not to you", p["to"].as_str().unwrap_or("?")));
    }
    Ok(d.value)
}

#[derive(serde::Deserialize)]
pub struct DepositReq {
    coins: Vec<CoinIn>,
}

/// Marks coins spent inside `tx`; the whole batch fails on any double spend
/// (a coin listed twice in one batch counts as one).
async fn spend_all_in(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, coins: &[CoinIn], kind: &str, account: &str) -> Result<(), (StatusCode, Json<Value>)> {
    for c in coins {
        let n = sqlx::query("INSERT OR IGNORE INTO spent_coins (coin, denomination, kind, account) VALUES ($1,$2,$3,$4)")
            .bind(&c.coin).bind(&c.denomination).bind(kind).bind(account).execute(&mut **tx).await.map_err(internal)?.rows_affected();
        if n == 0 {
            return Err((StatusCode::CONFLICT, Json(json!({"error": {"message": format!("coin {} was already spent", c.coin), "type": "double_spend", "coin": c.coin}}))));
        }
    }
    Ok(())
}

/// Marks coins spent atomically; the whole batch fails on any double spend.
async fn spend_all(app: &App, coins: &[CoinIn], kind: &str, account: &str) -> Result<(), (StatusCode, Json<Value>)> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    spend_all_in(&mut tx, coins, kind, account).await?;
    tx.commit().await.map_err(internal)?;
    Ok(())
}

/// The agent id the depositor presents as (coins must have been spent *to* it).
async fn depositor(app: &App, auth: &Auth) -> Result<(String, String), (StatusCode, Json<Value>)> {
    let account = accounts::account_of_auth(app, auth).await?;
    let to = match auth {
        Auth::Proven(a) | Auth::Unproven(a) => a.to_string(),
        _ => format!("acct:{account}"),
    };
    Ok((account, to))
}

/// `POST /v1/exchange/deposit` — coins spent to me become balance.
pub async fn deposit(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<DepositReq>) -> ApiResult {
    if !enabled() { return Err(regulatory()); }
    let (account, me) = depositor(&app, &auth).await?;
    if req.coins.is_empty() || req.coins.len() > 200 {
        return Err(err(StatusCode::BAD_REQUEST, "1..=200 coins"));
    }
    let mut total = 0i64;
    for c in &req.coins {
        // Own withdrawn coins may be spent to oneself (agent or acct:).
        let ok_to = [me.as_str(), &format!("acct:{account}")];
        let v = ok_to.iter().find_map(|t| verify_coin(&app.exchange, c, t).ok()).ok_or_else(|| {
            let e = verify_coin(&app.exchange, c, &me).err().unwrap_or_default();
            err(StatusCode::BAD_REQUEST, format!("coin {}: {e}", c.coin.chars().take(20).collect::<String>()))
        })?;
        total += v;
    }
    // Spending the coins and crediting the account are one transaction: a crash
    // between them must not burn the depositor's coins. The `coins` account holds
    // exactly what is in circulation; if it cannot cover a valid deposit the
    // exchange has minted more than it issued — refuse loudly, never silently.
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    spend_all_in(&mut tx, &req.coins, "deposit", &account).await?;
    let moved = wallet::move_credits_in(&mut tx, COINS_ACCOUNT, &account, total, 0, "deposit", "deposit", None, "coins redeemed", "monedas depositadas").await?;
    if moved.is_none() {
        drop(tx);
        tracing::error!(%account, total, "circulation invariant violated: coins account cannot cover a valid deposit");
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "exchange cannot cover this deposit — circulation invariant violated, operator alerted"));
    }
    tx.commit().await.map_err(internal)?;
    tracing::info!(%account, total, coins = req.coins.len(), "coins deposited");
    Ok(Json(json!({"ok": true, "amountMinor": total, "coins": req.coins.len(), "balance": credits::balance(&app, &account).await?})).into_response())
}

#[derive(serde::Deserialize)]
pub struct RefreshReq {
    coins: Vec<CoinIn>,
    blinded: Vec<BlindReq>,
}

/// `POST /v1/exchange/refresh` — received coins in, fresh blinded coins out,
/// same value. The account balance never moves; circulation stays equal.
pub async fn refresh(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<RefreshReq>) -> ApiResult {
    if !enabled() { return Err(regulatory()); }
    let (account, me) = depositor(&app, &auth).await?;
    let mut old = 0i64;
    for c in &req.coins {
        old += verify_coin(&app.exchange, c, &me).map_err(|e| err(StatusCode::BAD_REQUEST, format!("coin {}: {e}", c.coin.chars().take(20).collect::<String>())))?;
    }
    let mut new = 0i64;
    for b in &req.blinded {
        new += app.exchange.denoms.get(&b.denomination).ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("unknown denomination {}", b.denomination)))?.value;
    }
    if old == 0 || old != new {
        return Err(err(StatusCode::BAD_REQUEST, format!("refresh must keep the value: in {old}, out {new}")));
    }
    // Sign first (pure, and it can fail on a malformed blinded value), then
    // burn the coins: a refused refresh must leave the coins spendable.
    let mut sigs = Vec::new();
    for b in &req.blinded {
        sigs.push(blind_sign(&app.exchange.denoms[&b.denomination], &b.blinded)?);
    }
    spend_all(&app, &req.coins, "refresh", &account).await?;
    tracing::info!(%account, value = old, coins = req.coins.len(), "coins refreshed");
    Ok(Json(json!({"ok": true, "amountMinor": old, "signatures": sigs})).into_response())
}

/// `GET /v1/exchange/coins/{coin}` — spent? (any bearer; for online double-spend checks)
pub async fn coin_status(State(app): State<Shared>, Path(coin): Path<String>) -> ApiResult {
    let row: Option<(String, String)> = sqlx::query_as("SELECT kind, spent_at FROM spent_coins WHERE coin = $1").bind(&coin).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"coin": coin, "spent": row.is_some(), "kind": row.as_ref().map(|r| r.0.clone()), "at": row.map(|r| r.1)})).into_response())
}

/// Circulation for /admin/metrics and the wallet page.
pub async fn circulation(app: &App) -> i64 {
    credits::balance(app, COINS_ACCOUNT).await.unwrap_or(0)
}

/// Client-side helpers shared with tests: blind a coin id for a denomination.
pub fn blind(pk: &Pk, coin_id: &str) -> anyhow::Result<BlindingResult> {
    Ok(pk.blind(&mut blind_rsa_signatures::DefaultRng, coin_id.as_bytes())?)
}
pub fn finalize(pk: &Pk, blind_sig_b64: &str, br: &BlindingResult, coin_id: &str) -> anyhow::Result<Signature> {
    let bs = BlindSignature(base64::engine::general_purpose::STANDARD.decode(blind_sig_b64)?);
    Ok(pk.finalize(&bs, br, coin_id.as_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blind_sign_finalize_verify_and_spend() {
        let kp = Kp::generate(&mut blind_rsa_signatures::DefaultRng, 2048).unwrap();
        let d = Denom { id: "d100-test".into(), value: 100, pk_pem: kp.pk.to_pem().unwrap(), pk: kp.pk.clone(), sk: kp.sk.clone(), expires_at: "2099".into() };
        let mut ex = Exchange { denoms: HashMap::new() };
        ex.denoms.insert(d.id.clone(), d);
        // Client: an ephemeral coin identity.
        let coin = yaya_wire::Keypair::generate();
        let id = coin.id().to_string();
        let br = blind(&ex.denoms["d100-test"].pk, &id).unwrap();
        // Exchange signs what it cannot read.
        let bs = blind_sign(&ex.denoms["d100-test"], &b64(&br.blind_message.0)).unwrap();
        let sig = finalize(&ex.denoms["d100-test"].pk, &bs, &br, &id).unwrap();
        // Spend to bob, verify as bob.
        let spend = coin.envelope(json!({"coin": id, "to": "agent:bob", "nonce": "n1", "at": "now"}));
        let c = CoinIn { coin: id.clone(), denomination: "d100-test".into(), sig: b64(&sig.0), randomizer: b64(&br.msg_randomizer.unwrap().0), spend: spend.clone() };
        assert_eq!(verify_coin(&ex, &c, "agent:bob").unwrap(), 100);
        assert!(verify_coin(&ex, &c, "agent:eve").unwrap_err().contains("not to you"));
        // A tampered signature fails; a spend signed by another key fails.
        let mut bad = c.clone(); bad.sig = b64(&vec![0u8; 256]);
        assert!(verify_coin(&ex, &bad, "agent:bob").is_err());
        let other = yaya_wire::Keypair::generate();
        let mut forged = c.clone(); forged.spend = other.envelope(json!({"coin": id, "to": "agent:bob"}));
        assert!(verify_coin(&ex, &forged, "agent:bob").unwrap_err().contains("not signed by the coin"));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_agent, Keypair};

    struct Minted { kp: yaya_wire::Keypair, denom: String, sig: String, randomizer: String }

    fn spend_to(m: &Minted, to: &str) -> Value {
        let id = m.kp.id().to_string();
        json!({"coin": id, "denomination": m.denom, "sig": m.sig, "randomizer": m.randomizer,
               "spend": m.kp.envelope(json!({"coin": id, "to": to, "nonce": uuid::Uuid::new_v4().to_string(), "at": "now"}))})
    }

    /// Blinds `n` fresh coins of `value`, the client side of a withdrawal.
    fn blinded(app: &App, value: i64, n: usize) -> (Vec<(yaya_wire::Keypair, BlindingResult)>, Vec<Value>, String) {
        let d = app.exchange.by_value(value).unwrap();
        let mut keep = Vec::new();
        let mut req = Vec::new();
        for _ in 0..n {
            let kp = yaya_wire::Keypair::generate();
            let br = blind(&d.pk, &kp.id().to_string()).unwrap();
            req.push(json!({"denomination": d.id, "blinded": b64(&br.blind_message.0)}));
            keep.push((kp, br));
        }
        (keep, req, d.id.clone())
    }

    fn finish(app: &App, denom: &str, keep: Vec<(yaya_wire::Keypair, BlindingResult)>, sigs: &Value) -> Vec<Minted> {
        let pk = &app.exchange.denoms[denom].pk;
        keep.into_iter().zip(sigs.as_array().unwrap()).map(|((kp, br), s)| {
            let sig = finalize(pk, s.as_str().unwrap(), &br, &kp.id().to_string()).unwrap();
            Minted { kp, denom: denom.to_string(), sig: b64(&sig.0), randomizer: b64(&br.msg_randomizer.unwrap().0) }
        }).collect()
    }

    /// EXCHANGE_ENABLED is process-wide, so the whole flow is one test.
    #[tokio::test]
    async fn coins_withdraw_pay_refresh_and_deposit_without_losing_value() {
        let app = testkit::app().await;
        let (alice, bob) = (Keypair::generate(), Keypair::generate());
        account_with_agent(&app, "alice", "51900000001", &alice).await;
        account_with_agent(&app, "bob", "51900000002", &bob).await;
        credits::add(&app, "alice", 1_000, "topup", None, None).await.unwrap();
        credits::add(&app, "alice", 5_000, "grant", None, None).await.unwrap();
        let (_, keys) = anon(&app, "GET", "/v1/exchange/keys", None).await;
        assert_eq!(keys["denominations"].as_array().unwrap().len(), denom_values().len());
        assert!(keys["signature"]["sig"].is_string());

        std::env::remove_var("EXCHANGE_ENABLED");
        let (keep, req, denom) = blinded(&app, 100, 3);
        assert_eq!(as_agent(&app, &alice, "POST", "/v1/exchange/withdraw", Some(json!({"coins": req}))).await.0, 503);
        std::env::set_var("EXCHANGE_ENABLED", "1");
        assert_eq!(as_agent(&app, &alice, "POST", "/v1/exchange/withdraw", Some(json!({"coins": []}))).await.0, 400);
        assert_eq!(as_agent(&app, &alice, "POST", "/v1/exchange/withdraw", Some(json!({"coins": [{"denomination": "nope", "blinded": "AA=="}]}))).await.0, 400);
        let (big, big_req, _) = blinded(&app, 1000, 2);
        drop(big);
        assert_eq!(as_agent(&app, &alice, "POST", "/v1/exchange/withdraw", Some(json!({"coins": big_req}))).await.0, 402, "plan credits never become coins");
        let (st, w) = as_agent(&app, &alice, "POST", "/v1/exchange/withdraw", Some(json!({"coins": req}))).await;
        assert_eq!((st, w["amountMinor"].clone(), w["balance"].clone()), (200, json!(300), json!(5_700)), "{w}");
        assert_eq!(circulation(&app).await, 300);
        let coins = finish(&app, &denom, keep, &w["signatures"]);

        // Alice pays bob two coins offline; bob deposits them.
        let bob_id = bob.id().to_string();
        let pay = json!({"coins": [spend_to(&coins[0], &bob_id), spend_to(&coins[1], &bob_id)]});
        assert_eq!(as_agent(&app, &alice, "POST", "/v1/exchange/deposit", Some(pay.clone())).await.0, 400, "spent to bob, not to alice");
        let (st, d) = as_agent(&app, &bob, "POST", "/v1/exchange/deposit", Some(pay.clone())).await;
        assert_eq!((st, d["balance"].clone()), (200, json!(200)), "{d}");
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/deposit", Some(pay)).await.0, 409, "double spend");
        assert_eq!(anon(&app, "GET", &format!("/v1/exchange/coins/{}", coins[0].kp.id()), None).await.1["spent"], true);
        let junk = json!({"coins": [{"coin": "€".repeat(12), "denomination": denom, "sig": "", "randomizer": "", "spend": {}}]});
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/deposit", Some(junk.clone())).await.0, 400, "a junk coin id is a 400, not a panic");
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/refresh", Some(json!({"coins": junk["coins"], "blinded": []}))).await.0, 400);
        let twice = json!({"coins": [spend_to(&coins[2], &bob_id), spend_to(&coins[2], &bob_id)]});
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/deposit", Some(twice)).await.0, 409, "one coin twice in a batch");
        assert_eq!(credits::balance(&app, "bob").await.unwrap(), 200, "a refused batch leaves no trace");

        // Refresh: a malformed blinded output must not burn the coin going in.
        let d = app.exchange.by_value(100).unwrap().id.clone();
        let bad = json!({"coins": [spend_to(&coins[2], &bob_id)], "blinded": [{"denomination": d, "blinded": b64(b"not a blinded message")}]});
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/refresh", Some(bad)).await.0, 400);
        assert_eq!(anon(&app, "GET", &format!("/v1/exchange/coins/{}", coins[2].kp.id()), None).await.1["spent"], false, "the coin survives a failed refresh");
        let (keep, req, _) = blinded(&app, 100, 1);
        let wrong_value = json!({"coins": [spend_to(&coins[2], &bob_id)], "blinded": [req[0].clone(), req[0].clone()]});
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/refresh", Some(wrong_value)).await.0, 400);
        let (st, r) = as_agent(&app, &bob, "POST", "/v1/exchange/refresh", Some(json!({"coins": [spend_to(&coins[2], &bob_id)], "blinded": req}))).await;
        assert_eq!(st, 200, "{r}");
        let fresh = finish(&app, &denom, keep, &r["signatures"]);
        let (_, d) = as_agent(&app, &bob, "POST", "/v1/exchange/deposit", Some(json!({"coins": [spend_to(&fresh[0], &format!("acct:bob"))]}))).await;
        assert_eq!(d["balance"], 300, "own coins may be spent to the account");
        assert_eq!(circulation(&app).await, 0, "everything issued came back");

        // The daily cap counts what was already withdrawn today.
        credits::add(&app, "bob", 30_000, "topup", None, None).await.unwrap();
        let (_, req, _) = blinded(&app, 5000, 5);
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/withdraw", Some(json!({"coins": req[..4].to_vec()}))).await.0, 200);
        assert_eq!(as_agent(&app, &bob, "POST", "/v1/exchange/withdraw", Some(json!({"coins": req[4..].to_vec()}))).await.0, 429);
        std::env::remove_var("EXCHANGE_ENABLED");
    }
}

