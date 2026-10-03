//! Customers as people rather than as conversations.
//!
//! A `peer` is a chat: `com.instagram.android:panaderia_rosa`. A *person* is
//! the human behind however many chats they open. The difference matters the
//! moment a customer switches app — asks the price on Instagram, sends the
//! address on WhatsApp — because keyed on `peer` alone the agent meets a
//! stranger the second time and asks for everything again.
//!
//! Identity arrives from the phone as `(channel, handle)`: a channel groups
//! apps that share an address space, a handle is the customer's phone digits
//! where the app exposes them and their normalised display name where it does
//! not (see `Channel.kt`). Seeing the same pair twice is, by definition, the
//! same person; linking two *different* pairs needs evidence, and the only
//! evidence that holds is a phone number or an email the customer themselves
//! gave us.
//!
//! **What this deliberately does not do:** guess. Two people called "Ana" on
//! two apps stay two people until something ties them together. A wrong merge
//! shows one customer another customer's order history, which is far worse
//! than asking a returning customer their name again.

use serde_json::{json, Value};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::contacts::{digits, same_phone};

/// Find (or create) the person behind a conversation, and record that this
/// channel/handle is one of their addresses.
///
/// `peer` is stored on the link purely as a back-reference: it remains the key
/// for history, the audit chain and billing, and nothing here rewrites it.
pub async fn resolve(
    db: &SqlitePool,
    business_id: Uuid,
    channel: &str,
    handle: &str,
    display_name: Option<&str>,
    is_phone: bool,
    peer: &str,
) -> Option<Uuid> {
    if channel.is_empty() || handle.is_empty() {
        return None;
    }
    // 1. Known address — the common case, and the only one that needs no proof.
    let existing: Option<(Uuid,)> = sqlx::query_as(
        "SELECT person_id FROM person_links WHERE business_id = $1 AND channel = $2 AND handle = $3",
    )
    .bind(business_id)
    .bind(channel)
    .bind(handle)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if let Some((person_id,)) = existing {
        let _ = sqlx::query(
            "UPDATE person_links SET last_seen = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), \
             display_name = COALESCE($4, display_name), peer = $5 \
             WHERE business_id = $1 AND channel = $2 AND handle = $3",
        )
        .bind(business_id).bind(channel).bind(handle).bind(display_name).bind(peer)
        .execute(db).await;
        touch(db, person_id).await;
        attach(db, business_id, peer, person_id).await;
        return Some(person_id);
    }

    // 2. New address. A phone handle can be matched against people we already
    //    know; a display-name handle cannot, and inventing a match on a name
    //    would be the wrong-merge failure this module exists to avoid.
    let phone = if is_phone {
        Some(digits(handle)).filter(|d| d.len() >= 7)
    } else {
        None
    };
    let person_id = match &phone {
        Some(d) => by_phone(db, business_id, d).await,
        None => None,
    };

    let person_id = match person_id {
        Some(id) => id,
        None => {
            let id = Uuid::new_v4();
            let name = display_name.filter(|s| !s.trim().is_empty() && !is_phone);
            let _ = sqlx::query(
                "INSERT INTO people (id, business_id, name, phone) VALUES ($1,$2,$3,$4)",
            )
            .bind(id).bind(business_id).bind(name).bind(&phone)
            .execute(db).await;
            id
        }
    };

    let _ = sqlx::query(
        "INSERT INTO person_links (business_id, channel, handle, person_id, display_name, peer, is_phone) \
         VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (business_id, channel, handle) DO UPDATE SET \
         last_seen = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), peer = excluded.peer",
    )
    .bind(business_id).bind(channel).bind(handle).bind(person_id)
    .bind(display_name).bind(peer).bind(if is_phone { 1 } else { 0 })
    .execute(db).await;

    attach(db, business_id, peer, person_id).await;
    Some(person_id)
}

