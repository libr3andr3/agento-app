//! Voice turns (Whisper + TTS) and catalog-photo extraction.

use super::*;

// ------------------------------------------------------------------ voice

/// Voice turn for the onboarding chat: raw audio in (m4a/wav/ogg bytes),
/// Whisper transcript → onboarding agent → Piper TTS reply out (base64 WAV).
pub(super) async fn voice_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty audio body"));
    }
    // Charged before the upload: a 25 MB body forwarded to Whisper is the most
    // expensive thing one device token can ask for.
    crate::limits::charge(&state.db, business_id, crate::limits::Meter::Stt)
        .await
        .map_err(|e| err(StatusCode::TOO_MANY_REQUESTS, e))?;
    let Some(primary) = state.yaya_audio.as_ref().or(state.audio.as_ref()) else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "speech is not configured"));
    };
    let fallback = if state.yaya_audio.is_some() { state.audio.as_ref() } else { None };
    let transcript = crate::voice::transcribe(primary, fallback, body.to_vec(), "audio.m4a")
        .await
        .map_err(internal)?;
    if transcript.is_empty() {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "could not hear anything"));
    }

    let peer: (String,) = sqlx::query_as("SELECT owner_phone FROM businesses WHERE id = $1")
        .bind(business_id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    let history = load_history(&state, business_id, "onboarding", &peer.0).await?;
    let outcome = agents::run_onboarding_agent(&state, business_id, Some(&transcript), &history)
        .await
        .map_err(agent_err)?;
    append_message(&state, business_id, "onboarding", &peer.0, "user", &transcript).await?;
    append_message(&state, business_id, "onboarding", &peer.0, "assistant", &outcome.reply).await?;

    // TTS is best-effort: text always comes back even if synthesis fails.
    let lang = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map(|c| crate::locale::Locale::from_values(&c.values).language)
        .unwrap_or_else(|_| "es".into());
    let audio_b64 = match crate::voice::synthesize(state.yaya_audio.as_ref(), state.audio.as_ref(), &outcome.reply, &lang).await {
        Ok(wav) => {
            use base64::Engine;
            Some(base64::engine::general_purpose::STANDARD.encode(wav))
        }
        Err(e) => {
            tracing::warn!("tts failed: {e}");
            None
        }
    };

    let (action, action_data) = outcome
        .action
        .map(|(a, d)| (json!(a), d))
        .unwrap_or((Value::Null, Value::Null));
    Ok(Json(json!({
        "transcript": transcript,
        "agentResponse": outcome.reply,
        "action": action,
        "actionData": action_data,
        "audioBase64": audio_b64,
        "audioFormat": "wav"
    })))
}

// ----------------------------------------------------------- catalog photo

