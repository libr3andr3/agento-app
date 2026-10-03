//! The network, from the person's side (`Scope::Assistant`, client mode only).
//!
//! The orchestrator finds businesses on the yaya network, reads their offer,
//! reputation and free slots, talks to them through the E2E relay (where the
//! business's own agent books, quotes and takes orders), keeps the resulting
//! bookings, and rates them. Replies arrive through the client-mode inbox
//! loop (`network::inbox_loop`) and are correlated back to the waiting
//! `ask_business` call by sender and `inReplyTo` — see `network::Asks`.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};
use crate::network;

pub struct Market;

fn spec(name: &str, desc: &str, params: Value) -> Value {
    json!({"type": "function",
           "function": {"name": name, "description": desc, "parameters": params}})
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

/// Resolve "@handle" / "urn:agent:yaya:x" / "agent:hex" / a plain handle
/// to an agent id, via the registry index when needed.
async fn resolve(state: &crate::AppState, who: &str) -> anyhow::Result<(String, Option<String>, Option<String>)> {
    let who = who.trim();
    if who.starts_with("agent:") {
        // Name is nice-to-have; don't fail the call on a missing card.
        let facts = network::registry_get(state, &format!("/v1/agents/{who}/facts")).await.ok();
        return Ok((
            who.to_string(),
            facts.as_ref().and_then(|f| s(&f["agent_name"]).map(|n| n.trim_start_matches("urn:agent:yaya:").to_string())),
            facts.as_ref().and_then(|f| s(&f["label"])),
        ));
    }
    // The model often passes the display name ("Barberia Yaya") instead of
    // the handle. A raw name in a signed path breaks the request signature
    // (reqwest percent-encodes the URL, the signature covered the raw
    // string), so only a slug ever reaches the index; if the slug is not a
    // handle, fall back to a search and take the best hit.
    let handle = slugify(who.trim_start_matches("urn:agent:yaya:").trim_start_matches('@'));
    if handle.is_empty() {
        anyhow::bail!("unknown business '{who}'");
    }
    let by_handle = network::registry_get(state, &format!("/v1/index/urn:agent:yaya:{handle}")).await.ok()
        .and_then(|rec| s(&rec["agent_id"]));
    let (id, handle) = match by_handle {
        Some(id) => (id, handle),
        None => {
            let hits = network::find(state, who, None, None, None, None, 3).await?;
            let hit = hits["matches"].as_array().and_then(|m| m.first()).cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown business '{who}'"))?;
            let id = s(&hit["agent"]).ok_or_else(|| anyhow::anyhow!("unknown business '{who}'"))?;
            (id, s(&hit["handle"]).unwrap_or(handle))
        }
    };
    let facts = network::registry_get(state, &format!("/v1/agents/{id}/facts")).await.ok();
    Ok((id, Some(handle), facts.as_ref().and_then(|f| s(&f["label"]))))
}

/// "Barbería Yaya " → "barberia-yaya": lowercase ASCII letters, digits and
/// single dashes — the only characters a handle can carry, so the value is
/// safe inside a signed request path.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.trim().chars() {
        let mapped = match c.to_lowercase().next().unwrap_or(c) {
            'á' | 'à' | 'ä' | 'â' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o', 'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', 'ç' => 'c',
            c => c,
        };
        if mapped.is_ascii_alphanumeric() {
            out.push(mapped);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    out
}

#[cfg(test)]
mod slug_tests {
    use super::slugify;

    #[test]
    fn names_become_handles() {
        assert_eq!(slugify("Barbería Yaya"), "barberia-yaya");
        assert_eq!(slugify("  @Peluquería  Tito! "), "peluqueria-tito");
        assert_eq!(slugify("barberia-yaya"), "barberia-yaya");
        assert_eq!(slugify("Salón de Carla / Miraflores"), "salon-de-carla-miraflores");
        assert_eq!(slugify("###"), "");
    }

    #[test]
    fn slugs_never_need_url_encoding() {
        for s in ["Barbería Yaya", "a b	c", "ñandú & co", "x y z"] {
            assert!(slugify(s).chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{s}");
        }
    }
}

// ---------------------------------------------------------------- client

/// Trim a match hit to what the model needs to present it.
fn summarize_hit(h: &Value) -> Value {
    let o = &h["offer"];
    json!({
        "business": h["handle"].as_str().map(|x| format!("@{x}")).unwrap_or_else(|| h["agent"].as_str().unwrap_or("").into()),
        "name": h["name"], "industry": h["industry"],
        "location": o["location"], "distanceKm": h["distanceKm"],
        "rating": h["reputation"]["rating"], "reviews": h["reputation"]["reviews"],
        "hardwareAttested": h["hardwareVerified"],
        "askPriceMinor": h["askPrice"], "paidQuestions": h["askPrice"].as_i64().unwrap_or(0) > 0,
        "services": o["services"], "products": o["products"], "productCount": o["productCount"],
        "currency": o["currency"], "paymentRails": o["paymentRails"], "bookingDeposit": o["bookingDeposit"],
        "hours": o["hours"], "nextFreeSlots": o["nextSlots"], "delivery": o["delivery"]["zones"],
        "why": h["why"],
    })
}

async fn find_businesses(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let q = s(&args["query"]).ok_or_else(|| anyhow::anyhow!("missing 'query'"))?;
    let country = s(&args["country"]).or_else(|| s(&ctx.values["country"]));
    // The person's own position (from the app's location capture) rides
    // along so distance can rank — it is never published anywhere.
    let geo = match (ctx.values["geo"]["lat"].as_f64(), ctx.values["geo"]["lng"].as_f64()) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    };
    let city = s(&args["city"]).or_else(|| s(&ctx.values["location"]));
    let res = network::find(
        ctx.state, &q, country.as_deref(), city.as_deref(),
        s(&args["industry"]).as_deref(), geo, 6,
    ).await;
    match res {
        Ok(v) => {
            let hits: Vec<Value> = v["matches"].as_array().map(|a| a.iter().map(summarize_hit).collect()).unwrap_or_default();
            Ok(json!({"matches": hits, "count": hits.len(),
                      "note": if hits.is_empty() { "no business on the network matches yet — say so plainly and suggest how else to find one" } else { "present 2-3; slots are business-local times" }}))
        }
        Err(e) => Ok(json!({"error": format!("network unreachable: {e}")})),
    }
}

async fn business_details(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let who = s(&args["business"]).ok_or_else(|| anyhow::anyhow!("missing 'business'"))?;
    let (id, handle, name) = resolve(ctx.state, &who).await?;
    let card = network::registry_get(ctx.state, &format!("/v1/agents/{id}")).await.unwrap_or(Value::Null);
    let rep = network::reputation(ctx.state, &id).await.unwrap_or(Value::Null);
    let p = &card["payload"];
    Ok(json!({
        "business": handle.map(|h| format!("@{h}")).unwrap_or(id.clone()),
        "name": name.or_else(|| s(&p["name"])),
        "description": p["description"],
        "offer": p["offer"],
        "askPriceMinor": p["offer"]["askPrice"], "paidQuestions": p["offer"]["askPrice"].as_i64().unwrap_or(0) > 0,
        "reputation": {
            "rating": rep["rating"], "reviews": rep["reviews"], "tags": rep["tags"],
            "hardwareAttested": rep["hardwareVerified"], "memberSince": rep["memberSince"],
            "recent": rep["recent"],
        },
    }))
}

/// Talk to a business through its agent; wait for the reply inline.
async fn ask_business(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let who = s(&args["business"]).ok_or_else(|| anyhow::anyhow!("missing 'business'"))?;
    let message = s(&args["message"]).ok_or_else(|| anyhow::anyhow!("missing 'message'"))?;
    let state = ctx.state;
    let (id, handle, name) = resolve(state, &who).await?;
    if id == state.identity.id() {
        return Ok(json!({"error": "that is you"}));
    }
    let live = state.asks.is_live();
    // Anything that arrived earlier (late replies) is surfaced first. With
    // the inbox loop running the loop already stashed those as unread.
    let mut extra: Vec<Value> = Vec::new();
    if !live {
        for (_, from, v) in network::poll_inbox(state, 0).await.unwrap_or_default() {
            if from != id {
                let t = s(&v["text"]).unwrap_or_default();
                stash_unread(state, &from, &t).await;
                extra.push(json!({"from": from, "text": t}));
            } else {
                extra.push(json!({"from": from, "text": v["text"], "earlier": true}));
            }
        }
    }
    // Paid consultations: the card says the price; the person must have
    // agreed (pay=true) before a céntimo moves.
    let price = network::registry_get(state, &format!("/v1/agents/{id}")).await
        .map(|c| network::ask_price_value(&c["payload"]["offer"]))
        .unwrap_or(0);
    // Consent is to a price, not to "whatever it costs now": a business may
    // change its price between the quote and the person's yes.
    let agreed = args["agreed_price_minor"].as_i64().unwrap_or(0);
    let pay = args["pay"].as_bool().unwrap_or(false) && price <= agreed;
    if price > 0 && !pay {
        return Ok(json!({"status": "payment_required", "business": who, "priceMinor": price,
            "price": format!("S/ {:.2}", price as f64 / 100.0),
            "note": "this business's agent charges per question. Tell the person the price; if they agree, call ask_business again with pay=true and agreed_price_minor set to this priceMinor — the amount is taken from their Yaya balance (wallet_status shows it). Never pay without their yes; if the price changed, ask again.",
            "other": extra}));
    }
    // Register before sending: a fast business must not answer into a void.
    let mut rx = if live { Some(state.asks.register(&id)) } else { None };
    let sent = if price > 0 {
        network::send_paid(state, &id, &json!({"text": message}), price).await
    } else {
        network::send(state, &id, &json!({"text": message})).await
    };
    let sent_id = match sent {
        Ok(v) => v,
        Err(e) => {
            if live { state.asks.unregister(&id); }
            let m = e.to_string();
            if m.contains("402") {
                return Ok(json!({"status": "insufficient_balance", "business": who, "priceMinor": price, "detail": m,
                    "note": "the person's Yaya balance does not cover this question. Say the price and that a recarga (Yape/Plin, from S/ 20 at agente.ceo/app) lands in about a minute.",
                    "other": extra}));
            }
            return Err(e);
        }
    };
    sqlx::query(
        "INSERT INTO network_threads (agent, handle, name, last_text) VALUES ($1,$2,$3,$4) \
         ON CONFLICT (agent) DO UPDATE SET handle = COALESCE(excluded.handle, network_threads.handle), \
           name = COALESCE(excluded.name, network_threads.name), last_text = excluded.last_text, \
           updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')",
    )
    .bind(&id).bind(&handle).bind(&name).bind(&message)
    .execute(&state.db).await?;

    // The business agent runs an LLM turn on its phone: typically 5-20 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ASK_TIMEOUT_SECS);
    let mut reply: Option<Value> = None;
    match rx.as_mut() {
        Some(rx) => {
            while reply.is_none() {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() { break; }
                match tokio::time::timeout(left, rx.recv()).await {
                    Ok(Some(v)) => {
                        if correlates(&v, &sent_id) {
                            reply = Some(v);
                        } else {
                            // The answer to an earlier, abandoned question.
                            extra.push(json!({"from": id, "text": v["text"], "earlier": true}));
                        }
                    }
                    _ => break,
                }
            }
            state.asks.unregister(&id);
        }
        None => {
            while std::time::Instant::now() < deadline && reply.is_none() {
                let left = deadline.saturating_duration_since(std::time::Instant::now()).as_secs().min(25).max(1);
                for (_, from, v) in network::poll_inbox(state, left).await.unwrap_or_default() {
                    if from == id {
                        reply = Some(v);
                    } else {
                        let t = s(&v["text"]).unwrap_or_default();
                        stash_unread(state, &from, &t).await;
                        extra.push(json!({"from": from, "text": t}));
                    }
                }
            }
        }
    }
    let Some(r) = reply else {
        return Ok(json!({"status": "no_reply_yet", "business": who,
            "note": format!("the business's phone did not answer within {ASK_TIMEOUT_SECS} s (offline or busy). Tell the person you'll check again; my_bookings shows any reply that arrives later."),
            "other": extra}));
    };
    let text = s(&r["text"]).unwrap_or_default();
    let action = s(&r["action"]);
    let data = &r["actionData"];
    if action.as_deref() == Some("payment_required") {
        let p = data["priceMinor"].as_i64().unwrap_or(price);
        return Ok(json!({"status": "payment_required", "business": who, "reply": text, "priceMinor": p,
            "price": format!("S/ {:.2}", p as f64 / 100.0),
            "note": "the business answered with its price instead of an answer. Ask the person; with their yes call ask_business again with pay=true.",
            "other": extra}));
    }
    let booking = record_outcome(&state.db, &id, name.as_deref(), action.as_deref(), data).await;
    // Words are not bookings. A business agent can say "¡listo, reservado!"
    // without having registered anything; only its structured action counts.
    let registered = !booking.is_null();
    Ok(json!({
        "status": "replied", "business": who, "reply": text,
        "action": action, "actionData": data, "booking": booking,
        "registered": registered,
        "note": if registered { "the business REGISTERED this — it is real; relay the details" }
                else { "NOTHING was registered in this exchange (no booking/order action). If the person asked to book or order, the business's wording does not count: tell the person it is not yet registered and ask the business again, explicitly, to register it." },
        "other": extra,
    }))
}

/// How long one `ask_business` call waits for the business's phone.
const ASK_TIMEOUT_SECS: u64 = if cfg!(test) { 1 } else { 60 };

/// Does `reply` answer the message we just sent? Businesses echo the relay
/// id they received as `inReplyTo`; a reply without one is taken as ours
/// (older cores), one with a different id belongs to an earlier question.
fn correlates(reply: &Value, sent_id: &str) -> bool {
    match reply["inReplyTo"].as_str() {
        Some(r) if !sent_id.is_empty() => r == sent_id,
        _ => true,
    }
}

/// A business answered after the asking tool stopped waiting (or the app
/// was closed): keep the text for `my_bookings` and still honour any
/// booking/order it registered — words are cheap, actions are the ledger.
pub(crate) async fn late_reply(state: &crate::AppState, from: &str, v: &Value) {
    let text = s(&v["text"]).unwrap_or_default();
    stash_unread(state, from, &text).await;
    let name: Option<(Option<String>,)> = sqlx::query_as("SELECT name FROM network_threads WHERE agent = $1")
        .bind(from).fetch_optional(&state.db).await.ok().flatten();
    let _ = record_outcome(&state.db, from, name.and_then(|n| n.0).as_deref(), s(&v["action"]).as_deref(), &v["actionData"]).await;
}

async fn stash_unread(state: &crate::AppState, from: &str, text: &str) {
    let _ = sqlx::query(
        "INSERT INTO network_threads (agent, unread) VALUES ($1, $2) \
         ON CONFLICT (agent) DO UPDATE SET unread = excluded.unread, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')",
    )
    .bind(from).bind(text).execute(&state.db).await;
}

/// The status a business's outcome really carries. A re-confirmation comes
/// back as `status:"already_booked"` with the live state in `bookingStatus`
/// ("confirmed" / "pending_payment"); that is still a registered booking.
fn booking_status(data: &Value) -> &str {
    match data["status"].as_str().unwrap_or("") {
        "already_booked" => data["bookingStatus"].as_str().filter(|b| !b.is_empty()).unwrap_or("confirmed"),
        other => other,
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::booking_status;
    use serde_json::json;

    #[tokio::test]
    async fn already_booked_replies_land_in_the_ledger_once() {
        let db = crate::db::open("sqlite::memory:").await.unwrap();
        let biz = "agent:1f27cf15b4beb8ca4321950ac5076dedf970a611ed9bec716fb8b910f1a622f0";
        // First confirmation: the business books and says so.
        let first = json!({"appointment_id": "b4422f0c", "status": "confirmed", "starts_at": "2026-08-29T15:00:00-05:00", "service": "Corte de cabello", "price": 25.0});
        let r = super::record_outcome(&db, biz, Some("Barberia Yaya"), Some("book_appointment"), &first).await;
        assert_eq!(r["recorded"], "appointment");
        // Re-confirmation: "already booked", live state in bookingStatus.
        let again = json!({"appointment_id": "b4422f0c", "status": "already_booked", "bookingStatus": "confirmed", "paid": false});
        let r2 = super::record_outcome(&db, biz, Some("Barberia Yaya"), Some("book_appointment"), &again).await;
        assert_eq!(r2["recorded"], "appointment");
        assert_eq!(r2["alreadyKnown"], true);
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as("SELECT status, remote_id, service FROM client_bookings WHERE agent = $1").bind(biz).fetch_all(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "one appointment, not two");
        assert_eq!(rows[0].0, "confirmed");
        assert_eq!(rows[0].1, "b4422f0c");
        assert_eq!(rows[0].2.as_deref(), Some("Corte de cabello"));
        // A reply without an action registers nothing.
        let none = super::record_outcome(&db, biz, None, None, &json!({})).await;
        assert!(none.is_null(), "{none}");
        // A fresh "already booked" for an id we never saw is still a booking.
        let unseen = json!({"appointment_id": "zz-new", "status": "already_booked", "bookingStatus": "pending_payment"});
        let r3 = super::record_outcome(&db, biz, Some("Barberia Yaya"), Some("book_appointment"), &unseen).await;
        assert_eq!(r3["status"], "pending_payment");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM client_bookings WHERE agent = $1").bind(biz).fetch_one(&db).await.unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn already_booked_counts_as_the_live_state() {
        assert_eq!(booking_status(&json!({"status": "already_booked", "bookingStatus": "confirmed"})), "confirmed");
        assert_eq!(booking_status(&json!({"status": "already_booked", "bookingStatus": "pending_payment"})), "pending_payment");
        assert_eq!(booking_status(&json!({"status": "already_booked"})), "confirmed");
        assert_eq!(booking_status(&json!({"status": "pending_payment"})), "pending_payment");
        assert_eq!(booking_status(&json!({})), "");
    }
}

/// A business agent's structured outcome → our bookings ledger.
async fn record_outcome(db: &crate::db::Db, agent: &str, name: Option<&str>, action: Option<&str>, data: &Value) -> Value {
    let status = booking_status(data);
    match action {
        Some("book_appointment") if matches!(status, "confirmed" | "pending_payment") => {
            let remote = s(&data["appointment_id"]);
            // A re-confirmation ("already booked") must not become a second
            // row: the business's appointment id is the identity.
            if let Some(r) = &remote {
                let (dup,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM client_bookings WHERE agent = $1 AND remote_id = $2")
                    .bind(agent).bind(r).fetch_one(db).await.unwrap_or((0,));
                if dup > 0 {
                    let _ = sqlx::query("UPDATE client_bookings SET status = $3 WHERE agent = $1 AND remote_id = $2")
                        .bind(agent).bind(r).bind(status).execute(db).await;
                    return json!({"recorded": "appointment", "alreadyKnown": true, "startsAt": data["starts_at"], "status": status, "service": data["service"], "price": data["price"]});
                }
            }
            let id = Uuid::new_v4();
            let _ = sqlx::query(
                "INSERT INTO client_bookings (id, agent, name, kind, remote_id, starts_at, service, price, status) \
                 VALUES ($1,$2,$3,'appointment',$4,$5,$6,$7,$8)",
            )
            .bind(id).bind(agent).bind(name)
            .bind(remote)
            .bind(s(&data["starts_at"]))
            .bind(s(&data["service"]))
            .bind(data["price"].as_f64())
            .bind(status)
            .execute(db).await;
            json!({"recorded": "appointment", "startsAt": data["starts_at"], "status": status, "service": data["service"], "price": data["price"]})
        }
        Some("create_order") if matches!(status, "confirmed" | "pending_payment" | "paid") => {
            let id = Uuid::new_v4();
            let _ = sqlx::query(
                "INSERT INTO client_bookings (id, agent, name, kind, remote_id, price, currency, status, service) \
                 VALUES ($1,$2,$3,'order',$4,$5,$6,$7,$8)",
            )
            .bind(id).bind(agent).bind(name)
            .bind(s(&data["orderId"]))
            .bind(data["total"].as_f64())
            .bind(s(&data["currency"]))
            .bind(status)
            .bind(data["items"].as_array().map(|a| a.iter().filter_map(|i| i["name"].as_str().or(i["product"].as_str())).collect::<Vec<_>>().join(", ")))
            .execute(db).await;
            json!({"recorded": "order", "total": data["totalFormatted"], "status": status})
        }
        Some("handle_cancellation") if status == "cancelled" => {
            let _ = sqlx::query(
                "UPDATE client_bookings SET status = 'cancelled' WHERE agent = $1 AND (remote_id = $2 OR $2 IS NULL) \
                 AND status <> 'cancelled'",
            )
            .bind(agent).bind(s(&data["appointment_id"]))
            .execute(db).await;
            json!({"recorded": "cancelled"})
        }
        Some("collect_payment") if status == "paid" => {
            // The reply names no booking: the newest one awaiting payment is
            // the one this transfer was for — never all of them.
            let _ = sqlx::query("UPDATE client_bookings SET status = 'paid' WHERE id = \
                (SELECT id FROM client_bookings WHERE agent = $1 AND status = 'pending_payment' ORDER BY created_at DESC LIMIT 1)")
                .bind(agent).execute(db).await;
            json!({"recorded": "paid"})
        }
        _ => Value::Null,
    }
}

async fn my_bookings(ctx: &ToolCtx<'_>, _args: &Value) -> anyhow::Result<Value> {
    let rows: Vec<(String, Option<String>, String, Option<String>, Option<String>, Option<f64>, String, i64, String)> = sqlx::query_as(
        "SELECT agent, name, kind, starts_at, service, price, status, reviewed, created_at FROM client_bookings \
         ORDER BY COALESCE(starts_at, created_at) DESC LIMIT 30",
    )
    .fetch_all(&ctx.state.db).await?;
    let threads: Vec<(String, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT agent, handle, name, unread FROM network_threads WHERE unread IS NOT NULL")
            .fetch_all(&ctx.state.db).await.unwrap_or_default();
    Ok(json!({
        "bookings": rows.into_iter().map(|(a, n, k, at, svc, p, st, rv, c)| json!({
            "business": n.unwrap_or(a), "kind": k, "startsAt": at, "service": svc, "price": p,
            "status": st, "reviewed": rv == 1, "madeAt": c,
        })).collect::<Vec<_>>(),
        "unreadReplies": threads.into_iter().map(|(a, h, n, u)| json!({
            "business": h.map(|h| format!("@{h}")).or(n).unwrap_or(a), "text": u,
        })).collect::<Vec<_>>(),
    }))
}

async fn rate_business(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let who = s(&args["business"]).ok_or_else(|| anyhow::anyhow!("missing 'business'"))?;
    let stars = args["stars"].as_i64().unwrap_or(0);
    if !(1..=5).contains(&stars) {
        return Ok(json!({"error": "stars must be 1-5"}));
    }
    let (id, _, _) = resolve(ctx.state, &who).await?;
    let tags: Vec<String> = args["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
    match network::post_review(ctx.state, &id, stars, s(&args["comment"]).as_deref().unwrap_or(""), &tags).await {
        Ok(v) => {
            let _ = sqlx::query("UPDATE client_bookings SET reviewed = 1 WHERE agent = $1").bind(&id).execute(&ctx.state.db).await;
            Ok(json!({"status": "ok", "rating_now": v["reputation"]["rating"], "reviews_now": v["reputation"]["reviews"]}))
        }
        Err(e) => Ok(json!({"error": e.to_string(),
            "note": "reviews are only accepted for businesses you actually talked to through the network"})),
    }
}

impl Plugin<Agente> for Market {
    fn name(&self) -> &'static str {
        "market"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        // ---- client
        k.tool(
            meta(Scope::Assistant, true),
            spec("find_businesses",
                 "Search the yaya network for local businesses that can serve a need. Returns ranked matches with services+prices, products, hours, the next FREE slots (business-local times), payment rails, deposit policy, delivery zones, rating/review count and whether the phone is hardware-attested.",
                 json!({"type": "object", "properties": {
                     "query": {"type": "string", "description": "what is needed, in the person's language, e.g. 'corte de cabello', 'veterinaria', 'pollo a la brasa delivery'"},
                     "city": {"type": "string", "description": "city or district to prefer (from PROFILE)"},
                     "industry": {"type": "string", "description": "optional vertical word: barbería, dentista, veterinaria, restaurante…"},
                     "country": {"type": "string", "description": "ISO-2, defaults to the person's country"}},
                   "required": ["query"]})),
            tool_fn(|c, a| Box::pin(find_businesses(c, a))),
        )?;
        k.tool(
            meta(Scope::Assistant, true),
            spec("business_details",
                 "Full public card of one business: offer (all services/products with prices, hours, next free slots, deposit, delivery) and reputation with recent reviews.",
                 json!({"type": "object", "properties": {"business": {"type": "string", "description": "@handle or agent id from find_businesses"}}, "required": ["business"]})),
            tool_fn(|c, a| Box::pin(business_details(c, a))),
        )?;
        k.tool(
            meta(Scope::Assistant, true),
            spec("ask_business",
                 "Send one message to a business's agent over the encrypted relay and wait for its reply (up to ~60 s). Use it to ask questions, check a specific time, BOOK an appointment or PLACE an order — the business agent does the booking and answers with status. Conversations continue across calls. Share only what the person authorised.",
                 json!({"type": "object", "properties": {
                     "business": {"type": "string", "description": "@handle or agent id"},
                     "message": {"type": "string", "description": "what to say, as the person's representative, in the business's language"},
                     "pay": {"type": "boolean", "description": "true ONLY after the person agreed to pay this business's per-question price (returned as payment_required on a first call)"},
                     "agreed_price_minor": {"type": "integer", "description": "the priceMinor the person agreed to; nothing above it is paid"}},
                   "required": ["business", "message"]})),
            tool_fn(|c, a| Box::pin(ask_business(c, a))),
        )?;
        k.tool(
            meta(Scope::Assistant, true),
            spec("my_bookings", "Appointments and orders made through the network, newest first, plus any business replies that arrived late.",
                 json!({"type": "object", "properties": {}, "required": []})),
            tool_fn(|c, a| Box::pin(my_bookings(c, a))),
        )?;
        k.tool(
            meta(Scope::Assistant, true),
            spec("rate_business",
                 "Leave or update the person's review of a business they dealt with (1-5 stars, short comment, optional tags: on_time, late, as_described, overcharged, friendly, rude, would_return). One standing review per business.",
                 json!({"type": "object", "properties": {
                     "business": {"type": "string"}, "stars": {"type": "integer", "minimum": 1, "maximum": 5},
                     "comment": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}},
                   "required": ["business", "stars"]})),
            tool_fn(|c, a| Box::pin(rate_business(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_correlate_by_relay_id() {
        assert!(correlates(&json!({"text": "ok", "inReplyTo": "m1"}), "m1"));
        assert!(!correlates(&json!({"text": "ok", "inReplyTo": "m0"}), "m1"));
        // Older cores answer without the id; an unknown send id accepts anything.
        assert!(correlates(&json!({"text": "ok"}), "m1"));
        assert!(correlates(&json!({"text": "ok", "inReplyTo": "m0"}), ""));
    }

    #[test]
    fn tool_specs_match_the_orchestrator_playbook() {
        let mut k = Kernel::new();
        Market.apply(&mut k).unwrap();
        let caps = crate::harness::ToolCaps::default();
        let names_for = |scope: Scope| -> Vec<String> {
            crate::harness::tool_specs(&k, scope, None, &caps)
                .as_array().cloned().unwrap_or_default().iter()
                .filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect()
        };
        let names = names_for(Scope::Assistant);
        for want in ["find_businesses", "business_details", "ask_business", "my_bookings", "rate_business"] {
            assert!(names.iter().any(|n| n == want), "missing {want} in {names:?}");
        }
        // Nothing leaks into a business's agents.
        assert!(names_for(Scope::Customer).is_empty());
        assert!(names_for(Scope::Onboarding).is_empty());
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use crate::testkit::{self, tool_ctx, Mock};

    const BIZ: &str = "agent:1f27cf15b4beb8ca4321950ac5076dedf970a611ed9bec716fb8b910f1a622f0";

    async fn client(m: &Mock) -> (crate::SharedState, Uuid) {
        let s = testkit::state_with(testkit::Opts { client_mode: true, upstream: Some(m.base.clone()) }).await;
        let me = crate::plugins::assistant::ensure_self(&s.db, "PE", Some("es")).await.unwrap();
        s.asks.set_live();
        (s, me)
    }

    fn card(price: i64) -> Value {
        json!({"payload": {"name": "Barbería Yaya", "offer": {"askPrice": price}}})
    }

    /// Answers the next message sent to BIZ as the business would.
    fn answer(s: &crate::SharedState, reply: Value) {
        let s = s.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if s.asks.deliver(BIZ, reply.clone()) { return; }
            }
        });
    }

    #[tokio::test]
    async fn a_paid_question_never_costs_more_than_what_the_person_agreed() {
        let m = Mock::start().await;
        m.on(&format!("/v1/agents/{BIZ}/facts"), json!({"label": "Barbería Yaya"}));
        m.on(&format!("/v1/agents/{BIZ}/inbox"), json!({"id": "box-1"}));
        m.set(&format!("/v1/agents/{BIZ}"), card(200));
        let (s, me) = client(&m).await;
        let c = tool_ctx(&s, me, None).await;
        let r = ask_business(&c, &json!({"business": BIZ, "message": "¿tienen hora?"})).await.unwrap();
        assert_eq!((r["status"].clone(), r["priceMinor"].clone()), (json!("payment_required"), json!(200)));
        // The business raises its price before the person's yes arrives.
        m.set(&format!("/v1/agents/{BIZ}"), card(500));
        let r = ask_business(&c, &json!({"business": BIZ, "message": "¿tienen hora?", "pay": true, "agreed_price_minor": 200})).await.unwrap();
        assert_eq!((r["status"].clone(), r["priceMinor"].clone()), (json!("payment_required"), json!(500)), "{r}");
        assert!(m.seen_path(&format!("/v1/agents/{BIZ}/inbox")).is_empty(), "nothing was paid");
        // pay=true without saying what was agreed pays nothing either.
        let r = ask_business(&c, &json!({"business": BIZ, "message": "x", "pay": true})).await.unwrap();
        assert_eq!(r["status"], "payment_required");
        // At or under the agreed price: paid and sent.
        answer(&s, json!({"text": "Sí, a las 5", "inReplyTo": "box-1"}));
        let r = ask_business(&c, &json!({"business": BIZ, "message": "x", "pay": true, "agreed_price_minor": 500})).await.unwrap();
        assert_eq!(r["status"], "replied", "{r}");
        assert_eq!(m.seen_path(&format!("/v1/agents/{BIZ}/inbox"))[0].body["payload"]["pay"]["amountMinor"], 500);
    }

    #[tokio::test]
    async fn words_are_not_bookings_actions_are() {
        let m = Mock::start().await;
        m.on(&format!("/v1/agents/{BIZ}/facts"), json!({})).on(&format!("/v1/agents/{BIZ}/inbox"), json!({"id": "b1"})).on(&format!("/v1/agents/{BIZ}"), card(0));
        let (s, me) = client(&m).await;
        let c = tool_ctx(&s, me, None).await;
        answer(&s, json!({"text": "¡Listo, reservado!", "inReplyTo": "b1"}));
        let r = ask_business(&c, &json!({"business": BIZ, "message": "resérvame"})).await.unwrap();
        assert_eq!((r["status"].clone(), r["registered"].clone()), (json!("replied"), json!(false)));
        answer(&s, json!({"text": "Reservado", "inReplyTo": "b1", "action": "book_appointment", "actionData": {"status": "confirmed", "appointment_id": "a1", "starts_at": "2026-09-22T10:00"}}));
        let r = ask_business(&c, &json!({"business": BIZ, "message": "resérvame"})).await.unwrap();
        assert_eq!(r["registered"], true);
        let mb = my_bookings(&c, &json!({})).await.unwrap();
        assert_eq!(mb["bookings"][0]["status"], "confirmed");
        // Nobody answers in time.
        let r = ask_business(&c, &json!({"business": BIZ, "message": "¿hola?"})).await.unwrap();
        assert_eq!(r["status"], "no_reply_yet");
        assert!(ask_business(&c, &json!({"business": s.identity.id(), "message": "x"})).await.unwrap()["error"] == "that is you");
        assert!(ask_business(&c, &json!({"business": BIZ})).await.is_err());
    }

    #[tokio::test]
    async fn insufficient_balance_is_said_plainly() {
        let m = Mock::start().await;
        m.on(&format!("/v1/agents/{BIZ}/facts"), json!({})).on(&format!("/v1/agents/{BIZ}"), card(100));
        m.on_status(&format!("/v1/agents/{BIZ}/inbox"), 402, json!({"error": {"message": "balance"}}));
        let (s, me) = client(&m).await;
        let c = tool_ctx(&s, me, None).await;
        let r = ask_business(&c, &json!({"business": BIZ, "message": "x", "pay": true, "agreed_price_minor": 100})).await.unwrap();
        assert_eq!(r["status"], "insufficient_balance");
    }

    #[tokio::test]
    async fn one_payment_marks_one_booking_paid() {
        let db = testkit::db().await;
        for (id, when) in [("a1", "2026-09-22T10:00"), ("a2", "2026-09-29T10:00")] {
            record_outcome(&db, BIZ, None, Some("book_appointment"), &json!({"status": "pending_payment", "appointment_id": id, "starts_at": when})).await;
        }
        let r = record_outcome(&db, BIZ, None, Some("collect_payment"), &json!({"status": "paid"})).await;
        assert_eq!(r["recorded"], "paid");
        let paid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_bookings WHERE status = 'paid'").fetch_one(&db).await.unwrap();
        assert_eq!(paid, 1, "one transfer pays one booking");
        record_outcome(&db, BIZ, None, Some("handle_cancellation"), &json!({"status": "cancelled", "appointment_id": "a2"})).await;
        let st: String = sqlx::query_scalar("SELECT status FROM client_bookings WHERE remote_id = 'a2'").fetch_one(&db).await.unwrap();
        assert_eq!(st, "cancelled");
        let o = record_outcome(&db, BIZ, Some("Bodega"), Some("create_order"), &json!({"status": "pending_payment", "orderId": "o1", "total": 30, "items": [{"product": "Polo"}], "totalFormatted": "S/ 30"})).await;
        assert_eq!(o["recorded"], "order");
    }

    #[tokio::test]
    async fn late_replies_ratings_and_details() {
        let m = Mock::start().await;
        m.on(&format!("/v1/agents/{BIZ}/facts"), json!({"label": "Barbería Yaya"}))
            .on(&format!("/v1/agents/{BIZ}/reputation"), json!({"rating": 4.8, "reviews": 3}))
            .on(&format!("/v1/agents/{BIZ}/reviews"), json!({"reputation": {"rating": 4.9, "reviews": 4}}))
            .on(&format!("/v1/agents/{BIZ}"), card(0));
        let (s, me) = client(&m).await;
        let c = tool_ctx(&s, me, None).await;
        late_reply(&s, BIZ, &json!({"text": "Tu cita quedó para mañana", "action": "book_appointment", "actionData": {"status": "confirmed", "appointment_id": "z"}})).await;
        let mb = my_bookings(&c, &json!({})).await.unwrap();
        assert_eq!(mb["unreadReplies"][0]["text"], "Tu cita quedó para mañana");
        assert_eq!(mb["bookings"].as_array().unwrap().len(), 1);
        assert!(rate_business(&c, &json!({"business": BIZ, "stars": 9})).await.unwrap()["error"].is_string());
        assert_eq!(rate_business(&c, &json!({"business": BIZ, "stars": 5, "tags": ["puntual"]})).await.unwrap()["rating_now"], 4.9);
        assert_eq!(my_bookings(&c, &json!({})).await.unwrap()["bookings"][0]["reviewed"], true);
        let d = business_details(&c, &json!({"business": BIZ})).await.unwrap();
        assert_eq!((d["name"].clone(), d["reputation"]["rating"].clone()), (json!("Barbería Yaya"), json!(4.8)));
        m.on("/v1/match", json!({"matches": [{"handle": "barberia-yaya", "name": "Barbería Yaya", "offer": {"services": {"corte": 25}}, "askPrice": 0}]}));
        let f = find_businesses(&c, &json!({"query": "corte"})).await.unwrap();
        assert_eq!((f["count"].clone(), f["matches"][0]["business"].clone()), (json!(1), json!("@barberia-yaya")));
        assert!(find_businesses(&c, &json!({})).await.is_err());
    }

    #[test]
    fn replies_correlate_by_relay_id() {
        assert!(correlates(&json!({"inReplyTo": "a"}), "a"));
        assert!(!correlates(&json!({"inReplyTo": "b"}), "a"));
        assert!(correlates(&json!({}), "a"), "older cores send no id");
        assert!(correlates(&json!({"inReplyTo": "b"}), ""));
    }
}