/// Something taught us a phone, an email or a name for this person — a booking,
/// an order, the agent asking. This is where cross-app identity actually gets
/// made: an email given on Instagram that matches one given on WhatsApp is the
/// proof that merges the two.
pub async fn learn(
    db: &SqlitePool,
    business_id: Uuid,
    person_id: Uuid,
    phone: Option<&str>,
    name: Option<&str>,
    email: Option<&str>,
) {
    let name = name.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(80).collect::<String>());
    let email = email.map(|e| e.trim().to_lowercase()).filter(|e| e.contains('@'));
    let phone_d = phone.map(digits).filter(|d| d.len() >= 7);
    if name.is_none() && email.is_none() && phone_d.is_none() {
        return;
    }

    // A newly learned phone or email may already belong to someone we know.
    // Either one is proof: a new phone arriving with a known email still
    // names the person that email belongs to.
    let twin = match &phone_d {
        Some(d) => by_phone(db, business_id, d).await.filter(|t| *t != person_id),
        None => None,
    };
    let twin = match (twin, &email) {
        (Some(t), _) => Some(t),
        (None, Some(e)) => by_email(db, business_id, e).await,
        (None, None) => None,
    };
    let target = match twin {
        Some(other) if other != person_id => {
            merge(db, business_id, other, person_id).await;
            other
        }
        _ => person_id,
    };

    let _ = sqlx::query(
        "UPDATE people SET name = COALESCE($2, name), email = COALESCE($3, email), \
         phone = COALESCE(phone, $4), last_seen = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id = $1",
    )
    .bind(target).bind(&name).bind(&email).bind(&phone_d)
    .execute(db).await;
}

/// The person behind a conversation, if the identity layer knows one.
///
/// `person_links` is asked first because it is the identity layer's own table:
/// [`resolve`] writes the link at the start of every turn and [`merge`] moves
/// it, so it is correct even in the middle of the turn that merges two people.
/// `contacts.person_id` is the same fact copied into the CRM by [`attach`],
/// which is one step behind — the CRM row for a brand new customer does not
/// exist yet when their first message is being answered.
pub async fn for_peer(db: &SqlitePool, business_id: Uuid, peer: &str) -> Option<Uuid> {
    let linked: Option<(Uuid,)> = sqlx::query_as(
        "SELECT person_id FROM person_links WHERE business_id = $1 AND peer = $2 LIMIT 1",
    )
    .bind(business_id)
    .bind(peer)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if let Some((person_id,)) = linked {
        return Some(person_id);
    }
    let row: Option<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT person_id FROM contacts WHERE business_id = $1 AND peer = $2 LIMIT 1",
    )
    .bind(business_id)
    .bind(peer)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    row.and_then(|r| r.0)
}

/// The person behind a CRM row. A payment has no conversation to go by: the
/// payer was matched by their phone number, and this is the only handle on who
/// they are.
pub async fn for_contact(db: &SqlitePool, business_id: Uuid, contact_id: Uuid) -> Option<Uuid> {
    let row: Option<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT person_id FROM contacts WHERE business_id = $1 AND id = $2",
    )
    .bind(business_id)
    .bind(contact_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    row.and_then(|r| r.0)
}

/// Everything the agent should know about who it is talking to, including the
/// other apps they have reached us on. This is the per-customer memory: it is
/// what lets the agent say "same order as last time?" to someone who has only
/// ever messaged this particular app once.
pub async fn profile(db: &SqlitePool, business_id: Uuid, person_id: Uuid) -> Option<Value> {
    let p: (Option<String>, Option<String>, Option<String>, String, String) = sqlx::query_as(
        "SELECT name, phone, email, first_seen, last_seen FROM people WHERE id = $1 AND business_id = $2",
    )
    .bind(person_id).bind(business_id)
    .fetch_optional(db).await.ok().flatten()?;

    let links: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT channel, handle, display_name, peer FROM person_links \
         WHERE business_id = $1 AND person_id = $2 ORDER BY last_seen DESC",
    )
    .bind(business_id).bind(person_id)
    .fetch_all(db).await.unwrap_or_default();

    Some(json!({
        "personId": person_id,
        "name": p.0,
        "phone": p.1,
        "email": p.2,
        "firstSeen": p.3,
        "lastSeen": p.4,
        "channels": links.iter().map(|(c, h, d, peer)| json!({
            "channel": c, "handle": h, "displayName": d, "peer": peer
        })).collect::<Vec<_>>(),
        // True once they have reached us on more than one app: the agent should
        // not act surprised to hear from them somewhere new.
        "knownElsewhere": links.len() > 1,
    }))
}

