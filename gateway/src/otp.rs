//! One-time codes over WhatsApp and email — the whole of Yaya ID's proof.
//! Both channels are optional at boot (`WA_BRIDGE_URL`/`WA_BRIDGE_KEY`,
//! `SMTP_URL`/`MAIL_FROM`); a code goes out on every channel we have for
//! the person, and the check accepts it from either.

use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::Value;

pub struct Delivery {
    wa: Option<(reqwest::Client, String, String)>,
    mail: Option<(AsyncSmtpTransport<Tokio1Executor>, String)>,
}

impl Delivery {
    pub fn from_env() -> anyhow::Result<Self> {
        let wa = match std::env::var("WA_BRIDGE_URL") {
            Ok(u) if !u.trim().is_empty() => {
                let key = std::env::var("WA_BRIDGE_KEY").map_err(|_| anyhow::anyhow!("WA_BRIDGE_KEY must be set with WA_BRIDGE_URL"))?;
                Some((reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?, u.trim_end_matches('/').to_string(), key))
            }
            _ => None,
        };
        let mail = match std::env::var("SMTP_URL") {
            Ok(u) if !u.trim().is_empty() => {
                let from = std::env::var("MAIL_FROM").map_err(|_| anyhow::anyhow!("MAIL_FROM must be set with SMTP_URL"))?;
                Some((AsyncSmtpTransport::<Tokio1Executor>::from_url(u.trim())?.build(), from.trim_matches('"').to_string()))
            }
            _ => None,
        };
        tracing::info!(whatsapp = wa.is_some(), email = mail.is_some(), "otp delivery");
        Ok(Self { wa, mail })
    }

    /// WhatsApp through a bridge at `url` (tests point it at a fake).
    #[cfg(test)]
    pub fn for_test(url: &str) -> Self {
        Self { wa: Some((reqwest::Client::new(), url.trim_end_matches('/').to_string(), "k".into())), mail: None }
    }

    pub fn any(&self) -> bool {
        self.wa.is_some() || self.mail.is_some()
    }

    /// One plain WhatsApp text to `phone` (a top-up confirmation, the
    /// "modo manual" notice). False when there is no bridge or it refused.
    pub async fn send_text(&self, phone: &str, text: &str) -> bool {
        let Some((http, url, key)) = &self.wa else { return false };
        match http.post(format!("{url}/send")).header("x-bridge-key", key).json(&serde_json::json!({"to": phone, "text": text})).send().await {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => { let b: Value = r.json().await.unwrap_or_default(); tracing::warn!(%b, "whatsapp text not sent"); false }
            Err(e) => { tracing::warn!(error = %e, "whatsapp bridge unreachable"); false }
        }
    }

    /// Sends `code` wherever we can. Returns (whatsapp_sent, email_sent).
    pub async fn send(&self, phone: Option<&str>, email: Option<&str>, code: &str) -> (bool, bool) {
        let text = format!("{code} es tu código agente. Vence en 10 minutos. No lo compartas.");
        let mut wa_ok = false;
        if let Some(to) = phone {
            wa_ok = self.send_text(to, &text).await;
        }
        let mut mail_ok = false;
        if let (Some((smtp, from)), Some(to)) = (&self.mail, email) {
            let msg = Message::builder()
                .from(from.parse().unwrap_or_else(|_| "agente <no-reply@yaya.tech>".parse().unwrap()))
                .to(match to.parse() { Ok(m) => m, Err(_) => return (wa_ok, false) })
                .subject(format!("{code} es tu código agente"))
                .body(format!("{text}\n\nSi no pediste este código, ignora este correo."));
            match msg {
                Ok(m) => match smtp.send(m).await {
                    Ok(_) => mail_ok = true,
                    Err(e) => tracing::warn!(error = %e, "email otp not sent"),
                },
                Err(e) => tracing::warn!(error = %e, "email otp not built"),
            }
        }
        (wa_ok, mail_ok)
    }
}

/// Bare E.164 digits, no `+`. A 9-digit number starting with 9 is a
/// Peruvian mobile and gets 51; everything else must carry its country
/// code — guessing one would send someone else's phone a code.
pub fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_start_matches('0').to_string();
    match digits.len() {
        9 if digits.starts_with('9') => Some(format!("51{digits}")),
        10..=15 => Some(digits),
        _ => None,
    }
}

pub fn six_digits() -> String {
    let mut b = [0u8; 4];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

#[cfg(test)]
mod tests {
    #[test]
    fn phones() {
        assert_eq!(super::normalize_phone("999 888 777").as_deref(), Some("51999888777"));
        assert_eq!(super::normalize_phone("+51 999 888 777").as_deref(), Some("51999888777"));
        assert_eq!(super::normalize_phone("+1 (650) 555-0100").as_deref(), Some("16505550100"));
        assert!(super::normalize_phone("12345").is_none());
        assert_eq!(super::six_digits().len(), 6);
    }
}
