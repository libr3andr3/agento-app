//! Prepaid credits, phone side (agente/docs/CREDITS.md).
//!
//! The gateway owns the ledger; this module owns two things:
//!
//! 1. **Reporting outcomes.** When a booking or a sale becomes `confirmed`
//!    the callers below record it in `outcomes` and push it to
//!    `POST /v1/outcomes/confirm` — idempotent by the outcome id, so a retry
//!    after a network failure never double-charges. If the gateway is
//!    unreachable the row stays `synced = 0` and [`sync`] retries on the next
//!    turn or dashboard. Cancellations ask for the reversal the same way.
//!    Nothing here ever blocks a booking: the balance is not a gate.
//! 2. **The cached summary.** What the gateway last said (`GET /v1/credits`)
//!    lives in `settings.credits_summary`; the customer turn reads its `state`
//!    to know whether the account is in manual mode, `book_appointment` reads
//!    `depositsEnabled`, and `/api/credits` serves it when offline.
//!
//! The customer's number never leaves the phone: the gateway sees
//! `client_hash = sha256("{business_id}:{canonical phone}")`.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::AppState;

const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const KEY_SUMMARY: &str = "credits_summary";
const KEY_SUMMARY_AT: &str = "credits_summary_at";
/// The gateway's word, cached; below this we refresh before answering `/api/credits`.
const FRESH_SECS: i64 = 120;

/// Per-business hash of the customer's canonical number.
pub fn client_hash(business_id: Uuid, phone: &str) -> String {
    // Digits only: "+51 999…", "51999…" and a wallet's "999…" payer phone
    // must all be one client. (canon_phone keeps the '+' for peer keys.)
    let canon = crate::harness::canon_phone(phone);
    let digits: String = canon.chars().filter(|c| c.is_ascii_digit()).collect();
    let key = if digits.is_empty() { canon } else { digits };
    crate::db::sha256_hex(format!("{}:{key}", business_id.simple()).as_bytes())
}

/// Manual mode: the gateway said the account is past its grace floor.
pub fn is_manual(summary: &Value) -> bool {
    summary["state"].as_str() == Some("manual")
}

/// Whether deposits count in this business's country (default: yes, the
/// pre-credits behaviour, until the gateway has answered once).
pub fn deposits_enabled(summary: &Value) -> bool {
    summary["depositsEnabled"].as_bool().unwrap_or(true)
}

pub async fn cached(state: &AppState) -> Value {
    crate::account::setting(&state.db, KEY_SUMMARY).await
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null)
}

async fn cached_at(state: &AppState) -> i64 {
    crate::account::setting(&state.db, KEY_SUMMARY_AT).await.and_then(|s| s.parse().ok()).unwrap_or(0)
}

async fn remember(state: &AppState, summary: &Value) {
    let _ = crate::account::set_setting(&state.db, KEY_SUMMARY, &summary.to_string()).await;
    let _ = crate::account::set_setting(&state.db, KEY_SUMMARY_AT, &chrono::Utc::now().timestamp().to_string()).await;
}

/// The summary for the app: fresh from the gateway when it answers (after
/// pushing anything pending), else the cache marked `cached: true`.
pub async fn summary(state: &AppState) -> Value {
    push_pending(state).await;
    match state.registry.get("/v1/credits", SYNC_TIMEOUT).await {
        Ok(v) => {
            remember(state, &v).await;
            v
        }
        Err(e) => {
            tracing::debug!(error = %e, "credits not fetched; serving cache");
            let mut v = cached(state).await;
            if v.is_null() {
                v = json!({});
            }
            v["cached"] = json!(true);
            v
        }
    }
}

/// Refreshes the cache if it is older than [`FRESH_SECS`]. Cheap to call
/// from a customer turn: no network when fresh.
pub async fn sync(state: &AppState) -> Value {
    let age = chrono::Utc::now().timestamp() - cached_at(state).await;
    if age < FRESH_SECS {
        let c = cached(state).await;
        if !c.is_null() {
            return c;
        }
    }
    summary(state).await
}

