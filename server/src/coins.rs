//! Coins — GNU Taler's idea, in this agent's pocket.
//!
//! The exchange (the gateway, `/v1/exchange/*`) blind-signs coins per
//! denomination and publishes its keys signed by the registry key. This
//! module keeps the coins, and does the four things a Taler wallet does:
//!
//! * **withdraw** — split an amount into denominations, mint a fresh
//!   ed25519 key per coin, blind its id, get it signed, unblind, verify;
//! * **pay** — pick coins summing to the amount, sign a spend statement
//!   `{coin, to, amountMinor, nonce, at}` with each coin's key, hand the
//!   payment over — by relay box (`kind: "coins"`), over the mesh (A2A
//!   `yaya/coins`), or as JSON for Bluetooth/QR;
//! * **receive** — verify every coin **offline**: exchange signature under
//!   the pinned denomination key, spend statement signed by the coin, spent
//!   *to me*, never seen before. Store as `received`;
//! * **refresh / deposit** — when online, swap received coins for fresh
//!   ones we own (unlinkable; makes the payment final: the exchange will
//!   refuse a double spend) or turn coins into balance.
//!
//! No clearnet is involved in a payment itself: coins move device to device.

use base64::Engine;
use blind_rsa_signatures::{BlindSignature, MessageRandomizer, PublicKey, Randomized, Sha384, Signature, PSS};
use serde_json::{json, Value};
use yaya_wire::Keypair;

use crate::{AppState, SharedState};

type Pk = PublicKey<Sha384, PSS, Randomized>;
pub const KIND_PAYMENT: &str = "coins";
pub const KIND_RECEIPT: &str = "coins_receipt";
const KEYS_MAX_AGE_SECS: i64 = 6 * 3600;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn unb64(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s.trim())?)
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// --------------------------------------------------------- exchange keys

pub struct Denom {
    pub id: String,
    pub value: i64,
    pub pk: Pk,
}

/// Checks the registry signature over the keys document.
fn verify_keys_doc(doc: &Value) -> anyhow::Result<String> {
    let signer = doc["signature"]["signer"].as_str().ok_or_else(|| anyhow::anyhow!("keys: no signer"))?.to_string();
    let sig = hex::decode(doc["signature"]["sig"].as_str().unwrap_or(""))?;
    let mut body = doc.clone();
    body.as_object_mut().map(|o| o.remove("signature"));
    let id: yaya_wire::AgentId = signer.parse().map_err(|_| anyhow::anyhow!("keys: bad signer id"))?;
    use ed25519_dalek::Verifier;
    id.verifying_key().verify(&yaya_wire::canonical(&body), &ed25519_dalek::Signature::from_slice(&sig)?)
        .map_err(|_| anyhow::anyhow!("keys: registry signature does not verify"))?;
    Ok(signer)
}

/// The pinned keys document, refreshed when stale; TOFU on the signer.
pub async fn keys(state: &AppState, force: bool) -> anyhow::Result<Value> {
    let cached: Option<(String, String, String)> = sqlx::query_as("SELECT exchange, doc, fetched_at FROM exchange_keys WHERE id = 1").fetch_optional(&state.db).await?;
    if let Some((_, doc, at)) = &cached {
        let fresh = chrono::DateTime::parse_from_rfc3339(at).map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < KEYS_MAX_AGE_SECS).unwrap_or(false);
        if fresh && !force {
            return Ok(serde_json::from_str(doc)?);
        }
    }
    match state.registry.get("/v1/exchange/keys", std::time::Duration::from_secs(20)).await {
        Ok(doc) => {
            let signer = verify_keys_doc(&doc)?;
            if let Some((pinned, _, _)) = &cached {
                anyhow::ensure!(pinned == &signer, "exchange signer changed ({pinned} → {signer}); refusing");
            }
            sqlx::query("INSERT INTO exchange_keys (id, exchange, doc, fetched_at) VALUES (1, $1, $2, $3) ON CONFLICT (id) DO UPDATE SET doc = excluded.doc, fetched_at = excluded.fetched_at")
                .bind(&signer).bind(doc.to_string()).bind(now()).execute(&state.db).await?;
            Ok(doc)
        }
        Err(e) => match cached {
            Some((_, doc, _)) => Ok(serde_json::from_str(&doc)?),
            None => Err(anyhow::anyhow!("exchange keys not available yet ({e})")),
        },
    }
}

pub fn denoms(doc: &Value) -> Vec<Denom> {
    let mut v: Vec<Denom> = doc["denominations"].as_array().into_iter().flatten().filter_map(|d| {
        Some(Denom { id: d["id"].as_str()?.to_string(), value: d["value"].as_i64()?, pk: Pk::from_pem(d["pk"].as_str()?).ok()? })
    }).collect();
    v.sort_by(|a, b| b.value.cmp(&a.value));
    v
}

