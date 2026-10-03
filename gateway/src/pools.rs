//! Pools — group buying on the network. Consumers (or businesses) aggregate
//! demand so a seller ships one big order at the volume price; the network
//! holds the money in escrow and releases it when a majority of members
//! confirm delivery. Reputation is free: every confirmation is first-hand
//! evidence. The network's only take is a percentage of realized value.
//!
//! Nothing here is compiled into the phone: the core mounts these routes as
//! tools from `GET /v1/tools` (`tools.rs`), so pools ship without an APK.
//!
//! Flow: `POST /v1/pools` (organizer; `seller` defaults to the organizer) →
//! `POST /v1/pools/{id}/join {items:{papa:60}}` (escrow at the worst-case
//! tier) → `POST /v1/pools/{id}/close` (organizer, seller, or anyone past the
//! deadline: `shipped` at the tier reached, refunds the difference; or
//! `cancelled`, full refunds) → `POST /v1/pools/{id}/confirm {delivered}`
//! (members; majority releases to the seller minus the fee, or refunds).
use axum::{extract::{Path, Query, State}, http::StatusCode, response::IntoResponse, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{auth::require_linked, credits, err, internal, wallet, ApiResult, App, Auth, Shared};

fn fee_pct() -> i64 {
    std::env::var("POOL_FEE_PERCENT").ok().and_then(|v| v.parse().ok()).unwrap_or(2)
}
fn fee_cap() -> i64 {
    std::env::var("POOL_FEE_CAP_MINOR").ok().and_then(|v| v.parse().ok()).unwrap_or(2000)
}

/// Unit price (minor) of `item` when the whole order is `total_kg`.
fn unit_at(items: &Value, item: &str, total_kg: f64) -> Option<i64> {
    let tiers = items[item]["tiers"].as_array()?;
    let mut best: Option<(f64, i64)> = None;
    for t in tiers {
        let kg = t["kg"].as_f64().unwrap_or(0.0);
        let u = t["unitMinor"].as_i64()?;
        if total_kg >= kg && best.map(|b| kg >= b.0).unwrap_or(true) {
            best = Some((kg, u));
        }
    }
    best.map(|b| b.1)
}

fn amount_for(items: &Value, member_items: &Value, total_kg: f64) -> Option<i64> {
    let mut sum = 0i64;
    for (it, kg) in member_items.as_object()? {
        let kg = kg.as_f64()?;
        sum += (unit_at(items, it, total_kg)? as f64 * kg).round() as i64;
    }
    Some(sum)
}

async fn credit(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, account: &str, amount: i64, kind: &str, reference: &str, note: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if amount <= 0 { return Ok(()); }
    sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind, ref, note) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(amount).bind(credits::CURRENCY).bind(kind).bind(reference).bind(note)
        .execute(&mut **tx).await.map_err(internal)?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct PoolRow { id: String, seller: String, seller_acct: Option<String>, organizer: String, title: String, items: String, min_kg: f64, target_kg: f64, delivery_minor: i64, deadline: String, city: Option<String>, country: Option<String>, note: Option<String>, status: String, kg: f64, fee_minor: i64, created_at: String }

async fn load(app: &App, id: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let row: Option<PoolRow> = sqlx::query_as(
        "SELECT id, seller, seller_acct, organizer, title, items, min_kg, target_kg, delivery_minor, deadline, city, country, note, status, kg, fee_minor, created_at FROM pools WHERE id = $1")
        .bind(id).fetch_optional(&app.db).await.map_err(internal)?;
    let Some(PoolRow { id, seller, seller_acct, organizer, title, items, min_kg, target_kg, delivery_minor: delivery, deadline, city, country, note, status, kg, fee_minor: fee, created_at: created }) = row else {
        return Err(err(StatusCode::NOT_FOUND, "no such pool"));
    };
    let items: Value = serde_json::from_str(&items).unwrap_or(json!({}));
    let members: Vec<(String, String, f64, i64, Option<i64>, Option<i64>)> = sqlx::query_as("SELECT agent, items, kg, escrow_minor, due_minor, confirmed FROM pool_members WHERE pool = $1")
        .bind(&id).fetch_all(&app.db).await.map_err(internal)?;
    let total: f64 = members.iter().map(|m| m.2).sum();
    let price_now: Value = items.as_object().map(|o| o.keys().map(|k| (k.clone(), json!(unit_at(&items, k, total.max(min_kg))))).collect::<serde_json::Map<_, _>>()).map(Value::Object).unwrap_or(json!({}));
    let price_target: Value = items.as_object().map(|o| o.keys().map(|k| (k.clone(), json!(unit_at(&items, k, target_kg)))).collect::<serde_json::Map<_, _>>()).map(Value::Object).unwrap_or(json!({}));
    Ok(json!({
        "id": id, "seller": seller, "sellerAccount": seller_acct, "organizer": organizer, "title": title, "items": items,
        "minKg": min_kg, "targetKg": target_kg, "deliveryMinor": delivery, "deadline": deadline, "city": city, "country": country, "note": note,
        "status": status, "kg": if status == "open" { total } else { kg }, "feeMinor": fee, "createdAt": created,
        "members": members.iter().map(|m| json!({"agent": m.0, "items": serde_json::from_str::<Value>(&m.1).unwrap_or(json!({})), "kg": m.2, "escrowMinor": m.3, "dueMinor": m.4, "confirmed": m.5})).collect::<Vec<_>>(),
        "unitMinorNow": price_now, "unitMinorAtTarget": price_target,
        "fillPercent": if target_kg > 0.0 { (total / target_kg * 100.0).round() } else { 0.0 },
    }))
}

#[derive(Deserialize)]
pub struct ListQ { item: Option<String>, city: Option<String>, country: Option<String>, status: Option<String>, mine: Option<bool>, limit: Option<i64> }

/// `GET /v1/pools?item=&city=&status=open&mine=1` — open pools, fullest first.
pub async fn list(State(app): State<Shared>, Extension(auth): Extension<Auth>, Query(q): Query<ListQ>) -> ApiResult {
    let status = q.status.unwrap_or_else(|| "open".into());
    let me = crate::auth::agent_of(&app, &auth).await.ok();
    let mut sql = String::from("SELECT p.id FROM pools p WHERE p.status = $1");
    if q.city.is_some() { sql.push_str(" AND lower(p.city) = lower($2)"); }
    if q.country.is_some() { sql.push_str(" AND upper(p.country) = upper($3)"); }
    if q.item.is_some() { sql.push_str(" AND p.items LIKE $4"); }
    if q.mine.unwrap_or(false) { sql.push_str(" AND (p.organizer = $5 OR p.seller = $5 OR EXISTS (SELECT 1 FROM pool_members m WHERE m.pool = p.id AND m.agent = $5))"); }
    sql.push_str(" ORDER BY p.deadline LIMIT $6");
    let rows: Vec<(String,)> = sqlx::query_as(&sql)
        .bind(&status).bind(q.city.unwrap_or_default()).bind(q.country.unwrap_or_default()).bind(format!("%\"{}\"%", q.item.unwrap_or_default().to_lowercase()))
        .bind(me.unwrap_or_default()).bind(q.limit.unwrap_or(30).clamp(1, 100))
        .fetch_all(&app.db).await.map_err(internal)?;
    let mut out = Vec::new();
    for (id,) in rows {
        let mut p = load(&app, &id).await?;
        p.as_object_mut().map(|o| o.remove("members"));
        out.push(p);
    }
    out.sort_by(|a, b| b["fillPercent"].as_f64().unwrap_or(0.0).partial_cmp(&a["fillPercent"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    Ok(Json(json!({"pools": out, "count": out.len(), "feePercent": fee_pct()})).into_response())
}

pub async fn get(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(load(&app, &id).await?).into_response())
}

#[derive(Deserialize)]
pub struct CreateReq { title: String, seller: Option<String>, items: Value, min_kg: f64, target_kg: f64, delivery_minor: Option<i64>, deadline: String, city: Option<String>, country: Option<String>, note: Option<String> }

/// `POST /v1/pools` — signed. `items`: `{"papa": {"tiers": [{"kg": 0, "unitMinor": 187}, {"kg": 400, "unitMinor": 128}, {"kg": 800, "unitMinor": 110}]}}`.
pub async fn create(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(r): Json<CreateReq>) -> ApiResult {
    let (agent, _acct) = require_linked(&app, &auth).await?;
    let Some(obj) = r.items.as_object() else { return Err(err(StatusCode::BAD_REQUEST, "items must be an object of {item: {tiers: [{kg, unitMinor}]}}")); };
    if obj.is_empty() || obj.len() > 20 { return Err(err(StatusCode::BAD_REQUEST, "1 to 20 items")); }
    for (k, v) in obj {
        let ok = k.len() <= 40 && v["tiers"].as_array().map(|t| !t.is_empty() && t.iter().all(|x| x["unitMinor"].as_i64().map(|u| u > 0).unwrap_or(false))).unwrap_or(false);
        if !ok { return Err(err(StatusCode::BAD_REQUEST, format!("item {k}: tiers need kg and unitMinor > 0"))); }
    }
    if !(r.min_kg > 0.0 && r.target_kg >= r.min_kg) { return Err(err(StatusCode::BAD_REQUEST, "min_kg > 0 and target_kg >= min_kg")); }
    if chrono::DateTime::parse_from_rfc3339(&r.deadline).is_err() { return Err(err(StatusCode::BAD_REQUEST, "deadline must be RFC3339")); }
    let seller = r.seller.clone().unwrap_or_else(|| agent.clone());
    if seller.parse::<yaya_wire::AgentId>().is_err() { return Err(err(StatusCode::BAD_REQUEST, "seller must be an agent id")); }
    let seller_acct = crate::accounts::account_of_agent(&app, &seller).await?;
    let id = format!("pool_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let items = Value::Object(obj.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect());
    sqlx::query("INSERT INTO pools (id, seller, seller_acct, organizer, title, items, min_kg, target_kg, delivery_minor, deadline, city, country, note) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
        .bind(&id).bind(&seller).bind(seller_acct).bind(&agent).bind(r.title.chars().take(120).collect::<String>()).bind(items.to_string())
        .bind(r.min_kg).bind(r.target_kg).bind(r.delivery_minor.unwrap_or(0).max(0)).bind(&r.deadline).bind(r.city).bind(r.country.map(|c| c.to_uppercase())).bind(r.note.map(|n| n.chars().take(400).collect::<String>()))
        .execute(&app.db).await.map_err(internal)?;
    Ok((StatusCode::CREATED, Json(load(&app, &id).await?)).into_response())
}

#[derive(Deserialize)]
pub struct JoinReq { items: Value }

/// `POST /v1/pools/{id}/join {items: {papa: 60}}` — escrow at the worst-case (minimum) tier; the difference comes back at close.
pub async fn join(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, Json(r): Json<JoinReq>) -> ApiResult {
    let (agent, acct) = require_linked(&app, &auth).await?;
    let Some(acct) = acct else { return Err(err(StatusCode::PAYMENT_REQUIRED, "a Yaya account with balance is needed to join a pool")); };
    let p = load(&app, &id).await?;
    if p["status"] != "open" { return Err(err(StatusCode::CONFLICT, "pool is not open")); }
    let mut kg_total = 0.0;
    let mut clean = serde_json::Map::new();
    for (k, v) in r.items.as_object().ok_or_else(|| err(StatusCode::BAD_REQUEST, "items: {item: kg}"))? {
        let kg = v.as_f64().unwrap_or(0.0);
        if kg <= 0.0 || p["items"][k.to_lowercase()].is_null() { return Err(err(StatusCode::BAD_REQUEST, format!("{k}: not in this pool or kg <= 0"))); }
        kg_total += kg; clean.insert(k.to_lowercase(), json!(kg));
    }
    if kg_total <= 0.0 { return Err(err(StatusCode::BAD_REQUEST, "nothing to join with")); }
    let items = Value::Object(clean);
    let min_kg = p["minKg"].as_f64().unwrap_or(0.0);
    let escrow = amount_for(&p["items"], &items, min_kg).ok_or_else(|| err(StatusCode::BAD_REQUEST, "no price tier"))?
        + (p["deliveryMinor"].as_i64().unwrap_or(0) as f64 * kg_total / min_kg.max(kg_total)).round() as i64;
    let mut tx = app.db.begin().await.map_err(internal)?;
    let ok = wallet::spend_bought_in(&mut tx, &acct, escrow, "pool-escrow", Some(&id), &format!("pool {} — {:.0} kg", p["title"].as_str().unwrap_or(""), kg_total)).await?;
    if !ok {
        drop(tx);
        let bal = credits::balance(&app, &acct).await?;
        return Err(wallet::payment_required("joining this pool", escrow, bal));
    }
    sqlx::query("INSERT INTO pool_members (pool, agent, account, items, kg, escrow_minor) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT(pool, agent) DO UPDATE SET items = excluded.items, kg = pool_members.kg + excluded.kg, escrow_minor = pool_members.escrow_minor + excluded.escrow_minor")
        .bind(&id).bind(&agent).bind(&acct).bind(items.to_string()).bind(kg_total).bind(escrow).execute(&mut *tx).await.map_err(internal)?;
    sqlx::query("INSERT INTO pool_intents (agent, item, kg, city, country) SELECT $1, key, value, $3, $4 FROM json_each($2)")
        .bind(&agent).bind(items.to_string()).bind(p["city"].as_str()).bind(p["country"].as_str()).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    let mut out = load(&app, &id).await?;
    out["escrowMinor"] = json!(escrow);
    Ok(Json(out).into_response())
}

/// `POST /v1/pools/{id}/leave` — while open: full refund.
pub async fn leave(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    let p = load(&app, &id).await?;
    if p["status"] != "open" { return Err(err(StatusCode::CONFLICT, "pool is not open")); }
    let row: Option<(String, i64)> = sqlx::query_as("SELECT account, escrow_minor FROM pool_members WHERE pool = $1 AND agent = $2").bind(&id).bind(&agent).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((acct, escrow)) = row else { return Err(err(StatusCode::NOT_FOUND, "not a member")); };
    let mut tx = app.db.begin().await.map_err(internal)?;
    credit(&mut tx, &acct, escrow, "pool-refund", &id, "left the pool").await?;
    sqlx::query("DELETE FROM pool_members WHERE pool = $1 AND agent = $2").bind(&id).bind(&agent).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(json!({"ok": true, "refundedMinor": escrow})).into_response())
}

async fn refund_all(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: &str, note: &str) -> Result<i64, (StatusCode, Json<Value>)> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT account, escrow_minor FROM pool_members WHERE pool = $1").bind(id).fetch_all(&mut **tx).await.map_err(internal)?;
    let mut total = 0;
    for (acct, e) in rows { credit(tx, &acct, e, "pool-refund", id, note).await?; total += e; }
    sqlx::query("UPDATE pool_members SET escrow_minor = 0 WHERE pool = $1").bind(id).execute(&mut **tx).await.map_err(internal)?;
    Ok(total)
}

/// `POST /v1/pools/{id}/close` — organizer or seller any time; anyone once the deadline passed.
/// Reached the minimum → `shipped` at the tier of the total, refunds the escrow difference. Otherwise `cancelled`, full refunds.
pub async fn close(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    let p = load(&app, &id).await?;
    if p["status"] != "open" { return Err(err(StatusCode::CONFLICT, "pool is not open")); }
    let past = chrono::DateTime::parse_from_rfc3339(p["deadline"].as_str().unwrap_or("")).map(|d| d < chrono::Utc::now()).unwrap_or(true);
    if !(past || p["organizer"] == agent || p["seller"] == agent) { return Err(err(StatusCode::FORBIDDEN, "only the organizer or the seller may close before the deadline")); }
    let total = p["kg"].as_f64().unwrap_or(0.0);
    let mut tx = app.db.begin().await.map_err(internal)?;
    let status = if total >= p["minKg"].as_f64().unwrap_or(f64::MAX) {
        let members: Vec<(String, String, String, f64, i64)> = sqlx::query_as("SELECT agent, account, items, kg, escrow_minor FROM pool_members WHERE pool = $1").bind(&id).fetch_all(&mut *tx).await.map_err(internal)?;
        for (ag, acct, items, kg, escrow) in members {
            let items: Value = serde_json::from_str(&items).unwrap_or(json!({}));
            let due = amount_for(&p["items"], &items, total).unwrap_or(escrow) + (p["deliveryMinor"].as_i64().unwrap_or(0) as f64 * kg / total).round() as i64;
            let due = due.min(escrow);
            credit(&mut tx, &acct, escrow - due, "pool-refund", &id, "volume price reached").await?;
            sqlx::query("UPDATE pool_members SET due_minor = $3, escrow_minor = $3 WHERE pool = $1 AND agent = $2").bind(&id).bind(&ag).bind(due).execute(&mut *tx).await.map_err(internal)?;
        }
        "shipped"
    } else {
        refund_all(&mut tx, &id, "pool did not reach its minimum").await?;
        "cancelled"
    };
    sqlx::query("UPDATE pools SET status = $2, kg = $3, closed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = $1").bind(&id).bind(status).bind(total).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(load(&app, &id).await?).into_response())
}

#[derive(Deserialize)]
pub struct ConfirmReq { delivered: bool }

/// `POST /v1/pools/{id}/confirm {delivered}` — first-hand evidence from a member. A majority of members
/// saying delivered releases the escrow to the seller (minus the fee); a majority saying not delivered refunds everyone.
pub async fn confirm(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, Json(r): Json<ConfirmReq>) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    let p = load(&app, &id).await?;
    if p["status"] != "shipped" { return Err(err(StatusCode::CONFLICT, "pool is not awaiting delivery")); }
    let n = sqlx::query("UPDATE pool_members SET confirmed = $3 WHERE pool = $1 AND agent = $2").bind(&id).bind(&agent).bind(r.delivered as i64).execute(&app.db).await.map_err(internal)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::FORBIDDEN, "only members confirm")); }
    sqlx::query("INSERT INTO pool_intents (agent, item, kg, city, country) VALUES ($1, $2, 0, NULL, NULL)").bind(&agent).bind(format!("evidence:{}:{}", if r.delivered { "delivered" } else { "not_delivered" }, p["seller"].as_str().unwrap_or(""))).execute(&app.db).await.map_err(internal)?;
    let (members, yes, no): (i64, i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(confirmed = 1),0), COALESCE(SUM(confirmed = 0),0) FROM pool_members WHERE pool = $1").bind(&id).fetch_one(&app.db).await.map_err(internal)?;
    let seller_acct = match p["sellerAccount"].as_str() { Some(s) => Some(s.to_string()), None => crate::accounts::account_of_agent(&app, p["seller"].as_str().unwrap_or("")).await? };
    let mut tx = app.db.begin().await.map_err(internal)?;
    let mut status = "shipped";
    if yes * 2 > members {
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT account, escrow_minor FROM pool_members WHERE pool = $1").bind(&id).fetch_all(&mut *tx).await.map_err(internal)?;
        let gross: i64 = rows.iter().map(|r| r.1).sum();
        let fee = (gross * fee_pct() / 100).min(fee_cap());
        match seller_acct.clone() {
            Some(sa) => credit(&mut tx, &sa, gross - fee, "pool-sale", &id, &format!("pool {} delivered — {:.0} kg", p["title"].as_str().unwrap_or(""), p["kg"].as_f64().unwrap_or(0.0))).await?,
            None => credit(&mut tx, &wallet::platform_account(), gross - fee, "pool-sale-unclaimed", &id, "seller has no account yet").await?,
        }
        credit(&mut tx, &wallet::platform_account(), fee, "pool-fee", &id, "network fee").await?;
        sqlx::query("UPDATE pool_members SET escrow_minor = 0 WHERE pool = $1").bind(&id).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("UPDATE pools SET fee_minor = $2 WHERE id = $1").bind(&id).bind(fee).execute(&mut *tx).await.map_err(internal)?;
        status = "delivered";
    } else if no * 2 > members {
        refund_all(&mut tx, &id, "delivery failed (majority)").await?;
        status = "failed";
    }
    if status != "shipped" {
        sqlx::query("UPDATE pools SET status = $2 WHERE id = $1").bind(&id).bind(status).execute(&mut *tx).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    let mut out = load(&app, &id).await?;
    out["votes"] = json!({"members": members, "delivered": yes, "notDelivered": no});
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
pub struct IntentReq { item: String, kg: f64, city: Option<String>, country: Option<String> }

/// `POST /v1/pools/intent` — "I would buy this much of this item" — feeds the aggregate-demand signal that tells a seller a pool is worth opening.
pub async fn intent(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(r): Json<IntentReq>) -> ApiResult {
    let agent = crate::auth::agent_of(&app, &auth).await?;
    if r.kg <= 0.0 || r.item.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "item and kg > 0")); }
    sqlx::query("INSERT INTO pool_intents (agent, item, kg, city, country) VALUES ($1,$2,$3,$4,$5)")
        .bind(&agent).bind(r.item.trim().to_lowercase()).bind(r.kg).bind(r.city).bind(r.country.map(|c| c.to_uppercase())).execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
