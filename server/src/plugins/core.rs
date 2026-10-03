//! Baseline tools every agent has regardless of vertical.

use serde_json::{json, Value};

use crate::harness::{hook_fn, meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Core;

fn spec(name: &str, desc: &str, params: Value) -> Value {
    json!({"type": "function",
           "function": {"name": name, "description": desc, "parameters": params}})
}

async fn get_business_schema(ctx: &ToolCtx<'_>, _args: &Value) -> anyhow::Result<Value> {
    Ok(ctx.doc.clone())
}

async fn send_confirmation(_ctx: &ToolCtx<'_>, _args: &Value) -> anyhow::Result<Value> {
    // Delivery physically happens when the APK sends our reply through the
    // notification; this tool just acknowledges intent.
    Ok(json!({
        "status": "queued",
        "note": "confirmation text will be delivered in the chat reply"
    }))
}

impl Plugin<Agente> for Core {
    fn name(&self) -> &'static str {
        "core"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Both, true),
            spec("get_business_schema",
                 "The composed business schema: field definitions plus this business's values (trial values already merged in)",
                 json!({"type": "object", "properties": {}, "required": []})),
            tool_fn(|c, a| Box::pin(get_business_schema(c, a))),
        )?;
        k.tool(
            meta(Scope::Customer, true),
            spec("send_confirmation", "Queue a booking confirmation message", json!({
                "type": "object",
                "properties": {"phone": {"type": "string"}, "details": {"type": "string"}},
                "required": ["phone", "details"]
            })),
            tool_fn(|c, a| Box::pin(send_confirmation(c, a))),
        )?;

        // Capability honesty, generated from what is ACTUALLY mounted for
        // this call — so it stays true as plugins come and go, and costs a
        // few lines of context instead of a policy essay.
        let honesty = |payload: &mut Value| {
            let names = payload["tools"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let locked = payload["locked"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let locked_line = if locked.is_empty() {
                String::new()
            } else {
                format!(
                    " These additional tools UNLOCK once this customer has a booking: \
                     {locked}. You may tell customers what they enable (e.g. an email \
                     calendar invite with a reminder becomes available after booking) — \
                     but only call them after they unlock."
                )
            };
            let section = format!(
                "CAPABILITY HONESTY: your ONLY abilities are replying inside this chat and \
                 these tools: {names}.{locked_line} Never promise anything outside them: \
                 you cannot call, message anyone later, or act outside this chat. If asked \
                 for something beyond your tools, say plainly what you can and cannot do, \
                 offer the closest alternative, and log it with report_gap if available."
            );
            payload["sections"].as_array_mut().map(|s| s.push(json!(section)));
        };
        k.on(
            "prompt/customer",
            hook_fn(move |_rt, mut payload| {
                Box::pin(async move {
                    honesty(&mut payload);
                    Ok(payload)
                })
            }),
        );
        let honesty2 = honesty;
        k.on(
            "prompt/onboarding",
            hook_fn(move |_rt, mut payload| {
                Box::pin(async move {
                    honesty2(&mut payload);
                    Ok(payload)
                })
            }),
        );
        let honesty3 = honesty;
        k.on(
            "prompt/assistant",
            hook_fn(move |_rt, mut payload| {
                Box::pin(async move {
                    honesty3(&mut payload);
                    Ok(payload)
                })
            }),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, tool_ctx};

    #[tokio::test]
    async fn baseline_tools() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        assert_eq!(get_business_schema(&c, &json!({})).await.unwrap()["name"], "Tito");
        assert_eq!(send_confirmation(&c, &json!({})).await.unwrap()["status"], "queued");
    }
}
