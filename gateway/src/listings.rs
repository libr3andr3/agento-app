//! The market: information for sale, paid in credits.
//!
//! A listing is a **file** (a PDF handbook), a **bundle** (niche onboarding
//! questions, defaults and a skill text the buyer's agent mounts — the
//! "peluquería pro" pack), or a **note** (text). Anyone with an account can
//! sell; the platform sells as the `PLATFORM_ACCOUNT`. Buying moves the
//! price from buyer to seller minus the market fee (`wallet::move_credits`),
//! once per buyer per listing, and unlocks the content/download for good.
//! The buyer's phone pulls `GET /v1/purchases` and mounts what it finds.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{accounts, credits, err, internal, wallet, ApiResult, App, Auth, Shared};

const KINDS: [&str; 3] = ["file", "bundle", "note"];
const MAX_CONTENT: usize = 96 * 1024;

fn dir(app: &App) -> std::path::PathBuf {
    app.listing_dir.clone()
}

fn clean(s: Option<&str>, max: usize) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(max).collect())
}

fn niche_key(s: Option<&str>) -> Option<String> {
    clean(s, 64).map(|n| n.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>()).filter(|n| !n.is_empty())
}

/// Bundles must look like a bundle: `fields`/`defaults` objects, `skill`
/// text, `tools` list — nothing else is mounted on a phone.
fn check_content(kind: &str, content: &Value) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    if content.is_null() {
        return Ok(None);
    }
    let s = match kind {
        "bundle" => {
            let o = content.as_object().ok_or_else(|| err(StatusCode::BAD_REQUEST, "bundle content must be an object"))?;
            for k in o.keys() {
                if !["fields", "defaults", "skill", "tools", "name", "version", "questions"].contains(&k.as_str()) {
                    return Err(err(StatusCode::BAD_REQUEST, format!("bundle content: unknown key '{k}'")));
                }
            }
            if o.get("fields").is_some_and(|f| !f.is_object()) || o.get("defaults").is_some_and(|f| !f.is_object()) {
                return Err(err(StatusCode::BAD_REQUEST, "bundle fields/defaults must be objects"));
            }
            if o.get("skill").is_some_and(|f| !f.is_string()) {
                return Err(err(StatusCode::BAD_REQUEST, "bundle skill must be text"));
            }
            content.to_string()
        }
        "note" => content.as_str().map(String::from).unwrap_or_else(|| content.to_string()),
        _ => content.to_string(),
    };
    if s.len() > MAX_CONTENT {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "content too large (96 KB)"));
    }
    Ok(Some(s))
}

#[derive(serde::Deserialize, Default)]
pub struct ListingReq {
    #[serde(default)] kind: Option<String>,
    #[serde(default)] title: Option<String>,
    #[serde(default)] description: Option<String>,
    #[serde(default)] niche: Option<String>,
    #[serde(default, rename = "priceMinor")] price_minor: Option<i64>,
    #[serde(default)] content: Value,
    #[serde(default)] status: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    account: String,
    agent: Option<String>,
    kind: String,
    title: String,
    description: Option<String>,
    niche: Option<String>,
    price_minor: i64,
    currency: String,
    content: Option<String>,
    file_name: Option<String>,
    file_size: Option<i64>,
    file_sha256: Option<String>,
    content_type: Option<String>,
    status: String,
    sales: i64,
    created_at: String,
    updated_at: String,
}

const COLS: &str = "id, account, agent, kind, title, description, niche, price_minor, currency, content, file_name, file_size, file_sha256, content_type, status, sales, created_at, updated_at";

async fn seller_name(app: &App, account: &str, agent: Option<&str>) -> Value {
    if account == wallet::platform_account() {
        return json!("Yaya");
    }
    if let Some(a) = agent {
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT name, handle FROM agents WHERE agent = $1").bind(a).fetch_optional(&app.db).await.ok().flatten();
        if let Some((n, h)) = row {
            if let Some(h) = h { return json!(format!("@{h}")); }
            if let Some(n) = n { return json!(n); }
        }
    }
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT name FROM accounts WHERE id = $1").bind(account).fetch_optional(&app.db).await.ok().flatten();
    row.and_then(|r| r.0).map(Value::String).unwrap_or(Value::Null)
}

