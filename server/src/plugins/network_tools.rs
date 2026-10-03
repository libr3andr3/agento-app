//! Network-defined tools — the split between the APK and the network.
//!
//! The gateway publishes a manifest (`GET /v1/tools`): tool specs the model
//! sees plus the signed HTTP call the core makes for each. This plugin
//! mounts every entry as a real tool, so a feature like group buying (pools)
//! ships from the gateway to every phone without a new APK. The manifest is
//! cached in `settings.network_tools` (offline boots keep the last one) and
//! refreshed with the plan sync; a changed manifest applies on the next start.
//!
//! Manifest entry:
//! ```json
//! {"name": "join_pool", "scope": "onboarding|customer|assistant|both", "core": false,
//!  "description": "...", "parameters": {json schema},
//!  "call": {"method": "GET|POST", "path": "/v1/pools/{id}/join",
//!           "query": ["item"], "body": ["items"] | "*", "fixed": {"mine": "true"}},
//!  "note": "text appended to every result"}
//! ```
//! `{name}` in the path is filled from the arguments (url-encoded); `query`
//! names go to the query string; `body` names (or `*` = every remaining
//! argument) form the JSON body. `prompt: [{scope, text}]` adds guidance.
use serde_json::{json, Value};

use crate::harness::{hook_fn, meta, tool_fn, Agente, Kernel, Plugin, Scope};
use crate::network;

pub const SETTING: &str = "network_tools";

pub struct NetworkTools {
    pub manifest: Value,
}

fn scope_of(s: &str) -> Scope {
    match s {
        "customer" => Scope::Customer,
        "assistant" => Scope::Assistant,
        "both" => Scope::Both,
        _ => Scope::Onboarding,
    }
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Builds the request for one manifest entry from the model's arguments.
/// Returns `(method, path_with_query, body)`.
pub fn build_call(call: &Value, args: &Value) -> anyhow::Result<(String, String, Value)> {
    let method = call["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
    let template = call["path"].as_str().ok_or_else(|| anyhow::anyhow!("tool has no path"))?;
    let mut used = std::collections::HashSet::new();
    let mut path = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        path.push_str(&rest[..i]);
        let j = rest[i..].find('}').ok_or_else(|| anyhow::anyhow!("bad path template"))? + i;
        let name = &rest[i + 1..j];
        let v = args.get(name).filter(|v| !v.is_null()).ok_or_else(|| anyhow::anyhow!("missing argument `{name}`"))?;
        path.push_str(&urlencode(&as_text(v)));
        used.insert(name.to_string());
        rest = &rest[j + 1..];
    }
    path.push_str(rest);
    let mut qs: Vec<String> = Vec::new();
    for q in call["query"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if let Some(v) = args.get(q).filter(|v| !v.is_null()) {
            let t = as_text(v);
            if !t.trim().is_empty() {
                qs.push(format!("{q}={}", urlencode(&t)));
                used.insert(q.to_string());
            }
        }
    }
    if let Some(fixed) = call["fixed"].as_object() {
        for (k, v) in fixed {
            qs.push(format!("{k}={}", urlencode(&as_text(v))));
        }
    }
    if !qs.is_empty() {
        path.push('?');
        path.push_str(&qs.join("&"));
    }
    let body = match &call["body"] {
        Value::String(s) if s == "*" => Value::Object(args.as_object().cloned().unwrap_or_default().into_iter().filter(|(k, _)| !used.contains(k)).collect()),
        Value::Array(names) => Value::Object(names.iter().filter_map(Value::as_str).filter_map(|n| args.get(n).filter(|v| !v.is_null()).map(|v| (n.to_string(), v.clone()))).collect()),
        _ => json!({}),
    };
    Ok((method, path, body))
}

impl Plugin<Agente> for NetworkTools {
    fn name(&self) -> &'static str {
        "network_tools"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        for t in self.manifest["tools"].as_array().into_iter().flatten() {
            let (Some(name), Some(desc)) = (t["name"].as_str(), t["description"].as_str()) else { continue };
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.len() > 48 {
                tracing::warn!(name, "network tool skipped: bad name");
                continue;
            }
            let spec = json!({"type": "function", "function": {"name": name, "description": desc,
                "parameters": if t["parameters"].is_object() { t["parameters"].clone() } else { json!({"type": "object", "properties": {}, "required": []}) }}});
            let call = t["call"].clone();
            let note = t["note"].as_str().map(str::to_string);
            let tool_name = name.to_string();
            let when = t["when"].as_str().map(str::to_string);
            let m = match when {
                Some(w) => crate::harness::meta_when(scope_of(t["scope"].as_str().unwrap_or("onboarding")), t["core"].as_bool().unwrap_or(false), &w),
                None => meta(scope_of(t["scope"].as_str().unwrap_or("onboarding")), t["core"].as_bool().unwrap_or(false)),
            };
            k.tool(m, spec, tool_fn(move |c, a| {
                let call = call.clone();
                let note = note.clone();
                let tool_name = tool_name.clone();
                Box::pin(async move {
                    let (method, path, body) = match build_call(&call, a) {
                        Ok(x) => x,
                        Err(e) => return Ok(json!({"error": e.to_string()})),
                    };
                    let res = if method == "GET" { network::registry_get(c.state, &path).await } else { network::registry_post(c.state, &path, &body).await };
                    match res {
                        Ok(mut v) => {
                            if let (Some(n), Some(o)) = (&note, v.as_object_mut()) {
                                o.insert("note".into(), json!(n));
                            }
                            Ok(v)
                        }
                        Err(e) => {
                            let m = e.to_string();
                            tracing::info!(tool = %tool_name, error = %m, "network tool failed");
                            Ok(json!({"error": m, "note": "the network answered with an error; tell the owner plainly and do not retry more than once"}))
                        }
                    }
                })
            }))?;
        }
        for p in self.manifest["prompt"].as_array().into_iter().flatten() {
            let (Some(scope), Some(text)) = (p["scope"].as_str(), p["text"].as_str()) else { continue };
            let text = text.to_string();
            let event = match scope { "customer" => "prompt/customer", "assistant" => "prompt/assistant", _ => "prompt/onboarding" };
            k.on(event, hook_fn(move |_rt, mut payload| {
                let text = text.clone();
                Box::pin(async move {
                    payload["sections"].as_array_mut().map(|a| a.push(json!(text)));
                    Ok(payload)
                })
            }));
        }
        let n = self.manifest["tools"].as_array().map(|a| a.len()).unwrap_or(0);
        if n > 0 {
            tracing::info!(tools = n, version = ?self.manifest["version"], "network tools mounted");
        }
        Ok(())
    }
}

