//! The CRM on the phone: who the business talks to. Rows appear on their
//! own — a conversation creates the contact, a booking teaches its name and
//! email, a payment its payer — and the owner can correct anything.

use serde_json::{json, Value};
use sqlx::SqlitePool;
use uuid::Uuid;

pub(crate) fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// What a peer id says about the person: the chat app it came from and,
/// for phones, the number as digits.
fn split_peer(peer: &str) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(agent) = peer.strip_prefix("agent:") {
        return (Some("network".into()), None, Some(format!("agent:{agent}")));
    }
    let (source, tail) = match peer.split_once(':') {
        Some((pkg, rest)) => (Some(source_of(pkg)), rest),
        None => (None, peer),
    };
    let d = digits(tail);
    (source, if d.len() >= 7 { Some(d) } else { None }, None)
}

fn source_of(pkg: &str) -> String {
    let p = pkg.to_ascii_lowercase();
    for (needle, name) in [("whatsapp", "whatsapp"), ("instagram", "instagram"), ("orca", "messenger"), ("telegram", "telegram"), ("securesms", "signal"), ("musically", "tiktok"), ("messaging", "sms")] {
        if p.contains(needle) {
            return name.into();
        }
    }
    p
}

/// Same person if the last 9 digits agree (country code may be missing).
pub(crate) fn same_phone(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len()).min(9);
    n >= 7 && a[a.len() - n..] == b[b.len() - n..]
}

/// A message just went through with this peer: make sure they exist, count it.
///
/// The `WHERE peer IS NOT NULL` on the conflict target is not decoration:
/// `idx_contacts_peer` is a *partial* unique index, and SQLite matches a
/// conflict target to one only when the clauses agree. Without it the whole
/// statement fails to parse — which it silently did, because the result is
/// discarded, so every conversation since 007 left the CRM empty.
pub async fn touch(db: &SqlitePool, business_id: Uuid, peer: &str) {
    let (source, phone, agent) = split_peer(peer);
    let _ = sqlx::query(
        "INSERT INTO contacts (id, business_id, kind, peer, source, phone, agent_id, messages) VALUES ($1,$2,'customer',$3,$4,$5,$6,1) \
         ON CONFLICT (business_id, peer) WHERE peer IS NOT NULL DO UPDATE SET messages = contacts.messages + 1, \
           last_seen = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), phone = COALESCE(contacts.phone, excluded.phone)",
    )
    .bind(Uuid::new_v4()).bind(business_id).bind(peer).bind(source).bind(phone).bind(agent)
    .execute(db).await;
}

/// Something taught us a name, an email or a phone for `peer` (or, with no
/// peer — a payment — for whoever has this phone). Never blanks a field.
pub async fn learn(db: &SqlitePool, business_id: Uuid, peer: Option<&str>, phone: Option<&str>, name: Option<&str>, email: Option<&str>) {
    let name = name.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(80).collect::<String>());
    let email = email.map(|e| e.trim().to_lowercase()).filter(|e| e.contains('@'));
    let phone_d = phone.map(digits).filter(|d| d.len() >= 7);
    if name.is_none() && email.is_none() && phone_d.is_none() {
        return;
    }
    let id: Option<Uuid> = match peer {
        Some(p) => {
            touch(db, business_id, p).await;
            sqlx::query_as::<_, (Uuid,)>("SELECT id FROM contacts WHERE business_id = $1 AND peer = $2").bind(business_id).bind(p).fetch_optional(db).await.ok().flatten().map(|r| r.0)
        }
        None => match &phone_d {
            Some(d) => {
                let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as("SELECT id, phone FROM contacts WHERE business_id = $1 AND phone IS NOT NULL").bind(business_id).fetch_all(db).await.unwrap_or_default();
                rows.into_iter().find(|(_, p)| p.as_deref().is_some_and(|p| same_phone(p, d))).map(|(id, _)| id)
            }
            None => None,
        },
    };
    let Some(id) = id else {
        // A payer we have never talked to: still a person the business dealt with.
        let _ = sqlx::query("INSERT INTO contacts (id, business_id, kind, source, phone, name, email) VALUES ($1,$2,'customer','payment',$3,$4,$5)")
            .bind(Uuid::new_v4()).bind(business_id).bind(&phone_d).bind(&name).bind(&email).execute(db).await;
        return;
    };
    let _ = sqlx::query(
        "UPDATE contacts SET name = COALESCE($2, name), email = COALESCE($3, email), phone = COALESCE(phone, $4), \
         last_seen = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id = $1",
    ).bind(id).bind(&name).bind(&email).bind(&phone_d).execute(db).await;

    // The same evidence, to the identity layer. This is the one place a
    // customer's own phone or email reaches us — a booking, an order, a
    // payment — and those two fields are the only proof that merges two chats
    // into one person; a matching name or handle never is. A client that sends
    // no channel has no person to teach, and keeps one identity per chat
    // exactly as before.
    let person = match peer {
        Some(p) => crate::people::for_peer(db, business_id, p).await,
        None => crate::people::for_contact(db, business_id, id).await,
    };
    if let Some(person_id) = person {
        crate::people::learn(
            db,
            business_id,
            person_id,
            phone_d.as_deref(),
            name.as_deref(),
            email.as_deref(),
        )
        .await;
    }
}

