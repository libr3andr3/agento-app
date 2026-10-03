//! Media proxies: the business phone needs speech-to-text, text-to-speech
//! and catalog-photo vision, and it should need no provider keys for any of
//! them. Each call counts against the daily allowance like a chat call and,
//! when the meter is on, is charged for what it actually consumed — seconds
//! of audio, characters spoken, images looked at (`meter.rs`). Upstream keys
//! stay here.

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{auth::require_linked, charge, client_ip, env_or, err, meter, ApiResult, Auth, Kind, Shared};

/// The bearer token for an audio upstream. `STT_UPSTREAM_KEY` and
/// `TTS_UPSTREAM_KEY` exist so the audio upstreams can be something other
/// than OpenAI — our own Whisper and Kokoro, behind the HiPerGator tunnel —
/// without handing them the OpenAI key, which is the same secret that pays
/// for everything else on that account. Unset, both fall back to
/// `OPENAI_API_KEY`, so a gateway configured before this change is unchanged.
fn upstream_key(specific: &str) -> Result<String, (StatusCode, Json<Value>)> {
    std::env::var(specific).ok().filter(|s| !s.is_empty())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()))
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "audio not configured on this gateway"))
}

/// `POST /v1/audio/transcriptions` — raw multipart passthrough to Whisper.
pub async fn transcriptions(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    charge(&app, &agent, &client_ip(&headers, peer), Kind::Media).await?;
    let standing = meter::guard(&app, &agent).await?;
    let key = upstream_key("STT_UPSTREAM_KEY")?;
    let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let uploaded = body.len();
    let resp = app.http.post(env_or("STT_UPSTREAM_URL", "https://api.openai.com/v1/audio/transcriptions"))
        .bearer_auth(key).header("content-type", ct).body(body).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("stt upstream: {e}")))?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    let mut balance = None;
    if let Some(st) = standing.as_ref().filter(|_| status.is_success()) {
        let seconds = meter::seconds_of(&payload, uploaded);
        balance = meter::record_quietly(&app, &agent, &st.account, meter::Use::Transcribe { seconds }).await;
    }
    Ok(meter::with_balance((status, Json(payload)).into_response(), balance))
}

/// `POST /v1/audio/speech` — JSON in, audio bytes out (OpenAI TTS shape).
/// Input text is bounded: a render is billed by the character.
pub async fn speech(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    charge(&app, &agent, &client_ip(&headers, peer), Kind::Media).await?;
    let standing = meter::guard(&app, &agent).await?;
    let key = upstream_key("TTS_UPSTREAM_KEY")?;
    if body["input"].as_str().is_none_or(|t| t.is_empty() || t.chars().count() > 2000) {
        return Err(err(StatusCode::BAD_REQUEST, "input must be 1–2000 characters"));
    }
    // Billed by the character of input: the response is audio, so the text
    // we sent is the only thing either side can agree on afterwards.
    let chars = body["input"].as_str().map(|t| t.chars().count() as i64).unwrap_or(0);
    // Our own upstream (Kokoro) has its own voice namespace — "ef_dora", not
    // "alloy" — and the voice's first letter is how it picks the language.
    // Cores in the field were built against OpenAI's names, so an unset
    // `TTS_VOICE` leaves the request untouched (OpenAI upstream, as before)
    // and a set one pins every render to a voice this upstream knows.
    if let Ok(v) = std::env::var("TTS_VOICE") {
        if !v.is_empty() {
            body["voice"] = json!(v);
        }
    }
    let resp = app.http.post(env_or("TTS_UPSTREAM_URL", "https://api.openai.com/v1/audio/speech"))
        .bearer_auth(key).json(&body).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("tts upstream: {e}")))?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("audio/wav").to_string();
    let bytes = resp.bytes().await.map_err(|e| err(StatusCode::BAD_GATEWAY, format!("tts body: {e}")))?;
    let mut balance = None;
    if let Some(st) = standing.as_ref().filter(|_| status.is_success()) {
        balance = meter::record_quietly(&app, &agent, &st.account, meter::Use::Speak { chars }).await;
    }
    Ok(meter::with_balance((status, [("content-type", ct)], bytes).into_response(), balance))
}

/// `POST /v1/vision/chat/completions` — chat-completions with images, routed
/// to the vision upstream with the gateway's model.
pub async fn vision(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> ApiResult {
    let (agent, _) = require_linked(&app, &auth).await?;
    charge(&app, &agent, &client_ip(&headers, peer), Kind::Media).await?;
    let standing = meter::guard(&app, &agent).await?;
    let images = count_images(&body);
    let key = std::env::var("VISION_API_KEY").ok().filter(|s| !s.is_empty())
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "vision not configured on this gateway"))?;
    body["model"] = json!(env_or("VISION_MODEL", "deepseek-v4-flash-vision-exp"));
    body["n"] = json!(1);
    body["max_tokens"] = json!(body["max_tokens"].as_i64().unwrap_or(1024).clamp(1, 2048));
    body.as_object_mut().map(|o| o.remove("stream"));
    let resp = app.http.post(format!("{}/chat/completions", env_or("VISION_BASE_URL", "https://api.deepseek.com/v1").trim_end_matches('/')))
        .bearer_auth(key).json(&body).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("vision upstream: {e}")))?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    let mut balance = None;
    if let Some(st) = standing.as_ref().filter(|_| status.is_success()) {
        let (prompt, completion) = meter::usage_of(&payload).unwrap_or((0, 0));
        balance = meter::record_quietly(&app, &agent, &st.account, meter::Use::Vision { images, prompt, completion }).await;
    }
    Ok(meter::with_balance((status, Json(payload)).into_response(), balance))
}

