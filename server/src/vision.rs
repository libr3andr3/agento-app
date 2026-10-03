//! Catalog extraction from a photo, via an OpenRouter-hosted vision model.
//!
//! The owner points their camera at a menu or price list instead of typing it
//! item by item into the onboarding chat — for a bodega with eighty products
//! that interview never finished. The model reads the photo and returns
//! name+price pairs; routes.rs merges them into the client patch exactly like
//! a save_business_schema call would.
//!
//! Spatially gated like whatsapp.rs: mounts only when VISION_API_KEY is set,
//! and /api/catalog_photo answers 503 otherwise. The main LLM (DeepSeek) has
//! no image input, hence a second provider rather than a second use of
//! `llm::Llm`.

use anyhow::anyhow;
use serde_json::Value;

pub struct Vision {
    http: reqwest::Client,
    upstream: crate::upstream::Upstream,
    model: String,
}

impl Vision {
    /// Own key → the provider directly; otherwise the yaya.tech gateway,
    /// proven by the agent identity (the phone's normal case). `VISION=0`
    /// turns the feature off.
    pub fn from_env(identity: &crate::identity::Identity) -> anyhow::Result<Option<Self>> {
        if std::env::var("VISION").map(|v| v == "0").unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(Self {
            http: crate::net::client(std::time::Duration::from_secs(90)),
            upstream: crate::upstream::Upstream::resolve(
                "VISION_API_KEY", "VISION_BASE_URL", "https://api.deepseek.com/v1", "/v1/vision", identity,
            ),
            model: std::env::var("VISION_MODEL").unwrap_or_else(|_| "deepseek-v4-flash-vision-exp".into()),
        }))
    }

    /// One photo in, name+price pairs out. The image travels inline as a data
    /// URL — OpenRouter accepts that on the standard chat-completions shape,
    /// so no upload step and nothing to clean up afterwards.
    pub async fn extract_catalog(&self, image: &[u8], currency: &str) -> anyhow::Result<Vec<(String, f64)>> {
        use base64::Engine;
        let data_url = format!(
            "data:{};base64,{}",
            sniff_mime(image),
            base64::engine::general_purpose::STANDARD.encode(image)
        );
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text":
                    format!("This is a photo of a business's catalog, menu or price list \
                     (prices in {currency}). Extract EVERY item that has a \
                     visible price. Respond ONLY with JSON, no prose: \
                     {{\"items\":[{{\"name\":\"...\",\"price\":18.0}}]}}. Item names \
                     exactly as printed (fix obvious OCR errors), price as a plain \
                     number without currency symbol. Skip items whose price is not \
                     visible.")},
                {"type": "image_url", "image_url": {"url": data_url}}
            ]}],
            // A dense menu is easily a hundred items; a tight cap truncates the
            // JSON mid-array and the whole extraction parses to nothing.
            "max_tokens": 2000,
        });
        // OpenRouter can answer 200 with an {"error": ...} body and no choices
        // when the shared upstream pool is momentarily rate-limited. One short
        // retry rides out that blip; a second failure is reported as-is.
        let mut last_err = None;
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            let resp = self.upstream.post_json(&self.http, "/chat/completions", &body).send().await?;
            let status = resp.status();
            let payload: Value = resp.json().await?;
            match payload["choices"][0]["message"]["content"].as_str() {
                Some(reply) if status.is_success() => return Ok(parse_items(reply)),
                _ => last_err = Some(anyhow!("vision API {status}: {payload}")),
            }
        }
        Err(last_err.expect("loop ran"))
    }
}

/// JPEG and PNG are what Android's camera and gallery hand over; anything
/// unrecognized is labeled jpeg and left for the model to reject.
fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else {
        "image/jpeg"
    }
}

