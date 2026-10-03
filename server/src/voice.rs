use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tokio::process::Command;

use crate::upstream::Upstream;

async fn transcribe_one(api: &Upstream, audio: &[u8], filename: &str) -> Result<String> {
    let part = reqwest::multipart::Part::bytes(audio.to_vec())
        .file_name(filename.to_string())
        .mime_str("application/octet-stream")?;
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);
    let resp = api
        .post_multipart(&crate::net::client(std::time::Duration::from_secs(60)), "/audio/transcriptions", form)
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("whisper API {status}: {body}"));
    }
    body["text"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("no text in whisper response"))
}

/// Speech-to-text via Whisper. Tries `primary` first (the self-hosted yaya
/// node, when configured); on any failure — including the node being
/// between SLURM jobs — falls through to `fallback` (OpenAI direct, or
/// through the gateway) so a turn never loses its transcript to node churn.
pub async fn transcribe(primary: &Upstream, fallback: Option<&Upstream>, audio: Vec<u8>, filename: &str) -> Result<String> {
    match transcribe_one(primary, &audio, filename).await {
        Ok(text) => Ok(text),
        Err(e) => match fallback {
            Some(fb) => {
                tracing::warn!("whisper primary failed, falling back: {e}");
                transcribe_one(fb, &audio, filename).await
            }
            None => Err(e),
        },
    }
}

fn piper_dir() -> PathBuf {
    PathBuf::from(std::env::var("PIPER_DIR").unwrap_or_else(|_| "./tts".into()))
}

/// Crude language pick: Spanish diacritics/punctuation or common Spanish
/// stopwords → Spanish. Used for the TTS voice and the agent's canned
/// fallback reply.
pub fn is_spanish(text: &str) -> bool {
    if text.chars().any(|c| "áéíóúñ¿¡ÁÉÍÓÚÑ".contains(c)) {
        return true;
    }
    let words: Vec<String> = text.to_lowercase().split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).map(String::from).collect();
    // A Spanish greeting opening the message is enough on its own.
    if words.first().is_some_and(|w| ["hola", "buenas", "buenos", "gracias", "holi"].contains(&w.as_str())) {
        return true;
    }
    const COMMON: &[&str] = &["el", "la", "de", "que", "para", "gracias", "hola", "tu", "los", "quiero", "una", "cita", "precio", "cuanto", "tienen", "hay", "por", "favor", "con"];
    let mut seen: Vec<&str> = words.iter().map(String::as_str).filter(|w| COMMON.contains(w)).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len() >= 2
}

/// Clearly English: an English greeting, or two common English words.
pub fn is_english(text: &str) -> bool {
    let words: Vec<String> = text.to_lowercase().split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).map(String::from).collect();
    const GREETINGS: &[&str] = &["hello", "hi", "hey", "thanks", "thank"];
    const COMMON: &[&str] = &["the", "i", "you", "is", "are", "want", "please", "can", "do", "what", "how", "my", "appointment", "tomorrow", "today", "open", "price"];
    if words.first().is_some_and(|w| GREETINGS.contains(&w.as_str())) {
        return true;
    }
    let mut seen: Vec<&str> = words.iter().map(String::as_str).filter(|w| COMMON.contains(w)).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len() >= 2
}

/// Which language to answer in, for a business speaking `business_lang`.
/// The business's own language wins unless the message is clearly in the
/// other of Spanish/English: a one-word "hola" to a Peruvian shop is Spanish.
pub fn reply_language<'a>(business_lang: &'a str, text: &str) -> &'a str {
    match business_lang {
        "es" if is_english(text) && !is_spanish(text) => "en",
        "en" if is_spanish(text) => "es",
        other => other,
    }
}

