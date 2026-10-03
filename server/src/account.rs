//! The Yaya ID this installation is signed in as. Sign-in is a one-time
//! code (WhatsApp + email) checked at the gateway with a request signed by
//! the agent, which links the agent to the account on the spot and hands
//! back a device session. The session and the account's backup key live
//! sealed in the core's own database.

use serde_json::{json, Value};

use crate::{identity, AppState};

const SESSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct Signed {
    pub account_id: String,
    pub email: String,
    pub name: Option<String>,
    pub phone: Option<String>,
    pub session: String,
    pub backup_key: Option<[u8; 32]>,
    pub plan: Option<String>,
    pub last_backup_at: Option<String>,
}

pub async fn load(state: &AppState) -> anyhow::Result<Option<Signed>> {
    let row: Option<(String, String, Option<String>, Option<String>, Vec<u8>, Option<Vec<u8>>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT account_id, email, name, phone, session, backup_key, plan, last_backup_at FROM account WHERE id = 1",
    ).fetch_optional(&state.db).await?;
    let Some((account_id, email, name, phone, session, backup_key, plan, last_backup_at)) = row else { return Ok(None) };
    let session = String::from_utf8(identity::from_rest(&session, None)?)?;
    let backup_key = match backup_key {
        Some(b) => Some(identity::from_rest(&b, Some(32))?.try_into().map_err(|_| anyhow::anyhow!("bad backup key"))?),
        None => None,
    };
    Ok(Some(Signed { account_id, email, name, phone, session, backup_key, plan, last_backup_at }))
}

fn device_label() -> String {
    std::env::var("DEVICE_LABEL").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "agente".into())
}

/// What this installation is on the account: a `phone` (the app), a `node`
/// (this core on a machine with its own WhatsApp line), or `web`.
fn device_kind() -> String {
    match std::env::var("DEVICE_KIND").ok().as_deref() { Some("node") => "node".into(), Some("web") => "web".into(), _ => "phone".into() }
}

/// Provisioning path for nodes: the owner mints a one-time link token in
/// their console; this core presents it (signed, as itself) and the
/// gateway links it to the account as `DEVICE_KIND` and hands back a
/// device session — exactly what a sign-in would, without a code to relay.
pub async fn adopt(state: &AppState, token: &str) -> anyhow::Result<Signed> {
    let v = state.registry.post("/v1/account/agents", &json!({"token": token, "label": device_label(), "kind": device_kind()}), SESSION_TIMEOUT).await?;
    anyhow::ensure!(v["session"].is_string(), "gateway did not return a session for this token");
    let signed = store(state, &v).await?;
    crate::network::sync_plan(state).await;
    Ok(signed)
}

async fn store(state: &AppState, v: &Value) -> anyhow::Result<Signed> {
    let account_id = v["account"]["id"].as_str().ok_or_else(|| anyhow::anyhow!("no account in reply"))?.to_string();
    let email = v["account"]["email"].as_str().unwrap_or("").to_string();
    let name = v["account"]["name"].as_str().map(String::from);
    let phone = v["account"]["phone"].as_str().map(String::from);
    let session = v["session"].as_str().ok_or_else(|| anyhow::anyhow!("no session in reply"))?.to_string();
    let plan = v["plan"].as_str().map(String::from);
    // The agent is linked now: fetch the account's backup key as this agent.
    let key: Option<[u8; 32]> = state.registry.get("/v1/account/backup-key", SESSION_TIMEOUT).await.ok()
        .and_then(|k| k["key"].as_str().and_then(|h| hex::decode(h).ok()))
        .and_then(|b| b.try_into().ok());
    if key.is_none() {
        tracing::warn!("backup key not fetched; backups will retry after the next sign-in");
    }
    sqlx::query(
        "INSERT INTO account (id, account_id, email, name, phone, session, backup_key, plan) VALUES (1,$1,$2,$3,$4,$5,$6,$7) \
         ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id, email = excluded.email, name = excluded.name, phone = excluded.phone, \
           session = excluded.session, backup_key = excluded.backup_key, plan = excluded.plan, \
           linked_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), last_backup_at = NULL",
    )
    .bind(&account_id).bind(&email).bind(&name).bind(&phone)
    .bind(identity::at_rest(session.as_bytes())?).bind(match key { Some(k) => Some(identity::at_rest(&k)?), None => None }).bind(&plan)
    .execute(&state.db).await?;
    let _ = set_setting(&state.db, "guest", "0").await;
    tracing::info!(account = %account_id, %email, "signed in to Yaya ID");
    Ok(Signed { account_id, email, name, phone, session, backup_key: key, plan, last_backup_at: None })
}

