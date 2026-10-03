//! boot()/run() read the process environment, so they get their own test
//! binary (one process, one sequential test): nothing else shares its env.

use std::time::Duration;

fn scratch() -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("boot-test");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn set(k: &str, v: &str) {
    std::env::set_var(k, v);
}

#[tokio::test]
async fn boot_validates_its_config_and_run_serves() {
    let dir = scratch();
    set("DATABASE_URL", &format!("sqlite://{}", dir.join("core.db").display()));
    set("REGISTRY_URL", "http://127.0.0.1:9");
    set("AUDIO", "0");
    set("VISION", "0");
    set("MESH_CONF_PATH", &dir.join("yaya0.conf").to_string_lossy());
    set("NETWORK_INBOX", "0");

    // No schemas: refused with a clear reason.
    set("SCHEMAS_DIR", &dir.join("nope").to_string_lossy());
    let e = agente_core::boot().await.err().unwrap().to_string();
    assert!(e.contains("schemas dir not found"), "{e}");
    set("SCHEMAS_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/schemas"));

    // Placeholder or short secrets are refused.
    set("ADMIN_KEY", "agente-admin-dev-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    set("APP_KEY", &"a".repeat(40));
    assert!(agente_core::boot().await.err().unwrap().to_string().contains("placeholder"));
    set("ADMIN_KEY", "short");
    assert!(agente_core::boot().await.err().unwrap().to_string().contains("too short"));
    set("ADMIN_KEY", &"k".repeat(40));

    // Phone verification demands a WhatsApp transport.
    set("REQUIRE_PHONE_VERIFICATION", "1");
    assert!(agente_core::boot().await.err().unwrap().to_string().contains("WhatsApp is not configured"));
    set("REQUIRE_PHONE_VERIFICATION", "0");

    // A good config boots, records its boot, and keeps its identity.
    let s = agente_core::boot().await.unwrap();
    let first = s.identity.id();
    assert!(!s.client_mode && s.whatsapp.is_none() && s.audio.is_none() && s.vision.is_none());
    let boots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE kind = 'boot'").fetch_one(&s.db).await.unwrap();
    assert_eq!(boots, 1);
    drop(s);
    assert_eq!(agente_core::boot().await.unwrap().identity.id(), first, "the same database is the same agent");

    // Client mode creates the owner's own row.
    set("AGENT_MODE", "client");
    let s = agente_core::boot().await.unwrap();
    assert!(s.client_mode);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM businesses WHERE owner_phone = 'self'").fetch_one(&s.db).await.unwrap();
    assert_eq!(rows, 1);
    drop(s);
    set("AGENT_MODE", "business");

    // run() binds, reports the address, and serves.
    set("BIND_ADDR", "127.0.0.1:0");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(agente_core::run(move |addr| { let _ = tx.send(addr); }));
    let addr = tokio::time::timeout(Duration::from_secs(20), rx).await.unwrap().unwrap();
    let body = reqwest::get(format!("http://{addr}/health")).await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
    let r = reqwest::get(format!("http://{addr}/api/kernel")).await.unwrap();
    assert_eq!(r.status().as_u16(), 401, "every /api route wants the app key");
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