/// A booking or a sale just became `confirmed`: remember it and charge it.
/// Idempotent — calling it twice for the same id is a no-op.
pub async fn confirm(state: &AppState, business_id: Uuid, kind: &str, id: Uuid, phone: &str, customer: &str) {
    let hash = client_hash(business_id, phone);
    let fresh = sqlx::query("INSERT OR IGNORE INTO outcomes (id, business_id, kind, client_hash, customer) VALUES ($1, $2, $3, $4, $5)")
        .bind(id.to_string()).bind(business_id).bind(kind).bind(&hash).bind(customer.chars().take(60).collect::<String>())
        .execute(&state.db).await.map(|r| r.rows_affected()).unwrap_or(0);
    if fresh == 0 {
        return;
    }
    tracing::info!(%business_id, outcome = %id, kind, "outcome confirmed");
    push_pending(state).await;
}

/// The outcome was cancelled/refunded: ask for the charge back (the
/// gateway decides whether the 24 h window allows it).
pub async fn reverse(state: &AppState, id: Uuid) -> Value {
    let n = sqlx::query("UPDATE outcomes SET reversed = 1 WHERE id = $1 AND reversed = 0")
        .bind(id.to_string()).execute(&state.db).await.map(|r| r.rows_affected()).unwrap_or(0);
    if n == 0 {
        return json!({"reversed": false, "reason": "unknown or already reversed"});
    }
    push_pending(state).await;
    let row: Option<(i64,)> = sqlx::query_as("SELECT reversed FROM outcomes WHERE id = $1").bind(id.to_string()).fetch_optional(&state.db).await.ok().flatten();
    json!({"reversed": row.map(|r| r.0) == Some(2), "queued": row.map(|r| r.0) == Some(1)})
}

/// Pushes every unsynced outcome and every pending reversal. Best effort,
/// in order; stops at the first network failure (the rest wait).
pub async fn push_pending(state: &AppState) {
    let pending: Vec<(String, Vec<u8>, String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, business_id, kind, client_hash, customer, reversed FROM outcomes WHERE synced = 0 OR reversed = 1 ORDER BY created_at LIMIT 50",
    ).fetch_all(&state.db).await.unwrap_or_default();
    for (id, business, kind, hash, customer, reversed) in pending {
        let business_hex = Uuid::from_slice(&business).map(|u| u.simple().to_string()).unwrap_or_else(|_| hex::encode(&business));
        // Confirm first (a reversal of a never-charged outcome is meaningless).
        let (synced,): (i64,) = sqlx::query_as("SELECT synced FROM outcomes WHERE id = $1").bind(&id).fetch_one(&state.db).await.unwrap_or((0,));
        if synced == 0 {
            let body = json!({"outcomeId": id, "business": business_hex, "clientHash": hash, "kind": kind, "customer": customer});
            match state.registry.post("/v1/outcomes/confirm", &body, SYNC_TIMEOUT).await {
                Ok(v) => {
                    let _ = sqlx::query("UPDATE outcomes SET synced = 1, charged_cents = $2, is_new_client = $3, synced_at = $4 WHERE id = $1")
                        .bind(&id).bind((v["charged"].as_f64().unwrap_or(0.0) * 100.0).round() as i64).bind(v["isNewClient"].as_bool().map(|b| b as i64))
                        .bind(crate::db::now()).execute(&state.db).await;
                    apply_confirm_reply(state, &v).await;
                }
                Err(e) => {
                    tracing::debug!(error = %e, outcome = %id, "outcome not pushed yet");
                    return;
                }
            }
        }
        if reversed == 1 {
            match state.registry.post("/v1/outcomes/reverse", &json!({"outcomeId": id}), SYNC_TIMEOUT).await {
                Ok(v) => {
                    let _ = sqlx::query("UPDATE outcomes SET reversed = 2 WHERE id = $1").bind(&id).execute(&state.db).await;
                    apply_confirm_reply(state, &v).await;
                }
                // 409 = outside the 24 h window: settled, nothing to retry.
                Err(e) if e.to_string().contains("registry 409") || e.to_string().contains("registry 404") => {
                    let _ = sqlx::query("UPDATE outcomes SET reversed = 2 WHERE id = $1").bind(&id).execute(&state.db).await;
                }
                Err(e) => {
                    tracing::debug!(error = %e, outcome = %id, "reversal not pushed yet");
                    return;
                }
            }
        }
    }
}