/// Keep only characters Piper reads well: drop emoji/markdown noise.
fn sanitize_for_tts(text: &str) -> String {
    let cleaned: String = text
        .replace("**", "")
        .replace('*', "")
        .replace('#', "")
        .replace('`', "")
        .chars()
        .filter(|c| {
            c.is_alphanumeric()
                || c.is_whitespace()
                || "áéíóúñüÁÉÍÓÚÑÜ¿¡.,;:!?()'\"-/%€$".contains(*c)
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ------------------------------------------------------------------ TTS
//
// Provider chain, chosen per call: Fish Audio (80 languages, cheapest,
// needs a voice id) → ElevenLabs Flash v2.5 (32 languages) → OpenAI
// gpt-4o-mini-tts (50+ languages, already keyed for Whisper) → Piper on
// CPU (offline last resort, es/en only). TTS_PROVIDER forces the head of
// the chain; otherwise the first provider with a key wins, and an API
// failure falls through to the next so a turn never loses its audio.

fn provider(audio_api: Option<&Upstream>) -> String {
    std::env::var("TTS_PROVIDER").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let has = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);
        if has("FISH_API_KEY") && has("FISH_VOICE") {
            "fish".into()
        } else if has("ELEVENLABS_API_KEY") {
            "elevenlabs".into()
        } else if audio_api.is_some() {
            "openai".into()
        } else {
            "piper".into()
        }
    })
}

/// 16-bit mono PCM → WAV container (what the APK's MediaPlayer expects).
fn wav_from_pcm16(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * 2;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// Streaming TTS APIs (OpenAI, Fish) emit WAV with 0xFFFFFFFF placeholder
/// sizes in the RIFF and data chunks. Desktop players shrug; Android's
/// MediaPlayer refuses to prepare. Re-wrap the PCM with real sizes, keeping
/// the declared sample rate. Non-WAV or multi-channel input is returned
/// untouched (the APK will fail visibly rather than hear garbage).
fn normalize_wav(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return bytes.to_vec();
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut i = 12;
    let mut sample_rate = 0u32;
    let mut channels = 1u16;
    let mut bits = 16u16;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32_at(i + 4) as usize;
        if id == b"fmt " && i + 8 + 16 <= bytes.len() {
            channels = u16_at(i + 10);
            sample_rate = u32_at(i + 12);
            bits = u16_at(i + 22);
        }
        if id == b"data" {
            let start = i + 8;
            let end = if size == 0xFFFF_FFFF || start + size > bytes.len() { bytes.len() } else { start + size };
            if channels == 1 && bits == 16 && sample_rate > 0 {
                return wav_from_pcm16(&bytes[start..end], sample_rate);
            }
            return bytes.to_vec();
        }
        if size == 0xFFFF_FFFF { break; }
        // checked: on 32-bit Android a bogus chunk size must not wrap `i`.
        match i.checked_add(8).and_then(|x| x.checked_add(size)).and_then(|x| x.checked_add(size & 1)) {
            Some(next) => i = next,
            None => break,
        }
    }
    bytes.to_vec()
}

/// ElevenLabs Flash v2.5. Voice per language via ELEVEN_VOICE_<LANG>, else
/// ELEVEN_VOICE, else a warm multilingual premade voice. `language_code`
/// pins pronunciation so a Spanish business name inside a Hindi sentence
/// doesn't flip the whole line.
async fn synth_elevenlabs(text: &str, lang: &str) -> Result<Vec<u8>> {
    let key = std::env::var("ELEVENLABS_API_KEY")?;
    let voice = std::env::var(format!("ELEVEN_VOICE_{}", lang.to_uppercase()))
        .or_else(|_| std::env::var("ELEVEN_VOICE"))
        .unwrap_or_else(|_| "EXAVITQu4vr4xnSDxMaL".into()); // "Sarah" — multilingual premade
    let model = std::env::var("ELEVEN_MODEL").unwrap_or_else(|_| "eleven_flash_v2_5".into());
    let resp = crate::net::client(std::time::Duration::from_secs(30))
        .post(format!("https://api.elevenlabs.io/v1/text-to-speech/{voice}?output_format=pcm_22050"))
        .header("xi-api-key", key)
        .json(&serde_json::json!({
            "text": text,
            "model_id": model,
            "language_code": lang,
            "voice_settings": {"stability": 0.45, "similarity_boost": 0.8, "style": 0.2, "speed": 1.0}
        }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("elevenlabs {status}: {}", body.chars().take(300).collect::<String>()));
    }
    let pcm = resp.bytes().await?;
    if pcm.len() < 1000 {
        return Err(anyhow!("elevenlabs returned {} bytes", pcm.len()));
    }
    Ok(wav_from_pcm16(&pcm, 22050))
}

