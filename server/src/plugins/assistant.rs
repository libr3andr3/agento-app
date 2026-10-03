//! The consumer app's agent: what the personal assistant can do beyond
//! talking. Phase 1 is memory — `remember` / `forget` keep a small profile
//! of the phone's owner in their own row, so every conversation starts
//! knowing who it is talking to. Phase 2 mounts the yaya-network tools
//! (find businesses, ask/book through their agents) in this same scope.

use serde_json::{json, Value};

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Assistant;

fn spec(name: &str, desc: &str, params: Value) -> Value {
    json!({"type": "function",
           "function": {"name": name, "description": desc, "parameters": params}})
}

/// Keys the model may not persist, whatever it was told. A personal agent
/// that stores a card number "to be helpful" is a liability on a lost phone.
const FORBIDDEN: &[&str] = &["password", "contraseña", "clave", "pin", "card", "tarjeta", "cvv", "token", "secret"];

/// Whole words of a text (letters and digits), lowercased.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// A secret word as a whole word (`bank_pin`, `card_number`), not a
/// fragment of another one (`shopping_list`, `cardio_days`).
fn names_a_secret(s: &str) -> bool {
    words(s).iter().any(|w| FORBIDDEN.contains(&w.as_str()) || w == "cvc" || w == "password" || w == "passwd")
}

fn key_ok(key: &str) -> bool {
    let k = key.to_lowercase();
    !k.is_empty()
        && k.len() <= 48
        && k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        && !names_a_secret(&k)
        && !FORBIDDEN.iter().any(|f| f.len() > 4 && k.contains(f)) // "wifipassword", "mitarjeta"
}

/// Values that look like credentials are refused whatever the key says:
/// "mi pin es 1234" under `note` is still a PIN on a lost phone.
fn value_ok(value: &str) -> bool {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    let card_like = digits.len() >= 13 && digits.len() <= 19;
    let pin_like = names_a_secret(value) && digits.len() >= 3;
    !(card_like || pin_like)
}

async fn patch_profile(ctx: &ToolCtx<'_>, edit: impl FnOnce(&mut Value)) -> anyhow::Result<Value> {
    let (raw,): (String,) =
        sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
            .bind(ctx.business_id)
            .fetch_one(&ctx.state.db)
            .await?;
    let mut patch: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    if !patch["profile"].is_object() {
        patch["profile"] = json!({});
    }
    edit(&mut patch["profile"]);
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(patch.to_string())
        .bind(ctx.business_id)
        .execute(&ctx.state.db)
        .await?;
    Ok(patch["profile"].clone())
}

async fn remember(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let key = args["key"].as_str().unwrap_or("").trim().to_string();
    let value = args["value"].as_str().unwrap_or("").trim().to_string();
    if !key_ok(&key) {
        return Ok(json!({"error": "key rejected: use a short snake_case label and never store secrets"}));
    }
    if value.is_empty() || value.chars().count() > 300 {
        return Ok(json!({"error": "value must be 1-300 characters"}));
    }
    if !value_ok(&value) {
        return Ok(json!({"error": "value rejected: looks like a card number, PIN or password — never store those"}));
    }
    let profile = patch_profile(ctx, |p| p[&key] = json!(value)).await?;
    Ok(json!({"status": "ok", "profile": profile}))
}

async fn forget(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let key = args["key"].as_str().unwrap_or("").trim().to_string();
    let profile = patch_profile(ctx, |p| {
        if let Some(m) = p.as_object_mut() {
            m.remove(&key);
        }
    })
    .await?;
    Ok(json!({"status": "ok", "profile": profile}))
}

impl Plugin<Agente> for Assistant {
    fn name(&self) -> &'static str {
        "assistant"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Assistant, true),
            spec(
                "remember",
                "Save one durable fact about the person you're assisting (name, city, language, \
                 preferences, ongoing plans) so future conversations know it. Keys are short \
                 snake_case labels (e.g. name, city, diet, kids). Never store passwords, cards or secrets.",
                json!({"type": "object",
                       "properties": {"key": {"type": "string"}, "value": {"type": "string"}},
                       "required": ["key", "value"]}),
            ),
            tool_fn(|c, a| Box::pin(remember(c, a))),
        )?;
        k.tool(
            meta(Scope::Assistant, true),
            spec(
                "forget",
                "Remove a previously remembered fact by key, when the person asks you to forget it or it is no longer true.",
                json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}),
            ),
            tool_fn(|c, a| Box::pin(forget(c, a))),
        )?;
        Ok(())
    }
}

