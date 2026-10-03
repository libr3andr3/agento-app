//! Yape top-ups, matched by unique céntimos.
//!
//! Every Yape/Plin request the gateway opens on its own asks for an amount
//! no other pending request holds (`plans::insert_tagged`): the exact price
//! when it is free — nearly always — else the nearest free one in the order
//! +1, −1, +2, −2 céntimos. A request lives the day it was opened; paid,
//! cancelled or expired, its amount is free again at once. The house phone — the one whose Yape receives the
//! money — forwards each Yape notification here (`POST /v1/collector/yape`,
//! `x-collector-key`), and the amount alone says which request it pays.
//!
//! * **Package filter.** Only the Yape app's own notifications are taken
//!   (`YAPE_PACKAGES`, default `com.bcp.innovacxion.yapeapp`): a chat
//!   message that *says* "te envió S/ 20.37" is not a payment.
//! * **Deduplication.** A notification is stored once under
//!   sha256(package | its own post time | title | text). Android reposts the
//!   same notification when it is updated, and the phone retries when the
//!   network drops; both collapse onto the first row. Two *real* transfers
//!   of the same amount have different post times, and the second finds
//!   its request already paid (the claim in `plans::confirm_request` is
//!   atomic), so it is queued as `already_paid`, never credited twice.
//! * **Matching.** Exact amount against a live request → paid. A request is
//!   live while pending, and also after it expired when the notification's
//!   own time says the money moved before it did. Otherwise a *rounded*
//!   amount (the price without its céntimos, or up to the next sol) is
//!   accepted only when exactly one live request fits it. A payment made
//!   after its request expired still lands when exactly one expired request
//!   (last [`LATE_LOOKBACK_DAYS`]) had that exact amount.
//! * **Unmatched queue.** Anything else — ambiguous rounding, no request,
//!   request expired or already paid another way, wording we could not
//!   read — is kept with a `reason` and the refs an operator may mean, and
//!   the sales phone is told. `GET /admin/yape`, then `assign` or `dismiss`.
//!
//! The text is read here, deliberately, and not by an agent: this is our
//! own till, and a till is exact or it is wrong.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};

use crate::{err, internal, plans, ApiResult, App, Shared};

const YAPE: &str = "com.bcp.innovacxion.yapeapp";

/// Packages whose notifications are accepted (`YAPE_PACKAGES`, comma list).
pub fn packages() -> Vec<String> {
    std::env::var("YAPE_PACKAGES").ok().filter(|s| !s.trim().is_empty())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_else(|| vec![YAPE.to_string()])
}

// ------------------------------------------------------------------ reading

/// What one notification says.
#[derive(Debug, PartialEq)]
pub struct Reading {
    /// Money came in (as opposed to going out, a promo, a code).
    pub incoming: bool,
    pub amount_minor: Option<i64>,
    pub payer: Option<String>,
}

/// Lowercase, accents folded, one char for one char — so an index into the
/// folded text is the same index into the original.
fn fold(s: &str) -> Vec<char> {
    s.chars().map(|c| {
        let c = c.to_lowercase().next().unwrap_or(c);
        match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            '\u{a0}' | '\t' | '\r' => ' ',
            c => c,
        }
    }).collect()
}

fn find(h: &[char], needle: &str, from: usize) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()] == n[..])
}

/// "S/ 1,234.50", "S/.20", "S/ 5." → minor units.
fn amount_at(h: &[char], mut i: usize) -> Option<i64> {
    while i < h.len() && (h[i] == '.' || h[i] == ' ') {
        i += 1;
    }
    let start = i;
    while i < h.len() && (h[i].is_ascii_digit() || h[i] == ',' || h[i] == '.') {
        i += 1;
    }
    let mut raw: String = h[start..i].iter().collect();
    while raw.ends_with(['.', ',']) {
        raw.pop();
    }
    if raw.is_empty() || !raw.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    // The last separator followed by one or two digits is the decimal
    // point, whichever mark it is; every other separator groups thousands.
    let (int, dec) = match raw.rfind(['.', ',']) {
        Some(p) if raw.len() - p - 1 <= 2 => (&raw[..p], &raw[p + 1..]),
        _ => (raw.as_str(), ""),
    };
    let int: String = int.chars().filter(char::is_ascii_digit).collect();
    let soles: i64 = int.parse().ok()?;
    let cents: i64 = match dec.len() {
        0 => 0,
        1 => dec.parse::<i64>().ok()? * 10,
        _ => dec.parse().ok()?,
    };
    soles.checked_mul(100)?.checked_add(cents)
}

