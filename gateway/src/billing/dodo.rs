//! Dodo Payments — merchant of record for everyone outside Perú. We open a
//! hosted checkout session for the plan's product and learn about the
//! money from the webhook (Standard Webhooks signature).

use serde_json::{json, Value};

pub struct Dodo {
    http: reqwest::Client,
    base: String,
    key: String,
    webhook_secret: String,
    products: std::collections::HashMap<&'static str, String>,
    return_url: String,
}

impl Dodo {
    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Option<Self>> {
        let key = match std::env::var("DODO_API_KEY") { Ok(k) if !k.trim().is_empty() => k, _ => return Ok(None) };
        let mut products = std::collections::HashMap::new();
        // One product per tier, plus an optional one-sol credit product
        // (`DODO_PRODUCT_CREDITS`, quantity = soles) for card recargas.
        for t in ["pro", "max", "custom", "enterprise", "credits"] {
            if let Ok(p) = std::env::var(format!("DODO_PRODUCT_{}", t.to_uppercase())) { if !p.trim().is_empty() { products.insert(t, p); } }
        }
        if products.is_empty() {
            // Plans are retired (prepaid credits, gateway/src/prepaid.rs): a key
            // without plan products just means no plan checkout.
            tracing::info!("DODO_API_KEY set without DODO_PRODUCT_<TIER>: plan checkout off (prepaid top-ups use DODO_TIER_*)");
            return Ok(None);
        }
        Ok(Some(Self {
            http,
            base: crate::env_or("DODO_BASE_URL", "https://live.dodopayments.com").trim_end_matches('/').to_string(),
            key,
            webhook_secret: std::env::var("DODO_WEBHOOK_SECRET").unwrap_or_default(),
            products,
            return_url: crate::env_or("BILLING_RETURN_URL", "https://agente.ceo/app"),
        }))
    }

    /// `POST /checkouts` → hosted URL. `quantity` = months for one-off
    /// products (subscription products renew monthly on their own); for
    /// `credits`, `months` carries céntimos and quantity is whole soles.
    pub async fn checkout_url(&self, plan: &str, months: i64, email: &str, name: Option<&str>, country: &str, order_id: &str, account: &str) -> anyhow::Result<String> {
        let product = self.products.get(plan).ok_or_else(|| anyhow::anyhow!("no Dodo product for {plan}"))?;
        let quantity = if plan == "credits" { (months / 100).max(1) } else { months.max(1) };
        let body = json!({
            "product_cart": [{"product_id": product, "quantity": quantity}],
            "customer": {"email": email, "name": name.unwrap_or("")},
            "billing_address": {"country": country},
            "return_url": format!("{}?checkout={order_id}", self.return_url),
            "metadata": {"checkout": order_id, "account": account, "plan": plan, "months": months.to_string()},
        });
        let r = self.http.post(format!("{}/checkouts", self.base)).bearer_auth(&self.key).json(&body).send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        anyhow::ensure!(status.is_success(), "dodo {status}: {v}");
        v["checkout_url"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("no checkout_url: {v}"))
    }

    /// Standard Webhooks: `webhook-signature` = HMAC-SHA256(secret, "{id}.{ts}.{body}").
    /// Dodo documents hex; Standard Webhooks uses base64 with a `v1,` prefix —
    /// both are accepted.
    pub fn verify(&self, id: &str, ts: &str, signature: &str, body: &[u8]) -> bool {
        use hmac::{Hmac, Mac};
        if self.webhook_secret.is_empty() { return false; }
        let secret = self.webhook_secret.strip_prefix("whsec_").unwrap_or(&self.webhook_secret);
        let key_bytes = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(secret).unwrap_or_else(|_| secret.as_bytes().to_vec())
        };
        let mut mac = match Hmac::<sha2::Sha256>::new_from_slice(&key_bytes) { Ok(m) => m, Err(_) => return false };
        mac.update(format!("{id}.{ts}.").as_bytes());
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let hex = hex::encode(digest);
        let b64 = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode(digest) };
        signature.split(' ').any(|s| {
            let s = s.trim();
            let s = s.strip_prefix("v1,").unwrap_or(s);
            yaya_wire::secret::ct_eq(s, &hex) || yaya_wire::secret::ct_eq(s, &b64)
        })
    }
}