async fn to_json(app: &App, r: &Row, viewer: Option<&str>, with_content: bool) -> Value {
    let owner = viewer.is_some_and(|v| v == r.account);
    let bought = match viewer {
        Some(v) => sqlx::query_as::<_, (String,)>("SELECT at FROM purchases WHERE listing = $1 AND buyer = $2").bind(&r.id).bind(v).fetch_optional(&app.db).await.ok().flatten().map(|x| x.0),
        None => None,
    };
    let unlocked = owner || bought.is_some() || r.price_minor == 0;
    let mut v = json!({
        "id": r.id, "kind": r.kind, "title": r.title, "description": r.description, "niche": r.niche,
        "priceMinor": r.price_minor, "currency": r.currency, "status": r.status, "sales": r.sales,
        "seller": {"name": seller_name(app, &r.account, r.agent.as_deref()).await, "platform": r.account == wallet::platform_account(), "agent": r.agent},
        "file": r.file_name.as_ref().map(|n| json!({"name": n, "size": r.file_size, "sha256": r.file_sha256, "contentType": r.content_type})),
        "hasContent": r.content.is_some(),
        "createdAt": r.created_at, "updatedAt": r.updated_at,
        "mine": owner, "purchasedAt": bought, "unlocked": unlocked,
        "download": r.file_name.as_ref().map(|_| format!("/v1/listings/{}/download", r.id)),
    });
    if with_content && unlocked {
        v["content"] = r.content.as_deref().map(|c| serde_json::from_str::<Value>(c).unwrap_or_else(|_| Value::String(c.to_string()))).unwrap_or(Value::Null);
    }
    v
}

async fn fetch(app: &App, id: &str) -> Result<Row, (StatusCode, Json<Value>)> {
    sqlx::query_as::<_, Row>(&format!("SELECT {COLS} FROM listings WHERE id = $1")).bind(id).fetch_optional(&app.db).await.map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown listing"))
}

/// Creates a listing for `account`. Active as soon as it has something to
/// sell (content now, or a file uploaded next); a bare shell stays a draft.
pub async fn create_for(app: &App, account: &str, agent: Option<&str>, req: ListingReq) -> Result<Value, (StatusCode, Json<Value>)> {
    let kind = req.kind.as_deref().unwrap_or("file").trim().to_lowercase();
    if !KINDS.contains(&kind.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "kind must be file, bundle or note"));
    }
    let title = clean(req.title.as_deref(), 120).ok_or_else(|| err(StatusCode::BAD_REQUEST, "title is required"))?;
    let price = req.price_minor.unwrap_or(0);
    if !(0..=1_000_000).contains(&price) {
        return Err(err(StatusCode::BAD_REQUEST, "priceMinor must be 0..=1 000 000 (S/ 10 000)"));
    }
    let content = check_content(&kind, &req.content)?;
    let id = format!("lst_{}", uuid::Uuid::new_v4().simple().to_string()[..12].to_string());
    let status = if content.is_some() { "active" } else { "draft" };
    sqlx::query("INSERT INTO listings (id, account, agent, kind, title, description, niche, price_minor, currency, content, status) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
        .bind(&id).bind(account).bind(agent).bind(&kind).bind(&title).bind(clean(req.description.as_deref(), 2000)).bind(niche_key(req.niche.as_deref()))
        .bind(price).bind(credits::CURRENCY).bind(&content).bind(status)
        .execute(&app.db).await.map_err(internal)?;
    tracing::info!(%id, %account, %kind, price, "listing created");
    Ok(to_json(app, &fetch(app, &id).await?, Some(account), true).await)
}

/// `POST /v1/listings`
pub async fn create(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<ListingReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    let agent = match &auth { Auth::Proven(a) | Auth::Unproven(a) => Some(a.to_string()), _ => None };
    Ok(Json(create_for(&app, &account, agent.as_deref(), req).await?).into_response())
}

/// `POST /admin/listings` — the platform's own catalogue (handbook, rubro packs).
pub async fn admin_create(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<ListingReq>) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    Ok(Json(create_for(&app, &wallet::platform_account(), None, req).await?).into_response())
}

