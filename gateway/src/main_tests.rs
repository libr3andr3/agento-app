//! Router-level tests for the handlers that live in `main.rs`: the chat
//! proxy, the registry (publish / revoke / discover / facts) and the relay.

use serde_json::{json, Value};

use crate::testkit::{self, account_with_agent, anon, as_admin, as_agent, Keypair, Mock};
use crate::{credits, plans, App, Shared, Upstream};

async fn chat_app(m: &Mock) -> Shared {
    let url = format!("{}/v1/chat/completions", m.base);
    testkit::app_with(move |a| App { upstreams: vec![Upstream { url, key: "uk".into(), model: "yaya-libre".into() }], max_tokens_cap: 512, ..a }).await
}

fn answer() -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": "hola"}}], "usage": {"prompt_tokens": 3, "completion_tokens": 1}})
}

#[tokio::test]
async fn chat_is_shaped_by_the_gateway_not_the_caller() {
    let m = Mock::start().await;
    m.on("/v1/chat/completions", answer());
    let app = chat_app(&m).await;
    let kp = Keypair::generate();
    account_with_agent(&app, "a", "51900000001", &kp).await;
    let ask = json!({"model": "gpt-9-ultra", "max_tokens": 100_000, "stream": true, "n": 5,
                     "provider": {"only": ["evil"]}, "models": ["x"], "route": "fallback", "transforms": [], "fallbacks": ["y"],
                     "messages": [{"role": "user", "content": "hola"}]});
    assert_eq!(as_agent(&app, &Keypair::generate(), "POST", "/v1/chat/completions", Some(ask.clone())).await.0, 401, "unlinked");
    let (st, v) = as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(ask)).await;
    assert_eq!((st, v["choices"][0]["message"]["content"].clone()), (200, json!("hola")), "{v}");
    let sent = m.seen_path("/v1/chat/completions").pop().unwrap();
    assert_eq!(sent.headers["authorization"], "Bearer uk");
    let b = sent.body;
    assert_eq!((b["model"].clone(), b["max_tokens"].clone(), b["n"].clone()), (json!("yaya-libre"), json!(512), json!(1)));
    for k in ["stream", "provider", "models", "route", "transforms", "fallbacks"] {
        assert!(b.get(k).is_none(), "{k} must not reach the upstream");
    }
    assert_eq!(b["messages"][0]["content"], "hola");
}

#[tokio::test]
async fn past_the_daily_cap_credits_pay_until_they_run_out() {
    let m = Mock::start().await;
    m.on("/v1/chat/completions", answer());
    let app = chat_app(&m).await;
    let kp = Keypair::generate();
    account_with_agent(&app, "a", "51900000001", &kp).await;
    sqlx::query("DELETE FROM plans").execute(&app.db).await.unwrap();
    let agent = kp.id().to_string();
    sqlx::query("INSERT INTO usage (agent, day, n, media) VALUES ($1, $2, $3, 0)").bind(&agent).bind(crate::today()).bind(app.free_cap).execute(&app.db).await.unwrap();
    let hi = json!({"messages": [{"role": "user", "content": "hola"}]});
    let (st, v) = as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(hi.clone())).await;
    assert_eq!((st, v["error"]["type"].clone()), (429, json!("allowance")), "free cap reached, no credits: {v}");
    credits::add(&app, "a", credits::call_price(), "topup", None, None).await.unwrap();
    assert_eq!(as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(hi.clone())).await.0, 200, "a recarga buys the next call");
    assert_eq!(credits::balance(&app, "a").await.unwrap(), 0);
    assert_eq!(as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(hi)).await.0, 429);
    assert_eq!(m.seen_path("/v1/chat/completions").len(), 1, "refused calls never reach the upstream");
}

#[tokio::test]
async fn an_upstream_that_is_down_is_a_502() {
    let app = testkit::app_with(|a| App { upstreams: vec![Upstream { url: "http://127.0.0.1:9/v1/chat/completions".into(), key: "uk".into(), model: "m".into() }], ..a }).await;
    let kp = Keypair::generate();
    account_with_agent(&app, "a", "51900000001", &kp).await;
    assert_eq!(as_agent(&app, &kp, "POST", "/v1/chat/completions", Some(json!({"messages": []}))).await.0, 502);
}

