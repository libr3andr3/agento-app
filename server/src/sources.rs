//! Which apps bring money — learned, never listed.
//!
//! The phone forwards the raw notification of any app it has no verdict
//! for. The agent reads it and answers: money in or not, and which wallet.
//! Apps that turn out not to carry money are muted for a week (a wallet
//! that also sends promos is never muted once it has delivered money).
//! Every verdict is reported to the network, and the network hands back
//! priors: the packages other businesses in the country receive money
//! through, so a new phone forwards those eagerly from day one.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::AppState;

/// Everything the phone could see about a notification.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    #[serde(default)] pub package: String,
    #[serde(default)] pub app_label: String,
    #[serde(default)] pub installer: Option<String>,
    #[serde(default)] pub system_app: bool,
    #[serde(default)] pub channel_id: Option<String>,
    #[serde(default)] pub channel_name: Option<String>,
    #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub template: Option<String>,
    #[serde(default)] pub title: String,
    #[serde(default)] pub text: String,
    #[serde(default)] pub sub_text: Option<String>,
    #[serde(default)] pub info_text: Option<String>,
    #[serde(default)] pub summary_text: Option<String>,
    #[serde(default)] pub big_text: Option<String>,
    #[serde(default)] pub text_lines: Vec<String>,
    #[serde(default)] pub post_time: Option<i64>,
}

impl Envelope {
    /// Every text surface, deduplicated, in reading order.
    pub fn full_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut push = |s: &str| {
            let s = s.trim();
            if !s.is_empty() && !parts.iter().any(|p| p == s || p.contains(s)) {
                parts.push(s.to_string());
            }
        };
        push(&self.title);
        push(&self.text);
        if let Some(b) = &self.big_text { push(b); }
        for l in &self.text_lines { push(l); }
        if let Some(s) = &self.sub_text { push(s); }
        if let Some(s) = &self.summary_text { push(s); }
        if let Some(s) = &self.info_text { push(s); }
        parts.join(" | ")
    }

    /// What the agent reads: the whole thing, as data, not a sentence.
    pub fn as_json(&self) -> Value {
        json!({
            "package": self.package, "appLabel": self.app_label, "installer": self.installer,
            "systemApp": self.system_app, "channelId": self.channel_id, "channelName": self.channel_name,
            "category": self.category, "template": self.template, "title": self.title, "text": self.text,
            "subText": self.sub_text, "infoText": self.info_text, "summaryText": self.summary_text,
            "bigText": self.big_text, "textLines": self.text_lines, "postTime": self.post_time,
        })
    }
}

/// What the agent said about one notification.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Reading {
    pub money_in: bool,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub payer: Option<String>,
    pub payer_phone: Option<String>,
    /// The wallet/bank brand as a person would name it.
    pub wallet: Option<String>,
    /// payment | promo | otp | other — for the log.
    pub kind: String,
}

pub const MUTE_DAYS: i64 = 7;

/// `Some(until)` when this package should not be forwarded right now.
pub async fn muted_until(db: &SqlitePool, package: &str) -> Option<DateTime<Utc>> {
    let (class, until, money_seen): (String, Option<String>, i64) =
        sqlx::query_as("SELECT class, muted_until, money_seen FROM notification_sources WHERE package = $1")
            .bind(package).fetch_optional(db).await.ok().flatten()?;
    if class == "money" || money_seen > 0 {
        return None;
    }
    let until = DateTime::parse_from_rfc3339(until.as_deref()?).ok()?.with_timezone(&Utc);
    if until > Utc::now() { Some(until) } else { None }
}

