use super::*;
use crate::testkit::{self, account_with_agent, as_admin, Keypair, COLLECTOR_KEY};

const WHATSAPP: &str = "com.whatsapp";

fn ms(t: chrono::DateTime<chrono::Utc>) -> i64 {
    t.timestamp_millis()
}

fn now_ms() -> i64 {
    ms(chrono::Utc::now())
}

async fn post_as(app: &Shared, key: &str, package: &str, posted_at: i64, title: &str, text: &str) -> (u16, Value) {
    let body = json!({"package": package, "postedAt": posted_at, "title": title, "text": text});
    testkit::call(app, "POST", "/v1/collector/yape",
        &[("x-collector-key", key.to_string()), ("content-type", "application/json".into())],
        Some(serde_json::to_vec(&body).unwrap())).await
}

/// The house phone forwards one Yape notification.
async fn yape(app: &Shared, posted_at: i64, text: &str) -> Value {
    let (s, v) = post_as(app, COLLECTOR_KEY, YAPE, posted_at, "Confirmación de Pago", text).await;
    assert_eq!(s, 200, "{v}");
    v
}

fn soles(minor: i64) -> String {
    format!("{}.{:02}", minor / 100, minor % 100)
}

async fn friend(app: &Shared, id: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    account_with_agent(app, id, &format!("51980{n:06}"), &Keypair::generate()).await;
}

async fn recarga(app: &Shared, account: &str, minor: i64) -> (String, i64) {
    let r = plans::open_credits_request(app, &format!("acct:{account}"), minor).await.unwrap();
    (r["ref"].as_str().unwrap().to_string(), (r["amount"].as_f64().unwrap() * 100.0).round() as i64)
}

async fn status_of(app: &Shared, reference: &str) -> String {
    sqlx::query_as::<_, (String,)>("SELECT status FROM plan_requests WHERE ref = $1").bind(reference).fetch_one(&app.db).await.unwrap().0
}

async fn balance(app: &Shared, account: &str) -> i64 {
    crate::credits::balance(app, account).await.unwrap()
}

/// Makes `reference` past its life (still `pending` until the next sweep).
async fn expire(app: &Shared, reference: &str, ago_minutes: i64) {
    sqlx::query("UPDATE plan_requests SET expires_at = strftime('%Y-%m-%dT%H:%M:%fZ','now', $2) WHERE ref = $1")
        .bind(reference).bind(format!("-{ago_minutes} minutes")).execute(&app.db).await.unwrap();
}

fn minutes_ago(m: i64) -> i64 {
    now_ms() - m * 60_000
}

fn credited(minor: i64) -> i64 {
    minor + crate::wallet::bonus_for(minor)
}

// ------------------------------------------------------------------ reading

#[test]
fn reads_the_ways_yape_says_money_came_in() {
    let r = read("Confirmación de Pago", "Yape! Ana Pérez te envió un pago por S/ 20.37. El cód. de seguridad es: 482");
    assert_eq!(r, Reading { incoming: true, amount_minor: Some(2037), payer: Some("Ana Pérez".into()) });
    let r = read("Yape", "Luis Q. te yapeó S/ 35");
    assert_eq!((r.incoming, r.amount_minor, r.payer.as_deref()), (true, Some(3500), Some("Luis Q.")));
    let r = read("", "Recibiste S/ 1,250.05 de JUAN CARLOS RAMOS");
    assert_eq!((r.incoming, r.amount_minor, r.payer.as_deref()), (true, Some(125_005), Some("JUAN CARLOS RAMOS")));
    let r = read("", "Recibiste un yapeo de Rosa por S/ 20");
    assert_eq!((r.incoming, r.amount_minor, r.payer.as_deref()), (true, Some(2000), Some("Rosa")));
    assert_eq!(read("", "Maria te envio S/.25.50").amount_minor, Some(2550));
    assert_eq!(read("", "Maria te envió S/ 5.").amount_minor, Some(500));
    assert_eq!(read("", "Maria te envió S/ 20,37").amount_minor, Some(2037), "decimal comma");
    assert_eq!(read("", "Maria te envió S/ 1.000").amount_minor, Some(100_000), "three digits after a dot group thousands");
    assert_eq!(read("", "Maria te envió S/\u{a0}9.9").amount_minor, Some(990));
}

