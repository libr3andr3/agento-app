//! Catalog photos (D15). The bytes live on the phone in the `media` table;
//! a customer sees them through a private link the gateway serves for a few
//! minutes (`POST /v1/drop`) — the notification reply carries the URL, never
//! the image.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use crate::AppState;

/// Largest photo the phone stores (the app resizes to ~1280px first).
pub const MAX_BYTES: usize = 6 * 1024 * 1024;
/// Photos on one private link — a gallery, not an archive.
pub const MAX_PER_LINK: usize = 12;
/// How long a link lives at the gateway.
pub const LINK_TTL_SECS: u64 = 5 * 60;

pub async fn list(db: &sqlx::SqlitePool, business_id: Uuid) -> Result<Vec<Value>> {
    let rows: Vec<(Uuid, Option<String>, Option<String>, String, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, product, caption, mime, size, created_at FROM media \
         WHERE business_id = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(business_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, product, caption, mime, size, at)| {
            json!({"id": id, "product": product, "caption": caption, "mime": mime, "size": size, "createdAt": at.to_rfc3339()})
        })
        .collect())
}

pub async fn store(
    db: &sqlx::SqlitePool,
    business_id: Uuid,
    product: Option<&str>,
    caption: Option<&str>,
    mime: &str,
    bytes: &[u8],
) -> Result<Uuid> {
    if bytes.is_empty() {
        return Err(anyhow!("empty photo"));
    }
    if bytes.len() > MAX_BYTES {
        return Err(anyhow!("photo too large"));
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO media (id, business_id, product, caption, mime, size, bytes) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(id)
        .bind(business_id)
        .bind(product.map(|s| s.trim()).filter(|s| !s.is_empty()))
        .bind(caption.map(|s| s.trim()).filter(|s| !s.is_empty()))
        .bind(if mime.is_empty() { "image/jpeg" } else { mime })
        .bind(bytes.len() as i64)
        .bind(bytes)
        .execute(db)
        .await?;
    Ok(id)
}

pub async fn update(db: &sqlx::SqlitePool, business_id: Uuid, id: Uuid, product: Option<&str>, caption: Option<&str>) -> Result<bool> {
    let n = sqlx::query("UPDATE media SET product = COALESCE($3, product), caption = COALESCE($4, caption) WHERE business_id = $1 AND id = $2")
        .bind(business_id)
        .bind(id)
        .bind(product)
        .bind(caption)
        .execute(db)
        .await?
        .rows_affected();
    Ok(n > 0)
}

pub async fn bytes(db: &sqlx::SqlitePool, business_id: Uuid, id: Uuid) -> Result<Option<(String, Vec<u8>)>> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as("SELECT mime, bytes FROM media WHERE business_id = $1 AND id = $2")
        .bind(business_id)
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row)
}