/// Reads a Yape notification. Wording varies by app version, so this looks
/// for the few phrases Yape uses for money *received* and refuses the ones
/// for money sent; anything with an amount but neither goes to a human.
pub fn read(title: &str, text: &str) -> Reading {
    let original: Vec<char> = format!("{title}\n{text}").chars().collect();
    let h = fold(&original.iter().collect::<String>());
    let amount = find(&h, "s/", 0).and_then(|i| amount_at(&h, i + 2));
    const OUT: [&str; 5] = ["yapeaste", "enviaste", "pagaste", "transferiste", "tu yapeo a"];
    if OUT.iter().any(|m| find(&h, m, 0).is_some()) {
        return Reading { incoming: false, amount_minor: amount, payer: None };
    }
    // "<Name> te envió un pago por S/ 20.37"
    const BEFORE: [&str; 7] = [" te envio", " te yapeo", " te ha yapeado", " te ha enviado", " te han enviado", " te pago", " te transfirio"];
    for m in BEFORE {
        if let Some(i) = find(&h, m, 0) {
            let line_start = h[..i].iter().rposition(|c| matches!(c, '\n' | '!' | ':')).map_or(0, |p| p + 1);
            let name: String = original[line_start..i].iter().collect();
            let name = name.trim().to_string();
            return Reading { incoming: true, amount_minor: amount, payer: (!name.is_empty()).then_some(name) };
        }
    }
    // "Recibiste S/ 20.37 de <Name>", "Recibiste un yapeo de <Name> por S/ 20"
    for m in ["recibiste", "has recibido"] {
        if let Some(i) = find(&h, m, 0) {
            let payer = find(&h, " de ", i).map(|d| {
                let from = d + 4;
                let end = [find(&h, " por ", from), h[from..].iter().position(|c| matches!(c, '\n' | '.')).map(|p| from + p)]
                    .into_iter().flatten().min().unwrap_or(h.len());
                original[from..end].iter().collect::<String>().trim().to_string()
            }).filter(|n| !n.is_empty() && !n.starts_with("S/"));
            return Reading { incoming: true, amount_minor: amount, payer };
        }
    }
    Reading { incoming: false, amount_minor: amount, payer: None }
}

// ----------------------------------------------------------------- matching

/// How far back a payment may reach for a request that already expired.
pub const LATE_LOOKBACK_DAYS: i64 = 7;

/// A row of `plan_requests` a payment might belong to.
#[derive(Debug)]
struct Candidate {
    reference: String,
    status: String,
    amount: i64,
    base: i64,
    /// The payment was made (by the notification's own clock) while this
    /// request still held its amount — even if it reached us after.
    in_time: bool,
}