#[test]
fn money_going_out_and_promos_are_not_payments() {
    assert!(!read("", "Yapeaste S/ 20.37 a Rosa").incoming);
    assert!(!read("Yape", "Enviaste S/ 20 a Rosa").incoming);
    let promo = read("Yape", "¡Gana S/ 50 invitando a tus amigos!");
    assert_eq!((promo.incoming, promo.amount_minor), (false, Some(5000)));
    assert_eq!(read("Yape", "Tu código de verificación es 123456").amount_minor, None);
}

// ------------------------------------------------------------------ tagging

#[test]
fn offsets_go_exact_then_plus_one_minus_one() {
    let first: Vec<i64> = plans::offsets().take(7).collect();
    assert_eq!(first, vec![0, 1, -1, 2, -2, 3, -3]);
    assert_eq!(plans::offsets().count(), 199);
}

#[test]
fn a_request_lives_until_midnight_in_lima_and_at_least_an_hour() {
    use chrono::TimeZone;
    let noon_lima = chrono::Utc.with_ymd_and_hms(2026, 9, 23, 17, 0, 0).unwrap(); // 12:00 in Lima (UTC-5)
    assert_eq!(plans::expires_at(noon_lima), chrono::Utc.with_ymd_and_hms(2026, 9, 24, 5, 0, 0).unwrap());
    let late_lima = chrono::Utc.with_ymd_and_hms(2026, 9, 24, 4, 50, 0).unwrap(); // 23:50 in Lima
    assert_eq!(plans::expires_at(late_lima), late_lima + chrono::Duration::minutes(60));
}

#[tokio::test]
async fn first_come_pays_the_price_and_cents_come_back_when_cleared() {
    let app = testkit::app().await;
    let mut got = vec![];
    for i in 0..5 {
        let id = format!("f{i}");
        friend(&app, &id).await;
        got.push(recarga(&app, &id, 2_000).await);
    }
    let amounts: Vec<i64> = got.iter().map(|g| g.1).collect();
    assert_eq!(amounts, vec![2_000, 2_001, 1_999, 2_002, 1_998], "FIFO: exact, +1, −1, +2, −2");
    let (_, e) = recarga(&app, "f0", 2_000).await;
    assert_eq!(e, 2_000, "asking again reuses the pending request");
    let r = plans::open_credits_request(&app, "acct:f0", 2_000).await.unwrap();
    assert!(r["expiresAt"].is_string(), "{r}");

    // f0 pays: its S/ 20.00 is free again, and the next buyer gets it.
    yape(&app, now_ms(), "f0 te envió un pago por S/ 20.00").await;
    assert_eq!(status_of(&app, &got[0].0).await, "paid");
    friend(&app, "next").await;
    assert_eq!(recarga(&app, "next", 2_000).await.1, 2_000);
    // f1 lets the day run out: its +1 is free again too.
    expire(&app, &got[1].0, 1).await;
    friend(&app, "after").await;
    assert_eq!(recarga(&app, "after", 2_000).await.1, 2_001);
    assert_eq!(status_of(&app, &got[1].0).await, "expired");
    // A price nobody holds is always exact.
    assert_eq!(recarga(&app, "f0", 5_000).await.1, 5_000);
}

#[tokio::test]
async fn a_price_runs_out_of_amounts_rather_than_sharing_one() {
    let app = testkit::app().await;
    for tag in plans::offsets() {
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, amount_minor, base_minor, yape_tag, expires_at) \
                     VALUES ($1,'acct:x','credits',$2,'PEN',2000,$3,2000,$4, strftime('%Y-%m-%dT%H:%M:%fZ','now','+1 hour'))")
            .bind(format!("R{tag}")).bind((2000 + tag) as f64 / 100.0).bind(2000 + tag).bind(tag).execute(&app.db).await.unwrap();
    }
    friend(&app, "full").await;
    let e = plans::open_credits_request(&app, "acct:full", 2_000).await.unwrap_err();
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    sqlx::query("UPDATE plan_requests SET expires_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 minute') WHERE ref = 'R0'").execute(&app.db).await.unwrap();
    assert_eq!(recarga(&app, "full", 2_000).await.1, 2_000, "the exact price freed first is taken first");
}