async fn owned(app: &App, id: &str, account: &str) -> Result<Row, (StatusCode, Json<Value>)> {
    let r = fetch(app, id).await?;
    if r.account != account {
        return Err(err(StatusCode::FORBIDDEN, "not your listing"));
    }
    Ok(r)
}

pub async fn update_for(app: &App, account: &str, id: &str, req: ListingReq) -> Result<Value, (StatusCode, Json<Value>)> {
    let r = owned(app, id, account).await?;
    let title = clean(req.title.as_deref(), 120).unwrap_or(r.title.clone());
    let description = clean(req.description.as_deref(), 2000).or(r.description.clone());
    let price = req.price_minor.unwrap_or(r.price_minor);
    if !(0..=1_000_000).contains(&price) {
        return Err(err(StatusCode::BAD_REQUEST, "priceMinor must be 0..=1 000 000"));
    }
    let content = check_content(&r.kind, &req.content)?.or(r.content.clone());
    let niche = niche_key(req.niche.as_deref()).or(r.niche.clone());
    let sellable = content.is_some() || r.file_name.is_some();
    let status = match req.status.as_deref() {
        Some("hidden") => "hidden",
        Some("active") if sellable => "active",
        Some("active") => return Err(err(StatusCode::BAD_REQUEST, "add content or upload a file before activating")),
        // No status asked: a draft goes live once it has something to sell;
        // an active or hidden listing keeps what its owner chose.
        None if r.status == "draft" && sellable => "active",
        None => &r.status,
        Some(_) => return Err(err(StatusCode::BAD_REQUEST, "status must be active or hidden")),
    };
    sqlx::query("UPDATE listings SET title = $1, description = $2, price_minor = $3, content = $4, niche = $5, status = $6, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = $7")
        .bind(&title).bind(&description).bind(price).bind(&content).bind(&niche).bind(status).bind(id)
        .execute(&app.db).await.map_err(internal)?;
    Ok(to_json(app, &fetch(app, id).await?, Some(account), true).await)
}

/// `PATCH /v1/listings/{id}`
pub async fn update(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, Json(req): Json<ListingReq>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    Ok(Json(update_for(&app, &account, &id, req).await?).into_response())
}

pub async fn admin_update(State(app): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<ListingReq>) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    Ok(Json(update_for(&app, &wallet::platform_account(), &id, req).await?).into_response())
}

pub async fn upload_for(app: &App, account: &str, id: &str, headers: &HeaderMap, body: Bytes) -> Result<Value, (StatusCode, Json<Value>)> {
    let r = owned(app, id, account).await?;
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty body"));
    }
    let name = clean(headers.get("x-file-name").and_then(|v| v.to_str().ok()), 120)
        .map(|n| n.chars().filter(|c| !matches!(c, '/' | '\\' | '\0')).collect::<String>())
        .unwrap_or_else(|| format!("{}.bin", r.id));
    let ctype = clean(headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()), 100).unwrap_or_else(|| "application/octet-stream".into());
    let sha = yaya_wire::sha256_hex(&body);
    let d = dir(app);
    tokio::fs::create_dir_all(&d).await.map_err(internal)?;
    tokio::fs::write(d.join(format!("{}.bin", r.id)), &body).await.map_err(internal)?;
    sqlx::query("UPDATE listings SET file_name = $1, file_size = $2, file_sha256 = $3, content_type = $4, status = CASE WHEN status = 'draft' THEN 'active' ELSE status END, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = $5")
        .bind(&name).bind(body.len() as i64).bind(&sha).bind(&ctype).bind(id).execute(&app.db).await.map_err(internal)?;
    tracing::info!(%id, size = body.len(), %name, "listing file stored");
    Ok(to_json(app, &fetch(app, id).await?, Some(account), true).await)
}

/// `PUT /v1/listings/{id}/file` — raw bytes; `X-File-Name` + `Content-Type`.
pub async fn upload(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    Ok(Json(upload_for(&app, &account, &id, &headers, body).await?).into_response())
}