async fn candidates(app: &App, posted_at: &str, amount_minor: i64) -> Result<Vec<Candidate>, (StatusCode, Json<Value>)> {
    // Requests opened up to the lookback before the payment, and up to an
    // hour after it (the phone's clock is not ours). Old untagged rows have
    // no amount_minor/base_minor: their `amount` is the round price.
    let exact_expr = "COALESCE(amount_minor, CAST(ROUND(amount * 100) AS INTEGER))";
    let base_expr = "COALESCE(base_minor, amount_minor, CAST(ROUND(amount * 100) AS INTEGER))";
    let sql = format!(
        "SELECT ref, status, {exact_expr}, {base_expr}, \
                (status = 'pending' OR (status = 'expired' AND COALESCE(expires_at, strftime('%Y-%m-%dT%H:%M:%fZ', created_at, '+1 day')) >= $1)) \
         FROM plan_requests \
         WHERE currency = 'PEN' AND payment_id IS NULL \
         AND created_at >= strftime('%Y-%m-%dT%H:%M:%fZ', $1, '-{LATE_LOOKBACK_DAYS} days') \
         AND created_at <= strftime('%Y-%m-%dT%H:%M:%fZ', $1, '+1 hour') \
         AND ({exact_expr} = $2 OR {base_expr} = $2 OR (({exact_expr} + 99) / 100) * 100 = $2) \
         ORDER BY created_at",
    );
    let rows: Vec<(String, String, i64, i64, bool)> = sqlx::query_as(&sql).bind(posted_at).bind(amount_minor)
        .fetch_all(&app.db).await.map_err(internal)?;
    Ok(rows.into_iter().map(|(reference, status, amount, base, in_time)| Candidate { reference, status, amount, base, in_time }).collect())
}

