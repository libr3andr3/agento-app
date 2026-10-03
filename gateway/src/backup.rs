//! Encrypted snapshots of a phone's core database, for the web dashboard and
//! for restoring onto a new phone. The gateway is a locker: it sees
//! ciphertext and the client's own metadata (KDF parameters, agent id,
//! version) and never a key. Paid tiers only, last few kept.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{accounts, agent_of, err, internal, plans, ApiResult, App, Auth, Shared};

pub const META_HEADER: &str = "x-backup-meta";

/// JSON with every non-ASCII character `\u`-escaped: header values are
/// ASCII on the wire, and clients (OkHttp) read any other byte as Latin-1.
fn ascii_json(v: &Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn dir(app: &App, account: &str) -> std::path::PathBuf {
    app.backup_dir.join(account)
}

/// `PUT /v1/backup` — a linked, paid agent stores a snapshot.
pub async fn put(State(app): State<Shared>, Extension(auth): Extension<Auth>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let agent = agent_of(&app, &auth).await?;
    let account = accounts::account_of_agent(&app, &agent).await?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "agent is not linked to an agente account"))?;
    let plan = plans::effective_for(&app, &accounts::subject(&account)).await?;
    if !plans::backups_allowed(&app, &plan.plan) {
        return Err(err(StatusCode::PAYMENT_REQUIRED, "backups are part of the Pro and Max plans"));
    }
    if body.len() < 32 {
        return Err(err(StatusCode::BAD_REQUEST, "empty backup"));
    }
    let meta = headers.get(META_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("{}");
    let meta: Value = serde_json::from_str(meta).ok().filter(Value::is_object).ok_or_else(|| err(StatusCode::BAD_REQUEST, "X-Backup-Meta must be a JSON object"))?;
    if meta.to_string().len() > 4096 {
        return Err(err(StatusCode::BAD_REQUEST, "X-Backup-Meta too large"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let d = dir(&app, &account);
    tokio::fs::create_dir_all(&d).await.map_err(internal)?;
    tokio::fs::write(d.join(format!("{id}.bin")), &body).await.map_err(internal)?;
    sqlx::query("INSERT INTO backups (id, account, agent, size, meta) VALUES ($1,$2,$3,$4,$5)")
        .bind(&id).bind(&account).bind(&agent).bind(body.len() as i64).bind(meta.to_string())
        .execute(&app.db).await.map_err(internal)?;
    // Keep the newest N; delete the rest, files first.
    let old: Vec<(String,)> = sqlx::query_as("SELECT id FROM backups WHERE account = $1 ORDER BY created_at DESC LIMIT -1 OFFSET $2")
        .bind(&account).bind(app.backup_keep as i64).fetch_all(&app.db).await.map_err(internal)?;
    for (oid,) in old {
        let _ = tokio::fs::remove_file(d.join(format!("{oid}.bin"))).await;
        let _ = sqlx::query("DELETE FROM backups WHERE id = $1").bind(&oid).execute(&app.db).await;
    }
    tracing::info!(%account, %agent, size = body.len(), "backup stored");
    Ok(Json(json!({"ok": true, "id": id, "size": body.len(), "createdAt": chrono::Utc::now().to_rfc3339()})).into_response())
}

async fn fetch(app: &App, account: &str, id: Option<&str>) -> ApiResult {
    let row: Option<(String, String, String, String)> = match id {
        Some(id) => sqlx::query_as("SELECT id, agent, meta, created_at FROM backups WHERE account = $1 AND id = $2").bind(account).bind(id).fetch_optional(&app.db).await,
        None => sqlx::query_as("SELECT id, agent, meta, created_at FROM backups WHERE account = $1 ORDER BY created_at DESC LIMIT 1").bind(account).fetch_optional(&app.db).await,
    }.map_err(internal)?;
    let Some((id, agent, meta, at)) = row else { return Err(err(StatusCode::NOT_FOUND, "no backup yet")) };
    let bytes = tokio::fs::read(dir(app, account).join(format!("{id}.bin"))).await.map_err(|_| err(StatusCode::NOT_FOUND, "backup file missing"))?;
    let mut meta_v: Value = serde_json::from_str(&meta).ok().filter(Value::is_object).unwrap_or(json!({}));
    meta_v["id"] = json!(id);
    meta_v["agent"] = json!(agent);
    meta_v["createdAt"] = json!(at);
    Ok((
        StatusCode::OK,
        [("content-type", "application/octet-stream".to_string()), (META_HEADER, ascii_json(&meta_v))],
        bytes,
    ).into_response())
}

/// `GET /v1/backup/latest` — web session or linked agent.
pub async fn latest(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    fetch(&app, &account, None).await
}

/// `GET /v1/backup/{id}`.
pub async fn get(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let account = accounts::account_of_auth(&app, &auth).await?;
    fetch(&app, &account, Some(&id)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, agent_headers, raw, session_for, Keypair};

    async fn put_as(app: &Shared, kp: &Keypair, body: &[u8], meta: Option<&str>) -> u16 {
        let mut h = agent_headers(kp, "PUT", "/v1/backup", body);
        if let Some(m) = meta { h.push((META_HEADER, m.to_string())); }
        raw(app, "PUT", "/v1/backup", &h, Some(body.to_vec())).await.0
    }

    async fn pro(app: &Shared, id: &str) {
        plans::set(app, &accounts::subject(id), "pro", 1, "test", None).await.unwrap();
    }

    #[tokio::test]
    async fn paid_accounts_keep_their_newest_snapshots() {
        let app = testkit::app_with(|a| crate::App { backup_keep: 2, ..a }).await;
        let kp = Keypair::generate();
        account_with_agent(&app, "a", "51900000001", &kp).await;
        sqlx::query("DELETE FROM plans").execute(&app.db).await.unwrap();
        assert_eq!(put_as(&app, &kp, &[1; 64], None).await, 402, "free accounts have no locker");
        pro(&app, "a").await;
        assert_eq!(put_as(&app, &kp, &[1; 8], None).await, 400);
        assert_eq!(put_as(&app, &kp, &[1; 64], Some("not json")).await, 400);
        assert_eq!(put_as(&app, &kp, &[1; 64], Some(&format!("{{\"k\":\"{}\"}}", "x".repeat(5000)))).await, 400);
        assert_eq!(put_as(&app, &Keypair::generate(), &[1; 64], None).await, 401, "unlinked agents cannot store");
        for n in 1..=3u8 {
            assert_eq!(put_as(&app, &kp, &[n; 64], Some(r#"{"v":1}"#)).await, 200);
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM backups WHERE account = 'a'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read_dir(app.backup_dir.join("a")).unwrap().count(), 2, "pruned files are gone too");
        let token = session_for(&app, "a").await;
        let (st, body) = raw(&app, "GET", "/v1/backup/latest", &[("authorization", format!("Bearer {token}"))], None).await;
        assert_eq!((st, body), (200, vec![3; 64]));
        account_with_agent(&app, "b", "51900000002", &Keypair::generate()).await;
        let other = session_for(&app, "b").await;
        let (id,): (String,) = sqlx::query_as("SELECT id FROM backups WHERE account = 'a' LIMIT 1").fetch_one(&app.db).await.unwrap();
        assert_eq!(raw(&app, "GET", &format!("/v1/backup/{id}"), &[("authorization", format!("Bearer {other}"))], None).await.0, 404, "never another account's");
        assert_eq!(raw(&app, "GET", &format!("/v1/backup/{id}"), &[("authorization", format!("Bearer {token}"))], None).await.0, 200);
    }

    #[tokio::test]
    async fn any_metadata_the_locker_accepts_comes_back_on_restore() {
        let app = testkit::app().await;
        let kp = Keypair::generate();
        account_with_agent(&app, "a", "51900000001", &kp).await;
        pro(&app, "a").await;
        let token = session_for(&app, "a").await;
        let auth = [("authorization", format!("Bearer {token}"))];
        let restore = || raw(&app, "GET", "/v1/backup/latest", &auth, None);
        // A device name with an ñ, sent ASCII-escaped as the header allows.
        assert_eq!(put_as(&app, &kp, &[7; 64], Some(r#"{"device":"Moto de Pe\u00f1a"}"#)).await, 200);
        assert_eq!(restore().await.0, 200, "the restore must not fail on its own metadata");
        // Metadata that is JSON but not an object is refused up front.
        assert_eq!(put_as(&app, &kp, &[8; 64], Some("[1]")).await, 400);
        assert_eq!(restore().await.0, 200);
    }

    #[test]
    fn header_json_is_ascii() {
        let v = json!({"device": "Peña 🌵", "n": 1});
        let h = ascii_json(&v);
        assert!(h.is_ascii());
        assert_eq!(serde_json::from_str::<Value>(&h).unwrap(), v);
    }
}

