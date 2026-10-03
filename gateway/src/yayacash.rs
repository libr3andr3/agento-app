//! yaya.cash — our own payment processor. A business plan is paid by Yape/
//! Plin to Yaya's number; yaya.cash's phone app sees the push notification
//! and confirms the open charge whose céntimo-tagged amount matches. The
//! gateway opens that charge and polls it; nothing is trusted from the
//! client.
//!
//! Auth: a dashboard session JWT (`POST /api/auth/session`, 12 h) minted
//! from YAYACASH_USER / YAYACASH_PASSWORD and cached — the account's `ysk_`
//! key must never be rotated by us because the phone holds it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub struct YayaCash {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
    token: Mutex<Option<(Instant, String)>>,
}

impl YayaCash {
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let user = std::env::var("YAYACASH_USER").ok().filter(|s| !s.is_empty())?;
        let password = std::env::var("YAYACASH_PASSWORD").ok().filter(|s| !s.is_empty())?;
        let base = std::env::var("YAYACASH_URL").ok().filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://yaya.cash".into()).trim_end_matches('/').to_string();
        Some(Self { http, base, user, password, token: Mutex::new(None) })
    }

    async fn token(&self, force: bool) -> anyhow::Result<String> {
        if !force {
            let t = self.token.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, tok)) = t.as_ref() {
                if at.elapsed() < Duration::from_secs(10 * 3600) {
                    return Ok(tok.clone());
                }
            }
        }
        let v: Value = self.http.post(format!("{}/api/auth/session", self.base))
            .json(&json!({"userId": self.user, "password": self.password}))
            .send().await?.json().await?;
        let tok = v["token"].as_str().ok_or_else(|| anyhow::anyhow!("yaya.cash session: {v}"))?.to_string();
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), tok.clone()));
        Ok(tok)
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        for attempt in 0..2 {
            let tok = self.token(attempt > 0).await?;
            let mut req = self.http.request(method.clone(), format!("{}/api{}", self.base, path)).bearer_auth(tok);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            if status == 401 && attempt == 0 {
                continue;
            }
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            return Ok((status, v));
        }
        unreachable!()
    }

    /// Opens a charge for `price_minor` (+ a céntimo tag so the amount is
    /// unique among open charges; a dozen tries bounds the fan-out when the
    /// processor is busy). Returns (payment id, amount in minor units).
    pub async fn open_charge(&self, reference: &str, price_minor: i64, currency: &str, meta: Value) -> anyhow::Result<(String, i64)> {
        for tag in 1..=12i64 {
            let amount = price_minor + tag;
            let (status, v) = self.call(reqwest::Method::POST, "/payments/service", Some(&json!({
                "amount": amount, "currency": currency, "method": "yape", "reference": reference, "meta": meta,
            }))).await?;
            match status {
                201 => return Ok((v["paymentId"].as_str().unwrap_or("").to_string(), amount)),
                409 if v["error"].as_str().is_some_and(|e| e.contains("amount")) => continue,
                _ => anyhow::bail!("yaya.cash {status}: {v}"),
            }
        }
        anyhow::bail!("no free céntimo tag for this price")
    }

    /// `pending_service` | `confirmed_service` | `applied_subscription` | `not_found`.
    pub async fn charge_status(&self, reference: &str) -> anyhow::Result<String> {
        let (status, v) = self.call(reqwest::Method::GET, &format!("/payments/service/{reference}"), None).await?;
        if status != 200 {
            anyhow::bail!("yaya.cash {status}: {v}");
        }
        Ok(v["status"].as_str().unwrap_or("unknown").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Mock;

    fn client(m: &Mock) -> YayaCash {
        YayaCash { http: reqwest::Client::new(), base: m.base.clone(), user: "u".into(), password: "p".into(), token: Mutex::new(None) }
    }

    #[tokio::test]
    async fn charges_step_their_centimo_tag_past_busy_amounts() {
        let m = Mock::start().await;
        m.on("/api/auth/session", json!({"token": "t1"}));
        m.on_status("/api/payments/service", 409, json!({"error": "amount in use"}))
         .on_status("/api/payments/service", 409, json!({"error": "amount in use"}))
         .on_status("/api/payments/service", 201, json!({"paymentId": "pay_1"}));
        let yc = client(&m);
        let (id, amount) = yc.open_charge("YAYA-PRO-AB12", 10_000, "PEN", json!({"plan": "pro"})).await.unwrap();
        assert_eq!((id.as_str(), amount), ("pay_1", 10_003));
        let sent: Vec<i64> = m.seen_path("/api/payments/service").iter().map(|s| s.body["amount"].as_i64().unwrap()).collect();
        assert_eq!(sent, vec![10_001, 10_002, 10_003]);
        assert_eq!(m.seen_path("/api/auth/session").len(), 1, "the session is cached");
        let login = &m.seen_path("/api/auth/session")[0];
        assert_eq!(login.body, json!({"userId": "u", "password": "p"}));
        assert_eq!(m.seen_path("/api/payments/service")[0].headers["authorization"], "Bearer t1");
    }

    #[tokio::test]
    async fn a_rejected_session_logs_in_again_once() {
        let m = Mock::start().await;
        m.on("/api/auth/session", json!({"token": "old"})).on("/api/auth/session", json!({"token": "new"}));
        m.on_status("/api/payments/service/YAYA-1", 401, json!({})).on("/api/payments/service/YAYA-1", json!({"status": "confirmed_service"}));
        let yc = client(&m);
        assert_eq!(yc.charge_status("YAYA-1").await.unwrap(), "confirmed_service");
        let auths: Vec<String> = m.seen_path("/api/payments/service/YAYA-1").iter().map(|s| s.headers["authorization"].to_str().unwrap().to_string()).collect();
        assert_eq!(auths, vec!["Bearer old", "Bearer new"]);
        m.on_status("/api/payments/service/YAYA-2", 404, json!({"error": "nope"}));
        assert!(yc.charge_status("YAYA-2").await.is_err());
    }

    #[tokio::test]
    async fn busy_processors_and_bad_logins_are_errors() {
        let m = Mock::start().await;
        m.on("/api/auth/session", json!({"token": "t"}));
        m.on_status("/api/payments/service", 409, json!({"error": "amount in use"}));
        let yc = client(&m);
        assert!(yc.open_charge("R", 100, "PEN", json!({})).await.unwrap_err().to_string().contains("céntimo"));
        assert_eq!(m.seen_path("/api/payments/service").len(), 12);
        m.on_status("/api/payments/service", 500, json!({"error": "down"}));
        assert!(yc.open_charge("R", 100, "PEN", json!({})).await.is_err());
        let m2 = Mock::start().await;
        m2.on_status("/api/auth/session", 403, json!({"error": "bad password"}));
        assert!(client(&m2).charge_status("R").await.is_err());
        std::env::remove_var("YAYACASH_USER");
        assert!(YayaCash::from_env(reqwest::Client::new()).is_none());
    }
}