#[tokio::test]
async fn two_buyers_at_the_same_instant_get_different_amounts() {
    let app = testkit::app().await;
    friend(&app, "a").await;
    friend(&app, "b").await;
    let (x, y) = tokio::join!(recarga(&app, "a", 2_000), recarga(&app, "b", 2_000));
    let mut both = [x.1, y.1];
    both.sort();
    assert_eq!(both, [2_000, 2_001]);
}

// ----------------------------------------------------------------- matching

#[tokio::test]
async fn an_odd_amount_pays_its_request_exactly_once() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    friend(&app, "beto").await;
    let (ra, aa) = recarga(&app, "ana", 3_700).await; // odd recarga: S/ 37.00
    let (rb, ab) = recarga(&app, "beto", 3_700).await; // S/ 37.01
    assert_eq!((aa, ab), (3_700, 3_701));
    let t = now_ms();
    let text = format!("Yape! Beto Ruiz te envió un pago por S/ {}. El cód. de seguridad es: 204", soles(ab));
    let v = yape(&app, t, &text).await;
    assert_eq!((v["status"].clone(), v["ref"].clone(), v["matchKind"].clone(), v["duplicate"].clone()),
               (json!("matched"), json!(rb), json!("exact"), json!(false)), "{v}");
    assert_eq!(balance(&app, "beto").await, credited(3_700), "credited what was bought, not the céntimo");
    assert_eq!(status_of(&app, &ra).await, "pending", "the other S/ 37 buyer is untouched");
    let (paid,): (Option<i64>,) = sqlx::query_as("SELECT paid_minor FROM plan_requests WHERE ref = $1").bind(&rb).fetch_one(&app.db).await.unwrap();
    assert_eq!(paid, Some(ab));

    // Android reposts the same notification and the phone retries: one row.
    for _ in 0..3 {
        let again = yape(&app, t, &text).await;
        assert_eq!((again["duplicate"].clone(), again["status"].clone()), (json!(true), json!("matched")));
    }
    assert_eq!(balance(&app, "beto").await, credited(3_700), "no second credit");
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM yape_inbox").fetch_one(&app.db).await.unwrap();
    assert_eq!(rows, 1);

    // A genuinely second transfer of that amount (new notification time) is
    // money we hold for a refund, not a second recarga.
    let second = yape(&app, t + 60_000, &text).await;
    assert_eq!((second["status"].clone(), second["reason"].clone()), (json!("unmatched"), json!("already_paid")));
    assert_eq!(balance(&app, "beto").await, credited(3_700));
}

#[tokio::test]
async fn a_buyer_paying_one_centimo_less_gets_everything() {
    let app = testkit::app().await;
    for f in ["a", "b", "c"] { friend(&app, f).await; }
    recarga(&app, "a", 2_000).await;
    recarga(&app, "b", 2_000).await;
    let (rc, c) = recarga(&app, "c", 2_000).await;
    assert_eq!(c, 1_999);
    let v = yape(&app, now_ms(), "C te envió un pago por S/ 19.99").await;
    assert_eq!(v["ref"], json!(rc));
    assert_eq!(balance(&app, "c").await, credited(2_000));
}

#[tokio::test]
async fn two_transfers_of_one_amount_at_once_credit_once() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    let (_, amount) = recarga(&app, "ana", 2_000).await;
    let t = now_ms();
    let text = format!("Ana te envió un pago por S/ {}", soles(amount));
    let (x, y) = tokio::join!(yape(&app, t, &text), yape(&app, t + 1, &text));
    let matched = [&x, &y].iter().filter(|v| v["status"] == "matched").count();
    assert_eq!(matched, 1, "{x} {y}");
    assert_eq!(balance(&app, "ana").await, credited(2_000));
}