/// Remembers a verdict. Returns the mute deadline the phone should honour
/// (None = keep forwarding this app).
pub async fn remember(db: &SqlitePool, env: &Envelope, reading: &Reading) -> anyhow::Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    let row: Option<(i64, i64)> = sqlx::query_as("SELECT seen, money_seen FROM notification_sources WHERE package = $1")
        .bind(&env.package).fetch_optional(db).await?;
    let (seen, money_seen) = row.unwrap_or((0, 0));
    let money_seen = money_seen + reading.money_in as i64;
    let class = if money_seen > 0 { "money" } else { "not_money" };
    let mute = if money_seen == 0 { Some(now + Duration::days(MUTE_DAYS)) } else { None };
    sqlx::query(
        "INSERT INTO notification_sources (package, label, wallet, class, seen, money_seen, muted_until, last_seen) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT (package) DO UPDATE SET label = excluded.label, wallet = COALESCE(excluded.wallet, notification_sources.wallet), \
           class = excluded.class, seen = excluded.seen, money_seen = excluded.money_seen, \
           muted_until = excluded.muted_until, last_seen = excluded.last_seen",
    )
    .bind(&env.package).bind(&env.app_label).bind(reading.wallet.as_deref().filter(|w| !w.is_empty()))
    .bind(class).bind(seen + 1).bind(money_seen).bind(mute.map(|m| m.to_rfc3339())).bind(now.to_rfc3339())
    .execute(db).await?;
    Ok(mute)
}

/// Tells the network what this phone learned (best effort, throttled to
/// once a day per package unless the verdict changed).
pub async fn report(state: &AppState, env: &Envelope, reading: &Reading, country: &str) {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT class, reported_at FROM notification_sources WHERE package = $1")
        .bind(&env.package).fetch_optional(&state.db).await.ok().flatten();
    let Some((class, reported_at)) = row else { return };
    let fresh = reported_at.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map_or(false, |t| Utc::now().signed_duration_since(t.with_timezone(&Utc)) < Duration::days(1));
    if fresh && !reading.money_in {
        return;
    }
    let body = json!({"package": env.package, "label": env.app_label, "wallet": reading.wallet, "class": class, "country": country});
    if state.registry.post("/v1/sources", &body, std::time::Duration::from_secs(15)).await.is_ok() {
        let _ = sqlx::query("UPDATE notification_sources SET reported_at = $2 WHERE package = $1")
            .bind(&env.package).bind(Utc::now().to_rfc3339()).execute(&state.db).await;
    }
}

/// Packages the network considers money apps in this country: forwarded
/// eagerly by the phone and never muted.
pub async fn priors(state: &AppState, country: &str) -> Vec<Value> {
    state.registry.get(&format!("/v1/sources?country={country}"), std::time::Duration::from_secs(20)).await
        .ok().and_then(|v| v["sources"].as_array().cloned()).unwrap_or_default()
}