/// The manifest to boot with: the gateway's if reachable (and cached), else the cached one, else empty.
pub async fn load(db: &sqlx::SqlitePool, registry_url: &str) -> Value {
    let cached = crate::account::setting(db, SETTING).await.and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if std::env::var("NETWORK_TOOLS").map(|v| v == "0").unwrap_or(false) {
        return json!({"tools": []});
    }
    match fetch(registry_url).await {
        Ok(m) => {
            if cached.as_ref() != Some(&m) {
                let _ = crate::account::set_setting(db, SETTING, &m.to_string()).await;
            }
            m
        }
        Err(e) => {
            tracing::info!(error = %e, "network tools: using the cached manifest");
            cached.unwrap_or(json!({"tools": []}))
        }
    }
}

pub async fn fetch(registry_url: &str) -> anyhow::Result<Value> {
    let url = format!("{}/v1/tools", registry_url.trim_end_matches('/'));
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build()?;
    let v: Value = client.get(&url).send().await?.error_for_status()?.json().await?;
    anyhow::ensure!(v["tools"].is_array(), "manifest has no tools[]");
    Ok(v)
}

/// Refresh the cache (with the plan sync). A change applies on the next start.
pub async fn sync(state: &crate::AppState) {
    if let Ok(m) = fetch(&network::registry_url()).await {
        let cur = crate::account::setting(&state.db, SETTING).await;
        if cur.as_deref() != Some(&m.to_string()) {
            let _ = crate::account::set_setting(&state.db, SETTING, &m.to_string()).await;
            tracing::info!(tools = m["tools"].as_array().map(|a| a.len()).unwrap_or(0), "network tools manifest updated — mounts on next start");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_call;
    use serde_json::json;

    #[test]
    fn path_query_and_body_come_from_the_arguments() {
        let call = json!({"method": "POST", "path": "/v1/pools/{id}/join", "body": ["items"]});
        let (m, p, b) = build_call(&call, &json!({"id": "pool_ab c", "items": {"papa": 60}})).unwrap();
        assert_eq!(m, "POST");
        assert_eq!(p, "/v1/pools/pool_ab%20c/join");
        assert_eq!(b, json!({"items": {"papa": 60}}));
        let call = json!({"method": "GET", "path": "/v1/pools", "query": ["item", "city"], "fixed": {"mine": "true"}});
        let (_, p, b) = build_call(&call, &json!({"item": "papa", "city": null})).unwrap();
        assert_eq!(p, "/v1/pools?item=papa&mine=true");
        assert_eq!(b, json!({}));
        let call = json!({"method": "POST", "path": "/v1/pools", "body": "*"});
        let (_, _, b) = build_call(&call, &json!({"title": "x", "min_kg": 400})).unwrap();
        assert_eq!(b, json!({"title": "x", "min_kg": 400}));
        assert!(build_call(&json!({"path": "/v1/pools/{id}"}), &json!({})).is_err(), "missing path argument is an error");
    }

    #[test]
    fn manifest_mounts_as_tools() {
        let m = json!({"tools": [
            {"name": "find_pools", "scope": "onboarding", "description": "d", "parameters": {"type": "object", "properties": {}}, "call": {"method": "GET", "path": "/v1/pools"}},
            {"name": "bad name!", "description": "d", "call": {"path": "/x"}}
        ], "prompt": [{"scope": "onboarding", "text": "POOLS: ..."}]});
        let mut k = crate::harness::Kernel::new();
        k.load(vec![Box::new(super::NetworkTools { manifest: m })], &[]).unwrap();
        let specs = k.tool_specs(|_, _| true);
        let names: Vec<String> = specs.as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"find_pools".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("bad")));
    }
}