fn card(kp: &Keypair, name: &str, onboarded: bool) -> Value {
    kp.envelope(json!({"name": name, "industry": "barbería", "country": "PE", "onboarded": onboarded,
                       "description": "cortes", "skills": [{"id": "booking"}], "offer": {"askPrice": 50}}))
}

#[tokio::test]
async fn cards_publish_with_one_handle_that_survives_a_lost_phone() {
    let app = testkit::app().await;
    let (old, new, other) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    account_with_agent(&app, "a", "51900000001", &old).await;
    sqlx::query("INSERT INTO account_agents (agent, account) VALUES ($1, 'a')").bind(new.id().to_string()).execute(&app.db).await.unwrap();
    account_with_agent(&app, "b", "51900000002", &other).await;
    // Signed by someone else than the bearer: refused.
    assert_eq!(as_agent(&app, &old, "POST", "/v1/agents", Some(card(&other, "X", true))).await.0, 403);
    let (_, draft) = as_agent(&app, &old, "POST", "/v1/agents", Some(card(&old, "Barbería Don Pepe", false))).await;
    assert_eq!(draft["handle"], Value::Null, "no handle before onboarding is finished");
    let (st, v) = as_agent(&app, &old, "POST", "/v1/agents", Some(card(&old, "Barbería Don Pepe", true))).await;
    let handle = v["handle"].as_str().unwrap().to_string();
    assert_eq!(st, 200);
    assert!(handle.starts_with("barberia-don-pepe"), "{handle}");
    let (_, again) = as_agent(&app, &old, "POST", "/v1/agents", Some(card(&old, "Otro nombre", true))).await;
    assert_eq!(again["handle"], handle.as_str(), "a renamed business keeps its handle");
    let (_, twin) = as_agent(&app, &other, "POST", "/v1/agents", Some(card(&other, "Barbería Don Pepe", true))).await;
    assert_ne!(twin["handle"], handle.as_str(), "handles are unique");

    let (_, found) = anon(&app, "GET", "/v1/agents?country=pe&q=otro", None).await;
    assert_eq!(found["agents"].as_array().unwrap().len(), 1);
    assert_eq!((found["agents"][0]["askPrice"].clone(), found["agents"][0]["skills"].clone()), (json!(50), json!(["booking"])));
    let old_id = old.id().to_string();
    assert_eq!(anon(&app, "GET", &format!("/v1/agents/{old_id}"), None).await.0, 200);
    assert_eq!(anon(&app, "GET", &format!("/v1/index/@{handle}"), None).await.0, 200);
    assert_eq!(anon(&app, "GET", &format!("/v1/agents/{old_id}/facts"), None).await.0, 200);

    // Lost phone: the old key retires itself and names the new one.
    let rv = |kp: &Keypair, succ: Value| kp.envelope(json!({"successor": succ}));
    assert_eq!(as_agent(&app, &old, "POST", &format!("/v1/agents/{}/revoke", other.id()), Some(rv(&old, Value::Null))).await.0, 403, "only an agent can revoke itself");
    assert_eq!(as_agent(&app, &old, "POST", &format!("/v1/agents/{old_id}/revoke"), Some(rv(&old, json!(old_id.clone())))).await.0, 400);
    assert_eq!(as_agent(&app, &old, "POST", &format!("/v1/agents/{old_id}/revoke"), Some(rv(&old, json!(new.id().to_string())))).await.0, 200);
    assert_eq!(anon(&app, "GET", &format!("/v1/agents/{old_id}"), None).await.0, 404);
    assert_eq!(as_agent(&app, &old, "POST", "/v1/agents", Some(card(&old, "Zombie", true))).await.0, 401, "a revoked key is done");
    let (_, heir) = as_agent(&app, &new, "POST", "/v1/agents", Some(card(&new, "Barbería Don Pepe", true))).await;
    assert_eq!(heir["handle"], handle.as_str(), "the successor inherits the handle");
    let (_, r) = anon(&app, "GET", &format!("/v1/index/urn:agent:yaya:{handle}"), None).await;
    assert!(r.to_string().contains(&new.id().to_string()), "{r}");
}

