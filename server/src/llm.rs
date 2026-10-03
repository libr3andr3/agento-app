use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Minimal OpenAI-compatible chat-completions client (DeepSeek by default).
pub struct Llm {
    http: reqwest::Client,
    upstream: crate::upstream::Upstream,
    pub model: String,
    pub thinking: Option<bool>,
    max_tokens: i64,
    /// `LLM_VERIFIED` on: every completion goes sealed to an attested
    /// enclave instead, with no plaintext fallback.
    verified: Option<Box<dyn crate::verified::VerifiedBackend>>,
}

impl Llm {
    /// Default: the yaya.tech gateway, authenticated by the agent's own
    /// identity (free tier, daily caps enforced server-side). A business that
    /// wants full sovereignty sets LLM_BASE_URL + LLM_API_KEY to any
    /// OpenAI-compatible provider — including one on its own LAN.
    pub const DEFAULT_BASE_URL: &'static str = "https://llm.yaya.tech/v1";

    pub fn from_env(identity: &crate::identity::Identity) -> Result<Self> {
        Ok(Self {
            // 90s total: long enough for a slow completion, short enough that
            // one retry still fits inside the APK's 180s read timeout.
            http: crate::net::client(std::time::Duration::from_secs(90)),
            upstream: crate::upstream::Upstream::resolve("LLM_API_KEY", "LLM_BASE_URL", Self::DEFAULT_BASE_URL, "/v1", identity),
            model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-chat".into()),
            // LLM_THINKING=0 → `chat_template_kwargs.enable_thinking=false` (Qwen3.x
            // hybrid-thinking models answer directly: faster, cheaper, no leaked
            // reasoning). Unset → the field is not sent.
            thinking: match std::env::var("LLM_THINKING").ok().as_deref() { Some("0") => Some(false), Some("1") => Some(true), _ => None },
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(1024),
            verified: crate::verified::from_env(identity)?,
        })
    }

    /// A client for an explicit upstream, with the defaults `from_env` uses
    /// when nothing is set. Tests point this at a local fake provider.
    pub fn with_upstream(upstream: crate::upstream::Upstream, model: impl Into<String>) -> Self {
        Self {
            http: crate::net::client(std::time::Duration::from_secs(90)),
            upstream,
            model: model.into(),
            thinking: None,
            max_tokens: 1024,
            verified: None,
        }
    }

    /// The verified backend, when `LLM_VERIFIED` turned one on.
    pub fn verified(&self) -> Option<&dyn crate::verified::VerifiedBackend> {
        self.verified.as_deref()
    }

    pub fn base_url(&self) -> &str {
        self.upstream.base()
    }

    /// True when completions go through the gateway as this agent (metered
    /// by the plan); false with an own key (`LLM_API_KEY`).
    pub fn is_gateway(&self) -> bool {
        self.upstream.is_gateway()
    }