/// Fish Audio (fish.audio, OpenAudio/S2 family): 80 languages, WAV out,
/// ~$15 per million UTF-8 bytes on s2-pro; `s2.1-pro-free` is free under
/// fair use. A voice is a `reference_id` from their library or a clone —
/// FISH_VOICE (or FISH_VOICE_<LANG>) is mandatory, there is no default.
async fn synth_fish(text: &str, lang: &str) -> Result<Vec<u8>> {
    let key = std::env::var("FISH_API_KEY")?;
    let voice = std::env::var(format!("FISH_VOICE_{}", lang.to_uppercase()))
        .or_else(|_| std::env::var("FISH_VOICE"))
        .map_err(|_| anyhow!("FISH_VOICE not set"))?;
    let model = std::env::var("FISH_MODEL").unwrap_or_else(|_| "s2-pro".into());
    let resp = crate::net::client(std::time::Duration::from_secs(30))
        .post("https://api.fish.audio/v1/tts")
        .bearer_auth(key)
        .header("model", model)
        .json(&serde_json::json!({
            "text": text,
            "reference_id": voice,
            "format": "wav",
            "sample_rate": 24000,
            "latency": "balanced",
            "temperature": 0.7,
            "top_p": 0.7,
            "prosody": {"speed": 1.0, "volume": 0}
        }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("fish audio {status}: {}", body.chars().take(300).collect::<String>()));
    }
    let wav = resp.bytes().await?;
    if wav.len() < 1000 {
        return Err(anyhow!("fish audio returned {} bytes", wav.len()));
    }
    Ok(normalize_wav(&wav))
}

/// OpenAI gpt-4o-mini-tts. Returns WAV directly; the `instructions` field
/// is where the receptionist persona and the language live.
async fn synth_openai(audio_api: Option<&Upstream>, text: &str, lang: &str) -> Result<Vec<u8>> {
    let audio_api = audio_api.ok_or_else(|| anyhow!("no OPENAI_API_KEY and no agent identity"))?;
    let voice = std::env::var(format!("OPENAI_VOICE_{}", lang.to_uppercase()))
        .or_else(|_| std::env::var("OPENAI_VOICE"))
        .unwrap_or_else(|_| "coral".into());
    let model = std::env::var("OPENAI_TTS_MODEL").unwrap_or_else(|_| "gpt-4o-mini-tts".into());
    let lang_name = crate::locale::language_name(lang).split(' ').next().unwrap_or("English").to_string();
    let body = serde_json::json!({
        "model": model,
        "voice": voice,
        "input": text,
        "response_format": "wav",
        "instructions": format!(
            "You are a warm, upbeat receptionist for a small local business. Speak natural, \
             conversational {lang_name} with native pronunciation and local rhythm; friendly, \
             unhurried, never robotic. Read numbers, prices and times the way a local would say them."
        )
    });
    let resp = audio_api
        .post_json(&crate::net::client(std::time::Duration::from_secs(30)), "/audio/speech", &body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("openai tts {status}: {}", body.chars().take(300).collect::<String>()));
    }
    let wav = resp.bytes().await?;
    if wav.len() < 1000 {
        return Err(anyhow!("openai tts returned {} bytes", wav.len()));
    }
    Ok(normalize_wav(&wav))
}

