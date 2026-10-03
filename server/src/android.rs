//! JNI surface for the Android shell. Three calls: `start(configJson)` boots
//! the core on its own Tokio runtime and returns the loopback port, `stop()`
//! tears it down, `agentId()` exposes the installation's identity.
//!
//! Config arrives as a flat JSON object of environment variables — the same
//! names the server binary reads from `.env` — so there is one configuration
//! path and the Kotlin side never learns Rust types.

use std::sync::Mutex;

use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;

struct Running {
    runtime: tokio::runtime::Runtime,
    port: u16,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

fn init_logging() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("agente-core"),
        );
    });
}

#[no_mangle]
pub extern "system" fn Java_tech_yaya_agente_AgenteCore_start(
    mut env: JNIEnv,
    _class: JClass,
    config: JString,
) -> jint {
    init_logging();
    let config: String = match env.get_string(&config) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("bad config string: {e}");
            return -1;
        }
    };
    let mut guard = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = guard.as_ref() {
        return r.port as jint;
    }
    let vars: serde_json::Map<String, serde_json::Value> =
        match serde_json::from_str::<serde_json::Value>(&config) {
            Ok(serde_json::Value::Object(m)) => m,
            _ => {
                log::error!("config must be a JSON object");
                return -2;
            }
        };
    for (k, v) in vars {
        if let Some(s) = v.as_str() {
            std::env::set_var(k, s);
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            log::error!("tokio runtime: {e}");
            return -3;
        }
    };
    let (tx, rx) = std::sync::mpsc::channel::<Result<u16, String>>();
    let tx_err = tx.clone();
    runtime.spawn(async move {
        let res = crate::run(move |addr| {
            let _ = tx.send(Ok(addr.port()));
        })
        .await;
        if let Err(e) = res {
            log::error!("agente-core exited: {e:#}");
            let _ = tx_err.send(Err(format!("{e:#}")));
        }
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(Ok(port)) => {
            *guard = Some(Running { runtime, port });
            port as jint
        }
        Ok(Err(e)) => {
            log::error!("boot failed: {e}");
            -4
        }
        Err(_) => {
            log::error!("boot timed out");
            -5
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_tech_yaya_agente_AgenteCore_stop(_env: JNIEnv, _class: JClass) {
    let mut guard = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = guard.take() {
        r.runtime.shutdown_timeout(std::time::Duration::from_secs(3));
    }
}

#[no_mangle]
pub extern "system" fn Java_tech_yaya_agente_AgenteCore_port(_env: JNIEnv, _class: JClass) -> jint {
    RUNNING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|r| r.port as jint)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_tech_yaya_agente_AgenteCore_version(env: JNIEnv, _class: JClass) -> jstring {
    env.new_string(env!("CARGO_PKG_VERSION"))
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