#[tokio::test]
async fn sealed_boxes_are_delivered_once_even_to_racing_polls() {
    let app = testkit::app().await;
    let (a, b) = (Keypair::generate(), Keypair::generate());
    account_with_agent(&app, "a", "51900000001", &a).await;
    account_with_agent(&app, "b", "51900000002", &b).await;
    let (aid, bid) = (a.id().to_string(), b.id().to_string());
    let sealed = |n: i64| a.envelope(json!({"alg": "x25519-chacha20poly1305", "from": aid, "to": bid, "nonce": n.to_string(), "ct": "AAAA"}));
    let inbox = format!("/v1/agents/{bid}/inbox");
    assert_eq!(as_agent(&app, &a, "POST", &format!("/v1/agents/{aid}/inbox"), Some(sealed(0))).await.0, 400, "to must match the path");
    assert_eq!(as_agent(&app, &b, "POST", &inbox, Some(sealed(0))).await.0, 403, "signer must be the bearer");
    for n in 1..=3 {
        let (st, v) = as_agent(&app, &a, "POST", &inbox, Some(sealed(n))).await;
        assert_eq!(st, 200, "{v}");
    }
    let (x, y) = tokio::join!(as_agent(&app, &b, "GET", "/v1/inbox", None), as_agent(&app, &b, "GET", "/v1/inbox", None));
    let got = x.1["messages"].as_array().unwrap().len() + y.1["messages"].as_array().unwrap().len();
    assert_eq!(got, 3, "each box exactly once across both polls: {x:?} {y:?}");
    assert_eq!(as_agent(&app, &b, "GET", "/v1/inbox", None).await.1["messages"], json!([]));
    assert_eq!(as_agent(&app, &a, "GET", "/v1/inbox", None).await.1["messages"], json!([]), "nobody reads another inbox");
}

#[tokio::test]
async fn one_sender_cannot_flood_one_inbox() {
    let app = testkit::app_with(|a| App { relay_pair_per_day: 2, ..a }).await;
    let (a, b) = (Keypair::generate(), Keypair::generate());
    account_with_agent(&app, "a", "51900000001", &a).await;
    account_with_agent(&app, "b", "51900000002", &b).await;
    let (aid, bid) = (a.id().to_string(), b.id().to_string());
    let inbox = format!("/v1/agents/{bid}/inbox");
    let sealed = || a.envelope(json!({"from": aid, "to": bid, "ct": "AAAA"}));
    for _ in 0..2 { assert_eq!(as_agent(&app, &a, "POST", &inbox, Some(sealed())).await.0, 200); }
    assert_eq!(as_agent(&app, &a, "POST", &inbox, Some(sealed())).await.0, 429);
}

#[tokio::test]
async fn sales_sets_plans_by_account_email_or_agent() {
    let app = testkit::app().await;
    let kp = Keypair::generate();
    account_with_agent(&app, "a", "51900000001", &kp).await;
    assert_eq!(anon(&app, "POST", "/admin/plan", Some(json!({"account": "a", "plan": "pro"}))).await.0, 401);
    assert_eq!(as_admin(&app, "POST", "/admin/plan", Some(json!({"account": "a"}))).await.0, 400);
    assert_eq!(as_admin(&app, "POST", "/admin/plan", Some(json!({"email": "nobody@x.pe", "plan": "pro"}))).await.0, 404);
    assert_eq!(as_admin(&app, "POST", "/admin/plan", Some(json!({"agent": "bad", "plan": "pro"}))).await.0, 400);
    let (st, v) = as_admin(&app, "POST", "/admin/plan", Some(json!({"agent": kp.id().to_string(), "plan": "max", "cap": 7}))).await;
    assert_eq!((st, v["subject"].clone(), v["plan"].clone()), (200, json!("acct:a"), json!("max")), "an agent maps to its account: {v}");
    assert_eq!(plans::effective(&app, &kp.id().to_string()).await.unwrap().cap, 7);
    let (_, v) = as_admin(&app, "POST", "/admin/plan", Some(json!({"email": "A@TEST.PE", "plan": "pro"}))).await;
    assert_eq!(v["plan"], "pro");
    assert_eq!(anon(&app, "GET", "/admin/metrics", None).await.0, 401);
    assert_eq!(as_admin(&app, "GET", "/admin/metrics", None).await.0, 200);
    assert_eq!(anon(&app, "GET", "/.well-known/agent-facts", None).await.0, 200);
    assert_eq!(anon(&app, "GET", "/app", None).await.0, 200);
}
