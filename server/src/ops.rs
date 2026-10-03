//! The operator's own systems. A business that runs its own website and
//! back office (their website + their DB) plugs the agent in
//! here, from the owner's app:
//!
//! - `domain`      — their website; every link the agent sends lives on it.
//! - `profileUrl`  — where a participant's profile lives, as a template
//!                   (`https://{domain}/perfil/{id}`), used when their
//!                   webhook doesn't answer with a link of its own.
//! - `webhookUrl`  — where the agent POSTs signed events
//!                   (`participant.registered|updated|completed`, `ping`) so
//!                   THEIR code writes to THEIR databases. The core never
//!                   holds database credentials.
//!
//! Delivery is an outbox: every event is stored, tried at once, and retried
//! with backoff by [`flush`] until it lands or runs out of attempts.
//! Contract and verification examples: docs/INTEGRATION.md.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::db::Db;
use crate::AppState;

/// Placeholders a profile template may use.
pub const PLACEHOLDERS: &[&str] = &["id", "role", "phone", "slug"];

/// Attempts before an event is given up as `failed`.
pub const MAX_ATTEMPTS: i64 = 8;
/// Wait before retry n (seconds): 1 min, 5 min, 30 min, 2 h, 6 h, 12 h, 24 h.
const BACKOFF: [i64; 7] = [60, 300, 1800, 7200, 21600, 43200, 86400];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub domain: Option<String>,
    pub profile_url: Option<String>,
    pub webhook_url: Option<String>,
    pub secret: Option<String>,
    /// Send the participant their profile link when onboarding completes.
    pub send_profile_link: bool,
}

impl Config {
    /// The owner's view. The secret only on the owner's own device.
    pub fn to_json(&self, reveal_secret: bool) -> Value {
        let mut v = json!({
            "domain": self.domain,
            "profileUrl": self.profile_url,
            "webhookUrl": self.webhook_url,
            "sendProfileLink": self.send_profile_link,
            "hasSecret": self.secret.is_some(),
            "placeholders": PLACEHOLDERS,
        });
        if reveal_secret {
            v["secret"] = json!(self.secret);
        }
        v
    }

    fn stored(&self) -> Value {
        json!({"domain": self.domain, "profileUrl": self.profile_url, "webhookUrl": self.webhook_url,
               "secret": self.secret, "sendProfileLink": self.send_profile_link})
    }
}

/// "https://www.Example.com/" → "www.example.com". ASCII hostnames only.
pub fn normalize_domain(input: &str) -> Result<String> {
    let t = input.trim().to_lowercase();
    let t = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")).unwrap_or(&t);
    let host = t.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains('@') {
        bail!("a domain has no user part");
    }
    let host = host.split(':').next().unwrap_or("");
    if !host.is_ascii() {
        bail!("write the domain in plain ASCII (punycode for accented names: xn--…)");
    }
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !l.starts_with('-') && !l.ends_with('-')
    };
    let tld = labels.last().copied().unwrap_or("");
    if host.len() > 253 || labels.len() < 2 || !labels.iter().all(label_ok) || tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_lowercase()) {
        bail!("'{}' is not a domain like example.com", input.trim());
    }
    Ok(host.to_string())
}

pub fn default_template(domain: &str) -> String {
    format!("https://{domain}/perfil/{{id}}")
}

fn host_on(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'))
}

/// An https URL without a user part, on `domain` or a subdomain of it.
pub fn on_domain(url: &str, domain: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    u.scheme() == "https" && u.username().is_empty() && u.password().is_none()
        && u.host_str().is_some_and(|h| host_on(h, domain))
}

/// A profile template is https, on the domain (or a subdomain), and names
/// each profile by `{id}` or `{phone}`.
pub fn check_template(template: &str, domain: Option<&str>) -> Result<()> {
    let Some(domain) = domain else { bail!("set the website domain first") };
    let mut rest = template;
    let mut named = Vec::new();
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { bail!("unclosed {{ in the template") };
        let name = &rest[i + 1..i + j];
        if !PLACEHOLDERS.contains(&name) {
            bail!("unknown placeholder {{{name}}}: use {}", PLACEHOLDERS.iter().map(|p| format!("{{{p}}}")).collect::<Vec<_>>().join(", "));
        }
        named.push(name);
        rest = &rest[i + j + 1..];
    }
    if !named.iter().any(|n| *n == "id" || *n == "phone") {
        bail!("the template must contain {{id}} or {{phone}} so every profile gets its own link");
    }
    let sample = render(template, &[("id", "x"), ("role", "x"), ("phone", "1"), ("slug", "x")]);
    if !on_domain(&sample, domain) {
        bail!("profile links must be https:// on {domain} or one of its subdomains");
    }
    Ok(())
}