/// A prompt note about who the agent is talking to — the point of all of this.
///
/// Returns `None` for a first-time stranger: there is nothing to say, and an
/// empty section would only spend context. It says nothing about *what* the
/// customer bought or asked before either; the stored conversation log already
/// carries that per chat. What it adds is the one thing the log cannot know —
/// that this chat and another chat are the same human — so the agent stops
/// introducing itself to a customer it dealt with yesterday on another app.
pub async fn note(db: &SqlitePool, business_id: Uuid, peer: &str) -> Option<String> {
    let person_id = for_peer(db, business_id, peer).await?;
    let p = profile(db, business_id, person_id).await?;

    // Every other app this person has reached us on, this chat excluded.
    let others: Vec<String> = p["channels"]
        .as_array()?
        .iter()
        .filter(|c| c["peer"].as_str() != Some(peer))
        .filter_map(|c| c["channel"].as_str().map(str::to_string))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let known: Vec<String> = [
        p["name"].as_str().map(|v| format!("name {v}")),
        p["phone"].as_str().map(|v| format!("phone {v}")),
        p["email"].as_str().map(|v| format!("email {v}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if others.is_empty() && known.is_empty() {
        return None;
    }

    let mut s = String::from("WHO YOU ARE TALKING TO — ");
    if !others.is_empty() {
        s.push_str(&format!(
            "this is the same person who has already written to this business on {}. \
             Do not greet them as a stranger. ",
            others.join(", ")
        ));
    }
    if !known.is_empty() {
        s.push_str(&format!(
            "You already know their {} — never ask for those again; confirm at most. ",
            known.join(", ")
        ));
    }
    s.push_str(
        "Say nothing about which app they used before unless they raise it — a business \
         that has been paying attention is reassuring, one that announces it is watching is not.",
    );
    Some(s)
}

/// Fold `loser` into `keeper`: their addresses, then their learned fields.
/// Never run on a guess — only on a matching phone or email.
async fn merge(db: &SqlitePool, business_id: Uuid, keeper: Uuid, loser: Uuid) {
    if keeper == loser {
        return;
    }
    // A link is unique per (business, channel, handle), so a collision means
    // both people already held the same address — keep the keeper's and drop
    // the duplicate rather than failing the whole merge.
    let _ = sqlx::query(
        "UPDATE OR IGNORE person_links SET person_id = $1 WHERE business_id = $3 AND person_id = $2",
    )
    .bind(keeper).bind(loser).bind(business_id)
    .execute(db).await;
    let _ = sqlx::query("DELETE FROM person_links WHERE business_id = $2 AND person_id = $1")
        .bind(loser).bind(business_id).execute(db).await;
    let _ = sqlx::query("UPDATE contacts SET person_id = $1 WHERE business_id = $3 AND person_id = $2")
        .bind(keeper).bind(loser).bind(business_id).execute(db).await;
    // Fill the keeper's blanks from the loser before it goes.
    let _ = sqlx::query(
        "UPDATE people SET \
           name  = COALESCE((SELECT name  FROM people WHERE id = $1), (SELECT name  FROM people WHERE id = $2)), \
           phone = COALESCE((SELECT phone FROM people WHERE id = $1), (SELECT phone FROM people WHERE id = $2)), \
           email = COALESCE((SELECT email FROM people WHERE id = $1), (SELECT email FROM people WHERE id = $2)) \
         WHERE id = $1",
    )
    .bind(keeper).bind(loser).execute(db).await;
    let _ = sqlx::query("DELETE FROM people WHERE id = $1 AND business_id = $2")
        .bind(loser).bind(business_id).execute(db).await;
}

/// Phone match is fuzzy on purpose — the same number is written +51 999…,
/// 51999…, and 999… by three different apps.
async fn by_phone(db: &SqlitePool, business_id: Uuid, d: &str) -> Option<Uuid> {
    let rows: Vec<(Uuid, Option<String>)> =
        sqlx::query_as("SELECT id, phone FROM people WHERE business_id = $1 AND phone IS NOT NULL")
            .bind(business_id)
            .fetch_all(db)
            .await
            .unwrap_or_default();
    rows.into_iter()
        .find(|(_, p)| p.as_deref().is_some_and(|p| same_phone(p, d)))
        .map(|(id, _)| id)
}

async fn by_email(db: &SqlitePool, business_id: Uuid, e: &str) -> Option<Uuid> {
    sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM people WHERE business_id = $1 AND lower(email) = $2 LIMIT 1",
    )
    .bind(business_id)
    .bind(e)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| r.0)
}