    /// One chat-completions call. Returns the full assistant message object
    /// (may contain `tool_calls`).
    ///
    /// Connection failures, 429 and 5xx get exactly one retry after a short
    /// pause. Timeouts do NOT retry — the turn's latency budget is already
    /// spent, and a second 90s wait would blow past the client's own deadline.
    pub async fn chat(&self, messages: &[Value], tools: Option<&Value>) -> Result<Value> {
        if let Some(v) = self.verified.as_deref() {
            return self.chat_verified(v, messages, tools).await;
        }
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.6,
            "max_tokens": self.max_tokens,
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        if let Some(th) = self.thinking {
            body["chat_template_kwargs"] = json!({"enable_thinking": th});
        }
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
            let resp = match self.upstream.post_json(&self.http, "/chat/completions", &body).send().await {
                Ok(r) => r,
                Err(e) if e.is_connect() && attempt == 0 => {
                    tracing::warn!(error = %e, "LLM connect failed — retrying once");
                    last_err = Some(e.into());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            let payload: Value = resp.json().await?;
            if status.is_success() {
                return assistant_message(&payload);
            }
            let retryable = status.as_u16() == 429 || status.is_server_error();
            let err = anyhow!("LLM API {status}: {payload}");
            if retryable && attempt == 0 {
                tracing::warn!(%status, "LLM API transient error — retrying once");
                last_err = Some(err);
                continue;
            }
            return Err(err);
        }
        Err(last_err.expect("loop exits early unless an error was recorded"))
    }

    /// The same call, sealed to an attested enclave. The model comes from
    /// `VERIFIED_MODEL` or the pin set's `agento:default` alias — the
    /// gateway cannot see a sealed body, so it cannot choose it here.
    async fn chat_verified(
        &self,
        v: &dyn crate::verified::VerifiedBackend,
        messages: &[Value],
        tools: Option<&Value>,
    ) -> Result<Value> {
        let model = match std::env::var("VERIFIED_MODEL").ok().filter(|m| !m.trim().is_empty()) {
            Some(m) => m,
            None => v
                .model_for("agento:default")
                .await
                .ok_or_else(|| anyhow!("pin set maps no model for agento:default on {}", v.id()))?,
        };
        let mut body = json!({
            "model": model,
            "messages": messages,
            "temperature": 0.6,
            "max_tokens": self.max_tokens,
            "stream": false,
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        if let Some(th) = self.thinking {
            body["chat_template_kwargs"] = json!({"enable_thinking": th});
        }
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
            let reply = v.chat(&body).await?;
            if (200..300).contains(&reply.status) {
                return assistant_message(&reply.body);
            }
            let origin = if reply.authenticated { v.id() } else { "path, unauthenticated" };
            let err = anyhow!("LLM API {} ({origin}): {}", reply.status, reply.body);
            if attempt == 0 && (reply.status == 429 || reply.status >= 500) {
                tracing::warn!(status = reply.status, "verified LLM transient error — retrying once");
                continue;
            }
            return Err(err);
        }
        Err(anyhow!("verified LLM call failed twice"))
    }
}

/// The assistant message out of a chat-completions payload, cleaned up for
/// the conversation.
fn assistant_message(payload: &Value) -> Result<Value> {
    let mut msg = payload["choices"][0]["message"]
        .as_object()
        .map(|m| Value::Object(m.clone()))
        .ok_or_else(|| anyhow!("malformed LLM response: {payload}"))?;
    // Thinking models return their reasoning alongside the answer;
    // it is not part of the conversation and must not be echoed back.
    msg.as_object_mut().map(|m| m.remove("reasoning_content"));
    // Qwen3.x emits tool calls as `<tool_call><function=…><parameter=…>`
    // XML; servers whose tool parser expects JSON (vLLM `hermes`) hand
    // that back as plain content. Recover it into OpenAI `tool_calls`.
    let has_calls = msg["tool_calls"].as_array().is_some_and(|a| !a.is_empty());
    if !has_calls {
        if let Some((calls, rest)) = parse_xml_tool_calls(msg["content"].as_str().unwrap_or("")) {
            msg["tool_calls"] = Value::Array(calls);
            msg["content"] = if rest.is_empty() { Value::Null } else { Value::String(rest) };
        }
    }
    Ok(msg)
}

/// Qwen3.x XML tool-call recovery: `<tool_call><function=NAME><parameter=K>V</parameter>…</function></tool_call>`.
/// Returns the OpenAI-shaped calls and whatever prose surrounded them.
pub fn parse_xml_tool_calls(text: &str) -> Option<(Vec<Value>, String)> {
    if !text.contains("<tool_call>") {
        return None;
    }
    let mut calls = Vec::new();
    let mut rest = String::new();
    let mut cursor = text;
    let mut idx = 0usize;
    while let Some(start) = cursor.find("<tool_call>") {
        rest.push_str(&cursor[..start]);
        let after = &cursor[start + "<tool_call>".len()..];
        let (block, tail) = match after.find("</tool_call>") {
            Some(end) => (&after[..end], &after[end + "</tool_call>".len()..]),
            None => (after, ""),
        };
        if let Some(call) = parse_one_xml_call(block, idx) {
            calls.push(call);
            idx += 1;
        }
        cursor = tail;
    }
    rest.push_str(cursor);
    if calls.is_empty() {
        return None;
    }
    Some((calls, rest.trim().to_string()))
}

fn parse_one_xml_call(block: &str, idx: usize) -> Option<Value> {
    let fstart = block.find("<function=")? + "<function=".len();
    let fend = block[fstart..].find('>')? + fstart;
    let name = block[fstart..fend].trim().to_string();
    let body = &block[fend + 1..];
    let mut args = serde_json::Map::new();
    let mut cur = body;
    while let Some(ps) = cur.find("<parameter=") {
        let ks = ps + "<parameter=".len();
        let Some(ke) = cur[ks..].find('>') else { break };
        let key = cur[ks..ks + ke].trim().to_string();
        let vs = ks + ke + 1;
        let (val, tail) = match cur[vs..].find("</parameter>") {
            Some(ve) => (&cur[vs..vs + ve], &cur[vs + ve + "</parameter>".len()..]),
            None => (&cur[vs..], ""),
        };
        let val = val.trim();
        // Parameters arrive as text; keep JSON-looking values typed.
        let typed = serde_json::from_str::<Value>(val).unwrap_or(Value::String(val.to_string()));
        args.insert(key, typed);
        cur = tail;
    }
    if name.is_empty() {
        return None;
    }
    Some(json!({
        "id": format!("call_xml_{idx}"),
        "type": "function",
        "function": {"name": name, "arguments": Value::Object(args).to_string()},
    }))
}

#[cfg(test)]
mod xml_tests {
    use super::*;

    #[test]
    fn recovers_qwen_xml_tool_calls() {
        let t = "Claro.\n<tool_call>\n<function=check_availability>\n<parameter=date>\n2026-09-04\n</parameter>\n<parameter=n>\n3\n</parameter>\n</function>\n</tool_call>";
        let (calls, rest) = parse_xml_tool_calls(t).unwrap();
        assert_eq!(rest, "Claro.");
        assert_eq!(calls[0]["function"]["name"], "check_availability");
        let args: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["date"], "2026-09-04");
        assert_eq!(args["n"], 3);
    }

    #[test]
    fn plain_text_untouched() {
        assert!(parse_xml_tool_calls("hola, ¿en qué te ayudo?").is_none());
    }

    #[test]
    fn xml_edge_cases() {
        assert!(parse_xml_tool_calls("plain text").is_none());
        // Opening tag but no function inside: nothing recovered.
        assert!(parse_xml_tool_calls("<tool_call>oops</tool_call>").is_none());
        // Unterminated blocks still parse.
        let (c, rest) = parse_xml_tool_calls("<tool_call><function=f><parameter=a>{\"x\":1}").unwrap();
        assert_eq!(rest, "");
        assert_eq!(c[0]["function"]["arguments"], "{\"a\":{\"x\":1}}");
        // Two calls, prose between them, ids numbered.
        let (c, rest) = parse_xml_tool_calls("A <tool_call><function=f></function></tool_call> B <tool_call><function=g><parameter=t>hola</parameter></function></tool_call> C").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!((c[0]["id"].as_str(), c[1]["id"].as_str()), (Some("call_xml_0"), Some("call_xml_1")));
        assert_eq!(c[1]["function"]["arguments"], "{\"t\":\"hola\"}");
        assert_eq!(rest, "A  B  C");
        // A parameter tag without a closing '>' stops parsing that call's args.
        let (c, _) = parse_xml_tool_calls("<tool_call><function=f><parameter=x</tool_call>").unwrap();
        assert_eq!(c[0]["function"]["arguments"], "{}");
        assert!(parse_one_xml_call("<function= >", 0).is_none());
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;
    use crate::testkit::Mock;

    fn llm(m: &Mock) -> Llm {
        Llm::with_upstream(crate::upstream::Upstream::Direct { base: format!("{}/v1", m.base), key: "k".into() }, "m1")
    }

    #[tokio::test]
    async fn sends_the_request_and_returns_the_message() {
        let m = Mock::start().await;
        m.on("/chat/completions", json!({"choices": [{"message": {"role": "assistant", "content": "hola", "reasoning_content": "thinking…"}}]}));
        let mut l = llm(&m);
        l.thinking = Some(false);
        let tools = json!([{"type": "function", "function": {"name": "f"}}]);
        let msg = l.chat(&[json!({"role": "user", "content": "hi"})], Some(&tools)).await.unwrap();
        assert_eq!(msg, json!({"role": "assistant", "content": "hola"}), "reasoning is dropped");
        let req = &m.seen_path("/v1/chat/completions")[0];
        assert_eq!(req.headers["authorization"], "Bearer k");
        assert_eq!(req.body["model"], "m1");
        assert_eq!(req.body["max_tokens"], 1024);
        assert_eq!(req.body["tools"], tools);
        assert_eq!(req.body["chat_template_kwargs"], json!({"enable_thinking": false}));
        assert_eq!(l.base_url(), format!("{}/v1", m.base));
        assert!(!l.is_gateway());
    }

    #[tokio::test]
    async fn optional_fields_are_omitted() {
        let m = Mock::start().await;
        m.say("ok");
        llm(&m).chat(&[], None).await.unwrap();
        let b = &m.seen()[0].body;
        assert!(b.get("tools").is_none() && b.get("chat_template_kwargs").is_none());
    }

    #[tokio::test]
    async fn xml_tool_calls_in_content_are_recovered() {
        let m = Mock::start().await;
        m.say("<tool_call><function=book><parameter=day>lunes</parameter></function></tool_call>");
        let msg = llm(&m).chat(&[], None).await.unwrap();
        assert!(msg["content"].is_null());
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "book");
        // Real tool_calls are left alone.
        m.set("/chat/completions", json!({"choices": [{"message": {"content": "<tool_call><function=x></function></tool_call>", "tool_calls": [{"id": "1"}]}}]}));
        let msg = llm(&m).chat(&[], None).await.unwrap();
        assert_eq!(msg["tool_calls"], json!([{"id": "1"}]));
    }

    #[tokio::test]
    async fn transient_errors_retry_once() {
        let m = Mock::start().await;
        m.on_status("/chat/completions", 429, json!({"error": "slow down"}));
        m.say("second time lucky");
        assert_eq!(llm(&m).chat(&[], None).await.unwrap()["content"], "second time lucky");
        assert_eq!(m.seen().len(), 2);

        let m = Mock::start().await;
        m.on_status("/chat/completions", 503, json!({}));
        let e = llm(&m).chat(&[], None).await.unwrap_err().to_string();
        assert!(e.contains("503"), "{e}");
        assert_eq!(m.seen().len(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn client_errors_do_not_retry() {
        let m = Mock::start().await;
        m.on_status("/chat/completions", 400, json!({"error": "bad"}));
        assert!(llm(&m).chat(&[], None).await.unwrap_err().to_string().contains("400"));
        assert_eq!(m.seen().len(), 1);
    }

    #[tokio::test]
    async fn malformed_success_is_an_error() {
        let m = Mock::start().await;
        m.on("/chat/completions", json!({"choices": []}));
        assert!(llm(&m).chat(&[], None).await.unwrap_err().to_string().contains("malformed"));
    }

    #[tokio::test]
    async fn connection_refused_retries_once_then_fails() {
        let l = Llm::with_upstream(crate::upstream::Upstream::Direct { base: "http://127.0.0.1:9/v1".into(), key: "k".into() }, "m");
        let t = std::time::Instant::now();
        assert!(l.chat(&[], None).await.is_err());
        assert!(t.elapsed() >= std::time::Duration::from_millis(700), "waited before the retry");
    }
}