/// https anywhere; plain http only to a private or loopback address (their
/// ops box on the LAN or the same host).
pub fn check_webhook(url: &str) -> Result<()> {
    let u = reqwest::Url::parse(url.trim()).map_err(|_| anyhow!("'{url}' is not a URL"))?;
    let host = u.host_str().ok_or_else(|| anyhow!("the webhook URL has no host"))?;
    let private = match host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => host == "localhost",
    };
    match u.scheme() {
        "https" => Ok(()),
        "http" if private => Ok(()),
        "http" => bail!("use https:// (plain http only for a private or local address)"),
        s => bail!("unsupported scheme {s}:"),
    }
}

fn pct(v: &str) -> String {
    v.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// Fills `{name}` placeholders from `vars`, percent-encoding each value.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    vars.iter().fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), &pct(v)))
}

/// "Rosa Quispe Mamani" → "rosa-quispe-mamani".
pub fn slug(name: &str) -> String {
    let folded: String = name.to_lowercase().chars().map(|c| match c {
        'á' | 'à' | 'ä' | 'â' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' => 'o', 'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', c => c,
    }).map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let s = folded.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    s.chars().take(40).collect::<String>().trim_end_matches('-').to_string()
}

pub fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    use hmac::Mac;
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("hmac takes any key length");
    m.update(msg);
    hex::encode(m.finalize().into_bytes())
}

/// `x-agente-signature`: HMAC-SHA256 over `"{timestamp}.{body}"`.
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    format!("sha256={}", hmac_hex(secret.as_bytes(), format!("{timestamp}.{body}").as_bytes()))
}

fn key(business_id: Uuid) -> String {
    format!("ops:{business_id}")
}

pub async fn config(db: &Db, business_id: Uuid) -> Config {
    let v: Value = crate::account::setting(db, &key(business_id)).await
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Config {
        domain: s("domain"),
        profile_url: s("profileUrl"),
        webhook_url: s("webhookUrl"),
        secret: s("secret"),
        send_profile_link: v["sendProfileLink"].as_bool().unwrap_or(true),
    }
}

fn new_secret() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    format!("whsec_{}", hex::encode(b))
}

/// Applies a partial update from the owner (`domain`, `profileUrl`,
/// `webhookUrl`, `sendProfileLink`; an empty string clears). Nothing is
/// stored unless the whole result is valid.
pub async fn configure(db: &Db, business_id: Uuid, patch: &Value) -> Result<Config> {
    let mut c = config(db, business_id).await;
    let text = |k: &str| patch.get(k).filter(|v| !v.is_null()).map(|v| v.as_str().map(str::trim).map(str::to_string).ok_or_else(|| anyhow!("{k} must be text")));
    if let Some(d) = text("domain") {
        let d = d?;
        c.domain = if d.is_empty() { None } else { Some(normalize_domain(&d)?) };
    }
    if let Some(t) = text("profileUrl") {
        let t = t?;
        c.profile_url = if t.is_empty() { None } else { check_template(&t, c.domain.as_deref())?; Some(t) };
    }
    // A template must live on the current domain; otherwise the default.
    c.profile_url = match (&c.domain, c.profile_url.take()) {
        (None, _) => None,
        (Some(d), Some(t)) if check_template(&t, Some(d)).is_ok() => Some(t),
        (Some(d), _) => Some(default_template(d)),
    };
    if let Some(w) = text("webhookUrl") {
        let w = w?;
        if w.is_empty() {
            c.webhook_url = None;
        } else {
            check_webhook(&w)?;
            c.webhook_url = Some(w);
            c.secret.get_or_insert_with(new_secret);
        }
    }
    if let Some(b) = patch.get("sendProfileLink").filter(|v| !v.is_null()) {
        c.send_profile_link = b.as_bool().ok_or_else(|| anyhow!("sendProfileLink must be true or false"))?;
    }
    crate::account::set_setting(db, &key(business_id), &c.stored().to_string()).await?;
    tracing::info!(business = %business_id, domain = ?c.domain, webhook = c.webhook_url.is_some(), "ops configured");
    Ok(c)
}

