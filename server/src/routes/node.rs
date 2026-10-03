//! The brain endpoints wa-node calls (loopback, app key). One inbound text
//! or one spoken utterance in, the customer agent's reply out — unless the
//! text comes from the business's own owner, whose number reaches the
//! manager agent (a cloud-hosted business has no app on the owner's phone).

use super::*;

/// The owner's number, as registered at onboarding, compared by digits.
async fn is_owner(state: &SharedState, business_id: Uuid, phone: &str) -> bool {
    let owner: Option<(String,)> = sqlx::query_as("SELECT owner_phone FROM businesses WHERE id = $1")
        .bind(business_id).fetch_optional(&state.db).await.ok().flatten();
    owner.is_some_and(|(o,)| crate::node::digits(&o) == phone)
}

/// WhatsApp bold is *one* asterisk; models write two. Headings/bullets stay as text.
fn for_whatsapp(text: &str) -> String {
    text.replace("**", "*").replace("\n- ", "\n• ")
}

#[derive(Deserialize)]
pub(super) struct NodeMsgReq { from: String, #[serde(default)] name: Option<String>, text: String, #[serde(default)] id: Option<String> }

pub(super) async fn node_message(State(state): State<SharedState>, Json(req): Json<NodeMsgReq>) -> ApiResult {
    let phone = crate::node::digits(&req.from);
    if phone.len() < 8 || req.text.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "from and text required"));
    }
    let Some(bid) = crate::network::business_id(&state).await else {
        return Err(err(StatusCode::CONFLICT, "node has no business yet"));
    };
    if is_owner(&state, bid, &phone).await {
        let v = crate::owner::chat(&state, req.text.trim()).await.map_err(agent_err)?;
        tracing::info!(id = ?req.id, "node message from the owner answered by the manager agent");
        return Ok(Json(json!({
            "text": for_whatsapp(v["text"].as_str().unwrap_or("")), "action": v["action"], "actionData": v["actionData"], "agent": "owner",
        })));
    }
    let peer = crate::node::peer_of(&phone);
    if crate::node::is_opt_out(&req.text) {
        crate::node::opt_out(&state, &phone, &req.text).await;
        let _ = append_message(&state, bid, "customer", &peer, "user", req.text.trim()).await;
        let bye = "Listo, no te escribo más. ¡Gracias y que te vaya muy bien! 🙏";
        let _ = append_message(&state, bid, "customer", &peer, "assistant", bye).await;
        return Ok(Json(json!({"text": bye, "action": "opt_out"})));
    }
    if crate::node::opted_out(&state, &phone).await {
        return Ok(Json(json!({"text": "", "action": "ignored_opted_out"})));
    }
    crate::node::note_reply(&state, &phone).await;
    if let Some(n) = req.name.as_deref().filter(|n| !n.trim().is_empty()) {
        crate::contacts::touch(&state.db, bid, &peer).await;
        let _ = sqlx::query("UPDATE contacts SET name = COALESCE(name, $3) WHERE business_id = $1 AND peer = $2")
            .bind(bid).bind(&peer).bind(n.trim()).execute(&state.db).await;
    }
    let turn = customer_turn(&state, bid, &peer, req.text.trim()).await?;
    let text = for_whatsapp(turn["agentResponse"].as_str().unwrap_or(""));
    tracing::info!(%phone, id = ?req.id, "node message answered");
    Ok(Json(json!({"text": text, "action": turn["action"], "actionData": turn["actionData"]})))
}

#[derive(Deserialize)]
pub(super) struct NodeTurnReq { #[serde(rename = "callId")] call_id: String, from: String, text: String, #[serde(default)] purpose: Option<String>, #[serde(default)] outbound: bool }

pub(super) async fn node_turn(State(state): State<SharedState>, Json(req): Json<NodeTurnReq>) -> ApiResult {
    let phone = crate::node::digits(&req.from);
    if phone.len() < 8 || req.text.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "from and text required"));
    }
    let Some(bid) = crate::network::business_id(&state).await else {
        return Err(err(StatusCode::CONFLICT, "node has no business yet"));
    };
    let peer = crate::node::peer_of(&phone);
    if let Some(n) = crate::node::node(&state) {
        n.calls.lock().unwrap_or_else(|e| e.into_inner()).insert(peer.clone(), (req.purpose.clone().unwrap_or_else(|| "inbound call".into()), req.outbound));
    }
    let turn = customer_turn(&state, bid, &peer, &format!("(por llamada de voz) {}", req.text.trim())).await?;
    let mut text = turn["agentResponse"].as_str().unwrap_or("").to_string();
    let end = text.contains("[FIN]");
    text = text.replace("[FIN]", "").replace("**", "").replace('*', "").trim().to_string();
    tracing::info!(%phone, call = %req.call_id, end, "node call turn answered");
    Ok(Json(json!({"text": text, "end": end, "action": turn["action"]})))
}

#[derive(Deserialize)]
pub(super) struct NodeCallEndedReq { #[serde(rename = "callId")] call_id: String, from: String, reason: String, #[serde(default)] seconds: i64 }

pub(super) async fn node_call_ended(State(state): State<SharedState>, Json(req): Json<NodeCallEndedReq>) -> ApiResult {
    let phone = crate::node::digits(&req.from);
    let peer = crate::node::peer_of(&phone);
    if let Some(n) = crate::node::node(&state) {
        n.calls.lock().unwrap_or_else(|e| e.into_inner()).remove(&peer);
    }
    if let Some(bid) = crate::network::business_id(&state).await {
        let _ = append_message(&state, bid, "customer", &peer, "assistant", &format!("(llamada terminada: {}, {} s)", req.reason, req.seconds)).await;
    }
    tracing::info!(%phone, call = %req.call_id, reason = %req.reason, "node call ended");
    Ok(Json(json!({"ok": true})))
}
