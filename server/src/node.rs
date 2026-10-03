//! The node: this core with a WhatsApp number of its own, through `wa-node`
//! (services/wa-node — Go: whatsmeow session + meowcaller voice). The
//! sidecar owns the channel; this module owns the mind: every inbound text
//! or spoken utterance becomes an ordinary customer turn of the business
//! (peer `wa:<digits>`), outbound campaigns pace themselves from here, and
//! the owner steers it all through the manager agent or the console.
//!
//!   wa-node ──POST /api/node/message──▶ routes::node::message ──▶ customer_turn
//!   wa-node ──POST /api/node/turn─────▶ routes::node::turn    ──▶ customer_turn (call)
//!   campaign_loop ──POST {WA_NODE_URL}/send──▶ wa-node ──▶ WhatsApp

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{AppState, SharedState};

/// Client of the sidecar. Mounted as the `wa_node` kernel service by
/// `plugins::wa_node` when `WA_NODE_URL` is set.
pub struct WaNode {
    url: String,
    key: String,
    http: reqwest::Client,
    status_cache: Mutex<Option<(Instant, Value)>>,
    /// Live calls: peer → (purpose, outbound). Read into the prompt.
    pub calls: Mutex<HashMap<String, (String, bool)>>,
}

impl WaNode {
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("WA_NODE_URL").ok().map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty())?;
        let key = std::env::var("WA_NODE_KEY").ok().filter(|k| k.len() >= 32)?;
        Some(Self { url, key, http: crate::net::client(Duration::from_secs(60)), status_cache: Mutex::new(None), calls: Default::default() })
    }

    async fn call_api(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.url)).header("x-node-key", &self.key);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(anyhow!("wa-node {status}: {}", v["error"].as_str().unwrap_or("error")));
        }
        Ok(v)
    }

    /// Connection, pairing code, counters — cached a minute.
    pub async fn status(&self) -> Value {
        if let Some((t, v)) = self.status_cache.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            if t.elapsed() < Duration::from_secs(20) {
                return v;
            }
        }
        let v = match self.call_api(reqwest::Method::GET, "/status", None).await {
            Ok(v) => v,
            Err(e) => json!({"connected": false, "error": e.to_string()}),
        };
        *self.status_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        v
    }

    pub async fn send(&self, to: &str, text: &str) -> Result<String> {
        let v = self.call_api(reqwest::Method::POST, "/send", Some(json!({"to": to, "text": text}))).await?;
        Ok(v["messageId"].as_str().unwrap_or("").to_string())
    }

    pub async fn call(&self, to: &str, purpose: &str, greeting: Option<&str>) -> Result<String> {
        let v = self.call_api(reqwest::Method::POST, "/call", Some(json!({"to": to, "purpose": purpose, "greeting": greeting}))).await?;
        Ok(v["callId"].as_str().unwrap_or("").to_string())
    }
}

pub fn node(state: &AppState) -> Option<Arc<WaNode>> {
    state.kernel.service::<WaNode>("wa_node")
}

pub fn peer_of(phone: &str) -> String {
    format!("wa:{}", digits(phone))
}

pub fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// "No me escribas", "stop", "baja" — the contact is out, for good, on
/// every campaign. Cheap and conservative: only short, unambiguous replies.
pub fn is_opt_out(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.chars().count() > 60 {
        return false;
    }
    const WORDS: &[&str] = &[
        "stop", "baja", "no me escribas", "no me escriban", "no me interesa", "no gracias", "no, gracias",
        "dejen de escribir", "deja de escribir", "no molestar", "no molesten", "borrame", "bórrame", "eliminame", "elimíname",
        "no quiero", "unsubscribe", "no mas mensajes", "no más mensajes", "no me llames", "no me llamen",
    ];
    WORDS.iter().any(|w| t == *w || t.starts_with(&format!("{w} ")) || t.starts_with(&format!("{w}.")) || t.starts_with(&format!("{w},")))
}