/// A fresh signing secret; the old one stops verifying at once.
pub async fn rotate_secret(db: &Db, business_id: Uuid) -> Result<String> {
    let mut c = config(db, business_id).await;
    let s = new_secret();
    c.secret = Some(s.clone());
    crate::account::set_setting(db, &key(business_id), &c.stored().to_string()).await?;
    Ok(s)
}

fn stamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3f+00:00").to_string()
}

/// Queues `kind` for the webhook and tries it now. `Some(reply)` when it
/// landed (the webhook's JSON answer, `{}` when it sent none); `None` when
/// no webhook is set or this attempt failed (the outbox retries).
pub async fn emit(state: &AppState, business_id: Uuid, kind: &str, data: Value) -> Result<Option<Value>> {
    let c = config(&state.db, business_id).await;
    if c.webhook_url.is_none() {
        return Ok(None);
    }
    let name: Option<(String,)> = sqlx::query_as("SELECT name FROM businesses WHERE id = $1")
        .bind(business_id).fetch_optional(&state.db).await?;
    let id = format!("evt_{}", &Uuid::new_v4().simple().to_string()[..20]);
    let body = json!({
        "id": id,
        "type": kind,
        "createdAt": chrono::Utc::now().to_rfc3339(),
        "business": {"id": business_id, "name": name.map(|n| n.0), "domain": c.domain},
        "data": data,
    }).to_string();
    sqlx::query("INSERT INTO ops_events (id, business_id, kind, body) VALUES ($1, $2, $3, $4)")
        .bind(&id).bind(business_id).bind(kind).bind(&body).execute(&state.db).await?;
    Ok(attempt(state, &c, &id, &body, 0).await)
}

/// One POST of a stored event; settles its row either way.
async fn attempt(state: &AppState, c: &Config, id: &str, body: &str, attempts_before: i64) -> Option<Value> {
    let attempts = attempts_before + 1;
    let result: Result<Value> = async {
        let url = c.webhook_url.as_deref().ok_or_else(|| anyhow!("webhook removed"))?;
        let secret = c.secret.as_deref().ok_or_else(|| anyhow!("no signing secret"))?;
        let ts = chrono::Utc::now().timestamp();
        let kind = serde_json::from_str::<Value>(body).ok().and_then(|v| v["type"].as_str().map(str::to_string)).unwrap_or_default();
        let r = crate::net::client(std::time::Duration::from_secs(8))
            .post(url)
            .header("content-type", "application/json")
            .header("user-agent", "agente-ops/1")
            .header("x-agente-event", kind)
            .header("x-agente-delivery", id)
            .header("x-agente-timestamp", ts.to_string())
            .header("x-agente-signature", sign(secret, ts, body))
            .body(body.to_string())
            .send().await?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
        }
        Ok(serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or_else(|| json!({})))
    }.await;
    match result {
        Ok(reply) => {
            let _ = sqlx::query(
                "UPDATE ops_events SET status = 'delivered', attempts = $2, response = $3, last_error = NULL, \
                 delivered_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id = $1",
            ).bind(id).bind(attempts).bind(reply.to_string().chars().take(2000).collect::<String>()).execute(&state.db).await;
            Some(reply)
        }
        Err(e) => {
            let gave_up = attempts >= MAX_ATTEMPTS || c.webhook_url.is_none();
            let wait = BACKOFF[((attempts - 1) as usize).min(BACKOFF.len() - 1)];
            let _ = sqlx::query("UPDATE ops_events SET status = $2, attempts = $3, last_error = $4, next_at = $5 WHERE id = $1")
                .bind(id).bind(if gave_up { "failed" } else { "pending" }).bind(attempts)
                .bind(e.to_string()).bind(stamp(chrono::Utc::now() + chrono::Duration::seconds(wait)))
                .execute(&state.db).await;
            tracing::warn!(event = %id, attempts, gave_up, error = %e, "ops webhook delivery failed");
            None
        }
    }
}

/// Retries every due event. Returns how many landed.
pub async fn flush(state: &AppState) -> usize {
    let due: Vec<(String, Uuid, String, i64)> = sqlx::query_as(
        "SELECT id, business_id, body, attempts FROM ops_events WHERE status = 'pending' \
         AND next_at <= strftime('%Y-%m-%dT%H:%M:%f+00:00','now') ORDER BY created_at LIMIT 50",
    ).fetch_all(&state.db).await.unwrap_or_default();
    let mut landed = 0;
    for (id, business_id, body, attempts) in due {
        let c = config(&state.db, business_id).await;
        if attempt(state, &c, &id, &body, attempts).await.is_some() {
            landed += 1;
        }
    }
    landed
}