#[tokio::test]
async fn a_rounded_amount_matches_only_when_one_request_fits() {
    let app = testkit::app().await;
    for f in ["ana", "beto", "caro", "dani", "eli"] { friend(&app, f).await; }
    let (ra, _) = recarga(&app, "ana", 2_000).await; // 20.00
    let (rb, b) = recarga(&app, "beto", 2_000).await; // 20.01
    assert_eq!(b, 2_001);
    yape(&app, now_ms(), "Ana te envió un pago por S/ 20.00").await;
    assert_eq!(status_of(&app, &ra).await, "paid");
    // Beto drops the céntimo: S/ 20 for S/ 20.01, and nobody else fits.
    let v = yape(&app, now_ms() + 5, "Beto te envió un pago por S/ 20").await;
    assert_eq!((v["status"].clone(), v["ref"].clone(), v["matchKind"].clone()), (json!("matched"), json!(rb), json!("rounded")), "{v}");
    assert_eq!(balance(&app, "beto").await, credited(2_000));

    // Rounded up to the next sol also counts.
    recarga(&app, "eli", 5_000).await; // 50.00, holds the price
    friend(&app, "fer").await;
    let (rf, f) = recarga(&app, "fer", 5_000).await; // 50.01
    assert_eq!(f, 5_001);
    let v = yape(&app, now_ms() + 10, "Fer te yapeó S/ 51.00").await;
    assert_eq!((v["ref"].clone(), v["matchKind"].clone()), (json!(rf), json!("rounded")), "{v}");

    // Caro owes 30.01 and Dani 29.99 (someone paid the 30.00 already); a
    // round S/ 30 could be either, so a human decides.
    friend(&app, "x").await;
    let (rx, _) = recarga(&app, "x", 3_000).await;
    let (rc, c) = recarga(&app, "caro", 3_000).await;
    let (rd, d) = recarga(&app, "dani", 3_000).await;
    assert_eq!((c, d), (3_001, 2_999));
    yape(&app, now_ms() + 15, "X te envió un pago por S/ 30.00").await;
    assert_eq!(status_of(&app, &rx).await, "paid");
    let v = yape(&app, now_ms() + 20, "Caro te envió un pago por S/ 30").await;
    assert_eq!((v["status"].clone(), v["reason"].clone()), (json!("unmatched"), json!("ambiguous")), "{v}");
    let (_, q) = as_admin(&app, "GET", "/admin/yape", None).await;
    let item = &q["items"][0];
    let sug: Vec<&str> = item["suggestions"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(sug.contains(&rc.as_str()) && sug.contains(&rd.as_str()), "{q}");
    assert_eq!(item["payer"], json!("Caro"));
    assert_eq!(q["counts"]["unmatched"], json!(1));

    let id = item["id"].as_str().unwrap();
    let (s, r) = as_admin(&app, "POST", &format!("/admin/yape/{id}/assign"), Some(json!({"ref": rc, "note": "Caro confirmó por WhatsApp"}))).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(balance(&app, "caro").await, credited(3_000));
    assert_eq!(status_of(&app, &rd).await, "pending");
    let (s, _) = as_admin(&app, "POST", &format!("/admin/yape/{id}/assign"), Some(json!({"ref": rd}))).await;
    assert_eq!(s, 409, "assigned once");
}

#[tokio::test]
async fn paid_before_midnight_counts_even_when_the_notification_is_late() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    friend(&app, "beto").await;
    let (ra, aa) = recarga(&app, "ana", 2_000).await;
    expire(&app, &ra, 10).await; // the day ended ten minutes ago
    // Beto opens after: the price is free again, so he gets S/ 20.00 too.
    let (rb, ab) = recarga(&app, "beto", 2_000).await;
    assert_eq!((aa, ab), (2_000, 2_000));
    assert_eq!(status_of(&app, &ra).await, "expired");
    // Ana paid 15 minutes ago (the phone was offline): both requests held
    // S/ 20.00 at some point, so a person decides — nobody is credited twice
    // or credited someone else's money.
    let v = yape(&app, minutes_ago(15), "Ana te envió un pago por S/ 20.00").await;
    assert_eq!((v["status"].clone(), v["reason"].clone()), (json!("unmatched"), json!("ambiguous")), "{v}");
    // Beto pays now: only his request is live.
    let v = yape(&app, now_ms(), "Beto te envió un pago por S/ 20.00").await;
    assert_eq!((v["status"].clone(), v["ref"].clone()), (json!("matched"), json!(rb)), "{v}");
}

