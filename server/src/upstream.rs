//! Where an AI call goes, and how it authenticates.
//!
//! Every provider-shaped call (chat, Whisper, TTS, vision) has two homes:
//! the provider itself, with the deployment's own key (hosted server, or an
//! owner who brought their own key), or the yaya.tech gateway, which holds
//! the upstream keys and meters the call against this agent's identity.
//! This type makes that choice once and applies the right credentials to
//! every request, so no module needs to know which case it is in.

use serde_json::Value;

use crate::identity::Identity;

#[derive(Clone)]
pub enum Upstream {
    /// Straight to the provider with a key.
    Direct { base: String, key: String },
    /// Through the gateway, proven by the agent's own signature.
    Gateway { base: String, identity: Identity },
}

impl Upstream {
    /// `key_var` set → Direct at `base_var` (or `direct_default`).
    /// Otherwise → Gateway at `base_var` (or the registry + `gateway_path`).
    pub fn resolve(key_var: &str, base_var: &str, direct_default: &str, gateway_path: &str, identity: &Identity) -> Self {
        Self::resolve_with(|v| std::env::var(v).ok(), key_var, base_var, direct_default, gateway_path, identity)
    }

    /// [`resolve`](Self::resolve) over any variable lookup (tests pass a map).
    fn resolve_with(get: impl Fn(&str) -> Option<String>, key_var: &str, base_var: &str, direct_default: &str, gateway_path: &str, identity: &Identity) -> Self {
        let non_empty = |v: &str| get(v).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let base_override = non_empty(base_var);
        match non_empty(key_var) {
            Some(key) => Self::Direct { base: trim(base_override.unwrap_or_else(|| direct_default.to_string())), key },
            None => Self::Gateway {
                base: trim(base_override.unwrap_or_else(|| format!("{}{gateway_path}", crate::network::registry_url()))),
                identity: identity.clone(),
            },
        }
    }

    pub fn base(&self) -> &str {
        match self {
            Self::Direct { base, .. } | Self::Gateway { base, .. } => base,
        }
    }

    pub fn is_gateway(&self) -> bool {
        matches!(self, Self::Gateway { .. })
    }

    fn server_path(&self, path: &str) -> String {
        server_path(self.base(), path)
    }

    /// JSON POST to `base + path`, authenticated for this upstream.
    pub fn post_json(&self, client: &reqwest::Client, path: &str, body: &Value) -> reqwest::RequestBuilder {
        let req = client.post(format!("{}{path}", self.base())).json(body);
        match self {
            Self::Direct { key, .. } => req.bearer_auth(key),
            Self::Gateway { identity, .. } => {
                let bytes = serde_json::to_vec(body).unwrap_or_default();
                crate::identity::signed(req, identity, "POST", &self.server_path(path), Some(&bytes))
            }
        }
    }

    /// GET `base + path`, authenticated for this upstream.
    pub fn get(&self, client: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
        let req = client.get(format!("{}{path}", self.base()));
        match self {
            Self::Direct { key, .. } => req.bearer_auth(key),
            Self::Gateway { identity, .. } => crate::identity::signed(req, identity, "GET", &self.server_path(path), None),
        }
    }

    /// POST of opaque bytes (e.g. an EHBP-sealed body): the signature covers
    /// exactly the bytes sent, so the gateway authenticates ciphertext the
    /// same way it authenticates JSON.
    pub fn post_bytes(&self, client: &reqwest::Client, path: &str, bytes: Vec<u8>, content_type: &str) -> reqwest::RequestBuilder {
        let req = client.post(format!("{}{path}", self.base())).header(reqwest::header::CONTENT_TYPE, content_type);
        match self {
            Self::Direct { key, .. } => req.bearer_auth(key).body(bytes),
            Self::Gateway { identity, .. } => {
                let signed = crate::identity::signed(req, identity, "POST", &self.server_path(path), Some(&bytes));
                signed.body(bytes)
            }
        }
    }

    /// Multipart POST: the body is streamed, so the signature covers the
    /// request line only (the nonce still makes it single-use).
    pub fn post_multipart(&self, client: &reqwest::Client, path: &str, form: reqwest::multipart::Form) -> reqwest::RequestBuilder {
        let req = client.post(format!("{}{path}", self.base())).multipart(form);
        match self {
            Self::Direct { key, .. } => req.bearer_auth(key),
            Self::Gateway { identity, .. } => crate::identity::signed(req, identity, "POST", &self.server_path(path), None),
        }
    }
}

