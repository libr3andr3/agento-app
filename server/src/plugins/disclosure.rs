//! AI-transparency behavior (Peru Ley 31814 / DS 115-2025-PCM, and similar
//! disclosure rules elsewhere). Mounted like any other capability: unload the
//! plugin and the behavior is gone, no route code involved.
//!
//! Two prompt effects on the customer agent:
//! - always: never pose as a person; confirm being an AI when asked.
//! - on `firstContact` (no stored history for this peer): the reply must open
//!   with a localized "I'm {business}'s AI assistant" line. Network peers
//!   (`agent:<pk>`) are other agents, not people — no disclosure owed there.

use serde_json::json;

use crate::harness::{hook_fn, Agente, Kernel, Plugin};

pub struct Disclosure;

/// The first-contact line, in `lang`, naming the business.
pub async fn line(db: &sqlx::SqlitePool, business_id: uuid::Uuid, lang: &str) -> String {
    let name: Option<(String,)> = sqlx::query_as("SELECT name FROM businesses WHERE id = $1")
        .bind(business_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let name = name.map(|r| r.0).filter(|n| !n.trim().is_empty());
    let generic = if lang == "es" { "este negocio" } else { "this business" };
    crate::locale::t(lang, "ai_disclosure").replace("{business}", name.as_deref().unwrap_or(generic))
}

/// Whether a reply to `peer` must open with the disclosure: a person (not
/// another agent) writing for the first time.
pub fn owed(first_contact: bool, peer: &str) -> bool {
    first_contact && !peer.starts_with("agent:")
}

impl Plugin<Agente> for Disclosure {
    fn name(&self) -> &'static str {
        "disclosure"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.on(
            "prompt/customer",
            hook_fn(|rt, mut payload| {
                Box::pin(async move {
                    let mut section = String::from(
                        "TRANSPARENCY: you are the business's AI agent — never claim to \
                         be a person, and if asked whether they're talking to a bot, \
                         confirm it warmly and move on.",
                    );
                    let human = payload["peer"]
                        .as_str()
                        .map_or(true, |p| !p.starts_with("agent:"));
                    if payload["firstContact"] == true && human {
                        let name: Option<(String,)> =
                            sqlx::query_as("SELECT name FROM businesses WHERE id = $1")
                                .bind(rt.business_id)
                                .fetch_optional(&rt.state.db)
                                .await
                                .ok()
                                .flatten();
                        let name = name.map(|r| r.0).filter(|n| !n.trim().is_empty());
                        let lang = payload["lang"].as_str().unwrap_or("es");
                        // Onboarding always records a name; defensive only.
                        let generic =
                            if lang == "es" { "este negocio" } else { "this business" };
                        let line = crate::locale::t(lang, "ai_disclosure")
                            .replace("{business}", name.as_deref().unwrap_or(generic));
                        section.push_str(&format!(
                            "\nFIRST CONTACT: this is this customer's first-ever message \
                             to the business, and the law requires disclosing up front \
                             that they are talking to an AI. Your reply MUST begin with \
                             exactly this line, then continue naturally on the next line: \
                             \"{line}\""
                        ));
                    }
                    payload["sections"]
                        .as_array_mut()
                        .map(|s| s.push(json!(section)));
                    Ok(payload)
                })
            }),
        );
        Ok(())
    }
}