pub async fn opted_out(state: &AppState, phone: &str) -> bool {
    sqlx::query_as::<_, (String,)>("SELECT phone FROM opt_outs WHERE phone = $1").bind(phone)
        .fetch_optional(&state.db).await.ok().flatten().is_some()
}

pub async fn opt_out(state: &AppState, phone: &str, why: &str) {
    let _ = sqlx::query("INSERT OR IGNORE INTO opt_outs (phone, why) VALUES ($1, $2)").bind(phone).bind(why).execute(&state.db).await;
    let _ = sqlx::query("UPDATE campaign_contacts SET status = 'opted_out', note = $2 WHERE phone = $1").bind(phone).bind(why).execute(&state.db).await;
    tracing::info!(%phone, "opted out");
}

/// A reply from a contact we wrote to first: the campaign learns it.
pub async fn note_reply(state: &AppState, phone: &str) {
    let _ = sqlx::query(
        "UPDATE campaign_contacts SET status = 'replied', replied_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE phone = $1 AND status = 'sent'",
    ).bind(phone).execute(&state.db).await;
}

/// What the customer agent must know about this peer: that WE wrote first,
/// why, and how to behave — plus the live call, if any. Injected by
/// `network::peer_note` for `wa:` peers.
pub async fn campaign_note(state: &AppState, peer: &str) -> Option<String> {
    let phone = peer.strip_prefix("wa:")?;
    let mut out = String::new();
    let row: Option<(String, String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT c.name, c.goal, c.opener, cc.name, cc.status FROM campaign_contacts cc JOIN campaigns c ON c.id = cc.campaign_id \
         WHERE cc.phone = $1 AND cc.status IN ('sent','replied') ORDER BY cc.sent_at DESC LIMIT 1",
    ).bind(phone).fetch_optional(&state.db).await.ok().flatten();
    if let Some((cname, goal, opener, contact, _status)) = row {
        out.push_str(&format!(
            "OUTBOUND CONVERSATION (campaign '{cname}'): YOU wrote first — your opener was: \"{opener}\". \
             {contact}Goal of this conversation: {goal}. \
             Rules: you are the business's assistant reaching out, so be brief, useful and never pushy; one question per message; \
             if they say they are not interested, thank them warmly and stop — never insist; if asked, say plainly you are the AI \
             assistant of the business; if they want a person, say the owner will write to them and use report_gap.",
            contact = contact.map(|n| format!("The contact's name is {n}. ")).unwrap_or_default(),
        ));
    }
    if let Some(n) = node(state) {
        if let Some((purpose, outbound)) = n.calls.lock().unwrap_or_else(|e| e.into_inner()).get(peer).cloned() {
            if !out.is_empty() { out.push_str("\n\n"); }
            out.push_str(&format!(
                "VOICE CALL IN PROGRESS ({}): purpose — {purpose}. Speak in at most two short spoken sentences, no lists, no markdown, \
                 no emojis, no URLs; one question at a time. When the conversation is over, say goodbye and end your reply with the \
                 exact token [FIN] so the call hangs up.",
                if outbound { "you called them" } else { "they called you" }
            ));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

// ------------------------------------------------------------ campaigns

pub async fn create_campaign(state: &AppState, business_id: Uuid, name: &str, goal: &str, opener: &str, contacts: &[Value], daily_cap: i64, pace_secs: i64) -> Result<Value> {
    let name = name.trim();
    let goal = goal.trim();
    let opener = opener.trim();
    anyhow::ensure!(!name.is_empty() && !goal.is_empty() && !opener.is_empty(), "name, goal and opener are required");
    anyhow::ensure!(opener.chars().count() <= 800, "opener too long (800 chars max)");
    let id = format!("cmp_{}", &Uuid::new_v4().simple().to_string()[..12]);
    sqlx::query("INSERT INTO campaigns (id, business_id, name, goal, opener, daily_cap, pace_secs) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(&id).bind(business_id.to_string()).bind(name).bind(goal).bind(opener)
        .bind(daily_cap.clamp(1, 1000)).bind(pace_secs.clamp(20, 3600))
        .execute(&state.db).await?;
    let (mut added, mut skipped) = (0, 0);
    let mut seen = std::collections::HashSet::new();
    for c in contacts {
        let (phone, cname) = match c {
            Value::String(s) => (digits(s), None),
            v => (digits(v["phone"].as_str().unwrap_or("")), v["name"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())),
        };
        if phone.len() < 8 || phone.len() > 15 || !seen.insert(phone.clone()) || opted_out(state, &phone).await {
            skipped += 1;
            continue;
        }
        let r = sqlx::query("INSERT OR IGNORE INTO campaign_contacts (campaign_id, phone, name) VALUES ($1,$2,$3)")
            .bind(&id).bind(&phone).bind(&cname).execute(&state.db).await?;
        if r.rows_affected() == 1 { added += 1 } else { skipped += 1 }
    }
    tracing::info!(campaign = %id, %name, added, skipped, "campaign created");
    Ok(json!({"id": id, "name": name, "added": added, "skipped": skipped, "status": "active"}))
}

pub async fn set_campaign(state: &AppState, business_id: Uuid, id_or_name: &str, status: &str) -> Result<Value> {
    anyhow::ensure!(["active", "paused", "done"].contains(&status), "status must be active, paused or done");
    let n = sqlx::query("UPDATE campaigns SET status = $3 WHERE business_id = $1 AND (id = $2 OR name = $2)")
        .bind(business_id.to_string()).bind(id_or_name).bind(status).execute(&state.db).await?.rows_affected();
    anyhow::ensure!(n > 0, "no campaign named '{id_or_name}'");
    Ok(json!({"ok": true, "status": status}))
}

pub async fn campaigns_summary(state: &AppState, business_id: Uuid) -> Value {
    let rows: Vec<(String, String, String, String, String, i64, i64, String, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT c.id, c.name, c.goal, c.opener, c.status, c.daily_cap, c.pace_secs, c.created_at, \
                SUM(cc.status = 'pending'), SUM(cc.status = 'sent'), SUM(cc.status = 'replied'), SUM(cc.status = 'opted_out'), SUM(cc.status = 'failed') \
         FROM campaigns c LEFT JOIN campaign_contacts cc ON cc.campaign_id = c.id WHERE c.business_id = $1 GROUP BY c.id ORDER BY c.created_at DESC LIMIT 50",
    ).bind(business_id.to_string()).fetch_all(&state.db).await.unwrap_or_default();
    let (sent_today,): (i64,) = sqlx::query_as("SELECT count(*) FROM campaign_contacts WHERE sent_at >= strftime('%Y-%m-%dT00:00:00+00:00','now')")
        .fetch_one(&state.db).await.unwrap_or((0,));
    let (opt_outs,): (i64,) = sqlx::query_as("SELECT count(*) FROM opt_outs").fetch_one(&state.db).await.unwrap_or((0,));
    json!({
        "sentToday": sent_today, "optOuts": opt_outs,
        "campaigns": rows.into_iter().map(|(id, name, goal, opener, status, cap, pace, at, p, s, r, o, f)| json!({
            "id": id, "name": name, "goal": goal, "opener": opener, "status": status, "dailyCap": cap, "paceSecs": pace, "createdAt": at,
            "pending": p, "sent": s, "replied": r, "optedOut": o, "failed": f, "total": p + s + r + o + f,
        })).collect::<Vec<_>>(),
    })
}

/// Sends one opener when it is time. Runs forever; every pass is cheap.
pub async fn campaign_loop(state: SharedState) {
    let Some(n) = node(&state) else { return };
    tracing::info!("campaign scheduler running");
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if let Err(e) = tick(&state, &n).await {
            tracing::warn!(error = %e, "campaign tick failed");
        }
    }
}

async fn tick(state: &SharedState, n: &WaNode) -> Result<()> {
    let Some(bid) = crate::network::business_id(state).await else { return Ok(()) };
    let active: Vec<(String, String, String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT id, name, opener, daily_cap, pace_secs, quiet_from, quiet_to FROM campaigns WHERE business_id = $1 AND status = 'active' ORDER BY created_at",
    ).bind(bid.to_string()).fetch_all(&state.db).await?;
    if active.is_empty() {
        return Ok(());
    }
    // Quiet hours in the business's own clock; one pace shared by all campaigns.
    let values = crate::learning::compose(&state.db, &state.schemas_dir, bid).await?.values;
    let tz = crate::harness::biz_tz(&values);
    let hour = chrono::Utc::now().with_timezone(&tz).format("%H").to_string().parse::<i64>().unwrap_or(12);
    let last: Option<(String,)> = sqlx::query_as("SELECT MAX(sent_at) FROM campaign_contacts WHERE sent_at IS NOT NULL").fetch_optional(&state.db).await?.filter(|r: &(Option<String>,)| r.0.is_some()).map(|r| (r.0.unwrap(),));
    let since_last = last.and_then(|(s,)| chrono::DateTime::parse_from_rfc3339(&s).ok()).map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()).unwrap_or(i64::MAX);
    let (sent_today,): (i64,) = sqlx::query_as("SELECT count(*) FROM campaign_contacts WHERE sent_at >= strftime('%Y-%m-%dT00:00:00+00:00','now')").fetch_one(&state.db).await?;
    for (id, name, opener, cap, pace, quiet_from, quiet_to) in active {
        let quiet = if quiet_from > quiet_to { hour >= quiet_from || hour < quiet_to } else { hour >= quiet_from && hour < quiet_to };
        if quiet || sent_today >= cap {
            continue;
        }
        // Jitter: never a metronome.
        let jitter = (chrono::Utc::now().timestamp() % 17) * 3;
        if since_last < pace + jitter {
            return Ok(());
        }
        let Some((phone, cname)) = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT phone, name FROM campaign_contacts WHERE campaign_id = $1 AND status = 'pending' AND phone NOT IN (SELECT phone FROM opt_outs) ORDER BY rowid LIMIT 1",
        ).bind(&id).fetch_optional(&state.db).await? else {
            sqlx::query("UPDATE campaigns SET status = 'done' WHERE id = $1").bind(&id).execute(&state.db).await?;
            tracing::info!(campaign = %name, "campaign done: no pending contacts");
            continue;
        };
        let status = n.status().await;
        if status["connected"].as_bool() != Some(true) {
            tracing::debug!("wa-node not connected; campaign waits");
            return Ok(());
        }
        let text = opener.replace("{name}", cname.as_deref().unwrap_or("")).replace("  ", " ").replace(" ,", ",").trim().to_string();
        let peer = peer_of(&phone);
        match n.send(&phone, &text).await {
            Ok(_) => {
                sqlx::query("UPDATE campaign_contacts SET status = 'sent', sent_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE campaign_id = $1 AND phone = $2")
                    .bind(&id).bind(&phone).execute(&state.db).await?;
                // The opener is the first assistant turn of that conversation.
                let _ = crate::routes::append_message(state, bid, "customer", &peer, "assistant", &text).await;
                crate::contacts::touch(&state.db, bid, &peer).await;
                tracing::info!(campaign = %name, %phone, "opener sent");
            }
            Err(e) => {
                sqlx::query("UPDATE campaign_contacts SET status = 'failed', note = $3 WHERE campaign_id = $1 AND phone = $2")
                    .bind(&id).bind(&phone).bind(e.to_string()).execute(&state.db).await?;
                tracing::warn!(campaign = %name, %phone, error = %e, "opener failed");
            }
        }
        return Ok(()); // one send per tick, across all campaigns
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, api, Mock};

    fn wa(m: &Mock) -> WaNode {
        WaNode { url: m.base.clone(), key: "k".repeat(32), http: reqwest::Client::new(), status_cache: Mutex::new(None), calls: Default::default() }
    }

    async fn never_quiet(s: &AppState) {
        sqlx::query("UPDATE campaigns SET quiet_from = 0, quiet_to = 0").execute(&s.db).await.unwrap();
    }

    #[test]
    fn opt_out_phrases_are_short_and_unambiguous() {
        for t in ["STOP", "baja", "No me escribas.", "no gracias, ya tengo", "Bórrame", "unsubscribe"] {
            assert!(is_opt_out(t), "{t}");
        }
        for t in ["hola", "no sé", "stopper", "no quiero esperar tanto, ¿tienen hoy?".repeat(3).as_str(), "¿baja el precio?"] {
            assert!(!is_opt_out(t), "{t}");
        }
        assert_eq!(peer_of("+51 999-000"), "wa:51999000");
        assert_eq!(digits("a1b2"), "12");
    }

    #[test]
    fn needs_url_and_a_long_key() {
        assert!(WaNode::from_env().is_none(), "tests never set WA_NODE_URL");
    }

    #[tokio::test]
    async fn sidecar_client_sends_calls_and_caches_status() {
        let m = Mock::start().await;
        m.on("/send", json!({"messageId": "m1"})).on("/call", json!({"callId": "c1"})).on("/status", json!({"connected": true}));
        let n = wa(&m);
        assert_eq!(n.send("519", "hola").await.unwrap(), "m1");
        assert_eq!(m.seen_path("/send")[0].headers["x-node-key"], "k".repeat(32).as_str());
        assert_eq!(n.call("519", "cita", Some("hola")).await.unwrap(), "c1");
        assert_eq!(n.status().await["connected"], true);
        n.status().await;
        assert_eq!(m.seen_path("/status").len(), 1, "cached");
        m.on_status("/send", 502, json!({"error": "offline"}));
        m.set("/send", json!({"error": "offline"}));
        let bad = Mock::start().await;
        bad.on_status("/send", 502, json!({"error": "offline"}));
        assert!(wa(&bad).send("1", "x").await.unwrap_err().to_string().contains("offline"));
        let off = WaNode { url: "http://127.0.0.1:9".into(), ..wa(&m) };
        assert_eq!(off.status().await["connected"], false);
    }

    #[tokio::test]
    async fn campaigns_validate_dedupe_and_skip_opt_outs() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        assert!(create_campaign(&s, b, " ", "g", "o", &[], 10, 60).await.is_err());
        assert!(create_campaign(&s, b, "n", "g", &"x".repeat(801), &[], 10, 60).await.unwrap_err().to_string().contains("too long"));
        opt_out(&s, "51977000333", "stop").await;
        let r = create_campaign(&s, b, "Promo", "vender", "Hola {name}", &[
            json!("+51 977 000 111"), json!({"phone": "51977000111", "name": "dup"}), json!({"phone": "51977000222", "name": " Ana "}),
            json!("51977000333"), json!("123"),
        ], 5000, 1).await.unwrap();
        assert_eq!((r["added"].clone(), r["skipped"].clone()), (json!(2), json!(3)));
        let (cap, pace): (i64, i64) = sqlx::query_as("SELECT daily_cap, pace_secs FROM campaigns").fetch_one(&s.db).await.unwrap();
        assert_eq!((cap, pace), (1000, 20), "clamped");
        assert!(set_campaign(&s, b, "Promo", "paused").await.is_ok());
        assert!(set_campaign(&s, b, "Promo", "exploded").await.is_err());
        assert!(set_campaign(&s, b, "Nope", "done").await.is_err());
        let sum = campaigns_summary(&s, b).await;
        assert_eq!((sum["campaigns"][0]["status"].clone(), sum["campaigns"][0]["total"].clone(), sum["optOuts"].clone()), (json!("paused"), json!(2), json!(1)));
    }

    #[tokio::test]
    async fn tick_sends_one_opener_paced_and_finishes() {
        let m = Mock::start().await;
        m.on("/status", json!({"connected": true})).on("/send", json!({"messageId": "m"}));
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        create_campaign(&s, b, "Promo", "vender", "Hola {name}, tenemos cortes", &[json!({"phone": "51977000111", "name": "Ana"}), json!("51977000222")], 80, 20).await.unwrap();
        never_quiet(&s).await;
        let n = wa(&m);
        tick(&s, &n).await.unwrap();
        tick(&s, &n).await.unwrap(); // paced: nothing
        assert_eq!(m.seen_path("/send").len(), 1);
        assert_eq!(m.seen_path("/send")[0].body["text"], "Hola Ana, tenemos cortes");
        let opener: (String,) = sqlx::query_as("SELECT content FROM messages WHERE peer = 'wa:51977000111'").fetch_one(&s.db).await.unwrap();
        assert_eq!(opener.0, "Hola Ana, tenemos cortes", "the opener is the conversation's first turn");
        // A reply is noted; the campaign note tells the agent it wrote first.
        note_reply(&s, "51977000111").await;
        let note = campaign_note(&s, "wa:51977000111").await.unwrap();
        assert!(note.contains("YOU wrote first") && note.contains("Ana"));
        assert!(campaign_note(&s, "wa:1").await.is_none());
        assert!(campaign_note(&s, "com.whatsapp:1").await.is_none());
        // Pace elapsed: the second contact (no name) is written to cleanly.
        sqlx::query("UPDATE campaign_contacts SET sent_at = '2000-01-01T00:00:00+00:00' WHERE sent_at IS NOT NULL").execute(&s.db).await.unwrap();
        tick(&s, &n).await.unwrap();
        assert_eq!(m.seen_path("/send")[1].body["text"], "Hola, tenemos cortes");
        sqlx::query("UPDATE campaign_contacts SET sent_at = '2000-01-01T00:00:00+00:00' WHERE sent_at IS NOT NULL").execute(&s.db).await.unwrap();
        tick(&s, &n).await.unwrap();
        let (status,): (String,) = sqlx::query_as("SELECT status FROM campaigns").fetch_one(&s.db).await.unwrap();
        assert_eq!(status, "done");
    }

    #[tokio::test]
    async fn tick_waits_when_disconnected_and_records_failures() {
        let m = Mock::start().await;
        m.on("/status", json!({"connected": false}));
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        create_campaign(&s, b, "P", "g", "hola", &[json!("51977000111")], 80, 20).await.unwrap();
        never_quiet(&s).await;
        tick(&s, &wa(&m)).await.unwrap();
        assert!(m.seen_path("/send").is_empty());
        let m2 = Mock::start().await;
        m2.on("/status", json!({"connected": true})).on_status("/send", 500, json!({"error": "banned"}));
        tick(&s, &wa(&m2)).await.unwrap();
        let (st, note): (String, Option<String>) = sqlx::query_as("SELECT status, note FROM campaign_contacts").fetch_one(&s.db).await.unwrap();
        assert_eq!(st, "failed");
        assert!(note.unwrap().contains("banned"));
        // Quiet hours all day: nothing goes out.
        let s2 = testkit::state().await;
        let b2 = testkit::business(&s2.db).await;
        create_campaign(&s2, b2, "Q", "g", "hola", &[json!("51977000111")], 80, 20).await.unwrap();
        sqlx::query("UPDATE campaigns SET quiet_from = 1, quiet_to = 0").execute(&s2.db).await.unwrap();
        let m3 = Mock::start().await;
        m3.on("/status", json!({"connected": true})).on("/send", json!({}));
        // from > to: quiet when hour >= 1 or hour < 0 → quiet except 00:xx.
        tick(&s2, &wa(&m3)).await.unwrap();
        let hour: i64 = chrono::Utc::now().with_timezone(&chrono_tz::America::Lima).format("%H").to_string().parse().unwrap();
        assert_eq!(m3.seen_path("/send").len(), (hour == 0) as usize);
    }

    #[tokio::test]
    async fn opt_out_marks_every_campaign() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        create_campaign(&s, b, "A", "g", "o", &[json!("51977000111")], 80, 20).await.unwrap();
        assert!(!opted_out(&s, "51977000111").await);
        opt_out(&s, "51977000111", "baja").await;
        opt_out(&s, "51977000111", "baja").await;
        assert!(opted_out(&s, "51977000111").await);
        let (st,): (String,) = sqlx::query_as("SELECT status FROM campaign_contacts").fetch_one(&s.db).await.unwrap();
        assert_eq!(st, "opted_out");
    }

    #[tokio::test]
    async fn node_routes_turn_whatsapp_and_calls_into_customer_turns() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let msg = |from: &str, text: &str| json!({"from": from, "text": text, "name": "Ana"});
        assert_eq!(api(&s, "POST", "/api/node/message", None, Some(msg("51977000111", "hola"))).await.0, 409, "no business yet");
        testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"}));
        assert_eq!(api(&s, "POST", "/api/node/message", None, Some(msg("12", "hola"))).await.0, 400);
        m.say("**Hola** Ana\n- corte S/ 25");
        let (st, v) = api(&s, "POST", "/api/node/message", None, Some(msg("+51 977 000 111", "hola"))).await;
        assert_eq!(st, 200, "{v}");
        assert!(v["text"].as_str().unwrap().ends_with("*Hola* Ana\n• corte S/ 25"), "WhatsApp formatting: {v}");
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(msg("51977000111", "baja"))).await;
        assert_eq!(v["action"], "opt_out");
        assert_eq!(api(&s, "POST", "/api/node/message", None, Some(msg("51977000111", "hola otra vez"))).await.1["action"], "ignored_opted_out");
        // Voice turns.
        m.say("Perfecto, te espero mañana. [FIN]");
        let (_, v) = api(&s, "POST", "/api/node/turn", None, Some(json!({"callId": "c", "from": "51977000222", "text": "quiero cita"}))).await;
        assert_eq!(v["end"], true);
        let text = v["text"].as_str().unwrap();
        assert!(text.starts_with("Hola, soy el asistente con IA") && text.ends_with("Perfecto, te espero mañana."), "a first call is disclosed too: {text}");
        let sent = m.seen_path("/chat/completions").last().unwrap().body.to_string();
        assert!(sent.contains("(por llamada de voz) quiero cita"));
        assert_eq!(api(&s, "POST", "/api/node/call_ended", None, Some(json!({"callId": "c", "from": "51977000222", "reason": "hangup", "seconds": 42}))).await.1["ok"], true);
        let last: (String,) = sqlx::query_as("SELECT content FROM messages WHERE peer = 'wa:51977000222' ORDER BY idx DESC LIMIT 1").fetch_one(&s.db).await.unwrap();
        assert_eq!(last.0, "(llamada terminada: hangup, 42 s)");
    }

    /// A business hosted in the cloud has no app on the owner's phone: the
    /// owner's own WhatsApp number reaches the manager agent (the same chat
    /// as the console), never the customer one.
    #[tokio::test]
    async fn the_owner_s_own_number_reaches_the_manager_agent() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await; // owner +51 999 000 111
        m.on("/v1/credits", json!({"state": "ok"}));
        m.say("Listo, el **corte** queda en S/ 30.");
        let (st, v) = api(&s, "POST", "/api/node/message", None, Some(json!({"from": "51999000111", "text": "sube el corte a 30", "name": "Tito"}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["text"], "Listo, el *corte* queda en S/ 30.", "manager reply, WhatsApp-formatted, no customer disclosure");
        assert_eq!(v["agent"], "owner");
        let rows: Vec<(String,)> = sqlx::query_as("SELECT agent_type FROM messages WHERE content = 'sube el corte a 30'").fetch_all(&s.db).await.unwrap();
        assert_eq!(rows, vec![("onboarding".to_string(),)], "the owner's words are a manager turn");
        let contacts: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM contacts WHERE peer = 'wa:51999000111'").fetch_one(&s.db).await.unwrap();
        assert_eq!(contacts.0, 0, "the owner never becomes a customer contact");
    }

    #[tokio::test]
    async fn the_owner_saying_baja_is_not_an_opt_out() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"}));
        m.say("¿Quieres pausar la campaña?");
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(json!({"from": "+51 999 000 111", "text": "baja"}))).await;
        assert_eq!(v["agent"], "owner", "{v}");
        assert!(!opted_out(&s, "51999000111").await);
    }
}