pub async fn admin_upload(State(app): State<Shared>, headers: HeaderMap, Path(id): Path<String>, body: Bytes) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    Ok(Json(upload_for(&app, &wallet::platform_account(), &id, &headers, body).await?).into_response())
}

#[derive(serde::Deserialize, Default)]
pub struct ListQ {
    #[serde(default)] niche: Option<String>,
    #[serde(default)] kind: Option<String>,
    #[serde(default)] q: Option<String>,
    #[serde(default)] mine: Option<String>,
    #[serde(default)] limit: Option<i64>,
}

/// `GET /v1/listings` — public catalogue (active), or `?mine=1` for my own
/// including drafts and hidden ones.
pub async fn list(State(app): State<Shared>, Extension(auth): Extension<Auth>, Query(q): Query<ListQ>) -> ApiResult {
    let viewer = accounts::account_of_auth(&app, &auth).await.ok();
    let mine = q.mine.as_deref().is_some_and(|m| m == "1" || m == "true");
    if mine && viewer.is_none() {
        return Err(err(StatusCode::UNAUTHORIZED, "sign in to list your own"));
    }
    let like = q.q.as_deref().map(|s| format!("%{}%", s.trim().to_lowercase())).filter(|s| s.len() > 2);
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(&format!(
        "SELECT {COLS} FROM listings WHERE (($1 = 1 AND account = $2) OR ($1 = 0 AND status = 'active')) \
           AND ($3 IS NULL OR niche = $3 OR niche IS NULL) AND ($4 IS NULL OR kind = $4) \
           AND ($5 IS NULL OR lower(title) LIKE $5 OR lower(description) LIKE $5) \
         ORDER BY (account = $6) DESC, sales DESC, updated_at DESC LIMIT $7"))
        .bind(mine as i64).bind(viewer.clone().unwrap_or_default()).bind(niche_key(q.niche.as_deref())).bind(clean(q.kind.as_deref(), 10)).bind(&like)
        .bind(wallet::platform_account()).bind(q.limit.unwrap_or(60).clamp(1, 200))
        .fetch_all(&app.db).await.map_err(internal)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(to_json(&app, r, viewer.as_deref(), false).await);
    }
    Ok(Json(json!({"listings": out, "feePercent": wallet::market_fee_percent(), "currency": credits::CURRENCY})).into_response())
}

/// `GET /v1/listings/{id}` — details; content included once unlocked.
pub async fn get(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let viewer = accounts::account_of_auth(&app, &auth).await.ok();
    let r = fetch(&app, &id).await?;
    if r.status != "active" && viewer.as_deref() != Some(r.account.as_str()) {
        return Err(err(StatusCode::NOT_FOUND, "unknown listing"));
    }
    Ok(Json(to_json(&app, &r, viewer.as_deref(), true).await).into_response())
}