/// A confirm/reverse reply carries the new balance and state: fold it into
/// the cache so the next customer turn sees manual mode without a fetch.
async fn apply_confirm_reply(state: &AppState, v: &Value) {
    let mut c = cached(state).await;
    if c.is_null() {
        c = json!({});
    }
    for k in ["balance", "state", "grace", "depositsEnabled", "month", "topupUrl"] {
        if !v[k].is_null() {
            c[k] = v[k].clone();
        }
    }
    remember(state, &c).await;
}

/// `POST /api/topup/session` → the gateway (card: Dodo URL; yape: reference).
pub async fn topup(state: &AppState, tier: &str, amount: Option<i64>, method: &str) -> anyhow::Result<Value> {
    let body = json!({"tier": tier, "amount": amount});
    let path = if method == "yape" { "/v1/topup/yape" } else { "/v1/topup/session" };
    state.registry.post(path, &body, SYNC_TIMEOUT).await
}

/// Category + terms acceptance to the account (best effort after the
/// business exists; the prohibited check happened before).
pub async fn send_profile(state: &AppState, category: Option<&str>, terms_version: Option<&str>, terms_accepted_at: Option<&str>) -> anyhow::Result<Value> {
    state.registry.post("/v1/account/profile", &json!({"category": category, "termsVersion": terms_version, "termsAcceptedAt": terms_accepted_at}), SYNC_TIMEOUT).await
}

/// The gateway's categories (with the prohibited list), cached a day.
pub async fn categories(state: &AppState) -> Value {
    catalog(state, "/v1/categories", "categories_json").await
}

/// The gateway's money-app catalog, cached a day.
pub async fn wallets(state: &AppState) -> Value {
    catalog(state, "/v1/wallets", "wallets_json").await
}

async fn catalog(state: &AppState, path: &str, key: &str) -> Value {
    let at_key = format!("{key}_at");
    let age = chrono::Utc::now().timestamp() - crate::account::setting(&state.db, &at_key).await.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let cached = crate::account::setting(&state.db, key).await.and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if let (Some(c), true) = (&cached, age < 86_400) {
        return c.clone();
    }
    match state.registry.get(path, SYNC_TIMEOUT).await {
        Ok(v) if v.is_object() => {
            let _ = crate::account::set_setting(&state.db, key, &v.to_string()).await;
            let _ = crate::account::set_setting(&state.db, &at_key, &chrono::Utc::now().timestamp().to_string()).await;
            v
        }
        _ => cached.unwrap_or_else(|| json!({})),
    }
}

/// Is this category on the gateway's prohibited list? Unknown lists (no
/// network, ever) fall back to the bundled default.
pub async fn is_prohibited(state: &AppState, category: &str) -> bool {
    let cats = categories(state).await;
    let list: Vec<String> = match cats["prohibited"].as_array() {
        Some(a) if !a.is_empty() => a.iter().filter_map(|c| c["key"].as_str().map(String::from)).collect(),
        _ => DEFAULT_PROHIBITED.iter().map(|s| s.to_string()).collect(),
    };
    list.iter().any(|k| k == category)
}