/// Piper voice for a language. Override any with PIPER_VOICE_<LANG>=file.onnx
/// (relative to PIPER_DIR); languages without a voice on disk fall back to
/// English rather than failing the turn — the APK shows text either way.
fn voice_for(dir: &std::path::Path, lang: &str) -> PathBuf {
    if let Ok(v) = std::env::var(format!("PIPER_VOICE_{}", lang.to_uppercase())) {
        return dir.join(v);
    }
    let default = match lang {
        "es" => "es_ES-davefx-medium.onnx",
        "pt" => "pt_BR-faber-medium.onnx",
        "fr" => "fr_FR-siwis-medium.onnx",
        "de" => "de_DE-thorsten-medium.onnx",
        "it" => "it_IT-riccardo-x_low.onnx",
        "nl" => "nl_NL-mls-medium.onnx",
        "pl" => "pl_PL-darkman-medium.onnx",
        "tr" => "tr_TR-fettah-medium.onnx",
        "zh" => "zh_CN-huayan-medium.onnx",
        "vi" => "vi_VN-vais1000-medium.onnx",
        "ar" => "ar_JO-kareem-medium.onnx",
        "hi" | "ur" | "bn" | "en" | _ => "en_US-lessac-medium.onnx",
    };
    let p = dir.join(default);
    if p.exists() { p } else { dir.join("en_US-lessac-medium.onnx") }
}

/// Text-to-speech. `lang` is the business's language; for es/en businesses
/// the text heuristic still catches a reply in the other of the two (a
/// Peruvian shop answering a tourist in English gets an English voice).
///
/// `yaya`, when configured, is tried first (self-hosted, on our own
/// GPU nodes — same OpenAI-shaped `/audio/speech` call as the
/// "openai" tier, just against our own base+key). Everything below it is
/// unchanged, so node churn falls through exactly like an API failure
/// always has.
pub async fn synthesize(yaya: Option<&Upstream>, audio_api: Option<&Upstream>, text: &str, lang: &str) -> Result<Vec<u8>> {
    let effective = match lang {
        "es" | "en" => reply_language(lang, text),
        other => other,
    };
    let clean = sanitize_for_tts(text);
    if clean.is_empty() {
        return Err(anyhow!("nothing to speak"));
    }
    let base_chain: &[&str] = match provider(audio_api).as_str() {
        "fish" => &["fish", "elevenlabs", "openai", "piper"],
        "elevenlabs" => &["elevenlabs", "fish", "openai", "piper"],
        "openai" => &["openai", "piper"],
        _ => &["piper"],
    };
    let mut chain: Vec<&str> = Vec::with_capacity(base_chain.len() + 1);
    if yaya.is_some() {
        chain.push("yaya");
    }
    chain.extend_from_slice(base_chain);

    let mut last = anyhow!("no tts provider");
    for p in chain {
        let r = match p {
            "yaya" => synth_openai(yaya, &clean, effective).await,
            "fish" => synth_fish(&clean, effective).await,
            "elevenlabs" => synth_elevenlabs(&clean, effective).await,
            "openai" => synth_openai(audio_api, &clean, effective).await,
            _ => synth_piper(&clean, effective).await,
        };
        match r {
            Ok(wav) => return Ok(wav),
            Err(e) => {
                tracing::warn!(provider = p, lang = effective, "tts failed, falling through: {e}");
                last = e;
            }
        }
    }
    Err(last)
}

