//! The owner's work queue (D15): swiping an order or an appointment as
//! done, undoing it, cancelling, marking a no-show, confirming cash.

use super::*;
use axum::extract::Path;

#[derive(Deserialize)]
pub(super) struct StatusReq {
    /// done | undo | cancelled | no_show | paid
    status: String,
}

fn parse_id(id: &str) -> Result<Uuid, (StatusCode, Json<Value>)> {
    Uuid::parse_str(id).map_err(|_| err(StatusCode::BAD_REQUEST, "bad id"))
}

/// `POST /api/orders/{id}` — the pedidos board.
pub(super) async fn order_status(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<StatusReq>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let id = parse_id(&id)?;
    let sql = match req.status.as_str() {
        "done" => "UPDATE orders SET status = 'done', done_at = $3 WHERE business_id = $1 AND id = $2 AND status <> 'cancelled'",
        "undo" => "UPDATE orders SET status = CASE WHEN paid THEN 'confirmed' ELSE 'pending_payment' END, done_at = NULL WHERE business_id = $1 AND id = $2",
        "cancelled" => "UPDATE orders SET status = 'cancelled', done_at = NULL WHERE business_id = $1 AND id = $2",
        // The owner took cash / saw the transfer on their own screen.
        "paid" => "UPDATE orders SET paid = TRUE, status = CASE WHEN status = 'pending_payment' THEN 'confirmed' ELSE status END WHERE business_id = $1 AND id = $2",
        _ => return Err(err(StatusCode::BAD_REQUEST, "status must be done | undo | cancelled | paid")),
    };
    let n = sqlx::query(sql)
        .bind(business_id)
        .bind(id)
        .bind(crate::db::now())
        .execute(&state.db)
        .await
        .map_err(internal)?
        .rows_affected();
    if n == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such order"));
    }
    let row: (String, bool) = sqlx::query_as("SELECT status, paid FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    crate::audit::record(&state, Some(business_id), crate::audit::kind::STATUS, "owner", &id.to_string(),
        json!({"what": "order", "set": req.status, "status": row.0, "paid": row.1})).await;
    Ok(Json(json!({"id": id, "status": row.0, "paid": row.1})))
}

/// `POST /api/appointments/{id}` — the agenda.
pub(super) async fn appointment_status(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<StatusReq>,
) -> ApiResult {
    let business_id = auth(&state, &headers).await?;
    let id = parse_id(&id)?;
    let sql = match req.status.as_str() {
        "done" => "UPDATE appointments SET status = 'done', done_at = $3 WHERE business_id = $1 AND id = $2 AND status <> 'cancelled'",
        "undo" => "UPDATE appointments SET status = CASE WHEN paid THEN 'confirmed' ELSE 'pending_payment' END, done_at = NULL WHERE business_id = $1 AND id = $2",
        "cancelled" => "UPDATE appointments SET status = 'cancelled', done_at = NULL WHERE business_id = $1 AND id = $2",
        "no_show" => "UPDATE appointments SET status = 'no_show', done_at = $3 WHERE business_id = $1 AND id = $2",
        "paid" => "UPDATE appointments SET paid = TRUE, status = CASE WHEN status = 'pending_payment' THEN 'confirmed' ELSE status END WHERE business_id = $1 AND id = $2",
        _ => return Err(err(StatusCode::BAD_REQUEST, "status must be done | undo | cancelled | no_show | paid")),
    };
    let n = sqlx::query(sql)
        .bind(business_id)
        .bind(id)
        .bind(crate::db::now())
        .execute(&state.db)
        .await
        .map_err(internal)?
        .rows_affected();
    if n == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such appointment"));
    }
    let row: (String, bool) = sqlx::query_as("SELECT status, paid FROM appointments WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    crate::audit::record(&state, Some(business_id), crate::audit::kind::STATUS, "owner", &id.to_string(),
        json!({"what": "appointment", "set": req.status, "status": row.0, "paid": row.1})).await;
    Ok(Json(json!({"id": id, "status": row.0, "paid": row.1})))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, api, Mock};
    use serde_json::json;

    #[tokio::test]
    async fn appointment_lifecycle() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        let a = testkit::appointment(&s.db, b, "Ana", "pending_payment", false, Some(30.0), 2).await;
        let set = |st: &'static str| json!({"status": st});
        let path = format!("/api/appointments/{a}");
        let (c, v) = api(&s, "POST", &path, Some(&t), Some(set("paid"))).await;
        assert_eq!((c, v["status"].clone(), v["paid"].clone()), (200, json!("confirmed"), json!(true)));
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(set("done"))).await.1["status"], "done");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(set("undo"))).await.1["status"], "confirmed", "paid → back to confirmed");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(set("no_show"))).await.1["status"], "no_show");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(set("cancelled"))).await.1["status"], "cancelled");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(set("done"))).await.0, 404, "a cancelled booking cannot be done");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(set("teleport"))).await.0, 400);
        assert_eq!(api(&s, "POST", "/api/appointments/not-a-uuid", Some(&t), Some(set("done"))).await.0, 400);
        assert_eq!(api(&s, "POST", &path, None, Some(set("done"))).await.0, 401);
        let audited: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE kind = 'status'").fetch_one(&s.db).await.unwrap();
        assert_eq!(audited, 5);
    }

    #[tokio::test]
    async fn order_lifecycle_and_undo_of_unpaid() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        let o = testkit::order(&s.db, b, "Carlos", "pending_payment", false, Some(20.0)).await;
        let path = format!("/api/orders/{o}");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(json!({"status": "done"}))).await.1["status"], "done");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(json!({"status": "undo"}))).await.1["status"], "pending_payment");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(json!({"status": "no_show"}))).await.0, 400, "orders have no no-show");
        assert_eq!(api(&s, "POST", &path, Some(&t), Some(json!({"status": "paid"}))).await.1["paid"], true);
    }

    #[tokio::test]
    async fn one_business_cannot_touch_anothers_queue() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, _) = testkit::onboard(&s, &m).await;
        let other = testkit::business(&s.db).await;
        let a = testkit::appointment(&s.db, other, "X", "confirmed", false, None, 1).await;
        let o = testkit::order(&s.db, other, "Y", "confirmed", false, None).await;
        assert_eq!(api(&s, "POST", &format!("/api/appointments/{a}"), Some(&t), Some(json!({"status": "cancelled"}))).await.0, 404);
        assert_eq!(api(&s, "POST", &format!("/api/orders/{o}"), Some(&t), Some(json!({"status": "cancelled"}))).await.0, 404);
    }

    /// Pinned (flagged as a billing question): the owner cancelling or
    /// confirming cash here does not reverse or record an outcome, unlike
    /// the agent's handle_cancellation and the Yape webhook.
    #[tokio::test]
    async fn owner_actions_do_not_touch_outcomes() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (t, b) = testkit::onboard(&s, &m).await;
        let a = testkit::appointment(&s.db, b, "Ana", "pending_payment", false, Some(30.0), 2).await;
        api(&s, "POST", &format!("/api/appointments/{a}"), Some(&t), Some(json!({"status": "paid"}))).await;
        api(&s, "POST", &format!("/api/appointments/{a}"), Some(&t), Some(json!({"status": "cancelled"}))).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outcomes").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 0);
    }
}