/// "Tómale una foto a tu catálogo": raw image in (JPEG/PNG bytes), vision
/// model reads out item+price pairs, and they merge into the client patch the
/// same way a save_business_schema call would — so the agent can quote them
/// in the very next conversation.
pub(super) async fn catalog_photo(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty image body"));
    }
    let Some(vision) = &state.vision else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "catalog extraction is not configured"));
    };
    // Charged before the upload, like STT: the whole photo becomes paid input
    // tokens, so an unmetered retry loop is the expensive failure mode here.
    crate::limits::charge(&state.db, business_id, crate::limits::Meter::Vision)
        .await
        .map_err(|e| err(StatusCode::TOO_MANY_REQUESTS, e))?;
    let currency = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map(|c| crate::locale::Locale::from_values(&c.values).currency_phrase())
        .unwrap_or_else(|_| "the local currency".into());
    let items = vision.extract_catalog(&body, &currency).await.map_err(agent_err)?;
    if items.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no items with prices found — try a closer, sharper photo",
        ));
    }

    // A services business keeps its price list under `pricing`; everyone else
    // sells from `products`. Composed values decide, so a photo sent before
    // the interview settles businessKind lands in products, the safer default.
    let values = crate::learning::compose(&state.db, &state.schemas_dir, business_id)
        .await
        .map_err(internal)?
        .values;
    let saved_to = if values["businessKind"] == "services" { "pricing" } else { "products" };

    let entries: serde_json::Map<String, Value> = items
        .iter()
        .map(|(name, price)| (name.clone(), json!(price)))
        .collect();
    let row: (Value,) = sqlx::query_as("SELECT schema_config FROM businesses WHERE id = $1")
        .bind(business_id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    let mut patch = row.0;
    crate::learning::deep_merge(&mut patch, &json!({ saved_to: entries }));
    sqlx::query("UPDATE businesses SET schema_config = $1 WHERE id = $2")
        .bind(&patch)
        .bind(business_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    let count = items.len();
    Ok(Json(json!({
        "items": items
            .iter()
            .map(|(name, price)| json!({"name": name, "price": price}))
            .collect::<Vec<_>>(),
        "savedTo": saved_to,
        "count": count,
        "note": if count == 1 {
            "Agregué 1 artículo a tu catálogo".to_string()
        } else {
            format!("Agregué {count} artículos a tu catálogo")
        },
    })))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, Mock, APP_KEY};
    use serde_json::{json, Value};

    async fn post_bytes(s: &crate::SharedState, t: &str, path: &str, body: Vec<u8>) -> (u16, Value) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder().method("POST").uri(path).header("x-app-key", APP_KEY).header("authorization", format!("Bearer {t}"))
            .body(axum::body::Body::from(body)).unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
        let r = crate::routes::router(s.clone()).oneshot(req).await.unwrap();
        let st = r.status().as_u16();
        (st, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 8 << 20).await.unwrap()).unwrap_or(Value::Null))
    }

    fn with(s: crate::SharedState, f: impl FnOnce(crate::AppState) -> crate::AppState) -> crate::SharedState {
        std::sync::Arc::new(f(std::sync::Arc::try_unwrap(s).ok().unwrap()))
    }

    #[tokio::test]
    async fn a_voice_note_is_transcribed_answered_and_spoken() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        assert_eq!(post_bytes(&s, &t, "/api/voice_message", b"ogg".to_vec()).await.0, 503, "no speech configured");
        let up = crate::upstream::Upstream::Direct { base: format!("{}/v1", m.base), key: "k".into() };
        let s = with(s, |st| crate::AppState { audio: Some(up), ..st });
        assert_eq!(post_bytes(&s, &t, "/api/voice_message", vec![]).await.0, 400);
        m.on("/audio/transcriptions", json!({"text": "abro de 9 a 6"}));
        m.say("Anotado: de 9 a 6.");
        let (st, v) = post_bytes(&s, &t, "/api/voice_message", b"ogg".to_vec()).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!((v["transcript"].clone(), v["agentResponse"].clone()), (json!("abro de 9 a 6"), json!("Anotado: de 9 a 6.")));
        assert!(v["audioBase64"].is_null(), "TTS failed here; the text still comes back");
        let stt: i64 = sqlx::query_scalar("SELECT n FROM usage_counters WHERE kind = 'stt' AND business_id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_eq!(stt, 2, "metered before the upload");
        m.set("/audio/transcriptions", json!({"text": "  "}));
        assert_eq!(post_bytes(&s, &t, "/api/voice_message", b"ogg".to_vec()).await.0, 422);
    }

    #[tokio::test]
    async fn a_catalog_photo_fills_the_catalog() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        assert_eq!(post_bytes(&s, &t, "/api/catalog_photo", b"jpg".to_vec()).await.0, 503);
        let v = crate::vision::Vision::from_env(&s.identity).unwrap().unwrap();
        let s = with(s, |st| crate::AppState { vision: Some(v), ..st });
        assert_eq!(post_bytes(&s, &t, "/api/catalog_photo", vec![]).await.0, 400);
        let _ = b;
    }
}