/// Piper on CPU (offline fallback; only es/en voices ship on the node).
async fn synth_piper(clean: &str, effective: &str) -> Result<Vec<u8>> {
    let dir = piper_dir();
    let voice = voice_for(&dir, effective);
    let out = std::env::temp_dir().join(format!("agente-tts-{}.wav", uuid::Uuid::new_v4()));

    let mut child = Command::new(dir.join("piper").join("piper"))
        .arg("--model")
        .arg(&voice)
        .arg("--output_file")
        .arg(&out)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        stdin.write_all(clean.as_bytes()).await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!("piper exited with {status}"));
    }
    let bytes = tokio::fs::read(&out).await?;
    let _ = tokio::fs::remove_file(&out).await;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_wav_placeholders_are_rewritten() {
        let pcm: Vec<u8> = (0..4000u32).flat_map(|i| ((i % 200) as i16).to_le_bytes()).collect();
        let mut bad = wav_from_pcm16(&pcm, 24000);
        bad[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bad[40..44].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let fixed = normalize_wav(&bad);
        assert_eq!(fixed.len(), 44 + pcm.len());
        assert_eq!(u32::from_le_bytes(fixed[4..8].try_into().unwrap()), 36 + pcm.len() as u32);
        assert_eq!(u32::from_le_bytes(fixed[40..44].try_into().unwrap()), pcm.len() as u32);
        assert_eq!(u32::from_le_bytes(fixed[24..28].try_into().unwrap()), 24000);
        assert_eq!(&fixed[44..], &pcm[..]);
        // Already-good input is byte-identical.
        assert_eq!(normalize_wav(&fixed), fixed);
    }

    use crate::testkit::Mock;
    use serde_json::json;

    fn wav(pcm_len: usize, rate: u32) -> Vec<u8> {
        wav_from_pcm16(&vec![1u8; pcm_len], rate)
    }

    #[test]
    fn wav_header_layout() {
        let w = wav(10, 22050);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 1, "mono");
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 44100, "byte rate");
        assert_eq!(&w[36..40], b"data");
        assert_eq!(w.len(), 54);
    }

    #[test]
    fn normalize_wav_leaves_other_shapes_alone() {
        assert_eq!(normalize_wav(b"ID3 mp3 bytes"), b"ID3 mp3 bytes".to_vec());
        assert_eq!(normalize_wav(&[0u8; 43]), vec![0u8; 43]);
        // Stereo is returned untouched.
        let mut st = wav(100, 24000);
        st[22] = 2;
        assert_eq!(normalize_wav(&st), st);
        // No data chunk at all.
        let mut nodata = wav(100, 24000);
        nodata[36..40].copy_from_slice(b"junk");
        nodata[40..44].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(normalize_wav(&nodata), nodata);
    }

    #[test]
    fn normalize_wav_skips_extra_chunks_and_truncated_data() {
        // fmt, then a LIST chunk of odd size (padded), then data claiming more than present.
        let good = wav(20, 16000);
        let mut w = good[..36].to_vec();
        w.extend_from_slice(b"LIST");
        w.extend_from_slice(&3u32.to_le_bytes());
        w.extend_from_slice(&[b'a', b'b', b'c', 0]);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&1000u32.to_le_bytes());
        w.extend_from_slice(&[7u8; 20]);
        let fixed = normalize_wav(&w);
        assert_eq!(fixed.len(), 44 + 20);
        assert_eq!(&fixed[44..], &[7u8; 20]);
        assert_eq!(u32::from_le_bytes(fixed[24..28].try_into().unwrap()), 16000);
    }

    #[test]
    fn spanish_detection() {
        assert!(is_spanish("¿Tienes cita?"));
        assert!(is_spanish("hola quiero una cita para el lunes"));
        assert!(!is_spanish("hello I want an appointment"));
        assert!(!is_spanish("la"), "one stopword is not enough");
        assert!(is_spanish("Gracias, hola"));
    }

    #[test]
    fn tts_text_is_sanitised() {
        assert_eq!(sanitize_for_tts("**Hola** 👋  `code` #1  ¿qué tal? S/ 20%"), "Hola code 1 ¿qué tal? S/ 20%");
        assert_eq!(sanitize_for_tts("🎉🎉"), "");
    }

    #[test]
    fn provider_defaults_without_keys() {
        let up = Upstream::Direct { base: "x".into(), key: "k".into() };
        assert_eq!(provider(Some(&up)), "openai");
        assert_eq!(provider(None), "piper");
    }

    #[test]
    fn piper_voices_fall_back_to_english() {
        let dir = std::env::temp_dir().join(format!("piper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(voice_for(&dir, "es"), dir.join("en_US-lessac-medium.onnx"), "missing file falls back");
        std::fs::write(dir.join("es_ES-davefx-medium.onnx"), b"").unwrap();
        assert_eq!(voice_for(&dir, "es"), dir.join("es_ES-davefx-medium.onnx"));
        assert_eq!(voice_for(&dir, "hi"), dir.join("en_US-lessac-medium.onnx"));
        assert_eq!(piper_dir(), PathBuf::from("./tts"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn direct(m: &Mock) -> Upstream {
        Upstream::Direct { base: format!("{}/v1", m.base), key: "k".into() }
    }

    #[tokio::test]
    async fn transcribe_falls_back_on_failure() {
        let good = Mock::start().await;
        good.on("/audio/transcriptions", json!({"text": "  hola  "}));
        let bad = Mock::start().await;
        bad.on_status("/audio/transcriptions", 503, json!({}));
        assert_eq!(transcribe(&direct(&good), None, b"ogg".to_vec(), "a.ogg").await.unwrap(), "hola");
        assert_eq!(transcribe(&direct(&bad), Some(&direct(&good)), b"ogg".to_vec(), "a.ogg").await.unwrap(), "hola");
        assert_eq!(bad.seen().len(), 1);
        assert!(transcribe(&direct(&bad), None, vec![], "a.ogg").await.unwrap_err().to_string().contains("whisper API 503"));
        let empty = Mock::start().await;
        empty.on("/audio/transcriptions", json!({}));
        assert!(transcribe(&direct(&empty), None, vec![], "a").await.unwrap_err().to_string().contains("no text"));
    }

    #[tokio::test]
    async fn synthesize_prefers_the_yaya_node_and_normalises() {
        let yaya = Mock::start().await;
        let mut streamed = wav(2000, 24000);
        streamed[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        streamed[40..44].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        yaya.on_bytes("/audio/speech", streamed, &[]);
        let out = synthesize(Some(&direct(&yaya)), None, "**Hola**, ¿cómo estás?", "es").await.unwrap();
        assert_eq!(out.len(), 44 + 2000);
        let req = &yaya.seen()[0].body;
        assert_eq!(req["input"], "Hola, ¿cómo estás?");
        assert_eq!(req["voice"], "coral");
        assert!(req["instructions"].as_str().unwrap().contains("Spanish"));
    }

    #[tokio::test]
    async fn synthesize_falls_through_and_reports_the_last_error() {
        let yaya = Mock::start().await;
        yaya.on_status("/audio/speech", 500, json!({}));
        let api = Mock::start().await;
        api.on_bytes("/audio/speech", vec![1u8; 10], &[]); // too short to be audio
        let e = synthesize(Some(&direct(&yaya)), Some(&direct(&api)), "hello there", "en").await.unwrap_err();
        // Chain: yaya (500) → openai (10 bytes) → piper (no binary here).
        assert_eq!(yaya.seen().len(), 1);
        assert_eq!(api.seen().len(), 1);
        let _ = e;
        assert!(synthesize(None, None, "🎉", "es").await.unwrap_err().to_string().contains("nothing to speak"));
    }

    #[tokio::test]
    async fn synth_openai_needs_an_upstream_and_real_audio() {
        assert!(synth_openai(None, "x", "en").await.unwrap_err().to_string().contains("no OPENAI_API_KEY"));
        let m = Mock::start().await;
        m.on_bytes("/audio/speech", vec![0u8; 10], &[]);
        assert!(synth_openai(Some(&direct(&m)), "x", "en").await.unwrap_err().to_string().contains("returned 10 bytes"));
    }

    #[tokio::test]
    async fn keyed_providers_fail_cleanly_without_keys() {
        assert!(synth_fish("x", "es").await.is_err());
        assert!(synth_elevenlabs("x", "es").await.is_err());
        assert!(synth_piper("x", "es").await.is_err(), "no piper binary in the test environment");
    }

    #[test]
    fn replies_follow_the_business_unless_the_message_clearly_differs() {
        assert_eq!(reply_language("es", "hola"), "es");
        assert_eq!(reply_language("es", "ok"), "es");
        assert_eq!(reply_language("es", "Hello, do you have an appointment tomorrow?"), "en");
        assert_eq!(reply_language("es", "hi"), "en");
        assert_eq!(reply_language("en", "hola, quiero una cita"), "es");
        assert_eq!(reply_language("en", "ok"), "en");
        assert_eq!(reply_language("pt", "hello there you"), "pt");
        assert!(!is_english("hola que tal") && is_english("what is the price"));
    }
}