pub struct DemandQ { item: Option<String>, city: Option<String>, country: Option<String>, days: Option<i64> }

/// `GET /v1/pools/demand?item=&city=&days=7` — aggregate demand: kg and distinct agents per item over the window.
pub async fn demand(State(app): State<Shared>, Query(q): Query<DemandQ>) -> ApiResult {
    let days = q.days.unwrap_or(7).clamp(1, 60);
    let rows: Vec<(String, f64, i64)> = sqlx::query_as(
        "SELECT item, SUM(kg), COUNT(DISTINCT agent) FROM pool_intents WHERE item NOT LIKE 'evidence:%' AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now', $1) \
           AND ($2 = '' OR item = $2) AND ($3 = '' OR lower(city) = lower($3)) AND ($4 = '' OR upper(country) = upper($4)) GROUP BY item ORDER BY 2 DESC LIMIT 50")
        .bind(format!("-{days} days")).bind(q.item.unwrap_or_default().to_lowercase()).bind(q.city.unwrap_or_default()).bind(q.country.unwrap_or_default())
        .fetch_all(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"days": days, "demand": rows.into_iter().map(|(i, kg, n)| json!({"item": i, "kg": kg, "agents": n})).collect::<Vec<_>>()})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use yaya_wire::Keypair;

    async fn linked(app: &App, name: &str) -> (Keypair, String) {
        let kp = Keypair::generate();
        let id = kp.id().to_string();
        let acct = format!("acct_{name}");
        sqlx::query("INSERT INTO accounts (id, email, name, phone, password_hash) VALUES ($1,$2,$3,$4,'')").bind(&acct).bind(format!("{name}@t.est")).bind(name).bind(format!("519{name}")).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO account_agents (account, agent) VALUES ($1,$2)").bind(&acct).bind(&id).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO credit_ledger (id, account, delta, currency, kind) VALUES ($1,$2,$3,$4,'topup')").bind(uuid::Uuid::new_v4().to_string()).bind(&acct).bind(100_000).bind(credits::CURRENCY).execute(&app.db).await.unwrap();
        (kp, acct)
    }

    #[tokio::test]
    async fn pool_reaches_volume_price_and_pays_the_seller_on_majority_delivery() {
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("debug").try_init();
        let app = crate::test_app().await;
        let shared: Shared = Arc::new(app);
        let (farmer, farm_acct) = linked(&shared, "farm").await;
        let (r1, a1) = linked(&shared, "r1").await;
        let (r2, a2) = linked(&shared, "r2").await;
        let (r3, a3) = linked(&shared, "r3").await;
        let auth = |kp: &Keypair| Auth::Proven(kp.id());
        let items = json!({"papa": {"tiers": [{"kg": 0, "unitMinor": 187}, {"kg": 400, "unitMinor": 128}, {"kg": 800, "unitMinor": 110}]}});
        let created = create(State(shared.clone()), Extension(auth(&farmer)), Json(CreateReq {
            title: "Camión de papa Huancayo".into(), seller: None, items, min_kg: 400.0, target_kg: 800.0, delivery_minor: Some(25_000), deadline: "2030-01-01T00:00:00Z".into(), city: Some("Lima".into()), country: Some("PE".into()), note: None,
        })).await.unwrap();
        let body = axum::body::to_bytes(created.into_body(), 1 << 20).await.unwrap();
        let p: Value = serde_json::from_slice(&body).unwrap();
        let id = p["id"].as_str().unwrap().to_string();
        for (kp, kg) in [(&r1, 300.0), (&r2, 300.0), (&r3, 300.0)] {
            join(State(shared.clone()), Extension(auth(kp)), Path(id.clone()), Json(JoinReq { items: json!({"papa": kg}) })).await.unwrap();
        }
        // escrow at the minimum tier: 300 × 1.28 + delivery share 250 × 300/400 = 384 + 187.5
        assert_eq!(credits::balance(&shared, &a1).await.unwrap(), 100_000 - 38_400 - 18_750);
        close(State(shared.clone()), Extension(auth(&r1)), Path(id.clone())).await.unwrap_err(); // not organizer, before deadline
        close(State(shared.clone()), Extension(auth(&farmer)), Path(id.clone())).await.unwrap();
        let p = load(&shared, &id).await.unwrap();
        assert_eq!(p["status"], "shipped");
        // 900 kg → truck tier 1.10: due = 300 × 1.10 + 250 × 300/900 = 330 + 83.33
        assert_eq!(credits::balance(&shared, &a2).await.unwrap(), 100_000 - 33_000 - 8_333);
        confirm(State(shared.clone()), Extension(auth(&r1)), Path(id.clone()), Json(ConfirmReq { delivered: true })).await.unwrap();
        assert_eq!(load(&shared, &id).await.unwrap()["status"], "shipped", "one vote is not a majority");
        confirm(State(shared.clone()), Extension(auth(&r2)), Path(id.clone()), Json(ConfirmReq { delivered: true })).await.unwrap();
        let p = load(&shared, &id).await.unwrap();
        assert_eq!(p["status"], "delivered");
        let gross = 3 * (33_000 + 8_333);
        let fee = (gross * 2 / 100).min(2000);
        assert_eq!(credits::balance(&shared, &farm_acct).await.unwrap(), 100_000 + gross - fee);
        assert_eq!(p["feeMinor"], fee);
        assert_eq!(credits::balance(&shared, &a3).await.unwrap(), 100_000 - 33_000 - 8_333);
    }

    #[tokio::test]
    async fn pool_below_minimum_refunds_everyone() {
        let app = crate::test_app().await;
        let shared: Shared = Arc::new(app);
        let (farmer, _) = linked(&shared, "farm2").await;
        let (r1, a1) = linked(&shared, "r9").await;
        let items = json!({"cebolla": {"tiers": [{"kg": 0, "unitMinor": 238}, {"kg": 400, "unitMinor": 162}]}});
        let created = create(State(shared.clone()), Extension(Auth::Proven(farmer.id())), Json(CreateReq {
            title: "cebolla".into(), seller: None, items, min_kg: 400.0, target_kg: 800.0, delivery_minor: None, deadline: "2020-01-01T00:00:00Z".into(), city: None, country: None, note: None,
        })).await.unwrap();
        let p: Value = serde_json::from_slice(&axum::body::to_bytes(created.into_body(), 1 << 20).await.unwrap()).unwrap();
        let id = p["id"].as_str().unwrap().to_string();
        join(State(shared.clone()), Extension(Auth::Proven(r1.id())), Path(id.clone()), Json(JoinReq { items: json!({"cebolla": 50}) })).await.unwrap();
        assert!(credits::balance(&shared, &a1).await.unwrap() < 100_000);
        close(State(shared.clone()), Extension(Auth::Proven(r1.id())), Path(id.clone())).await.unwrap(); // past deadline: anyone
        assert_eq!(load(&shared, &id).await.unwrap()["status"], "cancelled");
        assert_eq!(credits::balance(&shared, &a1).await.unwrap(), 100_000);
    }
}