/// `POST /v1/listings/{id}/buy` — once per buyer; idempotent.
pub async fn buy(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let buyer = accounts::account_of_auth(&app, &auth).await?;
    let r = fetch(&app, &id).await?;
    if r.status != "active" && r.account != buyer {
        return Err(err(StatusCode::NOT_FOUND, "unknown listing"));
    }
    // The purchase row and the money move commit together: the row's
    // UNIQUE (listing, buyer) is what makes a second, racing buy a duplicate.
    let mut duplicate = false;
    if r.account != buyer {
        let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
        let fee = wallet::fee_of(r.price_minor, wallet::market_fee_percent());
        let inserted = sqlx::query("INSERT OR IGNORE INTO purchases (id, listing, buyer, seller, price, fee) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(format!("pur_{}", uuid::Uuid::new_v4().simple())).bind(&id).bind(&buyer).bind(&r.account).bind(r.price_minor).bind(fee)
            .execute(&mut *tx).await.map_err(internal)?.rows_affected();
        if inserted == 1 {
            if r.price_minor > 0 {
                let moved = wallet::move_credits_in(&mut tx, &buyer, &r.account, r.price_minor, wallet::market_fee_percent(), "purchase", "sale", Some(&id),
                    &format!("compra: {}", r.title), &format!("venta: {}", r.title)).await?;
                if moved.is_none() {
                    drop(tx);
                    return Err(wallet::payment_required(&format!("'{}'", r.title), r.price_minor, credits::balance(&app, &buyer).await?));
                }
            }
            sqlx::query("UPDATE listings SET sales = sales + 1 WHERE id = $1").bind(&id).execute(&mut *tx).await.map_err(internal)?;
            tx.commit().await.map_err(internal)?;
            tracing::info!(listing = %id, %buyer, seller = %r.account, charged = r.price_minor, "listing bought");
        } else {
            duplicate = true;
        }
    }
    let mut v = to_json(&app, &fetch(&app, &id).await?, Some(&buyer), true).await;
    v["ok"] = json!(true);
    v["duplicate"] = json!(duplicate);
    v["balance"] = json!(credits::balance(&app, &buyer).await?);
    Ok(Json(v).into_response())
}

/// `GET /v1/listings/{id}/download` — the file, for buyers and the seller.
pub async fn download(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let viewer = accounts::account_of_auth(&app, &auth).await?;
    let r = fetch(&app, &id).await?;
    let bought: Option<(String,)> = sqlx::query_as("SELECT at FROM purchases WHERE listing = $1 AND buyer = $2").bind(&id).bind(&viewer).fetch_optional(&app.db).await.map_err(internal)?;
    if r.account != viewer && bought.is_none() && r.price_minor > 0 {
        return Err(err(StatusCode::PAYMENT_REQUIRED, "buy this listing first"));
    }
    let Some(name) = r.file_name.clone() else { return Err(err(StatusCode::NOT_FOUND, "this listing has no file")) };
    let bytes = tokio::fs::read(dir(&app).join(format!("{}.bin", r.id))).await.map_err(|_| err(StatusCode::NOT_FOUND, "file missing"))?;
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, r.content_type.as_deref().unwrap_or("application/octet-stream").parse().unwrap_or(header::HeaderValue::from_static("application/octet-stream")));
    let disp = format!("attachment; filename=\"{}\"", name.replace('"', ""));
    if let Ok(v) = header::HeaderValue::from_str(&disp) { h.insert(header::CONTENT_DISPOSITION, v); }
    Ok((h, bytes).into_response())
}