/// Asks the gateway to send a code to this person (WhatsApp + email).
pub async fn otp_start(state: &AppState, email: Option<&str>, phone: Option<&str>, name: Option<&str>) -> anyhow::Result<Value> {
    state.registry.post("/v1/accounts/otp/start", &json!({"email": email, "phone": phone, "name": name}), SESSION_TIMEOUT).await
}

/// "Continuar con Yaya": the code proves the person; the agent is linked.
pub async fn otp_check(state: &AppState, email: Option<&str>, phone: Option<&str>, code: &str, name: Option<&str>) -> anyhow::Result<Signed> {
    let v = state.registry.post("/v1/accounts/otp/check", &json!({"email": email, "phone": phone, "code": code, "name": name, "device": device_label(), "kind": device_kind()}), SESSION_TIMEOUT).await?;
    let signed = store(state, &v).await?;
    crate::network::sync_plan(state).await;
    Ok(signed)
}

/// Ends the device session on the gateway (best effort) and forgets it here.
pub async fn logout(state: &AppState) -> anyhow::Result<()> {
    if let Some(s) = load(state).await? {
        let _ = state.registry.post_as_session("/v1/accounts/logout", &s.session, &json!({}), SESSION_TIMEOUT).await;
    }
    sqlx::query("DELETE FROM account WHERE id = 1").execute(&state.db).await?;
    Ok(())
}

/// Opens a plan purchase for the account (one month, or a year for twelve
/// months): the gateway answers with the amount, the Yape/Plin numbers and
/// the reference the owner writes in the transfer.
pub async fn plan_request(state: &AppState, plan: &str, months: i64) -> anyhow::Result<Value> {
    let Some(s) = load(state).await? else { anyhow::bail!("not signed in") };
    state.registry.post_as_session("/v1/account/plan/request", &s.session, &json!({"plan": plan, "months": months}), SESSION_TIMEOUT).await
}

/// Where a purchase stands; when it is paid the plan is refreshed here so
/// the whole app follows at once.
pub async fn plan_request_status(state: &AppState, reference: &str) -> anyhow::Result<Value> {
    let Some(s) = load(state).await? else { anyhow::bail!("not signed in") };
    let safe: String = reference.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
    let v = state.registry.get_as_session(&format!("/v1/account/plan/request/{safe}"), &s.session, SESSION_TIMEOUT).await?;
    if v["status"].as_str() == Some("paid") {
        crate::network::sync_plan(state).await;
    }
    Ok(v)
}

/// What the app shows: who is signed in and what the plan allows.
pub async fn status(state: &AppState) -> anyhow::Result<Value> {
    let signed = load(state).await?;
    let plan_info = state.plan_info.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let guest = is_guest(state).await;
    let share = shares_training(state).await;
    Ok(match signed {
        None => json!({"signedIn": false, "guest": guest, "shareTraining": share, "agent": state.identity.id()}),
        Some(s) => json!({
            "signedIn": true, "guest": false, "shareTraining": share, "accountId": s.account_id, "email": s.email, "name": s.name, "phone": s.phone,
            "plan": plan_info["plan"].as_str().or(s.plan.as_deref()),
            "backups": {"allowed": plan_info["backups"].as_bool().unwrap_or(false), "lastAt": s.last_backup_at},
            "agent": state.identity.id(),
        }),
    })
}

// ------------------------------------------------------------------ guests
//
// "Continuar sin cuenta": the receptionist on the owner's channels and
// nothing of the network. The switch below decides whether redacted
// conversations leave the phone to train the agents.

pub async fn setting(db: &sqlx::SqlitePool, key: &str) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = $1").bind(key).fetch_optional(db).await.ok().flatten().map(|r| r.0)
}