/// A price as the model wrote it: a number, or text like "25.50", "S/ 4" or
/// "1,50" (a decimal comma). Text without a number is no price.
fn price_of(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let s = v.as_str()?;
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let num: String = s[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
    num.replace(',', ".").trim_end_matches('.').parse().ok()
}

/// Turns the model's reply into clean (name, price) pairs.
///
/// Instruction-following is approximate: the JSON arrives bare, fenced in
/// markdown, or wrapped in a sentence of prose, so the parser peels fences and
/// falls back to the outermost braces before giving up. Zero and negative
/// prices are dropped (a "0" is the model transcribing a missing price, not a
/// free item), and names are deduped case-insensitively — menus repeat items
/// across sections and the first printing wins.
pub fn parse_items(reply: &str) -> Vec<(String, f64)> {
    let mut s = reply.trim();
    if let Some(rest) = s.strip_prefix("```") {
        s = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = s.rfind("```") {
            s = &s[..end];
        }
        s = s.trim();
    }
    let parsed: Option<Value> = serde_json::from_str(s).ok().or_else(|| {
        let start = s.find('{')?;
        let end = s.rfind('}')?;
        serde_json::from_str(&s[start..=end]).ok()
    });
    let Some(doc) = parsed else { return Vec::new() };

    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for it in doc["items"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(name) = it["name"].as_str().map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        let Some(price) = price_of(&it["price"]).filter(|p| *p > 0.0) else {
            continue;
        };
        if seen.insert(name.to_lowercase()) {
            items.push((name.to_string(), price));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json() {
        let items = parse_items(
            r#"{"items":[{"name":"Pollo a la brasa 1/4","price":18.0},{"name":"Inca Kola 1L","price":7.5}]}"#,
        );
        assert_eq!(items, vec![
            ("Pollo a la brasa 1/4".to_string(), 18.0),
            ("Inca Kola 1L".to_string(), 7.5),
        ]);
    }

    #[test]
    fn fenced_json() {
        let items = parse_items(
            "```json\n{\"items\":[{\"name\":\"Ceviche\",\"price\":25}]}\n```",
        );
        assert_eq!(items, vec![("Ceviche".to_string(), 25.0)]);
    }

    #[test]
    fn json_wrapped_in_prose() {
        let items = parse_items(
            "Here is the extraction: {\"items\":[{\"name\":\"Lomo saltado\",\"price\":22}]} — done.",
        );
        assert_eq!(items, vec![("Lomo saltado".to_string(), 22.0)]);
    }

    #[test]
    fn garbage_is_empty_not_an_error() {
        assert!(parse_items("I cannot see any menu in this image.").is_empty());
        assert!(parse_items("").is_empty());
        assert!(parse_items("{\"items\": \"none\"}").is_empty());
    }

    #[test]
    fn zero_and_negative_prices_are_dropped() {
        let items = parse_items(
            r#"{"items":[{"name":"Agua","price":0},{"name":"Rocoto","price":-3},{"name":"Chicha","price":4}]}"#,
        );
        assert_eq!(items, vec![("Chicha".to_string(), 4.0)]);
    }

    #[test]
    fn duplicate_names_dedupe_case_insensitively() {
        let items = parse_items(
            r#"{"items":[{"name":"Ceviche","price":25},{"name":"CEVICHE","price":28},{"name":"Tiradito","price":26}]}"#,
        );
        assert_eq!(items, vec![
            ("Ceviche".to_string(), 25.0),
            ("Tiradito".to_string(), 26.0),
        ]);
    }

    #[test]
    fn mime_sniffing() {
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a]), "image/png");
        assert_eq!(sniff_mime(&[0xff, 0xd8, 0xff, 0xe0]), "image/jpeg");
        assert_eq!(sniff_mime(b"not an image"), "image/jpeg", "jpeg is the default guess");
    }

    use crate::testkit::Mock;
    use serde_json::json;

    fn vision(m: &Mock) -> Vision {
        Vision { http: reqwest::Client::new(), upstream: crate::upstream::Upstream::Direct { base: format!("{}/v1", m.base), key: "vk".into() }, model: "vm".into() }
    }

    #[test]
    fn prices_written_as_text_are_kept() {
        // Models often quote numbers or keep the symbol despite the instruction.
        let items = parse_items(r#"{"items":[{"name":"Ceviche","price":"25.50"},{"name":"Chicha","price":"S/ 4"},{"name":"Agua","price":"gratis"},{"name":"Pan","price":"1,50"}]}"#);
        assert_eq!(items, vec![("Ceviche".to_string(), 25.5), ("Chicha".to_string(), 4.0), ("Pan".to_string(), 1.5)]);
    }

    #[test]
    fn blank_names_are_skipped() {
        assert!(parse_items(r#"{"items":[{"name":"  ","price":3},{"price":3}]}"#).is_empty());
    }

    #[test]
    fn on_by_default_through_the_gateway() {
        let v = Vision::from_env(&crate::identity::Identity::ephemeral()).unwrap().unwrap();
        assert!(v.upstream.is_gateway());
        assert_eq!(v.model, "deepseek-v4-flash-vision-exp");
    }

    #[tokio::test]
    async fn extract_sends_the_photo_inline_and_parses() {
        let m = Mock::start().await;
        m.say(r#"{"items":[{"name":"Lomo","price":22}]}"#);
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
        assert_eq!(vision(&m).extract_catalog(&png, "PEN").await.unwrap(), vec![("Lomo".to_string(), 22.0)]);
        let b = &m.seen()[0].body;
        assert_eq!(b["model"], "vm");
        assert!(b["messages"][0]["content"][0]["text"].as_str().unwrap().contains("prices in PEN"));
        assert!(b["messages"][0]["content"][1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(m.seen()[0].headers["authorization"], "Bearer vk");
    }

    #[tokio::test]
    async fn a_200_without_choices_is_retried_once() {
        let m = Mock::start().await;
        m.on("/chat/completions", json!({"error": {"message": "rate limited upstream"}}));
        m.say(r#"{"items":[{"name":"Pan","price":1}]}"#);
        assert_eq!(vision(&m).extract_catalog(b"x", "PEN").await.unwrap().len(), 1);
        assert_eq!(m.seen().len(), 2);
        let m = Mock::start().await;
        m.on_status("/chat/completions", 500, json!({"error": "down"}));
        let e = vision(&m).extract_catalog(b"x", "PEN").await.unwrap_err().to_string();
        assert!(e.contains("vision API 500"), "{e}");
        assert_eq!(m.seen().len(), 2);
    }
}