/// Ensures the row the personal assistant lives in (one per installation)
/// and returns its id. `learning::compose` needs a business row to layer
/// core defaults + locale + patch; the patch here is just `profile`.
pub async fn ensure_self(db: &sqlx::SqlitePool, country: &str, language: Option<&str>) -> anyhow::Result<uuid::Uuid> {
    if let Some((id,)) = sqlx::query_as::<_, (uuid::Uuid,)>(
        "SELECT id FROM businesses WHERE owner_phone = 'self' AND industry = 'assistant' LIMIT 1",
    )
    .fetch_optional(db)
    .await?
    {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4();
    // The device language wins over the country's default (a Quechua or
    // English speaker in Peru gets answered in their language).
    let patch = match language.map(|l| l.trim().to_lowercase()).filter(|l| l.len() == 2) {
        Some(l) => json!({"language": l}),
        None => json!({}),
    };
    sqlx::query(
        "INSERT INTO businesses (id, name, industry, owner_phone, country, onboarded, bundle, schema_config) \
         VALUES ($1, 'me', 'assistant', 'self', $2, 1, 'generic@1', $3)",
    )
    .bind(id)
    .bind(country.trim().to_ascii_uppercase())
    .bind(patch.to_string())
    .execute(db)
    .await?;
    tracing::info!(%id, country, "personal assistant profile created");
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_become_profile_keys() {
        assert!(key_ok("name"));
        assert!(key_ok("home_city"));
        assert!(!key_ok("wifi_password"));
        assert!(!key_ok("tarjeta"));
        assert!(!key_ok("has spaces"));
        assert!(!key_ok(""));
    }

    use crate::testkit::{self, tool_ctx};

    #[test]
    fn harmless_words_that_contain_secret_words_are_fine() {
        assert!(key_ok("shopping_list"), "'shopping' is not a pin");
        assert!(key_ok("cardio_days"), "'cardio' is not a card");
        assert!(!key_ok("bank_pin") && !key_ok("card_number") && !key_ok("api_token"));
        assert!(value_ok("voy a pintar el cuarto en 2024"));
        assert!(value_ok("cumpleaños de mamá: 12 de mayo"));
        assert!(!value_ok("mi pin es 4321"));
        assert!(!value_ok("la clave del wifi es 12345678"));
        assert!(!value_ok("4111 1111 1111 1111"));
        assert!(!value_ok("cvv 123 y 4567"));
    }

    #[tokio::test]
    async fn remember_and_forget_live_in_the_owners_own_row() {
        let s = testkit::state_with(testkit::Opts { client_mode: true, ..Default::default() }).await;
        let me = ensure_self(&s.db, "PE", Some("es")).await.unwrap();
        assert_eq!(ensure_self(&s.db, "PE", None).await.unwrap(), me, "one row per installation");
        let c = tool_ctx(&s, me, None).await;
        assert_eq!(remember(&c, &json!({"key": "city", "value": " Lima "})).await.unwrap()["profile"], json!({"city": "Lima"}));
        assert!(remember(&c, &json!({"key": "city", "value": ""})).await.unwrap()["error"].is_string());
        assert!(remember(&c, &json!({"key": "note", "value": "x".repeat(301)})).await.unwrap()["error"].is_string());
        assert!(remember(&c, &json!({"key": "tarjeta", "value": "x"})).await.unwrap()["error"].is_string());
        assert!(remember(&c, &json!({"key": "note", "value": "mi pin es 4321"})).await.unwrap()["error"].is_string());
        assert_eq!(forget(&c, &json!({"key": "city"})).await.unwrap()["profile"], json!({}));
        assert_eq!(forget(&c, &json!({"key": "never-set"})).await.unwrap()["status"], "ok");
    }
}
