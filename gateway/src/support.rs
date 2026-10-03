//! The support line: one WhatsApp number every "Hablar con soporte" button
//! opens — in the app, on the landing, in the console. It lives in the
//! gateway's `settings` table (switchable from the ops console the moment
//! a line dies), with `SUPPORT_PHONE` and a compiled-in default behind it.

use axum::{extract::State, http::{HeaderMap, StatusCode}, response::IntoResponse, Json};
use serde_json::{json, Value};

use crate::{err, internal, ApiResult, App, Shared};

/// The line enrolled on the ops console on 2026-08-30.
pub const DEFAULT_SUPPORT: &str = "51952183367";

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

pub async fn setting(app: &App, key: &str) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = $1")
        .bind(key).fetch_optional(&app.db).await.ok().flatten()
        .map(|r| r.0).filter(|s| !s.trim().is_empty())
}

pub async fn support_phone(app: &App) -> String {
    setting(app, "support_phone").await
        .or_else(|| std::env::var("SUPPORT_PHONE").ok().filter(|s| !s.trim().is_empty()))
        .map(|s| digits(&s)).filter(|s| s.len() >= 8)
        .unwrap_or_else(|| DEFAULT_SUPPORT.into())
}

/// What every client reads: the number, a ready WhatsApp link, the chat page.
pub async fn support_json(app: &App) -> Value {
    let phone = support_phone(app).await;
    let sales = std::env::var("SALES_PHONE").ok().map(|s| digits(&s)).filter(|s| s.len() >= 8);
    json!({
        "url": "/support",
        "phone": phone,
        "whatsapp": format!("https://wa.me/{phone}?text=Hola%2C%20necesito%20ayuda%20con%20agente"),
        "salesPhone": sales,
    })
}

/// `GET /v1/support` — public, unauthenticated: the landing reads it.
pub async fn public(State(app): State<Shared>) -> ApiResult {
    Ok(Json(support_json(&app).await).into_response())
}

pub async fn get_admin(State(app): State<Shared>, headers: HeaderMap) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    Ok(Json(support_json(&app).await).into_response())
}

#[derive(serde::Deserialize)]
pub struct SettingsReq {
    #[serde(default)] pub support_phone: Option<String>,
}

/// `POST /admin/settings {support_phone}` — switch the line.
pub async fn set(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<SettingsReq>) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    if let Some(p) = req.support_phone.as_deref() {
        let d = digits(p);
        if d.len() < 8 {
            return Err(err(StatusCode::BAD_REQUEST, "support_phone must be a phone with country code"));
        }
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES ('support_phone', $1, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        ).bind(&d).execute(&app.db).await.map_err(internal)?;
        tracing::info!(phone = %d, "support line switched");
    }
    Ok(Json(support_json(&app).await).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, anon, as_admin};

    #[tokio::test]
    async fn the_support_line_can_be_switched_from_the_console() {
        let app = testkit::app().await;
        std::env::remove_var("SUPPORT_PHONE");
        let (_, v) = anon(&app, "GET", "/v1/support", None).await;
        assert_eq!(v["phone"], DEFAULT_SUPPORT);
        assert!(v["whatsapp"].as_str().unwrap().starts_with(&format!("https://wa.me/{DEFAULT_SUPPORT}?")));
        assert_eq!(anon(&app, "POST", "/admin/settings", Some(json!({"support_phone": "51999"}))).await.0, 401);
        assert_eq!(anon(&app, "GET", "/admin/settings", None).await.0, 401);
        assert_eq!(as_admin(&app, "POST", "/admin/settings", Some(json!({"support_phone": "123"}))).await.0, 400);
        let (st, v) = as_admin(&app, "POST", "/admin/settings", Some(json!({"support_phone": "+51 999 888 777"}))).await;
        assert_eq!((st, v["phone"].clone()), (200, json!("51999888777")));
        assert_eq!(anon(&app, "GET", "/v1/support", None).await.1["phone"], "51999888777");
        assert_eq!(as_admin(&app, "GET", "/admin/settings", None).await.1["phone"], "51999888777");
        assert_eq!(as_admin(&app, "POST", "/admin/settings", Some(json!({}))).await.1["phone"], "51999888777", "no field, no change");
        assert_eq!(setting(&app, "missing").await, None);
    }
}