/// How many images a chat-completions request carries. An image is the
/// expensive part of a vision turn and the upstream's token count does not
/// always reflect it, so it is counted here, from what we were asked to send.
fn count_images(body: &Value) -> i64 {
    body["messages"].as_array().map(|msgs| {
        msgs.iter().map(|m| {
            m["content"].as_array().map(|parts| {
                parts.iter().filter(|p| p["type"] == "image_url").count() as i64
            }).unwrap_or(0)
        }).sum()
    }).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, account_with_agent, agent_headers, as_agent, call, Keypair, Mock};

    /// The upstreams are process-wide env, so every media case runs here.
    #[tokio::test]
    async fn media_proxies_keep_keys_here_and_shape_requests() {
        let app = testkit::app().await;
        let m = Mock::start().await;
        let kp = Keypair::generate();
        account_with_agent(&app, "a", "51900000001", &kp).await;
        for k in ["STT_UPSTREAM_KEY", "TTS_UPSTREAM_KEY", "OPENAI_API_KEY", "VISION_API_KEY", "TTS_VOICE"] { std::env::remove_var(k); }
        std::env::set_var("STT_UPSTREAM_URL", format!("{}/stt", m.base));
        std::env::set_var("TTS_UPSTREAM_URL", format!("{}/tts", m.base));
        std::env::set_var("VISION_BASE_URL", format!("{}/vis/", m.base));
        let say = json!({"input": "hola", "voice": "alloy"});
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/audio/speech", Some(say.clone())).await.0, 503, "no key, no audio");
        assert_eq!(as_agent(&app, &Keypair::generate(), "POST", "/v1/audio/speech", Some(say.clone())).await.0, 401, "unlinked");

        std::env::set_var("OPENAI_API_KEY", "sk-openai");
        std::env::set_var("STT_UPSTREAM_KEY", "stt-own");
        m.on("/stt", json!({"text": "buenas", "duration": 2.5}));
        let audio = vec![0u8; 4000];
        let mut h = agent_headers(&kp, "POST", "/v1/audio/transcriptions", &audio);
        h.push(("content-type", "multipart/form-data; boundary=x".into()));
        let (st, v) = call(&app, "POST", "/v1/audio/transcriptions", &h, Some(audio)).await;
        assert_eq!((st, v["text"].clone()), (200, json!("buenas")));
        let seen = m.seen_path("/stt").pop().unwrap();
        assert_eq!(seen.headers["authorization"], "Bearer stt-own", "the audio upstream gets its own key");
        assert_eq!(seen.headers["content-type"], "multipart/form-data; boundary=x");

        m.on("/tts", json!("RIFF"));
        for bad in [json!({"input": ""}), json!({"voice": "x"}), json!({"input": "a".repeat(2001)})] {
            assert_eq!(as_agent(&app, &kp, "POST", "/v1/audio/speech", Some(bad)).await.0, 400);
        }
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/audio/speech", Some(say.clone())).await.0, 200);
        let seen = m.seen_path("/tts").pop().unwrap();
        assert_eq!((seen.headers["authorization"].to_str().unwrap(), seen.body["voice"].as_str()), ("Bearer sk-openai", Some("alloy")), "falls back to the OpenAI key, voice untouched");
        std::env::set_var("TTS_VOICE", "ef_dora");
        as_agent(&app, &kp, "POST", "/v1/audio/speech", Some(say)).await;
        assert_eq!(m.seen_path("/tts").pop().unwrap().body["voice"], "ef_dora");

        let pic = json!({"model": "gpt-9", "stream": true, "max_tokens": 99_999, "messages": [
            {"role": "user", "content": [{"type": "text", "text": "¿qué es?"}, {"type": "image_url", "image_url": {"url": "data:,"}}, {"type": "image_url", "image_url": {"url": "data:,"}}]}]});
        assert_eq!(count_images(&pic), 2);
        assert_eq!(count_images(&json!({"messages": [{"content": "solo texto"}]})), 0);
        assert_eq!(as_agent(&app, &kp, "POST", "/v1/vision/chat/completions", Some(pic.clone())).await.0, 503);
        std::env::set_var("VISION_API_KEY", "vk");
        m.on("/vis/chat/completions", json!({"choices": [{"message": {"content": "una silla"}}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}}));
        let (st, v) = as_agent(&app, &kp, "POST", "/v1/vision/chat/completions", Some(pic)).await;
        assert_eq!((st, v["choices"][0]["message"]["content"].clone()), (200, json!("una silla")));
        let sent = m.seen_path("/vis/chat/completions").pop().unwrap().body;
        assert_eq!((sent["max_tokens"].clone(), sent["stream"].clone(), sent["n"].clone()), (json!(2048), Value::Null, json!(1)));
        assert_ne!(sent["model"], "gpt-9", "the gateway picks the model");
        for k in ["STT_UPSTREAM_URL", "TTS_UPSTREAM_URL", "VISION_BASE_URL", "STT_UPSTREAM_KEY", "OPENAI_API_KEY", "VISION_API_KEY", "TTS_VOICE"] { std::env::remove_var(k); }
    }
}