/// `GET /v1/purchases` — everything I bought, content included: the phone
/// mounts bundles from here.
pub async fn purchases(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let buyer = accounts::account_of_auth(&app, &auth).await?;
    let rows: Vec<(String, String, i64, String)> = sqlx::query_as("SELECT id, listing, price, at FROM purchases WHERE buyer = $1 ORDER BY at DESC LIMIT 200")
        .bind(&buyer).fetch_all(&app.db).await.map_err(internal)?;
    let mut out = Vec::new();
    for (pid, lid, price, at) in rows {
        if let Ok(r) = fetch(&app, &lid).await {
            let mut v = to_json(&app, &r, Some(&buyer), true).await;
            v["purchase"] = json!({"id": pid, "priceMinor": price, "at": at});
            out.push(v);
        }
    }
    Ok(Json(json!({"purchases": out})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, anon, as_admin, as_agent, call, Keypair};

    async fn people(app: &Shared) -> (Keypair, Keypair) {
        let (s, b) = (Keypair::generate(), Keypair::generate());
        account_with_agent(app, "seller", "51900000001", &s).await;
        account_with_agent(app, "buyer", "51900000002", &b).await;
        credits::add(app, "buyer", 1_000, "topup", None, None).await.unwrap();
        (s, b)
    }

    async fn note(app: &Shared, kp: &Keypair, price: i64) -> String {
        let (st, v) = as_agent(app, kp, "POST", "/v1/listings", Some(json!({"kind": "note", "title": "Guía", "priceMinor": price, "content": "secreto", "niche": "Peluquería-PE!"}))).await;
        assert_eq!(st, 200, "{v}");
        v["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn buying_charges_once_even_when_two_buys_race() {
        let app = testkit::app().await;
        let (s, b) = people(&app).await;
        let id = note(&app, &s, 300).await;
        let path = format!("/v1/listings/{id}/buy");
        let (x, y) = tokio::join!(as_agent(&app, &b, "POST", &path, None), as_agent(&app, &b, "POST", &path, None));
        assert_eq!((x.0, y.0), (200, 200));
        assert_eq!(credits::balance(&app, "buyer").await.unwrap(), 700, "charged once");
        assert_eq!(credits::balance(&app, "seller").await.unwrap(), 300 - wallet::fee_of(300, wallet::market_fee_percent()));
        let (sales,): (i64,) = sqlx::query_as("SELECT sales FROM listings WHERE id = $1").bind(&id).fetch_one(&app.db).await.unwrap();
        assert_eq!(sales, 1);
        let (_, again) = as_agent(&app, &b, "POST", &path, None).await;
        assert_eq!((again["duplicate"].clone(), again["content"].clone()), (json!(true), json!("secreto")));
    }

    #[tokio::test]
    async fn content_unlocks_only_for_buyers_and_owners() {
        let app = testkit::app().await;
        let (s, b) = people(&app).await;
        let id = note(&app, &s, 300).await;
        let (_, v) = as_agent(&app, &b, "GET", &format!("/v1/listings/{id}"), None).await;
        assert_eq!((v["unlocked"].clone(), v["content"].clone(), v["niche"].clone()), (json!(false), Value::Null, json!("peluquería-pe".chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>())));
        let (_, mine) = as_agent(&app, &s, "GET", &format!("/v1/listings/{id}"), None).await;
        assert_eq!(mine["content"], "secreto");
        // Too poor, then bought.
        let poor = Keypair::generate();
        account_with_agent(&app, "poor", "51900000003", &poor).await;
        assert_eq!(as_agent(&app, &poor, "POST", &format!("/v1/listings/{id}/buy"), None).await.0, 402);
        assert_eq!(credits::balance(&app, "seller").await.unwrap(), 0);
        as_agent(&app, &b, "POST", &format!("/v1/listings/{id}/buy"), None).await;
        let (_, p) = as_agent(&app, &b, "GET", "/v1/purchases", None).await;
        assert_eq!(p["purchases"][0]["content"], "secreto");
        assert_eq!(anon(&app, "GET", "/v1/purchases", None).await.0, 401);
        // The owner buying their own listing is free and records nothing.
        let (_, own) = as_agent(&app, &s, "POST", &format!("/v1/listings/{id}/buy"), None).await;
        assert_eq!((own["mine"].clone(), own["duplicate"].clone()), (json!(true), json!(false)));
    }

    #[tokio::test]
    async fn files_download_for_buyers_only() {
        let app = testkit::app().await;
        let (s, b) = people(&app).await;
        let (_, v) = as_agent(&app, &s, "POST", "/v1/listings", Some(json!({"kind": "file", "title": "Manual", "priceMinor": 100}))).await;
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["status"], "draft", "a shell stays a draft");
        assert_eq!(anon(&app, "GET", &format!("/v1/listings/{id}"), None).await.0, 404, "drafts are private");
        assert_eq!(as_agent(&app, &b, "POST", &format!("/v1/listings/{id}/buy"), None).await.0, 404);
        let signed = |kp: &Keypair, m: &str, p: &str, body: Vec<u8>, extra: &[(&'static str, String)]| {
            let mut h = testkit::agent_headers(kp, m, p, &body);
            h.extend(extra.iter().cloned());
            (h, body)
        };
        let up = format!("/v1/listings/{id}/file");
        let (h, body) = signed(&b, "PUT", &up, b"x".to_vec(), &[]);
        assert_eq!(call(&app, "PUT", &up, &h, Some(body)).await.0, 403, "not your listing");
        let (h, body) = signed(&s, "PUT", &up, b"%PDF-1.7 hola".to_vec(), &[("x-file-name", "../../etc/pa\\sswd.pdf".into()), ("content-type", "application/pdf".into())]);
        let (st, v) = call(&app, "PUT", &up, &h, Some(body)).await;
        assert_eq!((st, v["status"].clone(), v["file"]["name"].clone()), (200, json!("active"), json!("....etcpasswd.pdf")), "{v}");
        let dl = format!("/v1/listings/{id}/download");
        assert_eq!(as_agent(&app, &b, "GET", &dl, None).await.0, 402);
        as_agent(&app, &b, "POST", &format!("/v1/listings/{id}/buy"), None).await;
        let (h, _) = signed(&b, "GET", &dl, vec![], &[]);
        let res = testkit::raw(&app, "GET", &dl, &h, None).await;
        assert_eq!(res.0, 200);
        assert_eq!(res.1, b"%PDF-1.7 hola".to_vec());
    }

    #[tokio::test]
    async fn editing_a_hidden_listing_keeps_it_off_the_market() {
        let app = testkit::app().await;
        let (s, b) = people(&app).await;
        let id = note(&app, &s, 0).await;
        let p = format!("/v1/listings/{id}");
        assert_eq!(as_agent(&app, &s, "PATCH", &p, Some(json!({"status": "hidden"}))).await.1["status"], "hidden");
        let (_, v) = as_agent(&app, &s, "PATCH", &p, Some(json!({"title": "Guía 2"}))).await;
        assert_eq!((v["status"].clone(), v["title"].clone()), (json!("hidden"), json!("Guía 2")));
        assert_eq!(anon(&app, "GET", &p, None).await.0, 404);
        assert_eq!(as_agent(&app, &b, "PATCH", &p, Some(json!({"title": "mío"}))).await.0, 403);
        assert_eq!(as_agent(&app, &s, "PATCH", &p, Some(json!({"status": "sold"}))).await.0, 400);
        assert_eq!(as_agent(&app, &s, "PATCH", &p, Some(json!({"priceMinor": -1}))).await.0, 400);
        assert_eq!(as_agent(&app, &s, "PATCH", &p, Some(json!({"status": "active"}))).await.1["status"], "active");
        // A draft with nothing to sell cannot be activated.
        let (_, d) = as_agent(&app, &s, "POST", "/v1/listings", Some(json!({"kind": "file", "title": "Vacío"}))).await;
        assert_eq!(as_agent(&app, &s, "PATCH", &format!("/v1/listings/{}", d["id"].as_str().unwrap()), Some(json!({"status": "active"}))).await.0, 400);
    }

    #[tokio::test]
    async fn the_catalogue_filters_and_the_platform_sells_as_yaya() {
        let app = testkit::app().await;
        let (s, b) = people(&app).await;
        note(&app, &s, 10).await;
        let (st, v) = as_admin(&app, "POST", "/admin/listings", Some(json!({"kind": "bundle", "title": "Pack peluquería", "content": {"skill": "corta bien", "fields": {}}}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["seller"]["name"], "Yaya");
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(as_admin(&app, "PATCH", &format!("/admin/listings/{id}"), Some(json!({"priceMinor": 5}))).await.1["priceMinor"], 5);
        let (_, all) = anon(&app, "GET", "/v1/listings", None).await;
        assert_eq!(all["listings"].as_array().unwrap().len(), 2);
        assert_eq!(all["listings"][0]["seller"]["platform"], true, "the platform's own come first");
        let (_, kind) = anon(&app, "GET", "/v1/listings?kind=note&q=gu", None).await;
        assert_eq!(kind["listings"].as_array().unwrap().len(), 1);
        assert_eq!(anon(&app, "GET", "/v1/listings?mine=1", None).await.0, 401);
        let (_, mine) = as_agent(&app, &b, "GET", "/v1/listings?mine=1", None).await;
        assert_eq!(mine["listings"].as_array().unwrap().len(), 0);
        for bad in [json!({"kind": "video", "title": "x"}), json!({"title": " "}), json!({"title": "x", "priceMinor": 1_000_001}),
                    json!({"kind": "bundle", "title": "x", "content": {"evil": 1}}), json!({"kind": "bundle", "title": "x", "content": {"skill": 3}}),
                    json!({"kind": "bundle", "title": "x", "content": []}), json!({"kind": "note", "title": "x", "content": "a".repeat(MAX_CONTENT + 1)})] {
            assert!(matches!(as_agent(&app, &s, "POST", "/v1/listings", Some(bad.clone())).await.0, 400 | 413), "{bad}");
        }
    }
}