pub async fn set_setting(db: &sqlx::SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value").bind(key).bind(value).execute(db).await?;
    Ok(())
}

pub async fn is_guest(state: &AppState) -> bool {
    setting(&state.db, "guest").await.as_deref() == Some("1")
}

pub async fn shares_training(state: &AppState) -> bool {
    setting(&state.db, "share_training").await.as_deref() == Some("1")
}

/// Declares this agent a guest on the gateway and remembers the choice.
pub async fn declare_guest(state: &AppState, share: bool) -> anyhow::Result<Value> {
    let v = state.registry.post("/v1/agents/guest", &json!({"share": share}), SESSION_TIMEOUT).await?;
    set_setting(&state.db, "guest", "1").await?;
    set_setting(&state.db, "share_training", if share { "1" } else { "0" }).await?;
    crate::network::sync_plan(state).await;
    Ok(v)
}

/// The training switch (guests and accounts alike).
pub async fn set_share(state: &AppState, share: bool) -> anyhow::Result<()> {
    let _ = state.registry.post("/v1/agents/share", &json!({"share": share}), SESSION_TIMEOUT).await;
    set_setting(&state.db, "share_training", if share { "1" } else { "0" }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, Mock};

    #[test]
    fn device_defaults() {
        assert_eq!(device_label(), "agente");
        assert_eq!(device_kind(), "phone");
    }

    #[tokio::test]
    async fn signed_out_by_default() {
        let s = testkit::state().await;
        assert!(load(&s).await.unwrap().is_none());
        let st = status(&s).await.unwrap();
        assert_eq!(st, json!({"signedIn": false, "guest": false, "shareTraining": false, "agent": s.identity.id()}));
        assert!(plan_request(&s, "pro", 1).await.unwrap_err().to_string().contains("not signed in"));
        assert!(plan_request_status(&s, "R1").await.unwrap_err().to_string().contains("not signed in"));
        logout(&s).await.unwrap(); // no-op
    }

    #[tokio::test]
    async fn sign_in_stores_session_and_backup_key() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let signed = testkit::sign_in(&s, &m).await;
        assert_eq!((signed.account_id.as_str(), signed.session.as_str(), signed.plan.as_deref()), ("acc_1", "sess_abc", Some("pro")));
        assert_eq!(signed.backup_key, Some([1u8; 32]));
        let sent = &m.seen_path("/v1/accounts/otp/check")[0].body;
        assert_eq!((sent["code"].clone(), sent["kind"].clone(), sent["device"].clone()), (json!("123456"), json!("phone"), json!("agente")));
        // The check itself is signed as this agent.
        assert_eq!(m.seen_path("/v1/accounts/otp/check")[0].headers["authorization"], format!("Bearer {}", s.identity.id()).as_str());
        let again = load(&s).await.unwrap().unwrap();
        assert_eq!((again.email.as_str(), again.phone.as_deref(), again.backup_key), ("tito@x.pe", Some("+51999"), Some([1u8; 32])));
        let st = status(&s).await.unwrap();
        assert_eq!((st["signedIn"].clone(), st["plan"].clone(), st["backups"]["allowed"].clone()), (json!(true), json!("pro"), json!(false)));
        // Signing in ends guest mode.
        assert!(!is_guest(&s).await);
    }

    #[tokio::test]
    async fn sign_in_without_a_backup_key_still_signs_in() {
        let m = Mock::start().await;
        m.on("/v1/accounts/otp/check", json!({"account": {"id": "a", "email": "e"}, "session": "s"}));
        let s = testkit::state_on(&m).await;
        let signed = otp_check(&s, None, Some("+51999"), "1", None).await.unwrap();
        assert_eq!(signed.backup_key, None);
        assert_eq!(load(&s).await.unwrap().unwrap().backup_key, None);
    }

    #[tokio::test]
    async fn malformed_replies_are_refused() {
        let m = Mock::start().await;
        m.on("/v1/accounts/otp/check", json!({"session": "s"}));
        let s = testkit::state_on(&m).await;
        assert!(otp_check(&s, None, None, "1", None).await.err().unwrap().to_string().contains("no account"));
        m.set("/v1/accounts/otp/check", json!({"account": {"id": "a"}}));
        assert!(otp_check(&s, None, None, "1", None).await.err().unwrap().to_string().contains("no session"));
        m.on_status("/v1/account/agents", 200, json!({"account": {"id": "a"}}));
        assert!(adopt(&s, "tok").await.err().unwrap().to_string().contains("did not return a session"));
        assert!(load(&s).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn adopt_links_with_a_token() {
        let m = Mock::start().await;
        m.on("/v1/account/agents", json!({"account": {"id": "acc_9", "email": "n@x"}, "session": "node_sess"}));
        let s = testkit::state_on(&m).await;
        let signed = adopt(&s, "link-token").await.unwrap();
        assert_eq!(signed.account_id, "acc_9");
        assert_eq!(m.seen_path("/v1/account/agents")[0].body["token"], "link-token");
    }

    #[tokio::test]
    async fn otp_start_forwards_the_contact() {
        let m = Mock::start().await;
        m.on("/v1/accounts/otp/start", json!({"sent": true}));
        let s = testkit::state_on(&m).await;
        assert_eq!(otp_start(&s, Some("a@b"), None, Some("Ana")).await.unwrap()["sent"], true);
        assert_eq!(m.seen()[0].body, json!({"email": "a@b", "phone": null, "name": "Ana"}));
    }

    #[tokio::test]
    async fn logout_revokes_the_session_and_forgets_it() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::sign_in(&s, &m).await;
        m.on("/v1/accounts/logout", json!({}));
        logout(&s).await.unwrap();
        assert_eq!(m.seen_path("/v1/accounts/logout")[0].headers["authorization"], "Bearer sess_abc");
        assert!(load(&s).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn plan_requests_use_the_session_and_sanitise_the_reference() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::sign_in(&s, &m).await;
        m.on("/v1/account/plan/request", json!({"ref": "AG-1", "amount": 100}));
        assert_eq!(plan_request(&s, "pro", 12).await.unwrap()["ref"], "AG-1");
        assert_eq!(m.seen_path("/v1/account/plan/request")[0].body, json!({"plan": "pro", "months": 12}));
        m.on("/v1/account/plan/request/", json!({"status": "pending"}));
        plan_request_status(&s, "AG-1/../../admin?x=1").await.unwrap();
        let last = m.seen().last().unwrap().path.clone();
        assert!(last.ends_with("/v1/account/plan/request/AG-1adminx1"), "{last}");
        assert_eq!(m.seen().last().unwrap().headers["authorization"], "Bearer sess_abc");
        // Paid → the plan is re-synced.
        m.set("/v1/account/plan/request/", json!({"status": "paid"}));
        let before = m.seen_path("/v1/me").len();
        plan_request_status(&s, "AG-1").await.unwrap();
        assert_eq!(m.seen_path("/v1/me").len(), before + 1);
    }

    #[tokio::test]
    async fn guests_and_training_switch() {
        let m = Mock::start().await;
        m.on("/v1/agents/guest", json!({"ok": true}));
        m.on("/v1/agents/share", json!({"ok": true}));
        let s = testkit::state_on(&m).await;
        declare_guest(&s, true).await.unwrap();
        assert!(is_guest(&s).await && shares_training(&s).await);
        assert_eq!(status(&s).await.unwrap()["guest"], true);
        set_share(&s, false).await.unwrap();
        assert!(!shares_training(&s).await);
        // The local switch holds even when the gateway is unreachable.
        let off = testkit::state().await;
        set_share(&off, true).await.unwrap();
        assert!(shares_training(&off).await);
        assert!(declare_guest(&off, true).await.is_err());
        assert!(!is_guest(&off).await, "not a guest until the gateway agreed");
    }

    #[tokio::test]
    async fn settings_upsert() {
        let db = testkit::db().await;
        assert_eq!(setting(&db, "k").await, None);
        set_setting(&db, "k", "1").await.unwrap();
        set_setting(&db, "k", "2").await.unwrap();
        assert_eq!(setting(&db, "k").await.as_deref(), Some("2"));
    }
}