pub async fn delete(db: &sqlx::SqlitePool, business_id: Uuid, id: Uuid) -> Result<bool> {
    let n = sqlx::query("DELETE FROM media WHERE business_id = $1 AND id = $2")
        .bind(business_id)
        .bind(id)
        .execute(db)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Photos for a link: explicit ids win; else product names (case-insensitive
/// substring either way, so "polo" finds "Polo Yaya" and "polo yaya azul"
/// finds "polo"); else the newest ones.
async fn select(
    db: &sqlx::SqlitePool,
    business_id: Uuid,
    ids: &[Uuid],
    products: &[String],
) -> Result<Vec<(Uuid, Option<String>, Option<String>, String, Vec<u8>)>> {
    let all: Vec<(Uuid, Option<String>, Option<String>, String, Vec<u8>)> = sqlx::query_as(
        "SELECT id, product, caption, mime, bytes FROM media WHERE business_id = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(business_id)
    .fetch_all(db)
    .await?;
    let wanted: Vec<String> = products.iter().map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect();
    let picked: Vec<_> = if !ids.is_empty() {
        all.into_iter().filter(|r| ids.contains(&r.0)).collect()
    } else if !wanted.is_empty() {
        all.into_iter()
            .filter(|r| {
                let p = r.1.as_deref().unwrap_or("").to_lowercase();
                let c = r.2.as_deref().unwrap_or("").to_lowercase();
                wanted.iter().any(|w| (!p.is_empty() && (p.contains(w) || w.contains(&p))) || (!c.is_empty() && c.contains(w)))
            })
            .collect()
    } else {
        all
    };
    Ok(picked.into_iter().take(MAX_PER_LINK).collect())
}

/// Mints the private link. `{url, expiresAt, photos, products}` or an error
/// the caller words for whoever asked (the owner's screen or the customer agent).
pub async fn share(state: &AppState, business_id: Uuid, ids: &[Uuid], products: &[String], note: Option<&str>) -> Result<Value> {
    let picked = select(&state.db, business_id, ids, products).await?;
    if picked.is_empty() {
        return Ok(json!({"status": "no_photos", "note": "the catalog has no photos for that — describe it in words or ask the owner to add photos"}));
    }
    let title: (String,) = sqlx::query_as("SELECT name FROM businesses WHERE id = $1")
        .bind(business_id)
        .fetch_one(&state.db)
        .await?;
    use base64::Engine;
    let items: Vec<Value> = picked
        .iter()
        .map(|(_, product, caption, mime, bytes)| {
            json!({
                "name": product.clone().or_else(|| caption.clone()).unwrap_or_default(),
                "caption": caption,
                "mime": mime,
                "b64": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect();
    let names: Vec<String> = picked.iter().filter_map(|r| r.1.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let body = json!({
        "ttlSecs": LINK_TTL_SECS,
        "title": title.0,
        "note": note,
        "items": items,
    });
    let v = state
        .registry
        .post("/v1/drop", &body, Duration::from_secs(60))
        .await
        .map_err(|e| anyhow!("private link unavailable: {e}"))?;
    let url = v["url"].as_str().ok_or_else(|| anyhow!("gateway returned no url"))?.to_string();
    Ok(json!({
        "status": "ok",
        "url": url,
        "expiresAt": v["expiresAt"],
        "expiresInMinutes": LINK_TTL_SECS / 60,
        "photos": picked.len(),
        "products": names,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, Mock};

    #[tokio::test]
    async fn store_validates_and_trims() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        assert!(store(&db, b, None, None, "", &[]).await.unwrap_err().to_string().contains("empty"));
        assert!(store(&db, b, None, None, "", &vec![0u8; MAX_BYTES + 1]).await.unwrap_err().to_string().contains("too large"));
        let id = store(&db, b, Some("  Polo Yaya "), Some("   "), "", &[1, 2, 3]).await.unwrap();
        let l = list(&db, b).await.unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!((l[0]["id"].clone(), l[0]["product"].clone(), l[0]["caption"].clone(), l[0]["mime"].clone(), l[0]["size"].clone()),
                   (json!(id), json!("Polo Yaya"), Value::Null, json!("image/jpeg"), json!(3)));
        assert_eq!(bytes(&db, b, id).await.unwrap(), Some(("image/jpeg".to_string(), vec![1, 2, 3])));
    }

    #[tokio::test]
    async fn one_business_never_sees_anothers_photos() {
        let db = testkit::db().await;
        let (a, b) = (testkit::business(&db).await, testkit::business(&db).await);
        let id = store(&db, a, Some("x"), None, "image/png", &[1]).await.unwrap();
        assert!(list(&db, b).await.unwrap().is_empty());
        assert_eq!(bytes(&db, b, id).await.unwrap(), None);
        assert!(!update(&db, b, id, Some("hacked"), None).await.unwrap());
        assert!(!delete(&db, b, id).await.unwrap());
        assert_eq!(list(&db, a).await.unwrap()[0]["product"], "x");
    }

    #[tokio::test]
    async fn update_keeps_fields_not_given_and_delete_removes() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        let id = store(&db, b, Some("Polo"), Some("azul"), "image/jpeg", &[1]).await.unwrap();
        assert!(update(&db, b, id, None, Some("rojo")).await.unwrap());
        let l = list(&db, b).await.unwrap();
        assert_eq!((l[0]["product"].clone(), l[0]["caption"].clone()), (json!("Polo"), json!("rojo")));
        assert!(!update(&db, b, Uuid::new_v4(), Some("x"), None).await.unwrap());
        assert!(delete(&db, b, id).await.unwrap());
        assert!(!delete(&db, b, id).await.unwrap());
        assert!(list(&db, b).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn select_by_ids_then_products_then_newest() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        let polo = store(&db, b, Some("Polo Yaya"), None, "", &[1]).await.unwrap();
        let gorra = store(&db, b, Some("Gorra"), Some("negra con logo"), "", &[2]).await.unwrap();
        let _anon = store(&db, b, None, None, "", &[3]).await.unwrap();
        let ids = |v: Vec<(Uuid, Option<String>, Option<String>, String, Vec<u8>)>| v.into_iter().map(|r| r.0).collect::<Vec<_>>();
        assert_eq!(ids(select(&db, b, &[gorra], &["polo".into()]).await.unwrap()), vec![gorra], "explicit ids win");
        assert_eq!(ids(select(&db, b, &[], &["POLO".into()]).await.unwrap()), vec![polo]);
        assert_eq!(ids(select(&db, b, &[], &["polo yaya azul".into()]).await.unwrap()), vec![polo], "the wanted name may contain the product's");
        assert_eq!(ids(select(&db, b, &[], &["logo".into()]).await.unwrap()), vec![gorra], "captions match too");
        assert_eq!(select(&db, b, &[], &["  ".into()]).await.unwrap().len(), 3, "blank names mean everything");
        for i in 0..20 { store(&db, b, Some(&format!("p{i}")), None, "", &[0]).await.unwrap(); }
        assert_eq!(select(&db, b, &[], &[]).await.unwrap().len(), MAX_PER_LINK);
    }

    #[tokio::test]
    async fn share_mints_a_private_link() {
        let m = Mock::start().await;
        m.on("/v1/drop", json!({"url": "https://drop/abc", "expiresAt": "soon"}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        assert_eq!(share(&s, b, &[], &[], None).await.unwrap()["status"], "no_photos");
        store(&s.db, b, Some("Polo"), Some("azul"), "image/png", &[9, 9]).await.unwrap();
        store(&s.db, b, Some("Polo"), None, "image/png", &[8]).await.unwrap();
        let r = share(&s, b, &[], &["polo".into()], Some("para Ana")).await.unwrap();
        assert_eq!((r["status"].clone(), r["url"].clone(), r["photos"].clone(), r["products"].clone(), r["expiresInMinutes"].clone()),
                   (json!("ok"), json!("https://drop/abc"), json!(2), json!(["Polo"]), json!(5)));
        let sent = &m.seen_path("/v1/drop")[0].body;
        assert_eq!((sent["title"].clone(), sent["note"].clone(), sent["ttlSecs"].clone()), (json!("Tito"), json!("para Ana"), json!(LINK_TTL_SECS)));
        assert_eq!(sent["items"].as_array().unwrap().len(), 2);
        assert!(sent["items"][0]["b64"].is_string());
    }

    #[tokio::test]
    async fn share_errors_are_worded() {
        let m = Mock::start().await;
        m.on("/v1/drop", json!({}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        store(&s.db, b, Some("x"), None, "", &[1]).await.unwrap();
        assert!(share(&s, b, &[], &[], None).await.unwrap_err().to_string().contains("no url"));
        let off = testkit::state().await;
        let b = testkit::business(&off.db).await;
        store(&off.db, b, Some("x"), None, "", &[1]).await.unwrap();
        assert!(share(&off, b, &[], &[], None).await.unwrap_err().to_string().contains("private link unavailable"));
    }
}