/// The owner, from their Yaya account: one row per business, kind `owner`.
pub async fn owner(db: &SqlitePool, business_id: Uuid, name: Option<&str>, email: Option<&str>, phone: Option<&str>) {
    let _ = sqlx::query(
        "INSERT INTO contacts (id, business_id, kind, peer, source, phone, email, name) VALUES ($1,$2,'owner','owner','owner',$3,$4,$5) \
         ON CONFLICT (business_id, peer) WHERE peer IS NOT NULL DO UPDATE SET phone = COALESCE(excluded.phone, contacts.phone), \
           email = COALESCE(excluded.email, contacts.email), name = COALESCE(excluded.name, contacts.name)",
    ).bind(Uuid::new_v4()).bind(business_id).bind(phone.map(digits).filter(|d| d.len() >= 7)).bind(email).bind(name).execute(db).await;
}

fn row_json(r: Row) -> Value {
    let (id, kind, peer, source, phone, email, name, agent, notes, tags, messages, first, last, plan) = r;
    json!({"id": id, "kind": kind, "peer": peer, "source": source, "phone": phone, "email": email, "name": name, "agentId": agent,
           "notes": notes, "tags": serde_json::from_str::<Value>(&tags).unwrap_or(json!([])), "messages": messages, "firstSeen": first, "lastSeen": last,
           "plan": plan})
}

type Row = (Uuid, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, String, i64, String, String, Option<String>);
const COLS: &str = "id, kind, peer, source, phone, email, name, agent_id, notes, tags, messages, first_seen, last_seen, plan";

/// D14 (seller): remember which agente plan a contact is on, by phone —
/// matched on the last nine digits so peers and typed numbers agree.
pub async fn set_plan(db: &SqlitePool, business_id: Uuid, phone: &str, plan: &str) {
    let d = digits(phone);
    if d.len() < 8 {
        return;
    }
    let tail = format!("%{}", &d[d.len() - 8..]);
    let _ = sqlx::query("UPDATE contacts SET plan = $3 WHERE business_id = $1 AND (COALESCE(phone,'') LIKE $2 OR COALESCE(peer,'') LIKE $2)")
        .bind(business_id).bind(&tail).bind(plan).execute(db).await;
}

/// Owner first, then customers by recency; `q` matches name, phone, email.
/// Matched here rather than in SQL: SQLite's `lower()` folds ASCII only, so
/// "álvaro" would never find "Álvaro".
pub async fn list(db: &SqlitePool, business_id: Uuid, q: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let q = q.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM contacts WHERE business_id = $1 \
         ORDER BY CASE kind WHEN 'owner' THEN 0 ELSE 1 END, last_seen DESC"
    )).bind(business_id).fetch_all(db).await?;
    let hit = |r: &Row| match &q {
        None => true,
        Some(q) => [&r.6, &r.4, &r.5].iter().any(|f| f.as_deref().is_some_and(|v| v.to_lowercase().contains(q.as_str()))),
    };
    Ok(rows.into_iter().filter(hit).take(500).map(row_json).collect())
}

pub async fn by_peer(db: &SqlitePool, business_id: Uuid, peer: &str) -> Option<Value> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM contacts WHERE business_id = $1 AND peer = $2")).bind(business_id).bind(peer).fetch_optional(db).await.ok().flatten();
    row.map(row_json)
}