/// What this phone has learned itself, for the app's debug/log views.
pub async fn local(db: &SqlitePool) -> Vec<Value> {
    let rows: Vec<(String, String, Option<String>, String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT package, label, wallet, class, seen, money_seen, muted_until FROM notification_sources ORDER BY last_seen DESC LIMIT 100",
    ).fetch_all(db).await.unwrap_or_default();
    rows.into_iter().map(|(p, l, w, c, s, m, u)| json!({"package": p, "label": l, "wallet": w, "class": c, "seen": s, "moneySeen": m, "mutedUntil": u})).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, Mock};

    fn env(pkg: &str) -> Envelope {
        Envelope { package: pkg.into(), app_label: "Yape".into(), title: "Yape".into(), text: "Te yapeó S/ 20".into(), ..Default::default() }
    }
    fn money(wallet: Option<&str>) -> Reading {
        Reading { money_in: true, amount: Some(20.0), wallet: wallet.map(String::from), kind: "payment".into(), ..Default::default() }
    }
    fn promo() -> Reading {
        Reading { kind: "promo".into(), ..Default::default() }
    }

    #[test]
    fn full_text_dedupes_in_reading_order() {
        let e = Envelope {
            title: " Ana ".into(), text: "Te yapeó S/ 20".into(),
            big_text: Some("Te yapeó S/ 20".into()),
            text_lines: vec!["línea 1".into(), "".into(), "Ana".into()],
            sub_text: Some("Yape".into()), summary_text: Some("2 nuevos".into()), info_text: Some("ahora".into()),
            ..Default::default()
        };
        assert_eq!(e.full_text(), "Ana | Te yapeó S/ 20 | línea 1 | Yape | 2 nuevos | ahora");
        // A surface contained in an earlier one is dropped.
        let e = Envelope { title: "Pago recibido de Ana".into(), text: "Ana".into(), ..Default::default() };
        assert_eq!(e.full_text(), "Pago recibido de Ana");
        assert_eq!(Envelope::default().full_text(), "");
    }

    #[test]
    fn envelope_json_and_camel_case_input() {
        let e: Envelope = serde_json::from_value(json!({"package": "com.bcp.yape", "appLabel": "Yape", "systemApp": true, "textLines": ["a"], "postTime": 5})).unwrap();
        assert_eq!((e.app_label.as_str(), e.system_app, e.post_time), ("Yape", true, Some(5)));
        let j = e.as_json();
        assert_eq!(j["package"], "com.bcp.yape");
        assert_eq!(j["textLines"], json!(["a"]));
        assert_eq!(j.as_object().unwrap().len(), 16);
    }

    #[tokio::test]
    async fn non_money_apps_are_muted_for_a_week() {
        let db = testkit::db().await;
        assert_eq!(muted_until(&db, "com.promo").await, None, "unknown apps are forwarded");
        let until = remember(&db, &env("com.promo"), &promo()).await.unwrap().unwrap();
        let days = (until - Utc::now()).num_hours();
        assert!((MUTE_DAYS * 24 - 1..=MUTE_DAYS * 24).contains(&days));
        assert!(muted_until(&db, "com.promo").await.is_some());
        // An expired mute no longer applies.
        sqlx::query("UPDATE notification_sources SET muted_until = '2000-01-01T00:00:00+00:00'").execute(&db).await.unwrap();
        assert_eq!(muted_until(&db, "com.promo").await, None);
        sqlx::query("UPDATE notification_sources SET muted_until = 'garbage'").execute(&db).await.unwrap();
        assert_eq!(muted_until(&db, "com.promo").await, None);
    }

    #[tokio::test]
    async fn a_wallet_that_ever_brought_money_is_never_muted() {
        let db = testkit::db().await;
        assert_eq!(remember(&db, &env("com.bcp.yape"), &money(Some("Yape"))).await.unwrap(), None);
        // Later promos from the same wallet: still forwarded, wallet name kept.
        assert_eq!(remember(&db, &env("com.bcp.yape"), &promo()).await.unwrap(), None);
        assert_eq!(muted_until(&db, "com.bcp.yape").await, None);
        let l = local(&db).await;
        assert_eq!(l[0]["class"], "money");
        assert_eq!(l[0]["wallet"], "Yape");
        assert_eq!((l[0]["seen"].clone(), l[0]["moneySeen"].clone()), (json!(2), json!(1)));
        // An empty wallet name never overwrites a known one.
        remember(&db, &env("com.bcp.yape"), &money(Some(""))).await.unwrap();
        assert_eq!(local(&db).await[0]["wallet"], "Yape");
    }

    #[tokio::test]
    async fn report_is_throttled_unless_money() {
        let m = Mock::start().await;
        m.on("/v1/sources", json!({"ok": true}));
        let s = testkit::state_on(&m).await;
        // Unknown package: nothing to report.
        report(&s, &env("com.x"), &promo(), "PE").await;
        assert!(m.seen_path("/v1/sources").is_empty());
        remember(&s.db, &env("com.x"), &promo()).await.unwrap();
        report(&s, &env("com.x"), &promo(), "PE").await;
        report(&s, &env("com.x"), &promo(), "PE").await;
        assert_eq!(m.seen_path("/v1/sources").len(), 1, "once a day");
        let body = &m.seen_path("/v1/sources")[0].body;
        assert_eq!((body["class"].clone(), body["country"].clone()), (json!("not_money"), json!("PE")));
        // Money is always reported.
        report(&s, &env("com.x"), &money(Some("Yape")), "PE").await;
        assert_eq!(m.seen_path("/v1/sources").len(), 2);
    }

    #[tokio::test]
    async fn priors_come_from_the_network_or_nothing() {
        let m = Mock::start().await;
        m.on("/v1/sources", json!({"sources": [{"package": "com.bcp.yape"}]}));
        let s = testkit::state_on(&m).await;
        assert_eq!(priors(&s, "PE").await.len(), 1);
        assert!(m.seen()[0].path.ends_with("?country=PE"));
        assert!(priors(&*testkit::state().await, "PE").await.is_empty());
    }
}