/// Greedy split into denominations, largest first. Exact or nothing.
pub fn split(amount_minor: i64, values_desc: &[i64]) -> Option<Vec<i64>> {
    let mut left = amount_minor;
    let mut out = Vec::new();
    for v in values_desc {
        while left >= *v {
            out.push(*v);
            left -= v;
        }
    }
    (left == 0 && !out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------- coins

#[derive(sqlx::FromRow, Clone)]
struct CoinRow {
    coin: String,
    secret: Option<Vec<u8>>,
    denomination: String,
    value_minor: i64,
    sig: String,
    randomizer: String,
    spend: Option<String>,
    status: String,
}

const COLS: &str = "coin, secret, denomination, value_minor, sig, randomizer, spend, status";

fn coin_key(row: &CoinRow) -> anyhow::Result<Keypair> {
    let sealed = row.secret.as_ref().ok_or_else(|| anyhow::anyhow!("no key for {}", row.coin))?;
    let seed: [u8; 32] = crate::identity::from_rest(sealed, Some(32))?.try_into().map_err(|_| anyhow::anyhow!("bad seed"))?;
    Ok(Keypair::from_seed(seed))
}

/// The coin as the exchange/peer expects it, with a spend statement to `to`.
fn present(row: &CoinRow, to: &str, nonce: &str) -> anyhow::Result<Value> {
    let spend = match &row.spend {
        Some(s) if row.secret.is_none() => serde_json::from_str::<Value>(s)?,
        _ => coin_key(row)?.envelope(json!({"coin": row.coin, "to": to, "amountMinor": row.value_minor, "nonce": nonce, "at": now()})),
    };
    Ok(json!({"coin": row.coin, "denomination": row.denomination, "value": row.value_minor, "sig": row.sig, "randomizer": row.randomizer, "spend": spend}))
}

/// Withdraw `amount_minor` from the account balance into fresh coins.
pub async fn withdraw(state: &AppState, amount_minor: i64) -> anyhow::Result<Value> {
    anyhow::ensure!(amount_minor > 0, "amount must be positive");
    let doc = keys(state, false).await?;
    let ds = denoms(&doc);
    anyhow::ensure!(!ds.is_empty(), "the exchange has no denominations");
    let values: Vec<i64> = ds.iter().map(|d| d.value).collect();
    let plan = split(amount_minor, &values).ok_or_else(|| anyhow::anyhow!("amount must be a sum of coins {:?}", values.iter().map(|v| format!("S/ {:.2}", *v as f64 / 100.0)).collect::<Vec<_>>()))?;
    let mut minted = Vec::new();
    let mut req = Vec::new();
    for v in &plan {
        let d = ds.iter().find(|d| d.value == *v).unwrap();
        let kp = Keypair::generate();
        let id = kp.id().to_string();
        let br = d.pk.blind(&mut blind_rsa_signatures::DefaultRng, id.as_bytes())?;
        req.push(json!({"denomination": d.id, "blinded": b64(&br.blind_message.0)}));
        minted.push((kp, d.id.clone(), *v, br));
    }
    let r = state.registry.post("/v1/exchange/withdraw", &json!({"coins": req}), std::time::Duration::from_secs(60)).await?;
    let sigs = r["signatures"].as_array().cloned().unwrap_or_default();
    anyhow::ensure!(sigs.len() == minted.len(), "exchange returned {} signatures for {} coins", sigs.len(), minted.len());
    let mut stored = 0;
    for ((kp, did, v, br), bs) in minted.into_iter().zip(sigs) {
        let d = ds.iter().find(|d| d.id == did).unwrap();
        let bs = BlindSignature(unb64(bs.as_str().unwrap_or(""))?);
        let id = kp.id().to_string();
        let sig = d.pk.finalize(&bs, &br, id.as_bytes())?;
        let rnd = br.msg_randomizer.ok_or_else(|| anyhow::anyhow!("no randomizer"))?;
        sqlx::query("INSERT INTO coins (coin, secret, denomination, value_minor, sig, randomizer, status) VALUES ($1,$2,$3,$4,$5,$6,'fresh')")
            .bind(&id).bind(crate::identity::at_rest(&kp.seed())?).bind(&did).bind(v).bind(b64(&sig.0)).bind(b64(&rnd.0))
            .execute(&state.db).await?;
        stored += 1;
    }
    tracing::info!(amount = amount_minor, coins = stored, "coins withdrawn");
    Ok(json!({"ok": true, "amountMinor": amount_minor, "coins": stored, "balance": r["balance"], "status": status(state).await}))
}

// Coins we can spend are only `fresh` ones (withdrawn or refreshed by us):
// a received coin carries the payer's statement and is refreshed or
// deposited, never passed on.

fn pick(coins: &[CoinRow], amount: i64) -> Option<Vec<CoinRow>> {
    // Exact sum, each coin at most once. Greedy alone misses sums it could
    // make (S/ 6 from 5+2+2+2), so search per denomination, largest first
    // and as many of each as fit, backtracking with a memo of dead ends.
    if amount <= 0 {
        return None;
    }
    let mut values: Vec<i64> = coins.iter().map(|c| c.value_minor).filter(|v| *v > 0).collect();
    values.sort_unstable_by(|a, b| b.cmp(a));
    values.dedup();
    let have: Vec<usize> = values.iter().map(|v| coins.iter().filter(|c| c.value_minor == *v).count()).collect();
    fn search(i: usize, left: i64, values: &[i64], have: &[usize], take: &mut Vec<usize>, dead: &mut std::collections::HashSet<(usize, i64)>) -> bool {
        if left == 0 {
            return true;
        }
        if i == values.len() || dead.contains(&(i, left)) {
            return false;
        }
        let most = have[i].min((left / values[i]) as usize);
        for n in (0..=most).rev() {
            take[i] = n;
            if search(i + 1, left - n as i64 * values[i], values, have, take, dead) {
                return true;
            }
        }
        take[i] = 0;
        dead.insert((i, left));
        false
    }
    let mut take = vec![0; values.len()];
    if !search(0, amount, &values, &have, &mut take, &mut Default::default()) {
        return None;
    }
    // Keep the caller's order (oldest first within a value).
    let mut out = Vec::new();
    for (v, n) in values.iter().zip(take) {
        out.extend(coins.iter().filter(|c| c.value_minor == *v).take(n).cloned());
    }
    Some(out)
}

/// Build a payment to `to` worth `amount_minor`. Marks the coins spent.
pub async fn make_payment(state: &AppState, to: &str, amount_minor: i64, via: &str, note: Option<&str>) -> anyhow::Result<Value> {
    anyhow::ensure!(amount_minor > 0, "amount must be positive");
    // Network first, outside the write lock.
    let exchange = keys(state, false).await.ok().and_then(|d| d["signature"]["signer"].as_str().map(String::from));
    // Pick and spend under one write lock: two payments at once must never
    // both pick the same coin (the second payee would hold a double spend).
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    let have = sqlx::query_as::<_, CoinRow>(&format!("SELECT {COLS} FROM coins WHERE status = 'fresh' ORDER BY value_minor DESC, created_at")).fetch_all(&mut *tx).await?;
    let total: i64 = have.iter().map(|c| c.value_minor).sum();
    let chosen = pick(&have, amount_minor).ok_or_else(|| anyhow::anyhow!("cannot make exactly S/ {:.2} from the coins in the pocket (S/ {:.2} in {}); withdraw the right denominations", amount_minor as f64 / 100.0, total as f64 / 100.0, have.iter().map(|c| format!("S/{:.2}", c.value_minor as f64 / 100.0)).collect::<Vec<_>>().join(" ")))?;
    let nonce = format!("pay_{}", uuid::Uuid::new_v4().simple());
    let mut coins = Vec::new();
    for c in &chosen {
        coins.push(present(c, to, &nonce)?);
    }
    let payment = json!({
        "kind": KIND_PAYMENT, "v": 1, "from": state.identity.id(), "to": to, "amountMinor": amount_minor,
        "currency": "PEN", "nonce": nonce, "at": now(), "note": note, "coins": coins,
        "exchange": exchange,
    });
    for c in &chosen {
        let done = sqlx::query("UPDATE coins SET status = 'spent', peer = $1, payment = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE coin = $3 AND status = 'fresh'")
            .bind(to).bind(&nonce).bind(&c.coin).execute(&mut *tx).await?;
        anyhow::ensure!(done.rows_affected() == 1, "coin {} is no longer spendable", &c.coin[..20.min(c.coin.len())]);
    }
    sqlx::query("INSERT INTO coin_payments (id, direction, peer, amount, coins, via, note, payload) VALUES ($1,'out',$2,$3,$4,$5,$6,$7)")
        .bind(&nonce).bind(to).bind(amount_minor).bind(chosen.len() as i64).bind(via).bind(note).bind(payment.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!(%to, amount = amount_minor, coins = chosen.len(), via, "coins payment made");
    Ok(payment)
}

/// Pay `to` (agent id or @handle) — over the relay, the mesh, or as JSON
/// to hand over by Bluetooth/QR (`via = "offline"`).
pub async fn pay(state: &AppState, to: &str, amount_minor: i64, via: &str, note: Option<&str>) -> anyhow::Result<Value> {
    let to_id = resolve(state, to).await?;
    anyhow::ensure!(to_id != state.identity.id(), "that is you");
    // Where a mesh payment goes is known before any coin is spent: an
    // unlinked peer must not cost the payer the coins.
    let mesh_ip = if via == "mesh" {
        let ip: Option<(String,)> = sqlx::query_as("SELECT ip FROM mesh_peers WHERE agent = $1 AND status = 'linked'").bind(&to_id).fetch_optional(&state.db).await?;
        Some(ip.map(|r| r.0).ok_or_else(|| anyhow::anyhow!("{to} is not linked on the mesh (yaya mesh link)"))?)
    } else {
        None
    };
    let payment = make_payment(state, &to_id, amount_minor, via, note).await?;
    let delivered = match (via, mesh_ip) {
        ("relay", _) => { crate::network::send(state, &to_id, &payment).await?; json!("relay") }
        ("mesh", Some(ip)) => {
            let c = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;
            let r: Value = c.post(format!("http://{ip}:{}/a2a", crate::mesh::A2A_PORT)).json(&json!({"jsonrpc": "2.0", "id": payment["nonce"], "method": "yaya/coins", "params": payment})).send().await?.json().await?;
            anyhow::ensure!(r["result"]["ok"].as_bool().unwrap_or(false), "peer refused the payment: {}", r["error"]["message"].as_str().or(r["result"]["error"].as_str()).unwrap_or("?"));
            let _ = sqlx::query("UPDATE coin_payments SET acked = 1 WHERE id = $1").bind(payment["nonce"].as_str()).execute(&state.db).await;
            json!("mesh")
        }
        _ => json!("offline"),
    };
    Ok(json!({"ok": true, "to": to_id, "amountMinor": amount_minor, "delivered": delivered, "payment": payment, "status": status(state).await}))
}

async fn resolve(state: &AppState, who: &str) -> anyhow::Result<String> {
    let who = who.trim();
    if who.starts_with("agent:") {
        return Ok(who.to_string());
    }
    let handle = who.trim_start_matches("urn:agent:yaya:").trim_start_matches('@');
    let rec = crate::network::registry_get(state, &format!("/v1/index/urn:agent:yaya:{handle}")).await?;
    rec["agent_id"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("unknown agent '{who}'"))
}

/// Verify one presented coin offline. Returns its value.
pub fn verify_coin(ds: &[Denom], c: &Value, me: &str) -> Result<(String, i64), String> {
    let id = c["coin"].as_str().ok_or("coin: no id")?.to_string();
    if !yaya_wire::AgentId::looks_valid(&id) {
        return Err("coin: bad id".into());
    }
    let did = c["denomination"].as_str().ok_or("coin: no denomination")?;
    let d = ds.iter().find(|d| d.id == did).ok_or_else(|| format!("coin: unknown denomination {did}"))?;
    let sig = Signature(unb64(c["sig"].as_str().unwrap_or("")).map_err(|_| "coin: bad sig")?);
    let r: [u8; 32] = unb64(c["randomizer"].as_str().unwrap_or("")).map_err(|_| "coin: bad randomizer")?.try_into().map_err(|_| "coin: randomizer size")?;
    d.pk.verify(&sig, Some(MessageRandomizer(r)), id.as_bytes()).map_err(|_| "coin: exchange signature does not verify — not real money".to_string())?;
    let signer = yaya_wire::envelope::verify(&c["spend"]).map_err(|e| format!("coin: spend statement {e}"))?;
    if signer.to_string() != id {
        return Err("coin: spend statement not signed by the coin".into());
    }
    let p = &c["spend"]["payload"];
    if p["coin"].as_str() != Some(id.as_str()) || p["to"].as_str() != Some(me) {
        return Err(format!("coin: spent to {} — not to me", p["to"].as_str().unwrap_or("?")));
    }
    Ok((id, d.value))
}

/// Verifies every coin of a payment offline and sums them. A coin listed twice
/// is a forgery attempt (it would double-count), not a duplicate to tolerate.
pub fn verify_batch(ds: &[Denom], coins: &[Value], me: &str) -> Result<(Vec<(String, i64, Value)>, i64), String> {
    if coins.is_empty() {
        return Err("no coins in the payment".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(coins.len());
    let mut total = 0i64;
    for c in coins {
        let (id, v) = verify_coin(ds, c, me)?;
        if !seen.insert(id.clone()) {
            return Err(format!("coin {} is listed twice in one payment", &id[..20.min(id.len())]));
        }
        total += v;
        out.push((id, v, c.clone()));
    }
    Ok((out, total))
}

/// Receive a payment (from the relay, the mesh, Bluetooth, a pasted QR).
/// Every coin is verified offline; duplicates are refused.
pub async fn receive(state: &AppState, payment: &Value, via: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(payment["kind"].as_str() == Some(KIND_PAYMENT), "not a coins payment");
    let me = state.identity.id();
    anyhow::ensure!(payment["to"].as_str() == Some(me.as_str()), "payment is addressed to {}", payment["to"].as_str().unwrap_or("?"));
    let nonce = payment["nonce"].as_str().unwrap_or("").to_string();
    anyhow::ensure!(!nonce.is_empty(), "payment has no nonce");
    let doc = keys(state, false).await?;
    if let (Some(claimed), Some(pinned)) = (payment["exchange"].as_str(), doc["signature"]["signer"].as_str()) {
        anyhow::ensure!(claimed == pinned, "coins from another exchange ({claimed}); we trust {pinned}");
    }
    let ds = denoms(&doc);
    let coins = payment["coins"].as_array().cloned().unwrap_or_default();
    let (verified, total) = verify_batch(&ds, &coins, &me).map_err(|e| anyhow::anyhow!(e))?;
    anyhow::ensure!(total == payment["amountMinor"].as_i64().unwrap_or(-1), "coins sum {total} ≠ declared amount");
    let from = payment["from"].as_str().unwrap_or("").to_string();
    // One transaction: either every coin and the payment row land, or nothing does.
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    for (id, _, _) in &verified {
        let dup: Option<(String,)> = sqlx::query_as("SELECT status FROM coins WHERE coin = $1").bind(id).fetch_optional(&mut *tx).await?;
        anyhow::ensure!(dup.is_none(), "coin {} was already received — a re-used payment", &id[..20]);
    }
    for (id, v, c) in &verified {
        sqlx::query("INSERT INTO coins (coin, denomination, value_minor, sig, randomizer, spend, status, peer, payment) VALUES ($1,$2,$3,$4,$5,$6,'received',$7,$8)")
            .bind(id).bind(c["denomination"].as_str()).bind(v).bind(c["sig"].as_str()).bind(c["randomizer"].as_str()).bind(c["spend"].to_string()).bind(&from).bind(&nonce)
            .execute(&mut *tx).await?;
    }
    sqlx::query("INSERT OR IGNORE INTO coin_payments (id, direction, peer, amount, coins, via, note, payload) VALUES ($1,'in',$2,$3,$4,$5,$6,$7)")
        .bind(&nonce).bind(&from).bind(total).bind(verified.len() as i64).bind(via).bind(payment["note"].as_str()).bind(payment.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!(%from, amount = total, coins = verified.len(), via, "coins received (verified offline)");
    // Make it final when we can: refresh in the background.
    Ok(json!({"kind": KIND_RECEIPT, "ok": true, "nonce": nonce, "amountMinor": total, "coins": verified.len(), "verified": "offline: exchange signature + coin's own spend statement", "finality": "refresh at the exchange when online"}))
}

/// Refresh received coins into fresh ones we own. Returns what moved.
pub async fn refresh(state: &AppState) -> anyhow::Result<Value> {
    let rows = sqlx::query_as::<_, CoinRow>(&format!("SELECT {COLS} FROM coins WHERE status = 'received' ORDER BY created_at")).fetch_all(&state.db).await?;
    if rows.is_empty() {
        return Ok(json!({"ok": true, "refreshed": 0}));
    }
    let doc = keys(state, false).await?;
    let ds = denoms(&doc);
    let me = state.identity.id();
    let mut old = Vec::new();
    let mut new = Vec::new();
    let mut minted = Vec::new();
    for r in &rows {
        old.push(present(r, &me, "refresh")?);
        let d = ds.iter().find(|d| d.id == r.denomination).ok_or_else(|| anyhow::anyhow!("denomination {} gone", r.denomination))?;
        let kp = Keypair::generate();
        let id = kp.id().to_string();
        let br = d.pk.blind(&mut blind_rsa_signatures::DefaultRng, id.as_bytes())?;
        new.push(json!({"denomination": d.id, "blinded": b64(&br.blind_message.0)}));
        minted.push((kp, d.id.clone(), d.value, br));
    }
    let r = match state.registry.post("/v1/exchange/refresh", &json!({"coins": old, "blinded": new}), std::time::Duration::from_secs(60)).await {
        Ok(v) => v,
        Err(e) => {
            let m = e.to_string();
            if m.contains("double_spend") || m.contains("already spent") {
                // The payer cheated (or paid twice): mark the batch bad, one by one next time.
                for row in &rows {
                    let st: Value = crate::network::registry_get(state, &format!("/v1/exchange/coins/{}", row.coin)).await.unwrap_or(Value::Null);
                    if st["spent"].as_bool().unwrap_or(false) {
                        sqlx::query("UPDATE coins SET status = 'bad', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE coin = $1").bind(&row.coin).execute(&state.db).await?;
                        tracing::warn!(coin = %row.coin, peer = ?row_peer(row), "double-spent coin received — marked bad");
                    }
                }
            }
            return Err(anyhow::anyhow!("refresh failed: {m}"));
        }
    };
    let sigs = r["signatures"].as_array().cloned().unwrap_or_default();
    anyhow::ensure!(sigs.len() == minted.len(), "exchange returned {} signatures", sigs.len());
    for ((kp, did, v, br), bs) in minted.into_iter().zip(sigs) {
        let d = ds.iter().find(|d| d.id == did).unwrap();
        let id = kp.id().to_string();
        let sig = d.pk.finalize(&BlindSignature(unb64(bs.as_str().unwrap_or(""))?), &br, id.as_bytes())?;
        sqlx::query("INSERT INTO coins (coin, secret, denomination, value_minor, sig, randomizer, status) VALUES ($1,$2,$3,$4,$5,$6,'fresh')")
            .bind(&id).bind(crate::identity::at_rest(&kp.seed())?).bind(&did).bind(v).bind(b64(&sig.0)).bind(b64(&br.msg_randomizer.unwrap().0))
            .execute(&state.db).await?;
    }
    for row in &rows {
        sqlx::query("UPDATE coins SET status = 'refreshed', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE coin = $1").bind(&row.coin).execute(&state.db).await?;
    }
    tracing::info!(coins = rows.len(), "coins refreshed — payment final");
    Ok(json!({"ok": true, "refreshed": rows.len(), "amountMinor": r["amountMinor"]}))
}

fn row_peer(_r: &CoinRow) -> Option<String> { None }

/// Turn coins (fresh and received) into account balance.
pub async fn deposit(state: &AppState, amount_minor: Option<i64>) -> anyhow::Result<Value> {
    let mut rows = sqlx::query_as::<_, CoinRow>(&format!("SELECT {COLS} FROM coins WHERE status IN ('fresh','received') ORDER BY value_minor DESC")).fetch_all(&state.db).await?;
    if let Some(a) = amount_minor {
        rows = pick(&rows, a).ok_or_else(|| anyhow::anyhow!("cannot make exactly S/ {:.2} from the coins in the pocket", a as f64 / 100.0))?;
    }
    anyhow::ensure!(!rows.is_empty(), "no coins to deposit");
    let me = state.identity.id();
    let mut coins = Vec::new();
    for r in &rows {
        coins.push(present(r, &me, "deposit")?);
    }
    let r = state.registry.post("/v1/exchange/deposit", &json!({"coins": coins}), std::time::Duration::from_secs(60)).await?;
    for row in &rows {
        sqlx::query("UPDATE coins SET status = 'deposited', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE coin = $1").bind(&row.coin).execute(&state.db).await?;
    }
    tracing::info!(coins = rows.len(), amount = ?r["amountMinor"], "coins deposited");
    Ok(json!({"ok": true, "amountMinor": r["amountMinor"], "coins": rows.len(), "balance": r["balance"], "status": status(state).await}))
}

pub async fn status(state: &AppState) -> Value {
    let by: Vec<(String, i64, i64)> = sqlx::query_as("SELECT status, COUNT(*), COALESCE(SUM(value_minor),0) FROM coins GROUP BY status").fetch_all(&state.db).await.unwrap_or_default();
    let get = |s: &str| by.iter().find(|r| r.0 == s).map(|r| (r.1, r.2)).unwrap_or((0, 0));
    let keys_row: Option<(String, String, String)> = sqlx::query_as("SELECT exchange, doc, fetched_at FROM exchange_keys WHERE id = 1").fetch_optional(&state.db).await.ok().flatten();
    let doc: Value = keys_row.as_ref().and_then(|k| serde_json::from_str(&k.1).ok()).unwrap_or(Value::Null);
    let payments: Vec<(String, String, Option<String>, i64, i64, Option<String>, i64, String)> = sqlx::query_as("SELECT id, direction, peer, amount, coins, via, acked, at FROM coin_payments ORDER BY at DESC LIMIT 30").fetch_all(&state.db).await.unwrap_or_default();
    json!({
        "currency": "PEN",
        "fresh": {"coins": get("fresh").0, "amountMinor": get("fresh").1},
        "received": {"coins": get("received").0, "amountMinor": get("received").1},
        "spendableMinor": get("fresh").1, "pendingRefreshMinor": get("received").1,
        "spentMinor": get("spent").1, "depositedMinor": get("deposited").1, "badMinor": get("bad").1,
        "exchange": keys_row.as_ref().map(|k| json!({"id": k.0, "fetchedAt": k.2, "enabled": doc["enabled"], "denominations": doc["denominations"].as_array().map(|a| a.iter().filter_map(|d| d["value"].as_i64()).collect::<Vec<_>>()), "withdrawPerDay": doc["withdrawPerDay"]})),
        "me": state.identity.id(),
        "payments": payments.into_iter().map(|(id, dir, peer, amount, n, via, acked, at)| json!({"id": id, "direction": dir, "peer": peer, "amountMinor": amount, "coins": n, "via": via, "acked": acked == 1, "at": at})).collect::<Vec<_>>(),
    })
}

/// Relay protocol: a `coins` box is a payment; a `coins_receipt` acks ours.
pub async fn handle_inbox(state: &SharedState, from: &str, v: &Value) -> bool {
    match v["kind"].as_str() {
        Some(KIND_PAYMENT) => {
            let reply = match receive(state, v, "relay").await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(%from, error = %e, "coins payment refused");
                    json!({"kind": KIND_RECEIPT, "ok": false, "nonce": v["nonce"], "error": e.to_string()})
                }
            };
            let _ = crate::network::send(state, from, &reply).await;
            if reply["ok"].as_bool().unwrap_or(false) {
                let bg = state.clone();
                tokio::spawn(async move { if let Err(e) = refresh(&bg).await { tracing::debug!(error = %e, "refresh after receive"); } });
            }
            true
        }
        Some(KIND_RECEIPT) => {
            if let Some(n) = v["nonce"].as_str() {
                let _ = sqlx::query("UPDATE coin_payments SET acked = $1 WHERE id = $2 AND peer = $3").bind(v["ok"].as_bool().unwrap_or(false) as i64).bind(n).bind(from).execute(&state.db).await;
                tracing::info!(%from, nonce = n, ok = v["ok"].as_bool().unwrap_or(false), "coins receipt");
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_pick_are_exact() {
        assert_eq!(split(3700, &[5000, 2000, 1000, 500, 100]), Some(vec![2000, 1000, 500, 100, 100]));
        assert_eq!(split(150, &[5000, 2000, 1000, 500, 100]), None);
        // Céntimos: S/ 1.37 = 1 + 0.10×3 + 0.01×7
        assert_eq!(split(137, &[5000, 2000, 1000, 500, 100, 10, 1]).map(|v| v.len()), Some(11));
        assert_eq!(split(1, &[100, 10, 1]), Some(vec![1]));
        assert_eq!(split(0, &[100]), None);
        let row = |v: i64| CoinRow { coin: format!("c{v}"), secret: None, denomination: "d".into(), value_minor: v, sig: String::new(), randomizer: String::new(), spend: None, status: "fresh".into() };
        let pocket = vec![row(2000), row(1000), row(500), row(100), row(100)];
        assert_eq!(pick(&pocket, 1600).unwrap().iter().map(|c| c.value_minor).collect::<Vec<_>>(), vec![1000, 500, 100]);
        assert_eq!(pick(&pocket, 1700).unwrap().len(), 4);
        assert!(pick(&pocket, 5000).is_none());
    }

    #[test]
    fn offline_verification_rejects_forgeries() {
        use blind_rsa_signatures::KeyPair;
        let kp = KeyPair::<Sha384, PSS, Randomized>::generate(&mut blind_rsa_signatures::DefaultRng, 2048).unwrap();
        let ds = vec![Denom { id: "d100".into(), value: 100, pk: kp.pk.clone() }];
        let coin = Keypair::generate();
        let id = coin.id().to_string();
        let br = kp.pk.blind(&mut blind_rsa_signatures::DefaultRng, id.as_bytes()).unwrap();
        let bs = kp.sk.blind_sign(&br.blind_message).unwrap();
        let sig = kp.pk.finalize(&bs, &br, id.as_bytes()).unwrap();
        let mk = |spend: Value| json!({"coin": id, "denomination": "d100", "sig": b64(&sig.0), "randomizer": b64(&br.msg_randomizer.unwrap().0), "spend": spend});
        let good = mk(coin.envelope(json!({"coin": id, "to": "agent:me", "amountMinor": 100})));
        assert_eq!(verify_coin(&ds, &good, "agent:me").unwrap().1, 100);
        assert!(verify_coin(&ds, &good, "agent:other").unwrap_err().contains("not to me"));
        let other = Keypair::generate();
        let forged = mk(other.envelope(json!({"coin": id, "to": "agent:me"})));
        assert!(verify_coin(&ds, &forged, "agent:me").unwrap_err().contains("not signed by the coin"));
        // The same coin twice in one payment is refused, not double-counted.
        let (ok, total) = verify_batch(&ds, &[good.clone()], "agent:me").unwrap();
        assert_eq!((ok.len(), total), (1, 100));
        assert!(verify_batch(&ds, &[good.clone(), good.clone()], "agent:me").unwrap_err().contains("listed twice"));
        assert!(verify_batch(&ds, &[], "agent:me").unwrap_err().contains("no coins"));
        // A coin signed by a different exchange key is not real money.
        let kp2 = KeyPair::<Sha384, PSS, Randomized>::generate(&mut blind_rsa_signatures::DefaultRng, 2048).unwrap();
        let ds2 = vec![Denom { id: "d100".into(), value: 100, pk: kp2.pk }];
        assert!(verify_coin(&ds2, &good, "agent:me").unwrap_err().contains("not real money"));
    }
}

/// Whole-wallet tests against a fake exchange that really blind-signs.
#[cfg(test)]
mod wallet_tests {
    use super::*;
    use crate::testkit::{self, Mock};
    use blind_rsa_signatures::KeyPair;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex, OnceLock};

    type Rsa = KeyPair<Sha384, PSS, Randomized>;
    const VALUES: [i64; 4] = [2000, 500, 200, 100];

    fn rsa_keys() -> &'static Vec<Rsa> {
        static K: OnceLock<Vec<Rsa>> = OnceLock::new();
        K.get_or_init(|| VALUES.iter().map(|_| Rsa::generate(&mut blind_rsa_signatures::DefaultRng, 2048).unwrap()).collect())
    }

    fn registry_key() -> Keypair {
        Keypair::from_seed([42; 32])
    }

    fn keys_doc(signer: &Keypair) -> Value {
        let ds: Vec<Value> = VALUES.iter().zip(rsa_keys()).map(|(v, k)| json!({"id": format!("d{v}"), "value": v, "pk": k.pk.to_pem().unwrap()})).collect();
        let body = json!({"enabled": true, "currency": "PEN", "denominations": ds, "withdrawPerDay": 50000});
        let mut doc = body.clone();
        doc["signature"] = json!({"signer": signer.id().to_string(), "sig": signer.sign_hex(&yaya_wire::canonical(&body))});
        doc
    }

    fn sign_blinded(req: &Value) -> Vec<Value> {
        req.as_array().unwrap().iter().map(|c| {
            let i = VALUES.iter().position(|v| format!("d{v}") == c["denomination"].as_str().unwrap()).unwrap();
            let bs = rsa_keys()[i].sk.blind_sign(unb64(c["blinded"].as_str().unwrap()).unwrap()).unwrap();
            json!(b64(&bs.0))
        }).collect()
    }

    /// The exchange: keys, withdraw, refresh (with double-spend detection), deposit.
    struct Exchange {
        mock: Mock,
        spent: Arc<Mutex<HashSet<String>>>,
    }

    async fn exchange() -> Exchange {
        let mock = Mock::start().await;
        let spent: Arc<Mutex<HashSet<String>>> = Default::default();
        mock.on("/v1/exchange/keys", keys_doc(&registry_key()));
        mock.on_fn("/v1/exchange/withdraw", |s| (200, json!({"signatures": sign_blinded(&s.body["coins"]), "balance": 1})));
        let sp = spent.clone();
        mock.on_fn("/v1/exchange/refresh", move |s| {
            let mut spent = sp.lock().unwrap();
            for c in s.body["coins"].as_array().unwrap() {
                if !spent.insert(c["coin"].as_str().unwrap().to_string()) {
                    return (409, json!({"error": {"message": "double_spend"}}));
                }
            }
            let total: i64 = s.body["coins"].as_array().unwrap().iter().map(|c| c["value"].as_i64().unwrap()).sum();
            (200, json!({"signatures": sign_blinded(&s.body["blinded"]), "amountMinor": total}))
        });
        let sp = spent.clone();
        mock.on_fn("/v1/exchange/deposit", move |s| {
            let mut spent = sp.lock().unwrap();
            let mut total = 0;
            for c in s.body["coins"].as_array().unwrap() {
                if !spent.insert(c["coin"].as_str().unwrap().to_string()) {
                    return (409, json!({"error": {"message": "double_spend"}}));
                }
                total += c["value"].as_i64().unwrap();
            }
            (200, json!({"amountMinor": total, "balance": total}))
        });
        let sp = spent.clone();
        mock.on_fn("/v1/exchange/coins/", move |s| {
            let id = s.path.rsplit('/').next().unwrap().to_string();
            (200, json!({"spent": sp.lock().unwrap().contains(&id)}))
        });
        mock.on("/inbox", json!({"id": "box1"}));
        Exchange { mock, spent }
    }

    async fn wallet(x: &Exchange) -> SharedState {
        testkit::state_with(testkit::Opts { upstream: Some(x.mock.base.clone()), ..Default::default() }).await
    }

    async fn count(s: &AppState, status: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM coins WHERE status = $1").bind(status).fetch_one(&s.db).await.unwrap()
    }

    // ------------------------------------------------------------ keys doc

    #[test]
    fn keys_doc_signature_is_checked() {
        let k = registry_key();
        let doc = keys_doc(&k);
        assert_eq!(verify_keys_doc(&doc).unwrap(), k.id().to_string());
        let mut t = doc.clone();
        t["withdrawPerDay"] = json!(1);
        assert!(verify_keys_doc(&t).unwrap_err().to_string().contains("does not verify"));
        let mut t = doc.clone();
        t["signature"]["signer"] = json!("agent:bad");
        assert!(verify_keys_doc(&t).unwrap_err().to_string().contains("bad signer"));
        let mut t = doc.clone();
        t.as_object_mut().unwrap().remove("signature");
        assert!(verify_keys_doc(&t).unwrap_err().to_string().contains("no signer"));
        let mut t = doc;
        t["signature"]["sig"] = json!("zz");
        assert!(verify_keys_doc(&t).is_err());
    }

    #[test]
    fn denoms_sorted_desc_and_skip_broken_entries() {
        let mut doc = keys_doc(&registry_key());
        doc["denominations"].as_array_mut().unwrap().push(json!({"id": "broken", "value": 7, "pk": "not pem"}));
        doc["denominations"].as_array_mut().unwrap().push(json!({"id": "novalue", "pk": rsa_keys()[0].pk.to_pem().unwrap()}));
        let ds = denoms(&doc);
        assert_eq!(ds.iter().map(|d| d.value).collect::<Vec<_>>(), vec![2000, 500, 200, 100]);
        assert!(denoms(&json!({})).is_empty());
    }

    #[test]
    fn split_rejects_negative_and_empty_denominations() {
        assert_eq!(split(-100, &[100]), None);
        assert_eq!(split(100, &[]), None);
    }

    #[test]
    fn helpers() {
        assert_eq!(unb64(&format!("  {}  ", b64(b"hi"))).unwrap(), b"hi");
        assert!(unb64("%%").is_err());
        assert!(chrono::DateTime::parse_from_rfc3339(&now()).is_ok());
        assert!(row_peer(&CoinRow { coin: "c".into(), secret: None, denomination: "d".into(), value_minor: 1, sig: String::new(), randomizer: String::new(), spend: None, status: "fresh".into() }).is_none());
    }

    #[tokio::test]
    async fn keys_are_pinned_cached_and_survive_an_outage() {
        let x = exchange().await;
        let s = wallet(&x).await;
        let doc = keys(&s, false).await.unwrap();
        assert_eq!(doc["signature"]["signer"], json!(registry_key().id().to_string()));
        // Fresh cache: no second fetch.
        keys(&s, false).await.unwrap();
        assert_eq!(x.mock.seen_path("/v1/exchange/keys").len(), 1);
        // Forced: refetched.
        keys(&s, true).await.unwrap();
        assert_eq!(x.mock.seen_path("/v1/exchange/keys").len(), 2);
        // A different signer is refused (TOFU pin).
        x.mock.set("/v1/exchange/keys", keys_doc(&Keypair::from_seed([1; 32])));
        assert!(keys(&s, true).await.unwrap_err().to_string().contains("signer changed"));
        // Stale cache + exchange down → the cached document.
        sqlx::query("UPDATE exchange_keys SET fetched_at = '2000-01-01T00:00:00+00:00'").execute(&s.db).await.unwrap();
        let offline = testkit::state().await;
        sqlx::query("INSERT INTO exchange_keys (id, exchange, doc, fetched_at) VALUES (1, 'x', $1, '2000-01-01T00:00:00+00:00')").bind(doc.to_string()).execute(&offline.db).await.unwrap();
        assert_eq!(keys(&offline, false).await.unwrap(), doc);
        // No cache + down → a clear error.
        let empty = testkit::state().await;
        assert!(keys(&empty, false).await.unwrap_err().to_string().contains("not available yet"));
    }

    // ------------------------------------------------------------ withdraw

    #[tokio::test]
    async fn withdraw_mints_verifiable_coins() {
        let x = exchange().await;
        let s = wallet(&x).await;
        let r = withdraw(&s, 2800).await.unwrap();
        assert_eq!((r["ok"].clone(), r["coins"].clone(), r["amountMinor"].clone()), (json!(true), json!(4), json!(2800)));
        assert_eq!(r["status"]["fresh"]["amountMinor"], 2800);
        assert_eq!(count(&s, "fresh").await, 4);
        // What was sent to the exchange is blinded: coin ids never leave the device.
        let sent = x.mock.seen_path("/v1/exchange/withdraw")[0].body.to_string();
        let ids: Vec<String> = sqlx::query_scalar("SELECT coin FROM coins").fetch_all(&s.db).await.unwrap();
        assert!(ids.iter().all(|id| !sent.contains(id.as_str())));
    }

    #[tokio::test]
    async fn withdraw_rejects_bad_amounts_and_short_answers() {
        let x = exchange().await;
        let s = wallet(&x).await;
        assert!(withdraw(&s, 0).await.unwrap_err().to_string().contains("positive"));
        assert!(withdraw(&s, -5).await.is_err());
        assert!(withdraw(&s, 150).await.unwrap_err().to_string().contains("sum of coins"));
        x.mock.on_fn("/v1/exchange/withdraw", |_| (200, json!({"signatures": []})));
        assert!(withdraw(&s, 100).await.unwrap_err().to_string().contains("0 signatures for 1"));
        assert_eq!(count(&s, "fresh").await, 0);
    }

    // ----------------------------------------------------------- pay/receive

    #[test]
    fn pick_finds_exact_sums_greedy_misses() {
        let row = |i: usize, v: i64| CoinRow { coin: format!("c{i}"), secret: None, denomination: "d".into(), value_minor: v, sig: String::new(), randomizer: String::new(), spend: None, status: "fresh".into() };
        // S/5 + 3×S/2 in the pocket; S/6 = 2+2+2. Greedy takes the 5 and gets stuck.
        let pocket = vec![row(0, 500), row(1, 200), row(2, 200), row(3, 200)];
        let got = pick(&pocket, 600).expect("2+2+2 makes 6");
        assert_eq!(got.iter().map(|c| c.value_minor).sum::<i64>(), 600);
        assert_eq!(got.len(), 3);
        // Every coin used at most once.
        assert_eq!(got.iter().map(|c| c.coin.clone()).collect::<HashSet<_>>().len(), 3);
        assert!(pick(&pocket, 300).is_none());
        assert!(pick(&pocket, 0).is_none());
        assert!(pick(&[], 100).is_none());
        // Prefers few coins where it can.
        assert_eq!(pick(&pocket, 700).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn offline_payment_round_trip_then_refresh_makes_it_final() {
        let x = exchange().await;
        let (payer, payee) = (wallet(&x).await, wallet(&x).await);
        withdraw(&payer, 2700).await.unwrap(); // 2000 + 500 + 200
        let r = pay(&payer, &payee.identity.id(), 700, "offline", Some("almuerzo")).await.unwrap();
        assert_eq!(r["delivered"], "offline");
        let payment = r["payment"].clone();
        assert_eq!(payment["amountMinor"], 700);
        assert_eq!(payment["note"], "almuerzo");
        assert_eq!(payment["exchange"], json!(registry_key().id().to_string()));
        assert_eq!(count(&payer, "spent").await, 2);
        assert_eq!(status(&payer).await["spendableMinor"], 2000);

        let receipt = receive(&payee, &payment, "bluetooth").await.unwrap();
        assert_eq!((receipt["ok"].clone(), receipt["amountMinor"].clone(), receipt["kind"].clone()), (json!(true), json!(700), json!(KIND_RECEIPT)));
        assert_eq!(status(&payee).await["pendingRefreshMinor"], 700);
        // The same payment twice is refused.
        assert!(receive(&payee, &payment, "bluetooth").await.unwrap_err().to_string().contains("already received"));

        let rr = refresh(&payee).await.unwrap();
        assert_eq!((rr["refreshed"].clone(), rr["amountMinor"].clone()), (json!(2), json!(700)));
        let st = status(&payee).await;
        assert_eq!((st["spendableMinor"].clone(), st["pendingRefreshMinor"].clone()), (json!(700), json!(0)));
        assert_eq!(count(&payee, "refreshed").await, 2);
        // Nothing left to refresh.
        assert_eq!(refresh(&payee).await.unwrap()["refreshed"], 0);
        // The payee can now spend what it received.
        let back = pay(&payee, &payer.identity.id(), 700, "offline", None).await.unwrap();
        assert!(receive(&payer, &back["payment"], "qr").await.is_ok());
    }

    #[tokio::test]
    async fn double_spent_coins_are_marked_bad_on_refresh() {
        let x = exchange().await;
        let (payer, a, b) = (wallet(&x).await, wallet(&x).await, wallet(&x).await);
        withdraw(&payer, 100).await.unwrap();
        let p = make_payment(&payer, &a.identity.id(), 100, "offline", None).await.unwrap();
        receive(&a, &p, "qr").await.unwrap();
        refresh(&a).await.unwrap();
        // The cheat: re-present the same coin (already refreshed by A) to B,
        // re-signed to B with the coin key the payer still holds.
        sqlx::query("UPDATE coins SET status = 'fresh'").execute(&payer.db).await.unwrap();
        let p2 = make_payment(&payer, &b.identity.id(), 100, "offline", None).await.unwrap();
        receive(&b, &p2, "qr").await.unwrap(); // offline, B cannot know yet
        assert!(refresh(&b).await.unwrap_err().to_string().contains("double_spend"));
        assert_eq!(count(&b, "bad").await, 1);
        assert_eq!(status(&b).await["badMinor"], 100);
        let _ = &x.spent;
    }

    #[tokio::test]
    async fn receive_refuses_malformed_payments() {
        let x = exchange().await;
        let (payer, payee) = (wallet(&x).await, wallet(&x).await);
        withdraw(&payer, 300).await.unwrap();
        let good = make_payment(&payer, &payee.identity.id(), 300, "offline", None).await.unwrap();
        let bad = |f: &dyn Fn(&mut Value)| { let mut p = good.clone(); f(&mut p); p };
        let cases: Vec<(Value, &str)> = vec![
            (bad(&|p| p["kind"] = json!("other")), "not a coins payment"),
            (bad(&|p| p["to"] = json!("agent:someone")), "addressed to"),
            (bad(&|p| p["nonce"] = json!("")), "no nonce"),
            (bad(&|p| p["exchange"] = json!("agent:elsewhere")), "another exchange"),
            (bad(&|p| p["amountMinor"] = json!(9999)), "declared amount"),
            (bad(&|p| p["coins"] = json!([])), "no coins"),
        ];
        for (p, want) in cases {
            let e = receive(&payee, &p, "qr").await.unwrap_err().to_string();
            assert!(e.contains(want), "{want}: {e}");
        }
        // A payment addressed to someone else can't be redirected: the coins say who.
        let third = wallet(&x).await;
        let mut stolen = good.clone();
        stolen["to"] = json!(third.identity.id());
        assert!(receive(&third, &stolen, "qr").await.unwrap_err().to_string().contains("not to me"));
        assert_eq!(count(&payee, "received").await, 0);
        // And the good one still lands afterwards.
        assert!(receive(&payee, &good, "qr").await.is_ok());
    }

    #[tokio::test]
    async fn make_payment_guards() {
        let x = exchange().await;
        let s = wallet(&x).await;
        assert!(make_payment(&s, "agent:x", 0, "offline", None).await.unwrap_err().to_string().contains("positive"));
        let e = make_payment(&s, "agent:x", 100, "offline", None).await.unwrap_err().to_string();
        assert!(e.contains("cannot make exactly S/ 1.00"), "{e}");
        withdraw(&s, 500).await.unwrap();
        assert!(make_payment(&s, "agent:x", 100, "offline", None).await.is_err());
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM coin_payments").fetch_one(&s.db).await.unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn concurrent_payments_never_spend_the_same_coin() {
        let x = exchange().await;
        let s = wallet(&x).await;
        withdraw(&s, 100).await.unwrap();
        let (a, b) = tokio::join!(
            make_payment(&s, "agent:a", 100, "offline", None),
            make_payment(&s, "agent:b", 100, "offline", None),
        );
        assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1, "exactly one payment may use the only coin");
        let payments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM coin_payments").fetch_one(&s.db).await.unwrap();
        assert_eq!(payments, 1);
    }

    #[tokio::test]
    async fn pay_resolves_handles_and_refuses_self() {
        let x = exchange().await;
        let (payer, payee) = (wallet(&x).await, wallet(&x).await);
        for _ in 0..3 { withdraw(&payer, 100).await.unwrap(); }
        assert!(pay(&payer, &payer.identity.id(), 100, "offline", None).await.unwrap_err().to_string().contains("that is you"));
        x.mock.on("/v1/index/urn:agent:yaya:bodega", json!({"agent_id": payee.identity.id()}));
        let r = pay(&payer, "@bodega", 100, "offline", None).await.unwrap();
        assert_eq!(r["to"], json!(payee.identity.id()));
        let r = pay(&payer, " urn:agent:yaya:bodega ", 100, "offline", None).await.unwrap();
        assert_eq!(r["to"], json!(payee.identity.id()));
        x.mock.on("/v1/index/urn:agent:yaya:nadie", json!({}));
        assert!(pay(&payer, "@nadie", 100, "offline", None).await.unwrap_err().to_string().contains("unknown agent"));
    }

    #[tokio::test]
    async fn pay_over_relay_and_mesh() {
        let x = exchange().await;
        let (payer, payee) = (wallet(&x).await, wallet(&x).await);
        withdraw(&payer, 300).await.unwrap();
        let r = pay(&payer, &payee.identity.id(), 100, "relay", None).await.unwrap();
        assert_eq!(r["delivered"], "relay");
        let boxes = x.mock.seen_path(&format!("/v1/agents/{}/inbox", payee.identity.id()));
        assert_eq!(boxes.len(), 1);
        // Sealed: the relay never sees the coins.
        assert!(!boxes[0].body.to_string().contains(r["payment"]["coins"][0]["coin"].as_str().unwrap()));
        // Mesh without a link: refused before anything is spent.
        let e = pay(&payer, &payee.identity.id(), 200, "mesh", None).await.unwrap_err().to_string();
        assert!(e.contains("not linked on the mesh"), "{e}");
        assert_eq!(status(&payer).await["spendableMinor"], 200, "an undeliverable payment must not burn coins");
    }

    #[tokio::test]
    async fn deposit_all_or_exact() {
        let x = exchange().await;
        let s = wallet(&x).await;
        assert!(deposit(&s, None).await.unwrap_err().to_string().contains("no coins"));
        withdraw(&s, 800).await.unwrap(); // 500 + 200 + 100
        assert!(deposit(&s, Some(400)).await.unwrap_err().to_string().contains("cannot make exactly"));
        let r = deposit(&s, Some(300)).await.unwrap();
        assert_eq!((r["amountMinor"].clone(), r["coins"].clone()), (json!(300), json!(2)));
        let r = deposit(&s, None).await.unwrap();
        assert_eq!(r["amountMinor"], 500);
        assert_eq!(count(&s, "deposited").await, 3);
        assert_eq!(status(&s).await["depositedMinor"], 800);
    }

    #[tokio::test]
    async fn status_of_an_empty_wallet() {
        let s = testkit::state().await;
        let st = status(&s).await;
        assert_eq!(st["currency"], "PEN");
        assert_eq!(st["spendableMinor"], 0);
        assert!(st["exchange"].is_null());
        assert_eq!(st["me"], json!(s.identity.id()));
        assert_eq!(st["payments"], json!([]));
    }

    #[tokio::test]
    async fn inbox_handles_payments_receipts_and_ignores_the_rest() {
        let x = exchange().await;
        let (payer, payee) = (wallet(&x).await, wallet(&x).await);
        withdraw(&payer, 100).await.unwrap();
        let p = make_payment(&payer, &payee.identity.id(), 100, "relay", None).await.unwrap();
        assert!(handle_inbox(&payee, &payer.identity.id(), &p).await);
        // A receipt went back to the payer through the relay.
        assert_eq!(x.mock.seen_path(&format!("/v1/agents/{}/inbox", payer.identity.id())).len(), 1);
        // A refused payment still answers, with ok=false.
        assert!(handle_inbox(&payee, &payer.identity.id(), &p).await);
        assert_eq!(x.mock.seen_path(&format!("/v1/agents/{}/inbox", payer.identity.id())).len(), 2);
        // The payer applies the receipt only from the peer it paid.
        let receipt = json!({"kind": KIND_RECEIPT, "ok": true, "nonce": p["nonce"]});
        assert!(handle_inbox(&payer, "agent:stranger", &receipt).await);
        let acked: i64 = sqlx::query_scalar("SELECT acked FROM coin_payments").fetch_one(&payer.db).await.unwrap();
        assert_eq!(acked, 0);
        assert!(handle_inbox(&payer, &payee.identity.id(), &receipt).await);
        let acked: i64 = sqlx::query_scalar("SELECT acked FROM coin_payments").fetch_one(&payer.db).await.unwrap();
        assert_eq!(acked, 1);
        assert!(!handle_inbox(&payer, "agent:x", &json!({"kind": "chat"})).await);
    }
}