/// What became of one notification.
#[derive(Debug, PartialEq)]
enum Verdict {
    Matched { reference: String, kind: &'static str },
    Unmatched { reason: &'static str, suggestions: Vec<String> },
    Ignored { reason: &'static str },
}

fn refs(v: &[&Candidate]) -> Vec<String> {
    v.iter().map(|c| c.reference.clone()).collect()
}

async fn decide(app: &App, posted_at: &str, r: &Reading) -> Result<Verdict, (StatusCode, Json<Value>)> {
    let Some(paid) = r.amount_minor.filter(|a| *a > 0) else {
        return Ok(Verdict::Ignored { reason: "no_amount" });
    };
    if !r.incoming {
        // An amount, but not in words we know mean "received": a person
        // looks, rather than the till guessing either way.
        return Ok(Verdict::Unmatched { reason: "unread", suggestions: vec![] });
    }
    plans::expire_stale(app).await?;
    let all = candidates(app, posted_at, paid).await?;
    // Live: pending now, or paid for while it was (a notification that
    // reached us after midnight still counts from when the money moved).
    let live: Vec<&Candidate> = all.iter().filter(|c| c.in_time && (c.status == "pending" || c.status == "expired")).collect();
    let exact: Vec<&Candidate> = live.iter().copied().filter(|c| c.amount == paid).collect();
    if exact.len() == 1 {
        return Ok(Verdict::Matched { reference: exact[0].reference.clone(), kind: "exact" });
    }
    if exact.len() > 1 {
        // The same amount held twice: an expired request and the one that
        // took its amount after it, or two untagged legacy rows.
        return Ok(Verdict::Unmatched { reason: "ambiguous", suggestions: refs(&exact) });
    }
    // Rounded: the céntimos dropped (paid the price) or rounded up to the
    // next sol. Only when one live request fits — two friends buying the
    // same recarga who both round is a question for a human.
    let rounded: Vec<&Candidate> = live.iter().copied().filter(|c| c.base == paid || ((c.amount + 99) / 100) * 100 == paid).collect();
    if rounded.len() == 1 {
        return Ok(Verdict::Matched { reference: rounded[0].reference.clone(), kind: "rounded" });
    }
    if rounded.len() > 1 {
        return Ok(Verdict::Unmatched { reason: "ambiguous", suggestions: refs(&rounded) });
    }
    // Paid after its request expired: still theirs when exactly one expired
    // request carried that exact amount — the buyer is not punished for
    // being slow. More than one is a human's call.
    let expired: Vec<&Candidate> = all.iter().filter(|c| c.status == "expired" && c.amount == paid).collect();
    if expired.len() == 1 {
        return Ok(Verdict::Matched { reference: expired[0].reference.clone(), kind: "late" });
    }
    // Nothing fits: say why, so the operator knows whether to credit (late)
    // or refund (already paid, paid another way).
    let closed: Vec<&Candidate> = all.iter().filter(|c| c.amount == paid || c.base == paid).collect();
    let reason = if expired.len() > 1 {
        "late"
    } else if closed.iter().any(|c| c.status == "paid") {
        "already_paid"
    } else if closed.iter().any(|c| c.status == "expired") {
        "late"
    } else if closed.iter().any(|c| c.status == "cancelled") {
        "cancelled"
    } else {
        "no_request"
    };
    Ok(Verdict::Unmatched { reason, suggestions: refs(&closed) })
}

/// Settles one stored notification: decides, and on a match confirms the
/// request (which credits the plan / balance and issues the comprobante).
async fn settle(app: &App, id: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let (posted_at, title, text): (String, Option<String>, String) =
        sqlx::query_as("SELECT posted_at, title, text FROM yape_inbox WHERE id = $1")
            .bind(id).fetch_one(&app.db).await.map_err(internal)?;
    let r = read(title.as_deref().unwrap_or(""), &text);
    let mut verdict = decide(app, &posted_at, &r).await?;
    if let Verdict::Matched { reference, .. } = &verdict {
        match plans::confirm_request(app, reference).await {
            Ok(_) => {
                sqlx::query("UPDATE plan_requests SET paid_minor = $2 WHERE ref = $1")
                    .bind(reference).bind(r.amount_minor).execute(&app.db).await.map_err(internal)?;
            }
            // Paid by another notification a moment ago: this one is a
            // second transfer of the same amount.
            Err((StatusCode::CONFLICT, _)) => {
                verdict = Verdict::Unmatched { reason: "already_paid", suggestions: vec![reference.clone()] };
            }
            Err(e) => return Err(e),
        }
    }
    let (status, reason, reference, kind, suggestions) = match &verdict {
        Verdict::Matched { reference, kind } => ("matched", None, Some(reference.clone()), Some(*kind), None),
        Verdict::Unmatched { reason, suggestions } => ("unmatched", Some(*reason), None, None, Some(json!(suggestions).to_string())),
        Verdict::Ignored { reason } => ("ignored", Some(*reason), None, None, None),
    };
    sqlx::query("UPDATE yape_inbox SET status = $2, reason = $3, ref = $4, match_kind = $5, suggestions = $6, payer = $7, amount_minor = $8 WHERE id = $1")
        .bind(id).bind(status).bind(reason).bind(&reference).bind(kind).bind(&suggestions).bind(&r.payer).bind(r.amount_minor)
        .execute(&app.db).await.map_err(internal)?;
    tracing::info!(%id, status, reason = ?reason, reference = ?reference, kind = ?kind, amount = ?r.amount_minor, "yape notification settled");
    if status == "unmatched" {
        alert(app, r.amount_minor, reason.unwrap_or("")).await;
    }
    Ok(json!({"id": id, "status": status, "reason": reason, "ref": reference, "matchKind": kind, "amountMinor": r.amount_minor}))
}

/// Tells the sales phone a payment is waiting for a human. No payer name:
/// the queue has it, WhatsApp logs need not.
async fn alert(app: &App, amount_minor: Option<i64>, reason: &str) {
    let Some(phone) = std::env::var("SALES_PHONE").ok().filter(|s| !s.trim().is_empty()) else { return };
    let amount = amount_minor.map(|a| format!("S/ {}.{:02}", a / 100, a % 100)).unwrap_or_else(|| "?".into());
    let text = format!("Yape sin asignar: {amount} ({reason}). Revísalo en la consola → Pagos Yape.");
    if !app.otp.send_text(phone.trim(), &text).await {
        tracing::warn!("unmatched yape alert not delivered");
    }
}

// ---------------------------------------------------------------- endpoints

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    package: String,
    /// `Notification.when` (or the post time), epoch ms: the same for every
    /// repost of one notification.
    posted_at: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    big_text: Option<String>,
}

fn collector_ok(app: &App, headers: &HeaderMap) -> bool {
    let k = headers.get("x-collector-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    app.yape_collector_key.as_deref().is_some_and(|want| yaya_wire::secret::ct_eq(k, want))
}

/// `POST /v1/collector/yape` — the house phone forwards one notification.
pub async fn collect(State(app): State<Shared>, headers: HeaderMap, Json(n): Json<Notification>) -> ApiResult {
    if !collector_ok(&app, &headers) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad collector key"));
    }
    if !packages().iter().any(|p| p == &n.package) {
        // Not stored: it is not a payment notification, whatever it says.
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "only Yape notifications are collected"));
    }
    let text = match n.big_text.as_deref().map(str::trim) {
        Some(b) if b.len() > n.text.trim().len() => b.to_string(),
        _ => n.text.trim().to_string(),
    };
    let title = n.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(String::from);
    if text.is_empty() && title.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "empty notification"));
    }
    if text.len() > 2000 || title.as_ref().is_some_and(|t| t.len() > 500) {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "notification too long"));
    }
    let Some(posted) = chrono::DateTime::from_timestamp_millis(n.posted_at) else {
        return Err(err(StatusCode::BAD_REQUEST, "postedAt must be epoch milliseconds"));
    };
    let posted_at = posted.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let id = yaya_wire::sha256_hex(format!("{}|{}|{}|{}", n.package, n.posted_at, title.as_deref().unwrap_or(""), text).as_bytes());
    let fresh = sqlx::query("INSERT OR IGNORE INTO yape_inbox (id, package, posted_at, title, text, status) VALUES ($1,$2,$3,$4,$5,'new')")
        .bind(&id).bind(&n.package).bind(&posted_at).bind(&title).bind(&text)
        .execute(&app.db).await.map_err(internal)?.rows_affected() == 1;
    if !fresh {
        let (status, reference): (String, Option<String>) = sqlx::query_as("SELECT status, ref FROM yape_inbox WHERE id = $1")
            .bind(&id).fetch_one(&app.db).await.map_err(internal)?;
        return Ok(Json(json!({"id": id, "duplicate": true, "status": status, "ref": reference})).into_response());
    }
    let mut v = settle(&app, &id).await?;
    v["duplicate"] = json!(false);
    Ok(Json(v).into_response())
}

