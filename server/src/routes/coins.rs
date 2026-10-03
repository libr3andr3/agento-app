//! `/api/coins/*` — the owner's pocket of blind-signed coins (app key).

use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::{coins, SharedState};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()})))
}

pub(super) async fn status(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(coins::status(&state).await))
}

pub(super) async fn keys(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(coins::keys(&state, true).await.map_err(bad)?))
}

#[derive(serde::Deserialize)]
pub(super) struct AmountReq { #[serde(rename = "amountMinor")] amount_minor: Option<i64> }

pub(super) async fn withdraw(State(state): State<SharedState>, Json(req): Json<AmountReq>) -> ApiResult {
    Ok(Json(coins::withdraw(&state, req.amount_minor.unwrap_or(0)).await.map_err(bad)?))
}

#[derive(serde::Deserialize)]
pub(super) struct PayReq {
    to: String,
    #[serde(rename = "amountMinor")] amount_minor: i64,
    #[serde(default = "default_via")] via: String,
    #[serde(default)] note: Option<String>,
}
fn default_via() -> String { "relay".into() }

pub(super) async fn pay(State(state): State<SharedState>, Json(req): Json<PayReq>) -> ApiResult {
    Ok(Json(coins::pay(&state, &req.to, req.amount_minor, &req.via, req.note.as_deref()).await.map_err(bad)?))
}

#[derive(serde::Deserialize)]
pub(super) struct ReceiveReq { payment: Value, #[serde(default = "default_offline")] via: String }
fn default_offline() -> String { "offline".into() }

pub(super) async fn receive(State(state): State<SharedState>, Json(req): Json<ReceiveReq>) -> ApiResult {
    let r = coins::receive(&state, &req.payment, &req.via).await.map_err(bad)?;
    let bg = state.clone();
    tokio::spawn(async move { let _ = coins::refresh(&bg).await; });
    Ok(Json(r))
}

pub(super) async fn refresh(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(coins::refresh(&state).await.map_err(bad)?))
}

pub(super) async fn deposit(State(state): State<SharedState>, Json(req): Json<AmountReq>) -> ApiResult {
    Ok(Json(coins::deposit(&state, req.amount_minor.filter(|a| *a > 0)).await.map_err(bad)?))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api};
    use serde_json::json;

    #[tokio::test]
    async fn pocket_routes_report_errors_as_400() {
        let s = testkit::state().await;
        let (st, v) = api(&s, "GET", "/api/coins", None, None).await;
        assert_eq!((st, v["spendableMinor"].clone()), (200, json!(0)));
        for (p, b) in [("/api/coins/keys", json!({})), ("/api/coins/withdraw", json!({"amountMinor": 100})), ("/api/coins/withdraw", json!({})),
                       ("/api/coins/pay", json!({"to": "agent:x", "amountMinor": 100})), ("/api/coins/receive", json!({"payment": {}})),
                       ("/api/coins/deposit", json!({}))] {
            let (st, v) = api(&s, "POST", p, None, Some(b)).await;
            assert_eq!(st, 400, "{p}: {v}");
            assert!(v["error"].is_string());
        }
        assert_eq!(api(&s, "POST", "/api/coins/refresh", None, None).await.1["refreshed"], 0);
    }
}