async fn touch(db: &SqlitePool, person_id: Uuid) {
    let _ = sqlx::query(
        "UPDATE people SET last_seen = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id = $1",
    )
    .bind(person_id)
    .execute(db)
    .await;
}

/// Point the CRM row for this conversation at the person it belongs to.
pub async fn attach(db: &SqlitePool, business_id: Uuid, peer: &str, person_id: Uuid) {
    // The caller resolved this id before the turn ran, and the turn may have
    // merged that person into another and deleted them — a customer giving
    // their phone number mid-conversation does exactly that. The link moved
    // with the merge, so it is the current answer and the passed id only a
    // fallback; writing the stale one would point the CRM at a row that no
    // longer exists.
    let person_id = for_peer(db, business_id, peer).await.unwrap_or(person_id);
    let _ = sqlx::query(
        "UPDATE contacts SET person_id = $1 WHERE business_id = $2 AND peer = $3 AND person_id IS NULL",
    )
    .bind(person_id)
    .bind(business_id)
    .bind(peer)
    .execute(db)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real schema, migrations and all — so these tests fail if 021 ever
    /// stops applying, which a hand-rolled table would quietly hide.
    async fn db() -> (SqlitePool, Uuid) {
        let db = crate::testkit::db().await;
        let biz = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO businesses (id, name, industry, owner_phone) VALUES ($1, 'Tito', 'barbería', '+51999')",
        )
        .bind(biz)
        .execute(&db)
        .await
        .unwrap();
        (db, biz)
    }

    /// The whole point: two apps, one phone number, one customer.
    #[tokio::test]
    async fn same_number_on_two_apps_is_one_person() {
        let (db, biz) = db().await;

        let a = resolve(&db, biz, "whatsapp", "51999888777", Some("Ana"), true, "com.whatsapp:Ana").await.unwrap();
        // Same number as written by another app, without the country code.
        let b = resolve(&db, biz, "telegram", "999888777", Some("Ana"), true, "org.telegram.messenger:Ana").await.unwrap();
        assert_eq!(a, b, "the same phone on two channels must resolve to one person");

        let p = profile(&db, biz, a).await.unwrap();
        assert_eq!(p["channels"].as_array().unwrap().len(), 2);
        assert_eq!(p["knownElsewhere"], json!(true));
    }

    /// Two display names we have no evidence about stay apart — a wrong merge
    /// would show one customer another's history.
    #[tokio::test]
    async fn unrelated_handles_stay_separate() {
        let (db, biz) = db().await;

        let a = resolve(&db, biz, "instagram", "ana", Some("Ana"), false, "com.instagram.android:Ana").await.unwrap();
        let b = resolve(&db, biz, "messenger", "ana", Some("Ana"), false, "com.facebook.orca:Ana").await.unwrap();
        assert_ne!(a, b, "identical display names are not evidence of the same person");
    }

    /// …until the customer gives the same email on both, which is evidence.
    #[tokio::test]
    async fn a_shared_email_merges_them() {
        let (db, biz) = db().await;

        let a = resolve(&db, biz, "instagram", "ana", Some("Ana"), false, "ig:Ana").await.unwrap();
        let b = resolve(&db, biz, "messenger", "anita", Some("Anita"), false, "fb:Anita").await.unwrap();
        learn(&db, biz, a, None, None, Some("ana@example.com")).await;
        learn(&db, biz, b, None, None, Some("ana@example.com")).await;

        let links: Vec<(Uuid,)> = sqlx::query_as("SELECT person_id FROM person_links WHERE business_id = $1")
            .bind(biz).fetch_all(&db).await.unwrap();
        let distinct: std::collections::HashSet<_> = links.iter().map(|r| r.0).collect();
        assert_eq!(distinct.len(), 1, "a shared email must collapse them to one person");
    }

    /// The wiring that makes all of this reach a real customer: the agent takes
    /// a phone number for a booking, and the stranger on Instagram turns out to
    /// be the woman we already know from WhatsApp. `contacts::learn` is the one
    /// funnel every such fact goes through — a booking, an order, a payment.
    #[tokio::test]
    async fn a_phone_taken_for_a_booking_merges_the_two_apps() {
        let (db, biz) = db().await;
        let ig_peer = "com.instagram.android:Anita";

        let wa = resolve(&db, biz, "whatsapp", "51999888777", Some("Ana"), true, "com.whatsapp:Ana").await.unwrap();
        let ig = resolve(&db, biz, "instagram", "anita", Some("Anita"), false, ig_peer).await.unwrap();
        assert_ne!(wa, ig, "a display name alone is not evidence");

        // Written as a human would write it, on the other app.
        crate::contacts::learn(&db, biz, Some(ig_peer), Some("+51 999 888 777"), Some("Ana"), None).await;

        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM people WHERE business_id = $1")
            .bind(biz).fetch_one(&db).await.unwrap();
        assert_eq!(n, 1, "the number she gave is proof the two chats are one person");

        let note = note(&db, biz, ig_peer).await.unwrap();
        assert!(note.contains("whatsapp"), "the agent must be told where else she has written: {note}");
    }

    /// The turn resolves the person *before* it runs and attaches after, so a
    /// merge in between leaves the caller holding an id that has been deleted.
    /// Attaching it would point the CRM at nothing and the agent would forget
    /// her again on the very next message.
    #[tokio::test]
    async fn attach_after_a_merge_points_at_the_survivor() {
        let (db, biz) = db().await;
        let ig_peer = "com.instagram.android:Anita";

        let wa = resolve(&db, biz, "whatsapp", "51999888777", Some("Ana"), true, "com.whatsapp:Ana").await.unwrap();
        let ig = resolve(&db, biz, "instagram", "anita", Some("Anita"), false, ig_peer).await.unwrap();
        crate::contacts::learn(&db, biz, Some(ig_peer), Some("51999888777"), None, None).await;

        // What the route does at the end of the turn, with its stale id.
        attach(&db, biz, ig_peer, ig).await;

        let (person,): (Option<Uuid>,) = sqlx::query_as("SELECT person_id FROM contacts WHERE business_id = $1 AND peer = $2")
            .bind(biz).bind(ig_peer).fetch_one(&db).await.unwrap();
        assert_eq!(person, Some(wa), "the CRM row must follow the merge, not the id the turn started with");
        assert!(profile(&db, biz, person.unwrap()).await.is_some(), "and that person must still exist");
    }

    /// Re-resolving a known address must not create a second link or person.
    #[tokio::test]
    async fn resolving_twice_is_idempotent() {
        let (db, biz) = db().await;

        let a = resolve(&db, biz, "whatsapp", "51999888777", Some("Ana"), true, "w:Ana").await.unwrap();
        let b = resolve(&db, biz, "whatsapp", "51999888777", Some("Ana"), true, "w:Ana").await.unwrap();
        assert_eq!(a, b);
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM person_links WHERE business_id = $1")
            .bind(biz).fetch_one(&db).await.unwrap();
        assert_eq!(n, 1);
    }

    async fn people_count(db: &SqlitePool, biz: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM people WHERE business_id = $1").bind(biz).fetch_one(db).await.unwrap()
    }

    #[tokio::test]
    async fn empty_channel_or_handle_resolves_nobody() {
        let (db, biz) = db().await;
        assert_eq!(resolve(&db, biz, "", "x", None, false, "p").await, None);
        assert_eq!(resolve(&db, biz, "wa", "", None, false, "p").await, None);
        assert_eq!(people_count(&db, biz).await, 0);
    }

    #[tokio::test]
    async fn a_new_email_merges_even_when_a_new_phone_comes_with_it() {
        // Ana gave her email on Instagram. On WhatsApp she books with a phone
        // we have never seen AND the same email: the email is the proof.
        let (db, biz) = db().await;
        let ig = resolve(&db, biz, "instagram", "ana.r", Some("Ana"), false, "ig:ana").await.unwrap();
        learn(&db, biz, ig, None, None, Some("ana@mail.pe")).await;
        let wa = resolve(&db, biz, "whatsapp", "Ana R", Some("Ana R"), false, "wa:ana").await.unwrap();
        assert_ne!(ig, wa);
        learn(&db, biz, wa, Some("+51 977 000 111"), None, Some("ANA@mail.pe")).await;
        assert_eq!(people_count(&db, biz).await, 1);
        let p = profile(&db, biz, for_peer(&db, biz, "wa:ana").await.unwrap()).await.unwrap();
        assert_eq!((p["email"].clone(), p["phone"].clone()), (json!("ana@mail.pe"), json!("51977000111")));
    }

    #[tokio::test]
    async fn learn_ignores_empty_evidence_and_bounds_names() {
        let (db, biz) = db().await;
        let id = resolve(&db, biz, "wa", "h", None, false, "p").await.unwrap();
        learn(&db, biz, id, Some("12"), Some("  "), Some("nope")).await;
        let p = profile(&db, biz, id).await.unwrap();
        assert!(p["name"].is_null() && p["phone"].is_null() && p["email"].is_null());
        learn(&db, biz, id, None, Some(&"n".repeat(300)), None).await;
        assert_eq!(profile(&db, biz, id).await.unwrap()["name"].as_str().unwrap().len(), 80);
        // Learning your own phone twice is not a merge with yourself.
        learn(&db, biz, id, Some("999888777"), None, None).await;
        learn(&db, biz, id, Some("51999888777"), None, None).await;
        assert_eq!(people_count(&db, biz).await, 1);
    }

    #[tokio::test]
    async fn phone_handles_are_not_used_as_names() {
        let (db, biz) = db().await;
        let id = resolve(&db, biz, "whatsapp", "+51 999 888 777", Some("+51 999 888 777"), true, "wa:1").await.unwrap();
        let p = profile(&db, biz, id).await.unwrap();
        assert!(p["name"].is_null());
        assert_eq!(p["phone"], "51999888777");
        // A short phone-ish handle carries no phone.
        let id = resolve(&db, biz, "sms", "123", None, true, "sms:1").await.unwrap();
        assert!(profile(&db, biz, id).await.unwrap()["phone"].is_null());
    }

    #[tokio::test]
    async fn display_name_updates_but_never_blanks() {
        let (db, biz) = db().await;
        resolve(&db, biz, "ig", "h", Some("Old"), false, "ig:h").await;
        resolve(&db, biz, "ig", "h", None, false, "ig:h").await;
        resolve(&db, biz, "ig", "h", Some("New"), false, "ig:h2").await;
        let (d, peer): (Option<String>, String) = sqlx::query_as("SELECT display_name, peer FROM person_links").fetch_one(&db).await.unwrap();
        assert_eq!((d.as_deref(), peer.as_str()), (Some("New"), "ig:h2"));
    }

    #[tokio::test]
    async fn note_is_silent_for_strangers_and_specific_for_regulars() {
        let (db, biz) = db().await;
        assert_eq!(note(&db, biz, "wa:nobody").await, None);
        resolve(&db, biz, "whatsapp", "Rosa", Some("Rosa"), false, "wa:rosa").await;
        // The chat's display name is taken as their name (pinned behaviour).
        assert!(note(&db, biz, "wa:rosa").await.unwrap().contains("name Rosa"));
        let id = for_peer(&db, biz, "wa:rosa").await.unwrap();
        learn(&db, biz, id, None, Some("Rosa Q"), Some("rosa@x.pe")).await;
        let n = note(&db, biz, "wa:rosa").await.unwrap();
        assert!(n.contains("name Rosa Q") && n.contains("email rosa@x.pe") && !n.contains("Do not greet them as a stranger"));
    }

    #[tokio::test]
    async fn for_contact_and_for_peer_fallback() {
        let (db, biz) = db().await;
        assert_eq!(for_contact(&db, biz, Uuid::new_v4()).await, None);
        crate::contacts::touch(&db, biz, "wa:x").await;
        let (cid,): (Uuid,) = sqlx::query_as("SELECT id FROM contacts WHERE peer = 'wa:x'").fetch_one(&db).await.unwrap();
        assert_eq!(for_contact(&db, biz, cid).await, None, "no person yet");
        let pid = resolve(&db, biz, "wa", "x", None, false, "wa:x").await.unwrap();
        assert_eq!(for_contact(&db, biz, cid).await, Some(pid));
        // Without a link row, the CRM's copy answers.
        sqlx::query("DELETE FROM person_links").execute(&db).await.unwrap();
        assert_eq!(for_peer(&db, biz, "wa:x").await, Some(pid));
    }

    #[tokio::test]
    async fn people_never_cross_businesses() {
        let (db, biz) = db().await;
        let other = crate::testkit::business(&db).await;
        let a = resolve(&db, biz, "whatsapp", "51999888777", None, true, "wa:1").await.unwrap();
        let b = resolve(&db, other, "whatsapp", "51999888777", None, true, "wa:1").await.unwrap();
        assert_ne!(a, b, "the same customer of two businesses is two records");
        assert!(profile(&db, other, a).await.is_none());
        learn(&db, other, b, None, None, Some("x@y.z")).await;
        assert!(profile(&db, biz, a).await.unwrap()["email"].is_null());
    }

    #[tokio::test]
    async fn merge_fills_blanks_and_drops_duplicate_links() {
        let (db, biz) = db().await;
        let keeper = resolve(&db, biz, "wa", "k", None, false, "wa:k").await.unwrap();
        let loser = resolve(&db, biz, "ig", "l", Some("Luz"), false, "ig:l").await.unwrap();
        learn(&db, biz, loser, None, None, Some("luz@x.pe")).await;
        merge(&db, biz, keeper, loser).await;
        merge(&db, biz, keeper, keeper).await; // no-op
        let p = profile(&db, biz, keeper).await.unwrap();
        assert_eq!((p["name"].clone(), p["email"].clone()), (json!("Luz"), json!("luz@x.pe")));
        assert_eq!(p["channels"].as_array().unwrap().len(), 2);
        assert!(profile(&db, biz, loser).await.is_none());
    }
}