/// The path the server sees, for the request signature: whatever path
/// prefix `base` carries, plus `path`.
pub fn server_path(base: &str, path: &str) -> String {
    let after_host = base.find("://").map(|i| &base[i + 3..]).unwrap_or(base);
    let prefix = after_host.find('/').map(|i| &after_host[i..]).unwrap_or("");
    format!("{prefix}{path}")
}

fn trim(s: String) -> String {
    s.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_path_strips_host() {
        assert_eq!(server_path("https://llm.yaya.tech/v1", "/chat/completions"), "/v1/chat/completions");
        assert_eq!(server_path("https://api.openai.com", "/v1/audio/speech"), "/v1/audio/speech");
        assert_eq!(server_path("http://127.0.0.1:8120", "/v1/inbox?wait=25"), "/v1/inbox?wait=25");
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: std::collections::HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn resolve_picks_direct_with_a_key_else_gateway() {
        let id = Identity::ephemeral();
        let u = Upstream::resolve_with(env(&[("K", " sk-1 ")]), "K", "B", "https://api.x/v1/", "/v1", &id);
        assert!(!u.is_gateway());
        assert_eq!(u.base(), "https://api.x/v1");
        assert!(matches!(&u, Upstream::Direct { key, .. } if key == "sk-1"));
        let u = Upstream::resolve_with(env(&[("K", "sk"), ("B", "http://lan:8000/v1/")]), "K", "B", "https://api.x/v1", "/v1", &id);
        assert_eq!(u.base(), "http://lan:8000/v1");
        let u = Upstream::resolve_with(env(&[("K", "  ")]), "K", "B", "https://api.x/v1", "/v1", &id);
        assert!(u.is_gateway());
        assert_eq!(u.base(), format!("{}/v1", crate::network::registry_url()));
        let u = Upstream::resolve_with(env(&[("B", "https://gw.example/")]), "K", "B", "https://api.x/v1", "/v1", &id);
        assert!(u.is_gateway());
        assert_eq!(u.base(), "https://gw.example");
    }

    #[test]
    fn server_path_edge_cases() {
        assert_eq!(server_path("no-scheme/prefix", "/p"), "/prefix/p");
        assert_eq!(server_path("host-only", "/p"), "/p");
        assert_eq!(server_path("https://h/a/b", "/c"), "/a/b/c");
        assert_eq!(trim("https://h///".into()), "https://h");
    }

    #[tokio::test]
    async fn direct_uses_bearer_gateway_signs() {
        let c = reqwest::Client::new();
        let id = Identity::ephemeral();
        let d = Upstream::Direct { base: "http://h/v1".into(), key: "sk".into() };
        let r = d.post_json(&c, "/chat", &serde_json::json!({"a": 1})).build().unwrap();
        assert_eq!(r.url().as_str(), "http://h/v1/chat");
        assert_eq!(r.headers()["authorization"], "Bearer sk");
        assert!(r.headers().get(yaya_wire::reqsig::HEADER).is_none());

        let g = Upstream::Gateway { base: "http://h/v1".into(), identity: id.clone() };
        let body = serde_json::json!({"a": 1});
        let r = g.post_json(&c, "/chat", &body).build().unwrap();
        assert_eq!(r.headers()["authorization"], format!("Bearer {}", id.id()).as_str());
        let p = yaya_wire::reqsig::parse(r.headers()[yaya_wire::reqsig::HEADER].to_str().unwrap()).unwrap();
        let agent: yaya_wire::AgentId = id.id().parse().unwrap();
        // Signed over the server-side path and the exact body bytes.
        let hash = yaya_wire::sha256_hex(&serde_json::to_vec(&body).unwrap());
        assert!(yaya_wire::reqsig::verify(&agent, &p, "POST", "/v1/chat", &hash, p.ts, 30).is_ok());

        let form = || reqwest::multipart::Form::new().text("model", "whisper-1");
        let r = d.post_multipart(&c, "/audio", form()).build().unwrap();
        assert_eq!(r.headers()["authorization"], "Bearer sk");
        let r = g.post_multipart(&c, "/audio", form()).build().unwrap();
        let p = yaya_wire::reqsig::parse(r.headers()[yaya_wire::reqsig::HEADER].to_str().unwrap()).unwrap();
        assert!(yaya_wire::reqsig::verify(&agent, &p, "POST", "/v1/audio", "-", p.ts, 30).is_ok());
    }
}