/// The owner edits a contact: name, email, phone, notes, tags.
pub async fn update(db: &SqlitePool, business_id: Uuid, id: Uuid, patch: &Value) -> anyhow::Result<Option<Value>> {
    let s = |k: &str| patch[k].as_str().map(str::trim).map(|v| v.chars().take(200).collect::<String>());
    let tags = patch["tags"].as_array().map(|a| json!(a.iter().filter_map(|t| t.as_str()).take(10).collect::<Vec<_>>()).to_string());
    sqlx::query(
        "UPDATE contacts SET name = COALESCE($3, name), email = COALESCE($4, email), phone = COALESCE($5, phone), notes = COALESCE($6, notes), tags = COALESCE($7, tags) \
         WHERE id = $1 AND business_id = $2",
    ).bind(id).bind(business_id).bind(s("name")).bind(s("email").map(|e| e.to_lowercase())).bind(s("phone").map(|p| digits(&p))).bind(s("notes")).bind(tags)
    .execute(db).await?;
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM contacts WHERE id = $1 AND business_id = $2")).bind(id).bind(business_id).fetch_optional(db).await?;
    Ok(row.map(row_json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_split_into_source_and_number() {
        assert_eq!(split_peer("com.whatsapp:+51 999 888 777"), (Some("whatsapp".into()), Some("51999888777".into()), None));
        assert_eq!(split_peer("agent:abcd").0.as_deref(), Some("network"));
        assert!(same_phone("51999888777", "999888777"));
        assert!(!same_phone("51999888777", "51999888778"));
    }

    /// The CRM only exists if a conversation puts rows in it. This went
    /// unnoticed for a long time because the upsert's error is discarded:
    /// every message was answered, counted, billed — and no customer was ever
    /// recorded. Run against the real migrations so the partial index is the
    /// real one.
    #[tokio::test]
    async fn a_conversation_creates_and_counts_a_contact() {
        let db = crate::testkit::db().await;
        let biz = Uuid::new_v4();
        sqlx::query("INSERT INTO businesses (id, name, industry, owner_phone) VALUES ($1,'Tito','barbería','+51999')")
            .bind(biz).execute(&db).await.unwrap();

        touch(&db, biz, "com.whatsapp:Ana").await;
        touch(&db, biz, "com.whatsapp:Ana").await;

        let (n, messages, phone): (i64, i64, Option<String>) = sqlx::query_as(
            "SELECT COUNT(*), MAX(messages), MAX(phone) FROM contacts WHERE business_id = $1 AND peer = $2",
        ).bind(biz).bind("com.whatsapp:Ana").fetch_one(&db).await.unwrap();
        assert_eq!(n, 1, "the same chat is one customer, not one per message");
        assert_eq!(messages, 2, "the second message must count on the same row");
        assert_eq!(phone, None, "a display-name peer carries no number");

        // And the owner's own row, which upserts through the same clause.
        owner(&db, biz, Some("Tito"), Some("tito@example.com"), Some("+51 999 111 222")).await;
        owner(&db, biz, None, None, None).await;
        let (owners,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM contacts WHERE business_id = $1 AND kind = 'owner'")
            .bind(biz).fetch_one(&db).await.unwrap();
        assert_eq!(owners, 1);
    }

    use crate::testkit;

    #[test]
    fn source_names() {
        assert_eq!(source_of("com.instagram.android"), "instagram");
        assert_eq!(source_of("com.facebook.orca"), "messenger");
        assert_eq!(source_of("org.telegram.messenger"), "telegram");
        assert_eq!(source_of("org.thoughtcrime.securesms"), "signal");
        assert_eq!(source_of("com.zhiliaoapp.musically"), "tiktok");
        assert_eq!(source_of("com.google.android.apps.messaging"), "sms");
        assert_eq!(source_of("Com.Unknown.App"), "com.unknown.app");
    }

    #[test]
    fn split_peer_shapes() {
        assert_eq!(split_peer("agent:ff"), (Some("network".into()), None, Some("agent:ff".into())));
        assert_eq!(split_peer("+51 999 888 777"), (None, Some("51999888777".into()), None));
        assert_eq!(split_peer("com.whatsapp:12345"), (Some("whatsapp".into()), None, None), "too short to be a number");
    }

    #[test]
    fn same_phone_edges() {
        assert!(!same_phone("123456", "123456"), "fewer than 7 digits never match");
        assert!(same_phone("1234567", "1234567"));
        assert!(same_phone("5511999888777", "999888777"));
        assert_eq!(digits("+51 (999) 888-777"), "51999888777");
    }

    #[tokio::test]
    async fn learn_fills_without_blanking_and_normalises() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        let peer = "com.whatsapp:+51 999 888 777";
        learn(&db, b, Some(peer), None, Some("  Ana  "), Some(" ANA@Mail.pe ")).await;
        learn(&db, b, Some(peer), None, Some(""), Some("not-an-email")).await;
        let c = by_peer(&db, b, peer).await.unwrap();
        assert_eq!((c["name"].clone(), c["email"].clone(), c["phone"].clone()), (json!("Ana"), json!("ana@mail.pe"), json!("51999888777")));
        // Nothing to learn: not even a touch.
        learn(&db, b, Some("com.whatsapp:Nadie"), Some("12"), None, None).await;
        assert!(by_peer(&db, b, "com.whatsapp:Nadie").await.is_none());
        // Names are bounded.
        learn(&db, b, Some(peer), None, Some(&"x".repeat(200)), None).await;
        assert_eq!(by_peer(&db, b, peer).await.unwrap()["name"].as_str().unwrap().len(), 80);
    }

    #[tokio::test]
    async fn a_payer_is_matched_by_phone_or_becomes_a_contact() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        touch(&db, b, "com.whatsapp:+51 999 888 777").await;
        // Payment names the payer and a phone without the country code.
        learn(&db, b, None, Some("999 888 777"), Some("Ana Rojas"), None).await;
        let all = list(&db, b, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["name"], "Ana Rojas");
        // A payer we never talked to.
        learn(&db, b, None, Some("988 111 222"), Some("Carlos"), None).await;
        let all = list(&db, b, None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|c| c["source"] == "payment" && c["name"] == "Carlos"));
    }

    #[tokio::test]
    async fn list_orders_owner_first_and_searches() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        learn(&db, b, Some("com.whatsapp:1"), None, Some("Rosa"), Some("rosa@x.pe")).await;
        learn(&db, b, Some("com.whatsapp:2"), None, Some("Álvaro Núñez"), None).await;
        owner(&db, b, Some("Tito"), None, Some("+51 999 000 111")).await;
        let all = list(&db, b, None).await.unwrap();
        assert_eq!(all[0]["kind"], "owner");
        assert_eq!(all.len(), 3);
        assert_eq!(list(&db, b, Some("ROSA")).await.unwrap().len(), 1);
        assert_eq!(list(&db, b, Some("x.pe")).await.unwrap().len(), 1);
        assert_eq!(list(&db, b, Some("000111")).await.unwrap().len(), 1);
        assert_eq!(list(&db, b, Some("")).await.unwrap().len(), 3, "an empty query lists everyone");
        // Accented names are found however the owner types them.
        assert_eq!(list(&db, b, Some("álvaro")).await.unwrap().len(), 1, "lowercase accented query");
        assert_eq!(list(&db, b, Some("ÁLVARO")).await.unwrap().len(), 1, "uppercase accented query");
        assert_eq!(list(&db, b, Some("núñez")).await.unwrap().len(), 1);
        // Another business sees none of it.
        let other = testkit::business(&db).await;
        assert!(list(&db, other, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_by_the_owner() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        touch(&db, b, "com.whatsapp:Ana").await;
        let id: Uuid = serde_json::from_value(by_peer(&db, b, "com.whatsapp:Ana").await.unwrap()["id"].clone()).unwrap();
        let c = update(&db, b, id, &json!({"name": " Ana R ", "email": "A@B.PE", "phone": "+51 999", "notes": "vip", "tags": ["vip", 3, "x"]})).await.unwrap().unwrap();
        assert_eq!((c["name"].clone(), c["email"].clone(), c["phone"].clone(), c["notes"].clone(), c["tags"].clone()),
                   (json!("Ana R"), json!("a@b.pe"), json!("51999"), json!("vip"), json!(["vip", "x"])));
        // Fields not in the patch are kept.
        let c = update(&db, b, id, &json!({"notes": "nuevo"})).await.unwrap().unwrap();
        assert_eq!(c["name"], "Ana R");
        // Another business cannot edit it.
        let other = testkit::business(&db).await;
        assert!(update(&db, other, id, &json!({"name": "x"})).await.unwrap().is_none());
        assert_eq!(by_peer(&db, b, "com.whatsapp:Ana").await.unwrap()["name"], "Ana R");
    }

    #[tokio::test]
    async fn set_plan_matches_by_trailing_digits() {
        let db = testkit::db().await;
        let b = testkit::business(&db).await;
        touch(&db, b, "com.whatsapp:+51 999 888 777").await;
        set_plan(&db, b, "999888777", "pro").await;
        assert_eq!(by_peer(&db, b, "com.whatsapp:+51 999 888 777").await.unwrap()["plan"], "pro");
        set_plan(&db, b, "1234", "max").await; // too short: ignored
        assert_eq!(by_peer(&db, b, "com.whatsapp:+51 999 888 777").await.unwrap()["plan"], "pro");
    }
}
