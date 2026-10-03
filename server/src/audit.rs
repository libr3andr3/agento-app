//! The audit substrate (2026-08-30): an append-only, hash-chained, signed
//! record of what the agent did, on the phone, in the phone's own database.
//!
//! Every entry carries `hash = sha256(canonical(seq, ts, kind, actor,
//! subject, payload, prev_hash))` and `sig = agent identity's Ed25519
//! signature over hash`. SQLite triggers refuse UPDATE and DELETE on the
//! table; the chain refuses reordering; the signature refuses forgery by
//! anyone without the sealed identity seed. Timestamps are monotonic (never
//! earlier than the previous entry) and, because the phone's clock is the
//! owner's to set, periodically **anchored**: the gateway countersigns the
//! chain head with its own clock (`POST /v1/audit/anchor`), so any entry
//! between two anchors provably happened between two trusted instants.
//!
//! The log never leaves the phone in backups (`backup::PRIVATE`); a restored
//! phone starts a new chain and records that it did.

use anyhow::Result;
use serde_json::{json, Value};
use sha2::Digest;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

use crate::AppState;

/// One writer at a time: seq and prev_hash are read-then-written.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Entries since the last anchor attempt (throttle).
static SINCE_ANCHOR: AtomicU64 = AtomicU64::new(0);
static LAST_ANCHOR_MS: AtomicU64 = AtomicU64::new(0);

const ANCHOR_EVERY_ENTRIES: u64 = 25;
const ANCHOR_EVERY_SECS: u64 = 15 * 60;

