//! Encrypted snapshots of this installation's data — the web dashboard's
//! source and the way a new phone picks up where the old one left off.
//!
//! The snapshot is every business table as JSON (`{tables: {name:
//! {columns, rows}}}`), sealed with AES-256-GCM under the account's backup
//! key, which the gateway mints per account and hands to signed-in devices
//! and browsers. Files at rest are ciphertext; the key is the account's.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use serde_json::{json, Value};
use sqlx::{Column, Row, TypeInfo};
use std::time::Duration;

use crate::{account, AppState};

pub const FORMAT: &str = "agente-snapshot/1";
const NONCE_LEN: usize = 12;
/// Never part of a snapshot: the identity (per phone), the account row
/// (session + key), device attestation (per phone), migrations.
/// The audit chain is bound to the identity that signed it and is
/// append-only by trigger — it can neither be exported as evidence of another
/// phone nor imported over (a restore would have to DELETE it).
const PRIVATE: &[&str] = &["agent_identity", "account", "device_attestation", "_sqlx_migrations", "audit_log", "audit_anchors"];

async fn tables(db: &sqlx::SqlitePool) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| r.0).filter(|t| !PRIVATE.contains(&t.as_str())).collect())
}

fn cell(row: &sqlx::sqlite::SqliteRow, i: usize) -> Value {
    let ty = row.column(i).type_info().name().to_ascii_uppercase();
    if ty.contains("INT") || ty.contains("BOOL") {
        if let Ok(v) = row.try_get::<Option<i64>, _>(i) { return v.map(Value::from).unwrap_or(Value::Null); }
    }
    if ty.contains("REAL") || ty.contains("FLOA") || ty.contains("DOUB") || ty.contains("NUM") {
        if let Ok(v) = row.try_get::<Option<f64>, _>(i) { return v.map(Value::from).unwrap_or(Value::Null); }
    }
    if ty.contains("BLOB") {
        if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(i) {
            use base64::Engine;
            return v.map(|b| json!({"$b": base64::engine::general_purpose::STANDARD.encode(b)})).unwrap_or(Value::Null);
        }
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(i) { return v.map(Value::from).unwrap_or(Value::Null); }
    if let Ok(v) = row.try_get::<Option<i64>, _>(i) { return v.map(Value::from).unwrap_or(Value::Null); }
    if let Ok(v) = row.try_get::<Option<f64>, _>(i) { return v.map(Value::from).unwrap_or(Value::Null); }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(i) {
        use base64::Engine;
        return v.map(|b| json!({"$b": base64::engine::general_purpose::STANDARD.encode(b)})).unwrap_or(Value::Null);
    }
    Value::Null
}

/// Every business table, as JSON.
pub async fn export(db: &sqlx::SqlitePool) -> anyhow::Result<Value> {
    let mut out = serde_json::Map::new();
    for t in tables(db).await? {
        let rows = sqlx::query(&format!("SELECT * FROM \"{t}\"")).fetch_all(db).await?;
        let columns: Vec<String> = rows.first().map(|r| r.columns().iter().map(|c| c.name().to_string()).collect()).unwrap_or_default();
        let data: Vec<Value> = rows.iter().map(|r| Value::Array((0..r.len()).map(|i| cell(r, i)).collect())).collect();
        out.insert(t, json!({"columns": columns, "rows": data}));
    }
    Ok(json!({"format": FORMAT, "exportedAt": chrono::Utc::now().to_rfc3339(), "tables": out}))
}

enum Bind { Null, Int(i64), Real(f64), Text(String), Blob(Vec<u8>) }

fn bind_of(v: &Value) -> Bind {
    match v {
        Value::Null => Bind::Null,
        Value::Bool(b) => Bind::Int(*b as i64),
        Value::Number(n) => n.as_i64().map(Bind::Int).unwrap_or_else(|| Bind::Real(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Bind::Text(s.clone()),
        Value::Object(o) if o.contains_key("$b") => {
            use base64::Engine;
            Bind::Blob(base64::engine::general_purpose::STANDARD.decode(o["$b"].as_str().unwrap_or("")).unwrap_or_default())
        }
        other => Bind::Text(other.to_string()),
    }
}

/// Replaces every business table with the snapshot's rows. Columns the
/// current schema does not know are skipped, so an older phone's snapshot
/// still restores onto a newer build (migrations ran at boot).
pub async fn import(db: &sqlx::SqlitePool, snapshot: &Value) -> anyhow::Result<usize> {
    anyhow::ensure!(snapshot["format"].as_str() == Some(FORMAT), "not an agente snapshot");
    let known = tables(db).await?;
    let mut total = 0usize;
    let mut tx = db.begin().await?;
    sqlx::query("PRAGMA defer_foreign_keys = ON").execute(&mut *tx).await?;
    for (t, body) in snapshot["tables"].as_object().into_iter().flatten() {
        if !known.contains(t) { continue; }
        let have: Vec<(String,)> = sqlx::query_as(&format!("SELECT name FROM pragma_table_info('{t}')")).fetch_all(&mut *tx).await?;
        let have: Vec<String> = have.into_iter().map(|r| r.0).collect();
        let cols: Vec<(usize, String)> = body["columns"].as_array().into_iter().flatten().enumerate()
            .filter_map(|(i, c)| c.as_str().filter(|c| have.iter().any(|h| h == c)).map(|c| (i, c.to_string()))).collect();
        sqlx::query(&format!("DELETE FROM \"{t}\"")).execute(&mut *tx).await?;
        if cols.is_empty() { continue; }
        let sql = format!(
            "INSERT INTO \"{t}\" ({}) VALUES ({})",
            cols.iter().map(|(_, c)| format!("\"{c}\"")).collect::<Vec<_>>().join(", "),
            cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
        );
        for row in body["rows"].as_array().into_iter().flatten() {
            let cells = row.as_array().cloned().unwrap_or_default();
            let mut q = sqlx::query(&sql);
            for (i, _) in &cols {
                q = match bind_of(cells.get(*i).unwrap_or(&Value::Null)) {
                    Bind::Null => q.bind(None::<String>),
                    Bind::Int(n) => q.bind(n),
                    Bind::Real(f) => q.bind(f),
                    Bind::Text(s) => q.bind(s),
                    Bind::Blob(b) => q.bind(b),
                };
            }
            q.execute(&mut *tx).await?;
            total += 1;
        }
    }
    tx.commit().await?;
    Ok(total)
}

pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    let ct = Aes256Gcm::new(key.into()).encrypt(Nonce::from_slice(&nonce), plaintext).map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok([nonce.as_slice(), &ct].concat())
}

pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(blob.len() > NONCE_LEN, "backup too short");
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    Aes256Gcm::new(key.into()).decrypt(Nonce::from_slice(nonce), ct).map_err(|_| anyhow::anyhow!("backup cannot be opened with this password"))
}

fn meta(signed: &account::Signed, state: &AppState) -> Value {
    json!({
        "format": FORMAT, "alg": "aes-256-gcm", "kdf": "account-key", "account": signed.account_id,
        "agent": state.identity.id(), "core": env!("CARGO_PKG_VERSION"),
        "createdAt": chrono::Utc::now().to_rfc3339(),
    })
}

/// Export, seal, upload. Errors name the reason (not signed in, free plan…).
pub async fn run(state: &AppState) -> anyhow::Result<Value> {
    let signed = account::load(state).await?.ok_or_else(|| anyhow::anyhow!("not signed in"))?;
    let key = signed.backup_key.ok_or_else(|| anyhow::anyhow!("no backup key — sign in again"))?;
    let snapshot = export(&state.db).await?;
    let sealed = encrypt(&key, snapshot.to_string().as_bytes())?;
    let m = meta(&signed, state);
    let v = state.registry.put_bytes("/v1/backup", sealed, &[("x-backup-meta", m.to_string())], Duration::from_secs(120)).await?;
    sqlx::query("UPDATE account SET last_backup_at = $1 WHERE id = 1").bind(chrono::Utc::now().to_rfc3339()).execute(&state.db).await?;
    tracing::info!(size = v["size"].as_i64().unwrap_or(0), "backup uploaded");
    Ok(v)
}

/// Download the latest snapshot and replace local data with it.
pub async fn restore(state: &AppState) -> anyhow::Result<Value> {
    let signed = account::load(state).await?.ok_or_else(|| anyhow::anyhow!("not signed in"))?;
    let key = signed.backup_key.ok_or_else(|| anyhow::anyhow!("no backup key — sign in again"))?;
    let (bytes, headers) = state.registry.get_bytes("/v1/backup/latest", Duration::from_secs(120)).await?;
    let m: Value = headers.get("x-backup-meta").and_then(|v| v.to_str().ok()).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
    let plain = decrypt(&key, &bytes)?;
    let snapshot: Value = serde_json::from_slice(&plain)?;
    let rows = import(&state.db, &snapshot).await?;
    tracing::info!(rows, from = ?m["agent"].as_str(), "backup restored");
    crate::audit::record(state, None, crate::audit::kind::RESTORE, "owner", "", json!({
        "rows": rows, "fromAgent": m["agent"], "snapshotId": m["id"], "snapshotCreatedAt": m["createdAt"],
    })).await;
    Ok(json!({"restored": true, "rows": rows, "from": m}))
}

/// Backs up in the background when the plan allows and the last one is
/// older than six hours. Called after activity (turns, dashboard opens).
pub fn backup_if_due(state: crate::SharedState) {
    tokio::spawn(async move {
        let Ok(Some(signed)) = account::load(&state).await else { return };
        let allowed = state.plan_info.lock().unwrap_or_else(|e| e.into_inner())["backups"].as_bool().unwrap_or(false);
        if !allowed { return; }
        let due = signed.last_backup_at.as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or(true, |t| chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)).num_hours() >= 6);
        if !due { return; }
        if let Err(e) = run(&state).await {
            tracing::warn!(error = %e, "scheduled backup failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_wrong_key() {
        let k = [1u8; 32];
        let blob = encrypt(&k, b"{\"format\":\"x\"}").unwrap();
        assert_eq!(decrypt(&k, &blob).unwrap(), b"{\"format\":\"x\"}");
        assert!(decrypt(&[2u8; 32], &blob).is_err());
    }

    #[tokio::test]
    async fn export_import_roundtrip_on_a_fresh_db() {
        let db = crate::testkit::db().await;
        sqlx::query("INSERT INTO businesses (id, name, industry, owner_phone) VALUES ($1, 'Tito', 'barbería', '+51999')")
            .bind(uuid::Uuid::new_v4()).execute(&db).await.unwrap();
        let snap = export(&db).await.unwrap();
        assert_eq!(snap["tables"]["businesses"]["rows"].as_array().unwrap().len(), 1);
        assert!(snap["tables"].get("agent_identity").is_none());
        sqlx::query("DELETE FROM businesses").execute(&db).await.unwrap();
        let n = import(&db, &snap).await.unwrap();
        assert!(n >= 1);
        let (name,): (String,) = sqlx::query_as("SELECT name FROM businesses").fetch_one(&db).await.unwrap();
        assert_eq!(name, "Tito");
    }

    use crate::testkit::{self, Mock};

    #[test]
    fn decrypt_rejects_short_and_tampered() {
        let k = [3u8; 32];
        assert!(decrypt(&k, &[0u8; 12]).unwrap_err().to_string().contains("too short"));
        let mut b = encrypt(&k, b"x").unwrap();
        *b.last_mut().unwrap() ^= 1;
        assert!(decrypt(&k, &b).unwrap_err().to_string().contains("cannot be opened"));
        assert_ne!(encrypt(&k, b"x").unwrap(), encrypt(&k, b"x").unwrap());
    }

    #[test]
    fn bind_of_every_json_shape() {
        use base64::Engine;
        assert!(matches!(bind_of(&Value::Null), Bind::Null));
        assert!(matches!(bind_of(&json!(true)), Bind::Int(1)));
        assert!(matches!(bind_of(&json!(7)), Bind::Int(7)));
        assert!(matches!(bind_of(&json!(1.5)), Bind::Real(f) if f == 1.5));
        assert!(matches!(bind_of(&json!("s")), Bind::Text(ref t) if t == "s"));
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2]);
        assert!(matches!(bind_of(&json!({"$b": b64})), Bind::Blob(ref b) if b == &vec![1u8, 2]));
        assert!(matches!(bind_of(&json!([1])), Bind::Text(ref t) if t == "[1]"));
        assert!(matches!(bind_of(&json!({"a": 1})), Bind::Text(_)));
    }

    #[tokio::test]
    async fn export_leaves_private_tables_out_and_keeps_types() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        crate::audit::record(&s, Some(b), crate::audit::kind::BOOT, "system", "", json!({})).await;
        sqlx::query("INSERT INTO outcomes (id, business_id, kind, client_hash, charged_cents) VALUES ('o1', $1, 'sale', 'h', 150)").bind(b).execute(&s.db).await.unwrap();
        let snap = export(&s.db).await.unwrap();
        assert_eq!(snap["format"], FORMAT);
        for t in PRIVATE {
            assert!(snap["tables"].get(*t).is_none(), "{t} must never be exported");
        }
        let biz = &snap["tables"]["businesses"];
        let id_col = biz["columns"].as_array().unwrap().iter().position(|c| c == "id").unwrap();
        assert!(biz["rows"][0][id_col]["$b"].is_string(), "blobs are base64-wrapped");
        let o = &snap["tables"]["outcomes"];
        let cents = o["columns"].as_array().unwrap().iter().position(|c| c == "charged_cents").unwrap();
        assert_eq!(o["rows"][0][cents], 150);
        // Empty tables still appear, with no columns.
        assert_eq!(snap["tables"]["coins"]["rows"], json!([]));
    }

    #[tokio::test]
    async fn import_tolerates_unknown_tables_and_columns() {
        let db = testkit::db().await;
        let id = uuid::Uuid::new_v4();
        use base64::Engine;
        let snap = json!({"format": FORMAT, "tables": {
            "businesses": {"columns": ["id", "name", "industry", "owner_phone", "column_from_the_future"],
                           "rows": [[{"$b": base64::engine::general_purpose::STANDARD.encode(id.as_bytes())}, "Tito", "barbería", "+51", "x"]]},
            "table_from_the_future": {"columns": ["a"], "rows": [[1]]},
            "agent_identity": {"columns": ["id"], "rows": [[1]]},
        }});
        assert_eq!(import(&db, &snap).await.unwrap(), 1);
        let (bid, name): (uuid::Uuid, String) = sqlx::query_as("SELECT id, name FROM businesses").fetch_one(&db).await.unwrap();
        assert_eq!((bid, name.as_str()), (id, "Tito"));
        let ids: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_identity").fetch_one(&db).await.unwrap();
        assert_eq!(ids, 0, "private tables are never written by a restore");
        assert!(import(&db, &json!({"format": "other"})).await.unwrap_err().to_string().contains("not an agente snapshot"));
    }

    #[tokio::test]
    async fn import_is_all_or_nothing() {
        let db = testkit::db().await;
        testkit::business(&db).await;
        // Second row violates NOT NULL: the whole restore rolls back.
        let snap = json!({"format": FORMAT, "tables": {"businesses": {"columns": ["id", "name", "industry", "owner_phone"], "rows": [
            [{"$b": "AAAAAAAAAAAAAAAAAAAAAA=="}, "A", "x", "1"], [{"$b": "AQEBAQEBAQEBAQEBAQEBAQ=="}, null, "x", "1"]
        ]}}});
        assert!(import(&db, &snap).await.is_err());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM businesses WHERE name = 'Tito'").fetch_one(&db).await.unwrap();
        assert_eq!(n, 1, "the original data survives a failed restore");
    }

    #[tokio::test]
    async fn run_and_restore_through_the_gateway() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        assert!(run(&s).await.unwrap_err().to_string().contains("not signed in"));
        assert!(restore(&s).await.unwrap_err().to_string().contains("not signed in"));
        testkit::sign_in(&s, &m).await;
        testkit::business(&s.db).await;
        m.on("/v1/backup", json!({"size": 10}));
        let snap_rows = export(&s.db).await.unwrap()["tables"]["businesses"]["rows"].clone();
        run(&s).await.unwrap();
        let put = m.seen_path("/v1/backup").into_iter().find(|x| x.method == "PUT").unwrap();
        let meta: Value = serde_json::from_str(put.headers["x-backup-meta"].to_str().unwrap()).unwrap();
        assert_eq!((meta["format"].clone(), meta["account"].clone(), meta["agent"].clone()), (json!(FORMAT), json!("acc_1"), json!(s.identity.id())));
        assert!(load_last_backup(&s).await.is_some());
        // Restore: the mock serves the sealed blob of the current snapshot.
        let blob = encrypt(&[1u8; 32], export(&s.db).await.unwrap().to_string().as_bytes()).unwrap();
        let s2 = serve_blob(blob, meta.clone()).await;
        let b2 = testkit::state_on(&s2).await;
        testkit::sign_in(&b2, &s2).await;
        let r = restore(&b2).await.unwrap();
        assert_eq!(r["restored"], true);
        assert_eq!(export(&b2.db).await.unwrap()["tables"]["businesses"]["rows"], snap_rows);
        let restores: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE kind = 'restore'").fetch_one(&b2.db).await.unwrap();
        assert_eq!(restores, 1, "a restore is recorded in the new chain");
    }

    async fn load_last_backup(s: &AppState) -> Option<String> {
        crate::account::load(s).await.unwrap().unwrap().last_backup_at
    }

    /// A gateway serving `blob` as the latest backup, with its meta header.
    async fn serve_blob(blob: Vec<u8>, meta: Value) -> Mock {
        let m = Mock::start().await;
        m.on_bytes("/v1/backup/latest", blob, &[("x-backup-meta", &meta.to_string())]);
        m
    }

    #[tokio::test]
    async fn restore_with_the_wrong_key_changes_nothing() {
        let blob = encrypt(&[9u8; 32], b"{}").unwrap();
        let m = serve_blob(blob, json!({})).await;
        let s = testkit::state_on(&m).await;
        testkit::sign_in(&s, &m).await;
        testkit::business(&s.db).await;
        assert!(restore(&s).await.unwrap_err().to_string().contains("cannot be opened"));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM businesses").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 1);
    }

    /// OPEN QUESTION (flagged, not fixed): a restore replaces the coins table
    /// wholesale, so coins received after the snapshot was taken disappear.
    /// This test pins today's behaviour so a policy change is deliberate.
    #[tokio::test]
    async fn restore_replaces_coins_received_after_the_snapshot() {
        let db = testkit::db().await;
        let snap = export(&db).await.unwrap();
        sqlx::query("INSERT INTO coins (coin, denomination, value_minor, sig, randomizer, status) VALUES ('c1','d',100,'s','r','received')").execute(&db).await.unwrap();
        import(&db, &snap).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM coins").fetch_one(&db).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn backup_if_due_respects_plan_and_age() {
        let m = Mock::start().await;
        m.on("/v1/backup", json!({"size": 1}));
        let s = testkit::state_on(&m).await;
        testkit::sign_in(&s, &m).await;
        // Plan does not allow backups: nothing happens.
        backup_if_due(s.clone());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(m.seen_path("/v1/backup").iter().all(|x| x.method != "PUT"));
        *s.plan_info.lock().unwrap() = json!({"backups": true});
        backup_if_due(s.clone());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(m.seen_path("/v1/backup").iter().filter(|x| x.method == "PUT").count(), 1);
        // Just backed up: not due again.
        backup_if_due(s.clone());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(m.seen_path("/v1/backup").iter().filter(|x| x.method == "PUT").count(), 1);
    }
}