#[cfg(test)]
impl Dodo {
    pub fn for_test(webhook_secret: &str) -> Self {
        Self {
            http: reqwest::Client::new(), base: String::new(), key: String::new(),
            webhook_secret: webhook_secret.to_string(), products: Default::default(), return_url: String::new(),
        }
    }

    pub fn at_for_test(base: &str, webhook_secret: &str) -> Self {
        let mut d = Self::for_test(webhook_secret);
        d.base = base.trim_end_matches('/').to_string();
        d.key = "dk".into();
        for (t, p) in [("pro", "prod_pro"), ("max", "prod_max"), ("credits", "prod_credits")] { d.products.insert(t, p.into()); }
        d
    }

    /// What Dodo would put in `webhook-signature` for this body.
    pub fn sign_for_test(&self, id: &str, ts: &str, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use base64::Engine;
        let secret = self.webhook_secret.strip_prefix("whsec_").unwrap_or(&self.webhook_secret);
        let key = base64::engine::general_purpose::STANDARD.decode(secret).unwrap_or_else(|_| secret.as_bytes().to_vec());
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
        mac.update(format!("{id}.{ts}.").as_bytes());
        mac.update(body);
        format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Mock;

    #[tokio::test]
    async fn checkouts_name_the_product_and_come_back_to_the_order() {
        let m = Mock::start().await;
        m.on("/checkouts", json!({"checkout_url": "https://pay.dodo/x"}));
        let mut d = Dodo::for_test("s");
        d.base = m.base.clone();
        d.key = "dk".into();
        d.return_url = "https://agento.ceo/app".into();
        d.products.insert("pro", "prod_pro".into());
        d.products.insert("credits", "prod_cr".into());
        assert_eq!(d.checkout_url("pro", 3, "a@b.com", Some("Ann"), "US", "chk_1", "acct").await.unwrap(), "https://pay.dodo/x");
        let b = m.seen_path("/checkouts").pop().unwrap();
        assert_eq!(b.headers["authorization"], "Bearer dk");
        assert_eq!((b.body["product_cart"][0]["quantity"].clone(), b.body["return_url"].clone()), (json!(3), json!("https://agento.ceo/app?checkout=chk_1")));
        d.checkout_url("credits", 2_550, "a@b.com", None, "US", "chk_2", "acct").await.unwrap();
        assert_eq!(m.seen_path("/checkouts").pop().unwrap().body["product_cart"][0]["quantity"], 25, "credits: whole soles");
        assert!(d.checkout_url("max", 1, "a@b.com", None, "US", "c", "a").await.is_err(), "no product, no checkout");
        m.on_status("/checkouts", 422, json!({"message": "bad country"}));
        assert!(d.checkout_url("pro", 1, "a@b.com", None, "XX", "c", "a").await.is_err());
    }

    #[test]
    fn signatures_accept_hex_and_standard_webhooks_base64() {
        use base64::Engine;
        let secret = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(b"topsecret"));
        let d = Dodo::for_test(&secret);
        let sig = d.sign_for_test("msg_1", "1700000000", b"{}");
        assert!(d.verify("msg_1", "1700000000", &sig, b"{}"));
        assert!(d.verify("msg_1", "1700000000", &format!("v1,bogus {sig}"), b"{}"), "any listed signature may match");
        assert!(!d.verify("msg_2", "1700000000", &sig, b"{}"));
        assert!(!d.verify("msg_1", "1700000000", &sig, b"{\"x\":1}"));
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"topsecret").unwrap();
        mac.update(b"msg_1.1700000000.{}");
        assert!(d.verify("msg_1", "1700000000", &hex::encode(mac.finalize().into_bytes()), b"{}"), "Dodo's documented hex form");
        assert!(!Dodo::for_test("").verify("msg_1", "1", &sig, b"{}"));
    }
}