#[tokio::test]
async fn a_notification_delayed_past_midnight_still_pays() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    let (ra, aa) = recarga(&app, "ana", 2_000).await;
    expire(&app, &ra, 10).await;
    let v = yape(&app, minutes_ago(15), &format!("Ana te envió un pago por S/ {}", soles(aa))).await;
    assert_eq!((v["status"].clone(), v["ref"].clone(), v["matchKind"].clone()), (json!("matched"), json!(ra), json!("exact")), "{v}");
}

#[tokio::test]
async fn a_late_payment_lands_when_one_expired_request_fits_and_queues_otherwise() {
    let app = testkit::app().await;
    for f in ["ana", "beto", "caro"] { friend(&app, f).await; }
    let (ra, aa) = recarga(&app, "ana", 4_400).await;
    expire(&app, &ra, 60 * 20).await; // yesterday's request
    let v = yape(&app, now_ms(), &format!("Ana te envió un pago por S/ {}", soles(aa))).await;
    assert_eq!((v["status"].clone(), v["ref"].clone(), v["matchKind"].clone()), (json!("matched"), json!(ra), json!("late")), "{v}");
    assert_eq!(balance(&app, "ana").await, credited(4_400));

    // Two expired S/ 30.00 requests, one late S/ 30.00: whose?
    let (rb, _) = recarga(&app, "beto", 3_000).await;
    expire(&app, &rb, 60 * 20).await;
    let (rc, _) = recarga(&app, "caro", 3_000).await;
    expire(&app, &rc, 60 * 10).await;
    let v = yape(&app, now_ms(), "Caro te envió un pago por S/ 30.00").await;
    assert_eq!((v["status"].clone(), v["reason"].clone()), (json!("unmatched"), json!("late")), "{v}");
    let id = v["id"].as_str().unwrap();
    let (s, r) = as_admin(&app, "POST", &format!("/admin/yape/{id}/assign"), Some(json!({"ref": rc}))).await;
    assert_eq!(s, 200, "an operator can still honour it: {r}");
    assert_eq!(balance(&app, "caro").await, credited(3_000));
}

#[tokio::test]
async fn a_payment_for_a_request_paid_by_card_is_held_for_refund() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    let (ra, aa) = recarga(&app, "ana", 2_000).await;
    sqlx::query("UPDATE plan_requests SET status = 'cancelled' WHERE ref = $1").bind(&ra).execute(&app.db).await.unwrap();
    let v = yape(&app, now_ms(), &format!("Ana te envió un pago por S/ {}", soles(aa))).await;
    assert_eq!(v["reason"], json!("cancelled"));
    let id = v["id"].as_str().unwrap();
    let (s, _) = as_admin(&app, "POST", &format!("/admin/yape/{id}/assign"), Some(json!({"ref": ra}))).await;
    assert_eq!(s, 409);
    let (s, _) = as_admin(&app, "POST", &format!("/admin/yape/{id}/dismiss"), Some(json!({"note": "devuelto por Yape"}))).await;
    assert_eq!(s, 200);
    assert_eq!(balance(&app, "ana").await, 0);
    let (_, q) = as_admin(&app, "GET", "/admin/yape?status=dismissed", None).await;
    assert_eq!(q["items"][0]["note"], json!("devuelto por Yape"));
}