pub const DEFAULT_PROHIBITED: &[&str] = &["farmacia_sin_receta", "armas", "apuestas", "adulto", "cripto_intercambio", "prestamos"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hash_is_per_business_and_canonical() {
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        assert_eq!(client_hash(b1, "+51 999 000 111"), client_hash(b1, "51999000111"), "spelling of the number does not matter");
        assert_ne!(client_hash(b1, "51999000111"), client_hash(b2, "51999000111"), "the same person is a different client per business");
        assert_eq!(client_hash(b1, "51999000111").len(), 64);
        assert!(!client_hash(b1, "51999000111").contains("999"), "the number itself never appears");
    }

    #[test]
    fn manual_and_deposits_read_the_gateway_word() {
        assert!(is_manual(&json!({"state": "manual"})));
        assert!(!is_manual(&json!({"state": "grace"})));
        assert!(!is_manual(&Value::Null), "no word yet = never manual");
        assert!(deposits_enabled(&Value::Null), "unknown = keep collecting deposits");
        assert!(!deposits_enabled(&json!({"depositsEnabled": false})));
    }
}

/// The dashboard's `credits` block: the cached summary, trimmed.
pub async fn dashboard_block(state: &AppState) -> Value {
    let c = sync(state).await;
    if c.is_null() {
        return Value::Null;
    }
    json!({
        "balance": c["balance"], "currency": c["currency"], "state": c["state"], "grace": c["grace"],
        "prices": c["prices"], "buckets": c["buckets"], "depositsEnabled": c["depositsEnabled"],
        "month": c["month"], "tiers": c["tiers"], "topup": c["topup"], "cached": c["cached"],
    })
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::testkit::{self, Mock};

    async fn row(s: &AppState, id: Uuid) -> (i64, Option<i64>, Option<i64>, i64) {
        sqlx::query_as("SELECT synced, charged_cents, is_new_client, reversed FROM outcomes WHERE id = $1").bind(id.to_string()).fetch_one(&s.db).await.unwrap()
    }

    #[test]
    fn client_hash_edge_cases() {
        let b = Uuid::nil();
        // No digits at all: hashes the canonical text rather than an empty key.
        assert_ne!(client_hash(b, "abc"), client_hash(b, ""));
        // Known vector: sha256("<simple uuid>:51999000111").
        assert_eq!(client_hash(b, "+51 999-000-111"), crate::db::sha256_hex(format!("{}:51999000111", b.simple()).as_bytes()));
    }

    #[test]
    fn default_prohibited_list() {
        assert!(DEFAULT_PROHIBITED.contains(&"armas"));
        assert_eq!(DEFAULT_PROHIBITED.len(), 6);
    }

    #[tokio::test]
    async fn confirm_charges_once_and_folds_the_reply_into_the_cache() {
        let m = Mock::start().await;
        m.on("/v1/outcomes/confirm", json!({"charged": 1.5, "isNewClient": true, "balance": 8.5, "state": "ok", "ignored": 1}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        let id = Uuid::new_v4();
        confirm(&s, b, "booking", id, "+51 999 000 222", &"Ana ".repeat(40)).await;
        assert_eq!(row(&s, id).await, (1, Some(150), Some(1), 0));
        let sent = &m.seen_path("/v1/outcomes/confirm")[0].body;
        assert_eq!(sent["outcomeId"], json!(id.to_string()));
        assert_eq!(sent["business"], json!(b.simple().to_string()));
        assert_eq!(sent["clientHash"], json!(client_hash(b, "51999000222")));
        assert_eq!(sent["kind"], "booking");
        assert_eq!(sent["customer"].as_str().unwrap().chars().count(), 60, "names are bounded");
        assert!(!sent.to_string().contains("999000222"), "the number never leaves the phone");
        let c = cached(&s).await;
        assert_eq!((c["balance"].clone(), c["state"].clone()), (json!(8.5), json!("ok")));
        assert!(c.get("ignored").is_none());
        // Same outcome again: no second charge.
        confirm(&s, b, "booking", id, "+51 999 000 222", "Ana").await;
        assert_eq!(m.seen_path("/v1/outcomes/confirm").len(), 1);
    }

    #[tokio::test]
    async fn offline_outcomes_wait_and_are_pushed_in_order_later() {
        let s = testkit::state().await; // gateway unreachable
        let b = testkit::business(&s.db).await;
        let (a, c) = (Uuid::new_v4(), Uuid::new_v4());
        confirm(&s, b, "booking", a, "1", "A").await;
        confirm(&s, b, "sale", c, "2", "C").await;
        assert_eq!(row(&s, a).await.0, 0);
        assert_eq!(row(&s, c).await.0, 0);
        // Gateway back: move the same database onto a reachable registry.
        let m = Mock::start().await;
        m.on("/v1/outcomes/confirm", json!({"charged": 1}));
        let online = testkit::state_on(&m).await;
        let s2 = crate::AppState { db: s.db.clone(), ..std::sync::Arc::try_unwrap(online).ok().unwrap() };
        push_pending(&s2).await;
        assert_eq!(row(&s2, a).await.0, 1);
        assert_eq!(row(&s2, c).await.0, 1);
        let ids: Vec<Value> = m.seen_path("/v1/outcomes/confirm").iter().map(|x| x.body["outcomeId"].clone()).collect();
        assert_eq!(ids, vec![json!(a.to_string()), json!(c.to_string())]);
    }

    #[tokio::test]
    async fn push_stops_at_the_first_failure() {
        let m = Mock::start().await;
        m.on_status("/v1/outcomes/confirm", 500, json!({"error": "down"}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        confirm(&s, b, "booking", Uuid::new_v4(), "1", "A").await;
        confirm(&s, b, "booking", Uuid::new_v4(), "2", "B").await;
        // Each confirm pushes once and stops at the first (failing) row.
        assert_eq!(m.seen_path("/v1/outcomes/confirm").len(), 2);
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outcomes WHERE synced = 0").fetch_one(&s.db).await.unwrap();
        assert_eq!(pending, 2);
    }

    #[tokio::test]
    async fn reverse_paths() {
        let m = Mock::start().await;
        m.on("/v1/outcomes/confirm", json!({"charged": 1}));
        m.on("/v1/outcomes/reverse", json!({"balance": 10, "state": "ok"}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        assert_eq!(reverse(&s, Uuid::new_v4()).await, json!({"reversed": false, "reason": "unknown or already reversed"}));
        let id = Uuid::new_v4();
        confirm(&s, b, "sale", id, "1", "A").await;
        assert_eq!(reverse(&s, id).await, json!({"reversed": true, "queued": false}));
        assert_eq!(row(&s, id).await.3, 2);
        assert_eq!(cached(&s).await["balance"], 10);
        // Twice: already reversed.
        assert_eq!(reverse(&s, id).await["reversed"], false);
    }

    #[tokio::test]
    async fn reverse_outside_the_window_settles_and_network_errors_queue() {
        let m = Mock::start().await;
        m.on("/v1/outcomes/confirm", json!({"charged": 1}));
        m.on_status("/v1/outcomes/reverse", 409, json!({"error": {"message": "window closed"}}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        let id = Uuid::new_v4();
        confirm(&s, b, "sale", id, "1", "A").await;
        let r = reverse(&s, id).await;
        assert_eq!(row(&s, id).await.3, 2, "409 is final");
        assert_eq!(r["reversed"], true);
        let m2 = Mock::start().await;
        m2.on("/v1/outcomes/confirm", json!({"charged": 1}));
        m2.on_status("/v1/outcomes/reverse", 503, json!({}));
        let s = testkit::state_on(&m2).await;
        let b = testkit::business(&s.db).await;
        let id = Uuid::new_v4();
        confirm(&s, b, "sale", id, "1", "A").await;
        assert_eq!(reverse(&s, id).await, json!({"reversed": false, "queued": true}));
        assert_eq!(row(&s, id).await.3, 1);
    }

    #[tokio::test]
    async fn summary_fetches_or_serves_the_cache() {
        let s = testkit::state().await;
        assert_eq!(summary(&s).await, json!({"cached": true}));
        crate::account::set_setting(&s.db, KEY_SUMMARY, r#"{"balance": 3}"#).await.unwrap();
        assert_eq!(summary(&s).await, json!({"balance": 3, "cached": true}));
        let m = Mock::start().await;
        m.on("/v1/credits", json!({"balance": 7, "state": "ok"}));
        let s = testkit::state_on(&m).await;
        assert_eq!(summary(&s).await["balance"], 7);
        assert_eq!(cached(&s).await["balance"], 7);
        assert!(cached_at(&s).await > 0);
    }

    #[tokio::test]
    async fn sync_uses_a_fresh_cache_without_network() {
        let m = Mock::start().await;
        m.on("/v1/credits", json!({"balance": 1}));
        let s = testkit::state_on(&m).await;
        assert_eq!(sync(&s).await["balance"], 1);
        assert_eq!(sync(&s).await["balance"], 1);
        assert_eq!(m.seen_path("/v1/credits").len(), 1);
        // Stale: refetched.
        crate::account::set_setting(&s.db, KEY_SUMMARY_AT, "0").await.unwrap();
        sync(&s).await;
        assert_eq!(m.seen_path("/v1/credits").len(), 2);
    }

    #[tokio::test]
    async fn dashboard_block_trims_the_summary() {
        let m = Mock::start().await;
        m.on("/v1/credits", json!({"balance": 2, "currency": "USD", "state": "grace", "secret": "x"}));
        let s = testkit::state_on(&m).await;
        let d = dashboard_block(&s).await;
        assert_eq!((d["balance"].clone(), d["state"].clone()), (json!(2), json!("grace")));
        assert!(d.get("secret").is_none());
        assert_eq!(d.as_object().unwrap().len(), 11);
    }

    #[tokio::test]
    async fn topup_routes_by_method_and_profile_posts() {
        let m = Mock::start().await;
        m.on("/v1/topup/yape", json!({"ref": "Y1"}));
        m.on("/v1/topup/session", json!({"url": "https://pay"}));
        m.on("/v1/account/profile", json!({"ok": true}));
        let s = testkit::state_on(&m).await;
        assert_eq!(topup(&s, "t1", Some(20), "yape").await.unwrap()["ref"], "Y1");
        assert_eq!(topup(&s, "t1", None, "card").await.unwrap()["url"], "https://pay");
        assert_eq!(m.seen_path("/v1/topup/yape")[0].body, json!({"tier": "t1", "amount": 20}));
        assert!(send_profile(&s, Some("barberia"), Some("v3"), None).await.is_ok());
        assert_eq!(m.seen_path("/v1/account/profile")[0].body["termsVersion"], "v3");
        assert!(topup(&*testkit::state().await, "t", None, "card").await.is_err());
    }

    #[tokio::test]
    async fn catalogs_cache_a_day_and_fall_back() {
        let m = Mock::start().await;
        m.on("/v1/categories", json!({"prohibited": [{"key": "tabaco"}]}));
        m.on("/v1/wallets", json!({"wallets": ["Yape"]}));
        let s = testkit::state_on(&m).await;
        assert_eq!(wallets(&s).await["wallets"][0], "Yape");
        assert!(is_prohibited(&s, "tabaco").await);
        assert!(!is_prohibited(&s, "armas").await, "the gateway's list replaces the default");
        is_prohibited(&s, "x").await;
        assert_eq!(m.seen_path("/v1/categories").len(), 1, "cached");
        // Non-object answers are not cached.
        let m2 = Mock::start().await;
        m2.on("/v1/categories", json!(["nope"]));
        let s2 = testkit::state_on(&m2).await;
        assert_eq!(categories(&s2).await, json!({}));
        // Never reached: the bundled default applies.
        let off = testkit::state().await;
        assert!(is_prohibited(&off, "armas").await);
        assert!(!is_prohibited(&off, "barberia").await);
        // Empty list from the gateway also means the default.
        let m3 = Mock::start().await;
        m3.on("/v1/categories", json!({"prohibited": []}));
        assert!(is_prohibited(&*testkit::state_on(&m3).await, "apuestas").await);
    }
}
