//! agente's capability plugins. Everything the agents can do is mounted here;
//! `corazon::Corazon::load` resolves order from `inject` declarations.

pub mod assistant;
mod core;
pub(crate) mod delivery;
mod agro;
pub mod disclosure;
mod learning;
mod llm_adapter;
mod media;
pub mod network_tools;
pub(crate) mod market;
mod onboarding;
mod payments;
mod reminders;
mod sales;
pub(crate) mod scheduling;
mod seller;

use std::sync::Arc;

use crate::harness::{Agente, Plugin};

/// `client_mode` mounts the network tools of a personal orchestrator
/// (find/ask/book/rate businesses); a business runtime never gets them.
pub fn all(llm: Arc<crate::llm::Llm>, client_mode: bool) -> Vec<Box<dyn Plugin<Agente>>> {
    all_with(llm, client_mode, serde_json::json!({"tools": []}))
}

/// The same set plus the network-defined tools (`network_tools.rs`) from a
/// gateway manifest: features that ship from the gateway without an APK.
pub fn all_with(llm: Arc<crate::llm::Llm>, client_mode: bool, network_manifest: serde_json::Value) -> Vec<Box<dyn Plugin<Agente>>> {
    let mut v: Vec<Box<dyn Plugin<Agente>>> = vec![
        Box::new(llm_adapter::LlmAdapter { llm }),
        Box::new(core::Core),
        Box::new(disclosure::Disclosure),
        Box::new(assistant::Assistant),
        Box::new(scheduling::Scheduling),
        Box::new(sales::Sales),
        Box::new(delivery::Delivery),
        Box::new(media::Media),
        Box::new(payments::Payments),
        Box::new(onboarding::Onboarding),
        Box::new(learning::Learning),
        // Mounted everywhere, visible only on a seller account's phone
        // (ToolCaps::seller, set from the gateway's /v1/me).
        Box::new(seller::Seller),
        // Mounted everywhere, offered only by the `agro` bundle's tool list.
        Box::new(agro::Agro),
    ];
    if client_mode {
        v.push(Box::new(market::Market));
    }
    v.push(Box::new(network_tools::NetworkTools { manifest: network_manifest }));
    // Spatial gating: no mail transport configured → the plugin never mounts
    // → the tool doesn't exist → the agent never offers reminders it can't send.
    if let Some(r) = reminders::from_env() {
        v.push(Box::new(r));
    } else {
        tracing::info!("reminders plugin not mounted (set SMTP_URL or OUTBOX_DIR + MAIL_FROM)");
    }
    v
}

/// The reminders plugin with a dev outbox, for tests (it normally mounts
/// only when SMTP/OUTBOX_DIR is configured).
#[cfg(test)]
pub fn reminders_for_test(dir: std::path::PathBuf) -> reminders::Reminders {
    reminders::Reminders { mailer: std::sync::Arc::new(reminders::Mailer::Outbox(dir, "agente <t@agente.ceo>".parse().unwrap())) }
}
