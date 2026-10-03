//! Network-defined tools: the manifest the core mounts at boot so features
//! ship from the gateway without an APK release (agento/docs/CORE-API.md,
//! "network tools"). Each entry is a tool spec the model sees plus the
//! signed HTTP call the core makes on its behalf. The manifest is the
//! `settings.tools_manifest` row when set (`POST /admin/tools`), else the
//! compiled default below — which is how pools reach every phone.
use axum::{extract::State, http::{HeaderMap, StatusCode}, response::IntoResponse, Json};
use serde_json::{json, Value};

use crate::{err, internal, ApiResult, App, Shared};

pub fn default_manifest() -> Value {
    json!({
        "version": 1,
        "tools": [
            {"name": "find_pools", "scope": "onboarding", "description": "Compras en grupo (pools) abiertas en la red: varios negocios juntan pedidos de un mismo proveedor para pagar precio de volumen (camión = precio de chacra) y repartir el flete. Devuelve kg reunidos, % de llenado, precio actual y al llegar a la meta.",
             "parameters": {"type": "object", "properties": {"item": {"type": "string", "description": "producto, ej. papa"}, "city": {"type": "string"}, "country": {"type": "string"}}, "required": []},
             "call": {"method": "GET", "path": "/v1/pools", "query": ["item", "city", "country"]}},
            {"name": "pool_demand", "scope": "onboarding", "description": "Demanda agregada en la red por producto (kg y cuántos negocios la señalaron en los últimos días): la señal para abrir un pool que sí se llene.",
             "parameters": {"type": "object", "properties": {"item": {"type": "string"}, "city": {"type": "string"}, "country": {"type": "string"}, "days": {"type": "integer"}}, "required": []},
             "call": {"method": "GET", "path": "/v1/pools/demand", "query": ["item", "city", "country", "days"]}},
            {"name": "signal_demand", "scope": "onboarding", "description": "Avisa a la red cuánto de un producto compraría este negocio esta semana (sin compromiso). Con suficientes señales alguien abre un pool.",
             "parameters": {"type": "object", "properties": {"item": {"type": "string"}, "kg": {"type": "number"}, "city": {"type": "string"}, "country": {"type": "string"}}, "required": ["item", "kg"]},
             "call": {"method": "POST", "path": "/v1/pools/intent", "body": ["item", "kg", "city", "country"]}},
            {"name": "open_pool", "scope": "onboarding", "description": "Abre un pool con un proveedor: producto(s) con tramos de precio por volumen (unitMinor = céntimos por kg), mínimo y meta en kg, flete total y fecha límite. Confirma con el dueño antes: el pool es público.",
             "parameters": {"type": "object", "properties": {"title": {"type": "string"}, "seller": {"type": "string", "description": "agent id del proveedor (omitir si es este mismo negocio)"}, "items": {"type": "object", "description": "{\"papa\": {\"tiers\": [{\"kg\": 0, \"unitMinor\": 187}, {\"kg\": 400, \"unitMinor\": 128}, {\"kg\": 800, \"unitMinor\": 110}]}}"}, "min_kg": {"type": "number"}, "target_kg": {"type": "number"}, "delivery_minor": {"type": "integer"}, "deadline": {"type": "string", "description": "RFC3339"}, "city": {"type": "string"}, "country": {"type": "string"}, "note": {"type": "string"}}, "required": ["title", "items", "min_kg", "target_kg", "deadline"]},
             "call": {"method": "POST", "path": "/v1/pools", "body": "*"}},
            {"name": "join_pool", "scope": "onboarding", "description": "Suma este negocio a un pool con kg por producto. El importe queda en custodia (al precio del tramo mínimo; la diferencia vuelve al cerrar) y se libera al proveedor cuando la mayoría confirma la entrega. Pide confirmación explícita del dueño antes de llamar.",
             "parameters": {"type": "object", "properties": {"id": {"type": "string"}, "items": {"type": "object", "description": "{\"papa\": 60}"}}, "required": ["id", "items"]},
             "call": {"method": "POST", "path": "/v1/pools/{id}/join", "body": ["items"]}},
            {"name": "leave_pool", "scope": "onboarding", "description": "Sale de un pool abierto: devolución total de la custodia.",
             "parameters": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
             "call": {"method": "POST", "path": "/v1/pools/{id}/leave"}},
            {"name": "close_pool", "scope": "onboarding", "description": "Cierra un pool (organizador o proveedor; cualquiera tras la fecha límite): si alcanzó el mínimo sale al precio del volumen reunido, si no se cancela con devolución.",
             "parameters": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
             "call": {"method": "POST", "path": "/v1/pools/{id}/close"}},
            {"name": "confirm_pool_delivery", "scope": "onboarding", "description": "Confirma si el pedido del pool llegó (delivered true/false). Con mayoría de entregas se paga al proveedor; con mayoría de fallas se devuelve a todos. Es evidencia de primera mano para la reputación.",
             "parameters": {"type": "object", "properties": {"id": {"type": "string"}, "delivered": {"type": "boolean"}}, "required": ["id", "delivered"]},
             "call": {"method": "POST", "path": "/v1/pools/{id}/confirm", "body": ["delivered"]}},
            {"name": "my_pools", "scope": "onboarding", "description": "Pools donde este negocio participa, organiza o vende (abiertos por defecto; status=shipped para los que esperan entrega).",
             "parameters": {"type": "object", "properties": {"status": {"type": "string", "description": "open | shipped | delivered | cancelled"}}, "required": []},
             "call": {"method": "GET", "path": "/v1/pools", "query": ["status"], "fixed": {"mine": "true"}}}
        ],
        "prompt": [
            {"scope": "onboarding", "text": "COMPRAS EN GRUPO: cuando el dueño hable de comprar insumos (verduras, papa, insumos al por mayor), revisa find_pools y pool_demand antes de proponer comprar solo. Sumarse a un pool que ya tiene kg es lo más barato: precio de volumen + flete repartido. Explica el precio por kg, cuánto queda en custodia y cuándo llega. Nunca abras ni te sumes a un pool sin un sí explícito del dueño."}
        ]
    })
}

/// `GET /v1/tools` — public; the core caches it and mounts the tools.
pub async fn manifest(State(app): State<Shared>) -> ApiResult {
    let m = match crate::support::setting(&app, "tools_manifest").await {
        Some(s) => serde_json::from_str::<Value>(&s).unwrap_or_else(|_| default_manifest()),
        None => default_manifest(),
    };
    Ok(Json(m).into_response())
}

/// `POST /admin/tools {manifest}` — replace the manifest (an empty body restores the compiled default).
pub async fn set(State(app): State<Shared>, headers: HeaderMap, Json(req): Json<Value>) -> ApiResult {
    crate::require_admin(&app, &headers)?;
    let m = &req["manifest"];
    if m.is_null() {
        sqlx::query("DELETE FROM settings WHERE key = 'tools_manifest'").execute(&app.db).await.map_err(internal)?;
        return Ok(Json(json!({"ok": true, "manifest": "default"})).into_response());
    }
    let ok = m["tools"].as_array().map(|t| t.iter().all(|x| x["name"].is_string() && x["call"]["path"].is_string())).unwrap_or(false);
    if !ok { return Err(err(StatusCode::BAD_REQUEST, "manifest.tools[] need name and call.path")); }
    sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES ('tools_manifest', $1, strftime('%Y-%m-%dT%H:%M:%fZ','now')) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at")
        .bind(m.to_string()).execute(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"ok": true, "tools": m["tools"].as_array().map(|a| a.len()).unwrap_or(0)})).into_response())
}

#[allow(dead_code)]
fn _app(_: &App) {}