/// What the log records. Kept as strings in the table; this list is the
/// vocabulary the audit screen and the console understand.
pub mod kind {
    pub const CUSTOMER_TURN: &str = "customer_turn";
    pub const OWNER_TURN: &str = "owner_turn";
    pub const TOOL_CALL: &str = "tool_call";
    pub const OWNER_CMD: &str = "owner_cmd";
    pub const PAYMENT: &str = "payment";
    pub const STATUS: &str = "status";
    pub const SHARE: &str = "share";
    pub const CONFIG: &str = "config";
    pub const RESTORE: &str = "restore";
    pub const BOOT: &str = "boot";
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Canonical bytes the hash covers. Field order is fixed; payload is the
/// compact JSON as stored, so a verifier recomputes from the row alone.
fn canonical(seq: i64, ts: &str, kind: &str, actor: &str, subject: &str, payload: &str, prev: &str) -> Vec<u8> {
    format!("agente-audit-v1\n{seq}\n{ts}\n{kind}\n{actor}\n{subject}\n{payload}\n{prev}").into_bytes()
}

/// Appends one entry. Never fails the caller's work: an error is logged and
/// the turn goes on — the missing entry shows up as a gap only if the
/// chain is later verified against the messages/tool_events tables.
pub async fn record(state: &AppState, business_id: Option<Uuid>, kind: &str, actor: &str, subject: &str, payload: Value) {
    if let Err(e) = record_inner(state, business_id, kind, actor, subject, payload).await {
        tracing::warn!(kind, error = %e, "audit entry not recorded");
    }
}

async fn record_inner(state: &AppState, business_id: Option<Uuid>, kind: &str, actor: &str, subject: &str, payload: Value) -> Result<i64> {
    let _g = LOCK.lock().await;
    let last: Option<(i64, String, String)> = sqlx::query_as("SELECT seq, ts, hash FROM audit_log ORDER BY seq DESC LIMIT 1")
        .fetch_optional(&state.db)
        .await?;
    let (seq, prev_ts, prev_hash) = match last {
        Some((s, t, h)) => (s + 1, Some(t), h),
        None => (1, None, "genesis".to_string()),
    };
    // Monotonic: the phone's clock may jump back; the chain never does.
    let mut now = chrono::Utc::now();
    if let Some(p) = prev_ts.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        let p = p.with_timezone(&chrono::Utc) + chrono::Duration::milliseconds(1);
        if now < p {
            now = p;
        }
    }
    let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let payload_s = serde_json::to_string(&payload)?;
    let hash = sha256_hex(&canonical(seq, &ts, kind, actor, subject, &payload_s, &prev_hash));
    let sig = state.identity.sign(hash.as_bytes());
    sqlx::query(
        "INSERT INTO audit_log (seq, ts, kind, actor, subject, business_id, payload, prev_hash, hash, sig) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(seq)
    .bind(&ts)
    .bind(kind)
    .bind(actor)
    .bind(subject)
    .bind(business_id)
    .bind(&payload_s)
    .bind(&prev_hash)
    .bind(&hash)
    .bind(&sig)
    .execute(&state.db)
    .await?;
    SINCE_ANCHOR.fetch_add(1, Ordering::Relaxed);
    Ok(seq)
}

/// Verifies the whole chain: recomputed hashes, links, signatures against
/// this installation's identity, monotonic time. Returns what the owner's
/// screen shows.
pub async fn verify(state: &AppState) -> Result<Value> {
    let rows: Vec<(i64, String, String, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT seq, ts, kind, actor, subject, payload, prev_hash, hash, sig FROM audit_log ORDER BY seq",
    )
    .fetch_all(&state.db)
    .await?;
    let vk = state.identity.public_key();
    let mut prev = "genesis".to_string();
    let mut prev_seq = 0i64;
    let mut prev_ts = String::new();
    let mut problems: Vec<Value> = Vec::new();
    for (seq, ts, kind, actor, subject, payload, prev_hash, hash, sig) in &rows {
        if *seq != prev_seq + 1 {
            problems.push(json!({"seq": seq, "problem": "gap in sequence"}));
        }
        if prev_hash != &prev {
            problems.push(json!({"seq": seq, "problem": "broken link"}));
        }
        let expect = sha256_hex(&canonical(*seq, ts, kind, actor, subject, payload, prev_hash));
        if &expect != hash {
            problems.push(json!({"seq": seq, "problem": "hash mismatch"}));
        }
        let ok_sig = hex::decode(sig)
            .ok()
            .and_then(|b| ed25519_dalek::Signature::from_slice(&b).ok())
            .map(|s| ed25519_dalek::Verifier::verify(&vk, hash.as_bytes(), &s).is_ok())
            .unwrap_or(false);
        if !ok_sig {
            problems.push(json!({"seq": seq, "problem": "bad signature"}));
        }
        if !prev_ts.is_empty() && ts < &prev_ts {
            problems.push(json!({"seq": seq, "problem": "time went backwards"}));
        }
        prev = hash.clone();
        prev_seq = *seq;
        prev_ts = ts.clone();
        if problems.len() > 50 {
            break;
        }
    }
    let anchor: Option<(i64, String, String, String, String)> = sqlx::query_as(
        "SELECT seq, head_hash, anchored_at, anchor_sig, anchor_key FROM audit_anchors ORDER BY seq DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?;
    // The anchor must point at a hash that is really in the chain.
    let anchor_ok = match &anchor {
        Some((seq, head, _, _, _)) => rows.iter().any(|r| &r.0 == seq && &r.7 == head),
        None => false,
    };
    Ok(json!({
        "ok": problems.is_empty(),
        "entries": rows.len(),
        "head": rows.last().map(|r| r.7.clone()),
        "headSeq": rows.last().map(|r| r.0),
        "agent": state.identity.id(),
        "problems": problems,
        "anchor": anchor.map(|(seq, head, at, sig, key)| json!({
            "seq": seq, "head": head, "anchoredAt": at, "sig": sig, "key": key, "inChain": anchor_ok
        })),
        "unanchoredEntries": rows.len() as i64 - anchor_seq_of(&state.db).await.unwrap_or(0),
    }))
}

async fn anchor_seq_of(db: &sqlx::SqlitePool) -> Result<i64> {
    let r: Option<(i64,)> = sqlx::query_as("SELECT seq FROM audit_anchors ORDER BY seq DESC LIMIT 1").fetch_optional(db).await?;
    Ok(r.map(|x| x.0).unwrap_or(0))
}

/// Page of entries for the owner's screen, newest first.
pub async fn list(state: &AppState, before_seq: Option<i64>, limit: i64) -> Result<Vec<Value>> {
    let rows: Vec<(i64, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT seq, ts, kind, actor, subject, payload, hash FROM audit_log \
         WHERE ($1 IS NULL OR seq < $1) ORDER BY seq DESC LIMIT $2",
    )
    .bind(before_seq)
    .bind(limit.clamp(1, 200))
    .fetch_all(&state.db)
    .await?;
    let anchored_up_to = anchor_seq_of(&state.db).await.unwrap_or(0);
    Ok(rows
        .into_iter()
        .map(|(seq, ts, kind, actor, subject, payload, hash)| {
            json!({
                "seq": seq, "ts": ts, "kind": kind, "actor": actor, "subject": subject,
                "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                "hash": hash, "anchored": seq <= anchored_up_to,
            })
        })
        .collect())
}

/// Asks the gateway to countersign the current head with its clock. Stores
/// the anchor; returns it. Fails quietly offline — the next attempt covers
/// everything since.
pub async fn anchor(state: &AppState) -> Result<Value> {
    let head: Option<(i64, String, String)> = sqlx::query_as("SELECT seq, hash, ts FROM audit_log ORDER BY seq DESC LIMIT 1")
        .fetch_optional(&state.db)
        .await?;
    let Some((seq, hash, ts)) = head else { return Ok(json!({"status": "empty"})) };
    if anchor_seq_of(&state.db).await? >= seq {
        return Ok(json!({"status": "current", "seq": seq}));
    }
    let v = state
        .registry
        .post("/v1/audit/anchor", &json!({"seq": seq, "head": hash, "ts": ts}), Duration::from_secs(20))
        .await?;
    let at = v["anchoredAt"].as_str().unwrap_or_default().to_string();
    let sig = v["sig"].as_str().unwrap_or_default().to_string();
    let key = v["key"].as_str().unwrap_or_default().to_string();
    anyhow::ensure!(!at.is_empty() && !sig.is_empty(), "gateway returned no anchor");
    sqlx::query("INSERT OR REPLACE INTO audit_anchors (seq, head_hash, anchored_at, anchor_sig, anchor_key) VALUES ($1,$2,$3,$4,$5)")
        .bind(seq)
        .bind(&hash)
        .bind(&at)
        .bind(&sig)
        .bind(&key)
        .execute(&state.db)
        .await?;
    SINCE_ANCHOR.store(0, Ordering::Relaxed);
    LAST_ANCHOR_MS.store(now_ms(), Ordering::Relaxed);
    Ok(json!({"status": "anchored", "seq": seq, "head": hash, "anchoredAt": at, "key": key}))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Throttled anchoring, spawned from hot paths: every N entries or M
/// minutes, whichever first. Never blocks the caller.
pub fn anchor_if_due(state: crate::SharedState) {
    let n = SINCE_ANCHOR.load(Ordering::Relaxed);
    let age = now_ms().saturating_sub(LAST_ANCHOR_MS.load(Ordering::Relaxed)) / 1000;
    if n == 0 || (n < ANCHOR_EVERY_ENTRIES && age < ANCHOR_EVERY_SECS) {
        return;
    }
    LAST_ANCHOR_MS.store(now_ms(), Ordering::Relaxed);
    tokio::spawn(async move {
        if let Err(e) = anchor(&state).await {
            tracing::info!("audit anchor skipped: {e}");
        }
    });
}

/// Short, content-safe description of a text: length + hash. The customer's
/// words stay in `messages`; the audit proves which words without holding them.
pub fn digest_of(text: &str) -> Value {
    json!({"len": text.chars().count(), "sha256": sha256_hex(text.as_bytes())})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_is_stable_and_order_sensitive() {
        let a = canonical(1, "t", "k", "a", "s", "{}", "genesis");
        let b = canonical(1, "t", "k", "a", "s", "{}", "genesis");
        let c = canonical(2, "t", "k", "a", "s", "{}", "genesis");
        assert_eq!(sha256_hex(&a), sha256_hex(&b));
        assert_ne!(sha256_hex(&a), sha256_hex(&c));
    }

    use crate::testkit::{self, Mock};

    async fn drop_guards(s: &AppState) {
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'audit_log'").fetch_all(&s.db).await.unwrap();
        for n in names {
            sqlx::query(&format!("DROP TRIGGER {n}")).execute(&s.db).await.unwrap();
        }
    }

    #[test]
    fn digest_hides_the_text() {
        let d = digest_of("hola señor");
        assert_eq!(d["len"], 10);
        assert_eq!(d["sha256"], json!(sha256_hex("hola señor".as_bytes())));
        assert!(!d.to_string().contains("hola"));
    }

    #[test]
    fn canonical_layout() {
        assert_eq!(canonical(3, "t", "k", "a", "s", "{}", "p"), b"agente-audit-v1\n3\nt\nk\na\ns\n{}\np".to_vec());
    }

    #[tokio::test]
    async fn chain_links_signs_and_verifies() {
        let s = testkit::state().await;
        let v = verify(&s).await.unwrap();
        assert_eq!((v["ok"].clone(), v["entries"].clone(), v["head"].clone()), (json!(true), json!(0), Value::Null));
        let b = testkit::business(&s.db).await;
        record(&s, Some(b), kind::CUSTOMER_TURN, "customer", "+51999", json!({"n": 1})).await;
        record(&s, None, kind::CONFIG, "owner", "", json!({"x": true})).await;
        record(&s, Some(b), kind::PAYMENT, "system", "p1", json!({})).await;
        let v = verify(&s).await.unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!((v["entries"].clone(), v["headSeq"].clone()), (json!(3), json!(3)));
        assert_eq!(v["agent"], json!(s.identity.id()));
        assert_eq!(v["unanchoredEntries"], 3);
        let rows: Vec<(i64, String, String)> = sqlx::query_as("SELECT seq, prev_hash, hash FROM audit_log ORDER BY seq").fetch_all(&s.db).await.unwrap();
        assert_eq!(rows[0].1, "genesis");
        assert_eq!(rows[1].1, rows[0].2);
        assert_eq!(rows[2].1, rows[1].2);
    }

    #[tokio::test]
    async fn the_table_refuses_update_and_delete() {
        let s = testkit::state().await;
        record(&s, None, kind::BOOT, "system", "", json!({})).await;
        assert!(sqlx::query("UPDATE audit_log SET actor = 'x'").execute(&s.db).await.is_err());
        assert!(sqlx::query("DELETE FROM audit_log").execute(&s.db).await.is_err());
    }

    #[tokio::test]
    async fn verify_names_each_kind_of_tampering() {
        let s = testkit::state().await;
        for i in 0..4 {
            record(&s, None, kind::TOOL_CALL, "agent", &format!("t{i}"), json!({"i": i})).await;
        }
        drop_guards(&s).await;
        sqlx::query("UPDATE audit_log SET payload = '{\"i\":99}' WHERE seq = 2").execute(&s.db).await.unwrap();
        sqlx::query("UPDATE audit_log SET sig = '00' WHERE seq = 3").execute(&s.db).await.unwrap();
        sqlx::query("DELETE FROM audit_log WHERE seq = 4").execute(&s.db).await.unwrap();
        record(&s, None, kind::TOOL_CALL, "agent", "t5", json!({})).await; // seq 4 again, links to 3
        sqlx::query("UPDATE audit_log SET prev_hash = 'x' WHERE seq = 1").execute(&s.db).await.unwrap();
        let v = verify(&s).await.unwrap();
        assert_eq!(v["ok"], false);
        let probs: Vec<(i64, String)> = v["problems"].as_array().unwrap().iter().map(|p| (p["seq"].as_i64().unwrap(), p["problem"].as_str().unwrap().to_string())).collect();
        assert!(probs.contains(&(1, "broken link".into())), "{probs:?}");
        assert!(probs.contains(&(2, "hash mismatch".into())), "{probs:?}");
        assert!(probs.contains(&(3, "bad signature".into())), "{probs:?}");
        // A different identity cannot vouch for this chain.
        let other = testkit::state().await;
        let other = crate::AppState { db: s.db.clone(), ..std::sync::Arc::try_unwrap(other).ok().unwrap() };
        let v = verify(&other).await.unwrap();
        assert!(v["problems"].as_array().unwrap().iter().filter(|p| p["problem"] == "bad signature").count() >= 4);
    }

    #[tokio::test]
    async fn gaps_and_time_travel_are_reported() {
        let s = testkit::state().await;
        for _ in 0..3 {
            record(&s, None, kind::STATUS, "agent", "", json!({})).await;
        }
        drop_guards(&s).await;
        sqlx::query("DELETE FROM audit_log WHERE seq = 2").execute(&s.db).await.unwrap();
        sqlx::query("UPDATE audit_log SET ts = '2000-01-01T00:00:00.000Z' WHERE seq = 3").execute(&s.db).await.unwrap();
        let v = verify(&s).await.unwrap();
        let kinds: Vec<&str> = v["problems"].as_array().unwrap().iter().map(|p| p["problem"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"gap in sequence"));
        assert!(kinds.contains(&"time went backwards"));
    }

    #[tokio::test]
    async fn timestamps_never_go_backwards() {
        let s = testkit::state().await;
        record(&s, None, kind::BOOT, "system", "", json!({})).await;
        drop_guards(&s).await;
        // The phone's clock jumped back a year: the next entry still follows.
        sqlx::query("UPDATE audit_log SET ts = '2999-01-01T00:00:00.000Z'").execute(&s.db).await.unwrap();
        record(&s, None, kind::BOOT, "system", "", json!({})).await;
        let ts: String = sqlx::query_scalar("SELECT ts FROM audit_log WHERE seq = 2").fetch_one(&s.db).await.unwrap();
        assert_eq!(ts, "2999-01-01T00:00:00.001Z");
    }

    #[tokio::test]
    async fn list_pages_newest_first_and_marks_anchored() {
        let m = Mock::start().await;
        m.on("/v1/audit/anchor", json!({"anchoredAt": "2026-09-21T00:00:00Z", "sig": "ab", "key": "gw"}));
        let s = testkit::state_on(&m).await;
        for i in 0..5 {
            record(&s, None, kind::STATUS, "agent", &format!("s{i}"), json!({"i": i})).await;
        }
        let page = list(&s, None, 2).await.unwrap();
        assert_eq!(page.iter().map(|e| e["seq"].as_i64().unwrap()).collect::<Vec<_>>(), vec![5, 4]);
        assert_eq!(page[0]["payload"], json!({"i": 4}));
        let next = list(&s, Some(4), 10).await.unwrap();
        assert_eq!(next.len(), 3);
        assert_eq!(list(&s, None, 0).await.unwrap().len(), 1, "limit clamps to at least 1");
        assert!(page.iter().all(|e| e["anchored"] == false));
        anchor(&s).await.unwrap();
        record(&s, None, kind::STATUS, "agent", "after", json!({})).await;
        let all = list(&s, None, 500).await.unwrap();
        assert_eq!(all[0]["anchored"], false);
        assert!(all[1..].iter().all(|e| e["anchored"] == true));
    }

    #[tokio::test]
    async fn anchoring() {
        let m = Mock::start().await;
        m.on("/v1/audit/anchor", json!({"anchoredAt": "2026-09-21T00:00:00Z", "sig": "ab", "key": "gw"}));
        let s = testkit::state_on(&m).await;
        assert_eq!(anchor(&s).await.unwrap(), json!({"status": "empty"}));
        record(&s, None, kind::BOOT, "system", "", json!({})).await;
        let a = anchor(&s).await.unwrap();
        assert_eq!((a["status"].clone(), a["seq"].clone()), (json!("anchored"), json!(1)));
        let sent = &m.seen_path("/v1/audit/anchor")[0].body;
        assert_eq!(sent["seq"], 1);
        assert_eq!(anchor(&s).await.unwrap()["status"], "current");
        assert_eq!(m.seen_path("/v1/audit/anchor").len(), 1);
        let v = verify(&s).await.unwrap();
        assert_eq!(v["anchor"]["inChain"], true);
        assert_eq!(v["unanchoredEntries"], 0);
        // A gateway reply without a signature is refused.
        let m2 = Mock::start().await;
        m2.on("/v1/audit/anchor", json!({"anchoredAt": "x"}));
        let s2 = testkit::state_on(&m2).await;
        record(&s2, None, kind::BOOT, "system", "", json!({})).await;
        assert!(anchor(&s2).await.unwrap_err().to_string().contains("no anchor"));
        // Offline: an error, nothing stored.
        let off = testkit::state().await;
        record(&off, None, kind::BOOT, "system", "", json!({})).await;
        assert!(anchor(&off).await.is_err());
        assert_eq!(verify(&off).await.unwrap()["anchor"], Value::Null);
    }

    #[tokio::test]
    async fn anchor_pointing_outside_the_chain_is_flagged() {
        let s = testkit::state().await;
        record(&s, None, kind::BOOT, "system", "", json!({})).await;
        sqlx::query("INSERT INTO audit_anchors (seq, head_hash, anchored_at, anchor_sig, anchor_key) VALUES (1, 'forged', 't', 's', 'k')").execute(&s.db).await.unwrap();
        assert_eq!(verify(&s).await.unwrap()["anchor"]["inChain"], false);
    }

    #[tokio::test]
    async fn record_never_fails_the_caller() {
        let s = testkit::state().await;
        sqlx::query("DROP TABLE audit_log").execute(&s.db).await.unwrap();
        record(&s, None, kind::BOOT, "system", "", json!({})).await; // logs, returns
    }

    #[test]
    fn now_ms_is_recent() {
        assert!(now_ms() > 1_700_000_000_000);
    }
}
