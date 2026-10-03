//! `share_photos`: the customer agent hands over catalog photos as a private
//! link (D15). Core-mounted for every business — the tool answers honestly
//! (`no_photos`) when there is nothing to show, so a services business never
//! promises pictures it does not have.

use serde_json::{json, Value};

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Media;

async fn share_photos(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let products: Vec<String> = args["products"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let note = args["note"].as_str().map(str::trim).filter(|s| !s.is_empty());
    match crate::media::share(ctx.state, ctx.business_id, &[], &products, note).await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!("share_photos failed: {e}");
            Ok(json!({"status": "unavailable",
                      "note": "the private link could not be created right now — describe the product in words and offer to send photos later"}))
        }
    }
}

impl Plugin<Agente> for Media {
    fn name(&self) -> &'static str {
        "media"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, true),
            json!({"type": "function", "function": {
                "name": "share_photos",
                "description": "Show the customer photos from the business's catalog: models, colors, sizes, the menu, the place — whenever they ask to SEE something or a picture would sell better than words. \
                    Returns a PRIVATE link that expires in a few minutes: paste the url VERBATIM in your reply (no markdown) and say it lasts only a few minutes. \
                    Pass the product names they are asking about; omit for the whole catalog. If it returns no_photos, describe in words instead — never invent a link.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "products": {"type": "array", "items": {"type": "string"}, "description": "catalog product names to include (as the customer or the catalog names them)"},
                        "note": {"type": "string", "description": "one short line shown on the page, e.g. 'Tallas S a XL, envío a todo Lima'"}
                    },
                    "required": []
                }
            }}),
            tool_fn(|c, a| Box::pin(share_photos(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, tool_ctx, Mock};

    #[tokio::test]
    async fn share_photos_mints_a_link_or_says_why_not() {
        let m = Mock::start().await;
        m.on("/v1/drop", json!({"url": "https://drop/1", "expiresAt": "t"}));
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        assert_eq!(share_photos(&c, &json!({})).await.unwrap()["status"], "no_photos");
        crate::media::store(&s.db, b, Some("Polo"), None, "image/png", &[1]).await.unwrap();
        let r = share_photos(&c, &json!({"products": ["polo"], "note": " tallas S-XL "})).await.unwrap();
        assert_eq!(r["url"], "https://drop/1");
        assert_eq!(m.seen_path("/v1/drop")[0].body["note"], "tallas S-XL");
        let off = testkit::state().await;
        let b2 = testkit::business(&off.db).await;
        crate::media::store(&off.db, b2, Some("Polo"), None, "image/png", &[1]).await.unwrap();
        assert_eq!(share_photos(&tool_ctx(&off, b2, Some("p")).await, &json!({})).await.unwrap()["status"], "unavailable");
    }
}