/// Background retries, once a minute.
pub async fn flush_loop(state: crate::SharedState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        flush(&state).await;
    }
}

/// A `ping` to the webhook, for the owner's "probar" button. Errs when no
/// webhook is set.
pub async fn ping(state: &AppState, business_id: Uuid) -> Result<Value> {
    if config(&state.db, business_id).await.webhook_url.is_none() {
        bail!("no webhook configured");
    }
    Ok(match emit(state, business_id, "ping", json!({"message": "prueba desde agente"})).await? {
        Some(reply) => json!({"delivered": true, "reply": reply}),
        None => json!({"delivered": false,
            "error": recent(&state.db, business_id, 1).await.first().and_then(|e| e["lastError"].as_str().map(str::to_string))}),
    })
}

/// What a participant's link looks like with `template`, for previews.
pub fn example(template: &str) -> String {
    render(template, &[("id", "p7k2m9x4qa"), ("role", "productor"), ("phone", "51987654321"), ("slug", "rosa-quispe")])
}

/// The last deliveries, newest first, for the owner's screen.
pub async fn recent(db: &Db, business_id: Uuid, limit: i64) -> Vec<Value> {
    let rows: Vec<(String, String, String, i64, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT id, kind, status, attempts, last_error, created_at, delivered_at FROM ops_events \
         WHERE business_id = $1 ORDER BY created_at DESC, rowid DESC LIMIT $2",
    ).bind(business_id).bind(limit.clamp(1, 200)).fetch_all(db).await.unwrap_or_default();
    rows.into_iter().map(|(id, kind, status, attempts, err, created, delivered)| json!({
        "id": id, "type": kind, "status": status, "attempts": attempts,
        "lastError": err, "createdAt": created, "deliveredAt": delivered,
    })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, Mock};

    #[test]
    fn domains_are_normalized_or_refused() {
        assert_eq!(normalize_domain("example.com").unwrap(), "example.com");
        assert_eq!(normalize_domain(" https://www.Example.com/perfil?x=1 ").unwrap(), "www.example.com");
        assert_eq!(normalize_domain("http://api.example.org:8443").unwrap(), "api.example.org");
        for bad in ["", "localhost", "example", "-example.com", "example-.com", "exa_mple.com", "example.c", "example.123", "exámple.com", "a..com", &format!("{}.com", "a".repeat(64))] {
            assert!(normalize_domain(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn templates_live_on_the_domain() {
        let d = Some("example.com");
        assert_eq!(default_template("example.com"), "https://example.com/perfil/{id}");
        assert!(check_template("https://example.com/perfil/{id}", d).is_ok());
        assert!(check_template("https://app.example.com/p/{role}/{phone}?ref=wa", d).is_ok());
        assert!(check_template("https://example.com/perfil/{id}", None).is_err(), "no domain, no links");
        for bad in [
            "http://example.com/perfil/{id}",
            "https://evil.com/{id}",
            "https://example.com.evil.com/{id}",
            "https://evilexample.com/{id}",
            "https://example.com/perfil/{email}",
            "https://example.com/perfil/{slug}",
            "https://user@example.com/{id}",
            "example.com/{id}",
        ] {
            assert!(check_template(bad, d).is_err(), "{bad} accepted");
        }
        assert!(on_domain("https://example.com/p/77", "example.com"));
        assert!(on_domain("https://www.example.com/p/77", "example.com"));
        assert!(!on_domain("http://example.com/p/77", "example.com"));
        assert!(!on_domain("https://example.com.io/p/77", "example.com"));
        assert!(!on_domain("no es url", "example.com"));
    }

    #[test]
    fn webhooks_are_https_or_private() {
        assert!(check_webhook("https://api.example.com/agente/events").is_ok());
        assert!(check_webhook("http://10.0.0.5:8080/hook").is_ok());
        assert!(check_webhook("http://192.168.1.20/hook").is_ok());
        assert!(check_webhook("http://127.0.0.1:9000/hook").is_ok());
        assert!(check_webhook("http://[::1]:9000/hook").is_ok());
        assert!(check_webhook("http://api.example.com/hook").is_err(), "plain http over the internet");
        assert!(check_webhook("http://8.8.8.8/hook").is_err());
        assert!(check_webhook("ftp://example.com/x").is_err());
        assert!(check_webhook("hook").is_err());
    }

    #[test]
    fn render_fills_and_encodes() {
        let url = render("https://m.pe/{role}/{id}?n={slug}&p={phone}", &[("id", "a1 b"), ("role", "productor"), ("slug", "rosa-q"), ("phone", "51977")]);
        assert_eq!(url, "https://m.pe/productor/a1%20b?n=rosa-q&p=51977");
        assert_eq!(slug("  Rosa QUISPE  Mamaní "), "rosa-quispe-mamani");
        assert_eq!(slug("¿?"), "");
        assert_eq!(slug(&"ñ".repeat(80)).len(), 40);
    }

    #[test]
    fn signatures_are_hmac_sha256() {
        // RFC 4231, test case 2.
        assert_eq!(hmac_hex(b"Jefe", b"what do ya want for nothing?"), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(sign("Jefe", 1700000000, "{}"), format!("sha256={}", hmac_hex(b"Jefe", b"1700000000.{}")));
    }

    #[tokio::test]
    async fn configure_validates_before_storing() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        assert_eq!(config(&s.db, b).await, Config { send_profile_link: true, ..Default::default() }, "links on by default, nothing else");

        let c = configure(&s.db, b, &json!({"domain": "https://Example.com/"})).await.unwrap();
        assert_eq!(c.domain.as_deref(), Some("example.com"));
        assert_eq!(c.profile_url.as_deref(), Some("https://example.com/perfil/{id}"), "a domain brings the default template");
        assert!(c.secret.is_none(), "no webhook, no secret");

        assert!(configure(&s.db, b, &json!({"profileUrl": "https://evil.com/{id}"})).await.is_err());
        assert!(configure(&s.db, b, &json!({"webhookUrl": "http://evil.com/x"})).await.is_err());
        assert!(configure(&s.db, b, &json!({"domain": "nope"})).await.is_err());
        assert_eq!(config(&s.db, b).await, c, "a refused patch changes nothing");

        let c = configure(&s.db, b, &json!({"profileUrl": "https://app.example.com/p/{phone}", "webhookUrl": "https://api.example.com/ev", "sendProfileLink": false})).await.unwrap();
        let secret = c.secret.clone().unwrap();
        assert!(secret.starts_with("whsec_") && secret.len() >= 40, "{secret}");
        assert!(!c.send_profile_link);
        assert_eq!(configure(&s.db, b, &json!({"webhookUrl": "https://api.example.com/v2"})).await.unwrap().secret, Some(secret.clone()), "the secret survives a URL change");

        let c = configure(&s.db, b, &json!({"domain": "example.org"})).await.unwrap();
        assert_eq!(c.profile_url.as_deref(), Some("https://example.org/perfil/{id}"), "a template off the new domain resets");
        let c = configure(&s.db, b, &json!({"webhookUrl": ""})).await.unwrap();
        assert!(c.webhook_url.is_none());

        let v = c.to_json(false);
        assert_eq!(v["domain"], "example.org");
        assert_eq!(v["hasSecret"], true);
        assert!(v.get("secret").is_none() && !v.to_string().contains(&secret));
        assert_eq!(c.to_json(true)["secret"], secret);

        let fresh = rotate_secret(&s.db, b).await.unwrap();
        assert_ne!(fresh, secret);
        assert_eq!(config(&s.db, b).await.secret, Some(fresh));
    }

    #[tokio::test]
    async fn events_are_signed_and_the_reply_comes_back() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        assert_eq!(emit(&s, b, "ping", json!({})).await.unwrap(), None, "no webhook: nothing sent");
        assert!(recent(&s.db, b, 10).await.is_empty(), "nor queued");

        let c = configure(&s.db, b, &json!({"domain": "example.com", "webhookUrl": format!("http://127.0.0.1:{}/hook", m.base.rsplit(':').next().unwrap())})).await.unwrap();
        m.on("/hook", json!({"profileUrl": "https://example.com/p/77"}));
        let reply = emit(&s, b, "participant.completed", json!({"participant": {"role": "productor"}})).await.unwrap();
        assert_eq!(reply, Some(json!({"profileUrl": "https://example.com/p/77"})));

        let seen = m.seen_path("/hook").pop().unwrap();
        let h = |k: &str| seen.headers.get(k).unwrap().to_str().unwrap().to_string();
        assert_eq!(h("x-agente-event"), "participant.completed");
        let ts: i64 = h("x-agente-timestamp").parse().unwrap();
        assert!((chrono::Utc::now().timestamp() - ts).abs() < 60);
        assert_eq!(h("x-agente-signature"), sign(c.secret.as_deref().unwrap(), ts, &seen.body.to_string()), "their server can verify it");
        assert_eq!(seen.body["type"], "participant.completed");
        assert_eq!(seen.body["id"], h("x-agente-delivery"));
        assert_eq!(seen.body["business"]["domain"], "example.com");
        assert_eq!(seen.body["data"]["participant"]["role"], "productor");

        let log = recent(&s.db, b, 10).await;
        assert_eq!(log[0]["status"], "delivered");
        assert_eq!(log[0]["attempts"], 1);
    }

    #[tokio::test]
    async fn failed_events_wait_in_the_outbox_and_retry() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        configure(&s.db, b, &json!({"webhookUrl": format!("{}/hook", m.base)})).await.unwrap();
        m.on_status("/hook", 500, json!({"error": "db down"}));
        assert_eq!(emit(&s, b, "participant.registered", json!({})).await.unwrap(), None);
        let log = recent(&s.db, b, 10).await;
        assert_eq!((log[0]["status"].as_str(), log[0]["attempts"].as_i64()), (Some("pending"), Some(1)));
        assert!(log[0]["lastError"].as_str().unwrap().contains("500"));

        assert_eq!(flush(&s).await, 0, "not due yet");
        sqlx::query("UPDATE ops_events SET next_at = '2000-01-01T00:00:00.000+00:00'").execute(&s.db).await.unwrap();
        m.set("/hook", json!({"ok": true}));
        assert_eq!(flush(&s).await, 1);
        let log = recent(&s.db, b, 10).await;
        assert_eq!((log[0]["status"].as_str(), log[0]["attempts"].as_i64()), (Some("delivered"), Some(2)));
        let deliveries: std::collections::HashSet<String> = m.seen_path("/hook").iter().map(|x| x.body["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(deliveries.len(), 1, "a retry carries the same event id, so their side can dedupe");

        // Out of attempts: failed, never retried again.
        configure(&s.db, b, &json!({"webhookUrl": format!("{}/down", m.base)})).await.unwrap();
        m.on_status("/down", 500, json!({}));
        emit(&s, b, "participant.updated", json!({})).await.unwrap();
        sqlx::query("UPDATE ops_events SET attempts = 7, next_at = '2000-01-01T00:00:00.000+00:00' WHERE status = 'pending'").execute(&s.db).await.unwrap();
        assert_eq!(flush(&s).await, 0);
        sqlx::query("UPDATE ops_events SET next_at = '2000-01-01T00:00:00.000+00:00'").execute(&s.db).await.unwrap();
        assert_eq!(flush(&s).await, 0);
        assert_eq!(m.seen_path("/down").len(), 2, "a failed event is not retried");
        assert_eq!(recent(&s.db, b, 10).await[0]["status"], "failed");
    }

    /// Against a real receiver (scripts/ops-e2e.sh runs docs/examples/ops_receiver.py):
    /// `OPS_E2E_URL=http://127.0.0.1:8099/ OPS_E2E_SECRET_FILE=… cargo test --lib live_receiver -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_receiver() {
        let url = std::env::var("OPS_E2E_URL").expect("OPS_E2E_URL");
        let secret_file = std::env::var("OPS_E2E_SECRET_FILE").expect("OPS_E2E_SECRET_FILE");
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let c = configure(&s.db, b, &json!({"domain": "example.com", "webhookUrl": url})).await.unwrap();
        std::fs::write(&secret_file, c.secret.unwrap()).unwrap();
        assert_eq!(ping(&s, b).await.unwrap()["delivered"], true, "ping");
        let participant = json!({"participant": {"id": "e2e0000001", "role": "productor", "phone": "51977000111",
            "name": "Rosa", "status": "complete", "profile": {"nombre": "Rosa"}}});
        let reply = emit(&s, b, "participant.completed", participant).await.unwrap().expect("delivered");
        assert_eq!(reply["profileUrl"], "https://example.com/perfil/e2e0000001");
        assert!(on_domain(reply["profileUrl"].as_str().unwrap(), "example.com"));
        // A rotated secret the receiver doesn't know yet: refused, queued for retry.
        rotate_secret(&s.db, b).await.unwrap();
        assert_eq!(emit(&s, b, "participant.updated", json!({})).await.unwrap(), None);
        assert!(recent(&s.db, b, 1).await[0]["lastError"].as_str().unwrap().contains("401"));
    }
}