#[tokio::test]
async fn a_plan_paid_by_yape_turns_on() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    let r = plans::open_request(&app, "acct:ana", "pro", 1).await.unwrap();
    assert_eq!(r["amount"], json!(100.0), "{r}");
    let v = yape(&app, now_ms(), "Yape! Ana te envió un pago por S/ 100.00").await;
    assert_eq!(v["status"], json!("matched"));
    assert_eq!(plans::effective_for(&app, "acct:ana").await.unwrap().plan, "pro");
}

#[tokio::test]
async fn requests_from_before_the_tags_still_match_their_round_price() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months) VALUES ('YAYA-OLD','acct:ana','credits',50.0,'PEN',5000)")
        .execute(&app.db).await.unwrap();
    let v = yape(&app, now_ms(), "Ana te envió un pago por S/ 50.00").await;
    assert_eq!((v["ref"].clone(), v["matchKind"].clone()), (json!("YAYA-OLD"), json!("exact")), "{v}");
}

#[tokio::test]
async fn what_matches_nothing_waits_in_the_queue() {
    let app = testkit::app().await;
    let v = yape(&app, now_ms(), "Pedro te envió un pago por S/ 77.77").await;
    assert_eq!((v["status"].clone(), v["reason"].clone()), (json!("unmatched"), json!("no_request")));
    let v = yape(&app, now_ms(), "Tienes un nuevo movimiento de S/ 12.34").await;
    assert_eq!((v["status"].clone(), v["reason"].clone()), (json!("unmatched"), json!("unread")), "unknown wording is a human's call");
    let v = yape(&app, now_ms(), "Yapeaste S/ 10 a Rosa").await;
    assert_eq!(v["reason"], json!("unread"));
    let v = yape(&app, now_ms(), "Tu código de seguridad es 991").await;
    assert_eq!((v["status"].clone(), v["reason"].clone()), (json!("ignored"), json!("no_amount")));

    // A buyer who paid first and opened the request after: retry matches.
    friend(&app, "pedro").await;
    let (rp, p) = recarga(&app, "pedro", 7_700).await;
    assert_eq!(p, 7_700);
    let (_, q) = as_admin(&app, "GET", "/admin/yape", None).await;
    let id = q["items"].as_array().unwrap().iter().find(|i| i["amountMinor"] == json!(7777)).unwrap()["id"].as_str().unwrap().to_string();
    let (s, r) = as_admin(&app, "POST", &format!("/admin/yape/{id}/retry"), None).await;
    assert_eq!((s, r["status"].clone(), r["reason"].clone()), (200, json!("unmatched"), json!("no_request")), "77.77 is not 77.00: {r}");
    let (s, r) = as_admin(&app, "POST", &format!("/admin/yape/{id}/assign"), Some(json!({"ref": rp}))).await;
    assert_eq!(s, 200, "{r}");
}

#[tokio::test]
async fn only_the_house_phone_and_only_yape() {
    let app = testkit::app().await;
    friend(&app, "ana").await;
    let (_, amount) = recarga(&app, "ana", 2_000).await;
    let text = format!("Ana te envió un pago por S/ {}", soles(amount));
    let (s, _) = post_as(&app, "wrong-key-wrong-key-wrong-key-wrong", YAPE, now_ms(), "", &text).await;
    assert_eq!(s, 401);
    // A chat message quoting a Yape receipt is not a payment.
    let (s, _) = post_as(&app, COLLECTOR_KEY, WHATSAPP, now_ms(), "Ana", &text).await;
    assert_eq!(s, 422);
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM yape_inbox").fetch_one(&app.db).await.unwrap();
    assert_eq!(rows, 0, "nothing stored from the wrong package");
    assert_eq!(balance(&app, "ana").await, 0);

    let closed = testkit::app_with(|a| crate::App { yape_collector_key: None, ..a }).await;
    let (s, _) = post_as(&closed, COLLECTOR_KEY, YAPE, now_ms(), "", &text).await;
    assert_eq!(s, 401, "no key configured = closed");
    let (s, _) = testkit::anon(&app, "GET", "/admin/yape", None).await;
    assert_eq!(s, 401);
}