fn admin_ok(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let k = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if yaya_wire::secret::ct_eq(k, &app.admin_key) { Ok(()) } else { Err(err(StatusCode::UNAUTHORIZED, "bad admin key")) }
}

#[derive(serde::Deserialize)]
pub struct ListQ {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /admin/yape?status=unmatched|matched|ignored|resolved|dismissed|all`
pub async fn admin_list(State(app): State<Shared>, headers: HeaderMap, Query(q): Query<ListQ>) -> ApiResult {
    admin_ok(&app, &headers)?;
    let status = q.status.unwrap_or_else(|| "unmatched".into());
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    type Row = (String, String, String, Option<String>, String, Option<String>, Option<i64>, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, posted_at, received_at, title, text, payer, amount_minor, status, reason, ref, match_kind, suggestions, resolved_at, note \
         FROM yape_inbox WHERE ($1 = 'all' OR status = $1) ORDER BY posted_at DESC LIMIT $2")
        .bind(&status).bind(limit).fetch_all(&app.db).await.map_err(internal)?;
    let items: Vec<Value> = rows.into_iter().map(|r| json!({
        "id": r.0, "postedAt": r.1, "receivedAt": r.2, "title": r.3, "text": r.4, "payer": r.5, "amountMinor": r.6,
        "status": r.7, "reason": r.8, "ref": r.9, "matchKind": r.10,
        "suggestions": r.11.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!([])),
        "resolvedAt": r.12, "note": r.13,
    })).collect();
    let counts: Vec<(String, i64)> = sqlx::query_as("SELECT status, COUNT(*) FROM yape_inbox GROUP BY status")
        .fetch_all(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"items": items, "counts": counts.into_iter().collect::<std::collections::BTreeMap<_, _>>()})).into_response())
}

#[derive(serde::Deserialize)]
pub struct AssignReq {
    r#ref: String,
    #[serde(default)]
    note: Option<String>,
    /// Assign even though the request was cancelled (paid another way).
    #[serde(default)]
    force: bool,
}

async fn unmatched_row(app: &App, id: &str) -> Result<Option<i64>, (StatusCode, Json<Value>)> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as("SELECT status, amount_minor FROM yape_inbox WHERE id = $1")
        .bind(id).fetch_optional(&app.db).await.map_err(internal)?;
    match row {
        None => Err(err(StatusCode::NOT_FOUND, "unknown notification")),
        Some((s, a)) if s == "unmatched" => Ok(a),
        Some((s, _)) => Err(err(StatusCode::CONFLICT, format!("notification is {s}, not unmatched"))),
    }
}

/// `POST /admin/yape/{id}/assign {ref}` — a human says whose payment it is.
pub async fn admin_assign(State(app): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<AssignReq>) -> ApiResult {
    admin_ok(&app, &headers)?;
    let amount = unmatched_row(&app, &id).await?;
    let st: Option<(String,)> = sqlx::query_as("SELECT status FROM plan_requests WHERE ref = $1")
        .bind(&req.r#ref).fetch_optional(&app.db).await.map_err(internal)?;
    match st.as_ref().map(|s| s.0.as_str()) {
        None => return Err(err(StatusCode::NOT_FOUND, "unknown reference")),
        Some("paid") => return Err(err(StatusCode::CONFLICT, "that request is already paid — this transfer is a second payment; refund it")),
        Some("cancelled") if !req.force => {
            return Err(err(StatusCode::CONFLICT, "that request was paid another way (cancelled) — refund, or pass force:true"))
        }
        _ => {}
    }
    // Claim the notification first so two operators cannot both assign it.
    let claimed = sqlx::query("UPDATE yape_inbox SET status = 'resolved', ref = $2, match_kind = 'manual', note = $3, \
                               resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = $1 AND status = 'unmatched'")
        .bind(&id).bind(&req.r#ref).bind(&req.note).execute(&app.db).await.map_err(internal)?.rows_affected();
    if claimed == 0 {
        return Err(err(StatusCode::CONFLICT, "already handled"));
    }
    match plans::confirm_request(&app, &req.r#ref).await {
        Ok(v) => {
            sqlx::query("UPDATE plan_requests SET paid_minor = $2 WHERE ref = $1").bind(&req.r#ref).bind(amount)
                .execute(&app.db).await.map_err(internal)?;
            tracing::info!(%id, reference = %req.r#ref, "yape notification assigned by hand");
            Ok(Json(json!({"ok": true, "id": id, "ref": req.r#ref, "confirmed": v})).into_response())
        }
        Err(e) => {
            let _ = sqlx::query("UPDATE yape_inbox SET status = 'unmatched', ref = NULL, match_kind = NULL, resolved_at = NULL WHERE id = $1")
                .bind(&id).execute(&app.db).await;
            Err(e)
        }
    }
}

#[derive(serde::Deserialize)]
pub struct DismissReq {
    #[serde(default)]
    note: Option<String>,
}

/// `POST /admin/yape/{id}/dismiss {note}` — refunded, or not ours.
pub async fn admin_dismiss(State(app): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<DismissReq>) -> ApiResult {
    admin_ok(&app, &headers)?;
    unmatched_row(&app, &id).await?;
    sqlx::query("UPDATE yape_inbox SET status = 'dismissed', note = $2, resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = $1 AND status = 'unmatched'")
        .bind(&id).bind(&req.note).execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true, "id": id, "status": "dismissed"})).into_response())
}

/// `POST /admin/yape/{id}/retry` — match again (e.g. the buyer opened the
/// request after paying).
pub async fn admin_retry(State(app): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> ApiResult {
    admin_ok(&app, &headers)?;
    unmatched_row(&app, &id).await?;
    Ok(Json(settle(&app, &id).await?).into_response())
}

#[cfg(test)]
mod tests;
