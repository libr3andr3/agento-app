//! The owner's UI as data (DECISIONS D15).
//!
//! The app ships a fixed catalog of blocks; what it draws is a spec composed
//! like the schema: the vertical bundle's `ui:` template ⊕ the agent's own
//! design in the client patch (`schema_config.ui`, written by `design_ui`).
//! This module is the one place that knows which blocks exist, which ones a
//! business can use, and how to turn whatever the model produced into a spec
//! the app will render without surprises.

use serde_json::{json, Value};

/// Every block the app can render. Adding one means adding a renderer in
/// the Kotlin block catalog (Blocks.kt) — the names must match exactly.
pub const BLOCKS: &[&str] = &[
    "earnings",
    "attention",
    "orders_board",
    "agenda_day",
    "agenda_week",
    "catalog",
    "conversations",
    "contacts",
];

/// Icons the app knows how to draw for a tab.
pub const ICONS: &[&str] = &["home", "orders", "agenda", "catalog", "chats", "people", "money", "star"];

pub const MAX_TABS: usize = 4;
const MAX_LABEL: usize = 16;
const MAX_INTRO: usize = 140;

/// Which blocks make sense for a business of this kind. An agenda on a
/// products-only stall is an empty screen forever; a pedidos board on a
/// barbershop likewise. `both` gets everything.
pub fn block_allowed(block: &str, business_kind: &str) -> bool {
    match block {
        "orders_board" | "catalog" => business_kind != "services",
        "agenda_day" | "agenda_week" => business_kind != "products",
        _ => BLOCKS.contains(&block),
    }
}

/// The fallback template when a bundle has none: derived from what the
/// business sells. Labels are Spanish — the app re-labels nothing; the
/// agent does, in `design_ui`, in the owner's language and words.
pub fn default_template(business_kind: &str) -> Value {
    let tabs = match business_kind {
        "services" => json!([
            {"id": "agenda", "label": "Agenda", "icon": "agenda",
             "blocks": ["earnings", "attention", "agenda_day", "agenda_week"]},
            {"id": "clientes", "label": "Clientes", "icon": "chats",
             "blocks": ["conversations", "contacts"]}
        ]),
        "products" => json!([
            {"id": "pedidos", "label": "Pedidos", "icon": "orders",
             "blocks": ["earnings", "attention", "orders_board"]},
            {"id": "catalogo", "label": "Catálogo", "icon": "catalog",
             "blocks": ["catalog"]},
            {"id": "clientes", "label": "Clientes", "icon": "chats",
             "blocks": ["conversations", "contacts"]}
        ]),
        _ => json!([
            {"id": "hoy", "label": "Hoy", "icon": "home",
             "blocks": ["earnings", "attention", "orders_board", "agenda_day"]},
            {"id": "agenda", "label": "Agenda", "icon": "agenda",
             "blocks": ["agenda_week"]},
            {"id": "catalogo", "label": "Catálogo", "icon": "catalog",
             "blocks": ["catalog"]},
            {"id": "clientes", "label": "Clientes", "icon": "chats",
             "blocks": ["conversations", "contacts"]}
        ]),
    };
    json!({"tabs": tabs})
}

fn slug(s: &str) -> String {
    let out: String = s
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o', 'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', c => c,
        })
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    out.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

fn trim_to(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n { t.to_string() } else { t.chars().take(n).collect() }
}

/// One block entry as the model may write it: `"catalog"` or
/// `{"type": "orders_board", "opts": {…}}`. Returns the canonical object.
fn block_of(v: &Value, business_kind: &str) -> Option<Value> {
    let (ty, opts) = match v {
        Value::String(s) => (s.trim().to_lowercase(), Value::Null),
        Value::Object(o) => (
            o.get("type").and_then(|t| t.as_str()).unwrap_or("").trim().to_lowercase(),
            o.get("opts").cloned().unwrap_or(Value::Null),
        ),
        _ => return None,
    };
    if !BLOCKS.contains(&ty.as_str()) || !block_allowed(&ty, business_kind) {
        return None;
    }
    let mut b = json!({"type": ty});
    if opts.is_object() {
        b["opts"] = opts;
    }
    Some(b)
}

/// Turns a spec from anywhere (a bundle template, the model) into one the
/// app renders safely. Returns None when nothing usable is left — callers
/// fall back to the next layer down.
pub fn normalize(spec: &Value, business_kind: &str) -> Option<Value> {
    let tabs_in = spec.get("tabs")?.as_array()?;
    let mut tabs: Vec<Value> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    let mut seen_blocks: Vec<String> = Vec::new();
    for t in tabs_in {
        if tabs.len() >= MAX_TABS {
            break;
        }
        let Some(obj) = t.as_object() else { continue };
        let label = trim_to(obj.get("label").and_then(|v| v.as_str()).unwrap_or(""), MAX_LABEL);
        if label.is_empty() {
            continue;
        }
        let mut id = obj.get("id").and_then(|v| v.as_str()).map(slug).filter(|s| !s.is_empty()).unwrap_or_else(|| slug(&label));
        if id.is_empty() { id = format!("tab{}", tabs.len() + 1); }
        while seen_ids.contains(&id) {
            id.push('2');
        }
        let blocks: Vec<Value> = obj
            .get("blocks")
            .and_then(|b| b.as_array())
            .map(|a| a.iter().filter_map(|b| block_of(b, business_kind)).collect())
            .unwrap_or_default();
        // A block lives on one tab; the first tab that names it wins.
        let blocks: Vec<Value> = blocks
            .into_iter()
            .filter(|b| {
                let ty = b["type"].as_str().unwrap_or("").to_string();
                if seen_blocks.contains(&ty) { false } else { seen_blocks.push(ty); true }
            })
            .collect();
        if blocks.is_empty() {
            continue;
        }
        let icon = obj.get("icon").and_then(|v| v.as_str()).map(|s| s.trim().to_lowercase()).filter(|s| ICONS.contains(&s.as_str()))
            .unwrap_or_else(|| icon_for(&blocks));
        let mut tab = json!({"id": id, "label": label, "icon": icon, "blocks": blocks});
        if let Some(intro) = obj.get("intro").and_then(|v| v.as_str()).map(|s| trim_to(s, MAX_INTRO)).filter(|s| !s.is_empty()) {
            tab["intro"] = json!(intro);
        }
        seen_ids.push(tab["id"].as_str().unwrap().to_string());
        tabs.push(tab);
    }
    if tabs.is_empty() {
        return None;
    }
    // Money and attention belong on the home tab, above everything: an
    // owner must never have to hunt for "¿cómo voy?" or "el agente me necesita".
    let has = |ty: &str| seen_blocks.iter().any(|b| b == ty);
    let mut head: Vec<Value> = Vec::new();
    if !has("earnings") { head.push(json!({"type": "earnings"})); }
    if !has("attention") { head.push(json!({"type": "attention"})); }
    if !head.is_empty() {
        let first = tabs[0]["blocks"].as_array().cloned().unwrap_or_default();
        head.extend(first);
        tabs[0]["blocks"] = Value::Array(head);
    }
    let home = spec.get("home").and_then(|v| v.as_str()).map(slug)
        .filter(|h| tabs.iter().any(|t| t["id"].as_str() == Some(h)))
        .unwrap_or_else(|| tabs[0]["id"].as_str().unwrap().to_string());
    Some(json!({"version": 1, "home": home, "tabs": tabs}))
}

fn icon_for(blocks: &[Value]) -> String {
    let first = blocks.first().and_then(|b| b["type"].as_str()).unwrap_or("");
    match first {
        "orders_board" => "orders",
        "agenda_day" | "agenda_week" => "agenda",
        "catalog" => "catalog",
        "conversations" | "contacts" => "chats",
        "earnings" => "money",
        _ => "home",
    }
    .to_string()
}

/// The spec the app renders: the agent's design if it validates, else the
/// bundle template, else the kind-derived default. Always returns a spec.
pub fn compose(bundle_ui: &Value, patch_ui: &Value, business_kind: &str) -> Value {
    normalize(patch_ui, business_kind)
        .or_else(|| normalize(bundle_ui, business_kind))
        .unwrap_or_else(|| normalize(&default_template(business_kind), business_kind).expect("default template validates"))
}

/// The template the agent starts from in `design_ui`: the bundle's, or the
/// kind-derived one — normalized, so what it sees is what would render.
pub fn template(bundle_ui: &Value, business_kind: &str) -> Value {
    normalize(bundle_ui, business_kind)
        .unwrap_or_else(|| normalize(&default_template(business_kind), business_kind).expect("default template validates"))
}

/// The catalog as the model sees it in the tool description.
pub fn catalog_help() -> String {
    "earnings (hoy/semana/mes — always on the home tab), attention (questions the agent could not answer, customers asking for a human), \
     orders_board (product orders: paid ones to prepare, swipe = done), agenda_day (today's appointments, swipe = done), \
     agenda_week (the week's bookings on a grid), catalog (products with photos; share a private link), \
     conversations (recent chats), contacts (all customers). Icons: home, orders, agenda, catalog, chats, people, money, star."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_unknown_and_disallowed_blocks_and_caps_tabs() {
        let spec = json!({"tabs": [
            {"id": "x", "label": "Mesas", "blocks": ["orders_board", "magic", "agenda_week"]},
            {"label": "Fotos", "icon": "catalog", "blocks": ["catalog"]},
            {"label": "Vacía", "blocks": ["magic"]},
            {"label": "Chats", "blocks": ["conversations"]},
            {"label": "Más", "blocks": ["contacts"]},
            {"label": "Y más", "blocks": ["contacts"]},
        ]});
        let n = normalize(&spec, "products").unwrap();
        let tabs = n["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 4);
        let first: Vec<&str> = tabs[0]["blocks"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(first, vec!["earnings", "attention", "orders_board"], "agenda_week dropped for products, head blocks added");
        assert_eq!(n["home"], "x");
        assert_eq!(tabs[1]["id"], "fotos");
    }

    #[test]
    fn falls_back_layer_by_layer() {
        let ui = compose(&Value::Null, &json!({"tabs": []}), "services");
        let ids: Vec<&str> = ui["tabs"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["agenda", "clientes"]);
        let ui = compose(&json!({"tabs": [{"label": "Citas", "blocks": ["agenda_day"]}]}), &Value::Null, "services");
        assert_eq!(ui["tabs"][0]["label"], "Citas");
        assert_eq!(ui["tabs"][0]["blocks"][0]["type"], "earnings");
    }

    #[test]
    fn a_block_lives_on_one_tab() {
        let n = normalize(&json!({"tabs": [
            {"label": "A", "blocks": ["catalog", "conversations"]},
            {"label": "B", "blocks": ["conversations", "contacts"]},
        ]}), "both").unwrap();
        let b: Vec<&str> = n["tabs"][1]["blocks"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(b, vec!["contacts"]);
    }

    fn kotlin_cases(file: &str) -> Vec<String> {
        let re = regex::Regex::new(r#""([a-z_]+)"\s*->"#).unwrap();
        re.captures_iter(file).map(|c| c[1].to_string()).collect()
    }

    /// Every block the core may emit has a renderer in the app, and vice versa.
    #[test]
    fn blocks_match_the_app_renderers() {
        let kt = kotlin_cases(include_str!("../../android/app/src/main/java/tech/yaya/agente/Blocks.kt"));
        for b in BLOCKS {
            assert!(kt.iter().any(|k| k == b), "no Kotlin renderer for block {b}");
        }
    }

    #[test]
    fn icons_match_the_app_drawables() {
        let src = include_str!("../../android/app/src/main/java/tech/yaya/agente/UiSpec.kt");
        let icon_res = &src[src.find("fun iconRes").expect("iconRes in UiSpec.kt")..];
        let kt = kotlin_cases(icon_res);
        assert!(icon_res.contains("else -> R.drawable.ic_tab_home"), "home is the app's fallback drawable");
        for i in ICONS.iter().filter(|i| **i != "home") {
            assert!(kt.iter().any(|k| k == i), "no Kotlin drawable for icon {i}");
        }
    }

    #[test]
    fn block_allowed_by_kind() {
        assert!(!block_allowed("catalog", "services"));
        assert!(!block_allowed("orders_board", "services"));
        assert!(!block_allowed("agenda_day", "products"));
        assert!(!block_allowed("agenda_week", "products"));
        for b in BLOCKS {
            assert!(block_allowed(b, "both"));
        }
        assert!(!block_allowed("magic", "both"));
    }

    #[test]
    fn every_default_template_validates_for_its_kind() {
        for kind in ["services", "products", "both", "anything"] {
            let n = normalize(&default_template(kind), kind).unwrap_or_else(|| panic!("{kind}"));
            assert_eq!(n["version"], 1);
            assert_eq!(n["tabs"][0]["blocks"][0]["type"], "earnings");
            assert_eq!(template(&Value::Null, kind), n);
        }
    }

    #[test]
    fn slug_and_trim() {
        assert_eq!(slug("  Mis Citas Ñoñas! "), "mis-citas-nonas");
        assert_eq!(slug("Économie"), "economie");
        assert_eq!(slug("日本"), "", "non-Latin letters become separators");
        assert_eq!(slug("¡¡!!"), "");
        assert_eq!(trim_to("  hola  ", 10), "hola");
        assert_eq!(trim_to("ñandúñandú", 3), "ñan");
    }

    #[test]
    fn block_of_shapes() {
        assert_eq!(block_of(&json!(" Catalog "), "both"), Some(json!({"type": "catalog"})));
        assert_eq!(block_of(&json!({"type": "catalog", "opts": {"cols": 2}}), "both"), Some(json!({"type": "catalog", "opts": {"cols": 2}})));
        assert_eq!(block_of(&json!({"type": "catalog", "opts": "x"}), "both"), Some(json!({"type": "catalog"})), "non-object opts dropped");
        assert_eq!(block_of(&json!(5), "both"), None);
        assert_eq!(block_of(&json!({"opts": {}}), "both"), None);
    }

    #[test]
    fn labels_ids_icons_and_intro_are_sanitised() {
        let n = normalize(&json!({"home": "Caja", "tabs": [
            {"label": "   ", "blocks": ["catalog"]},
            {"label": "Una etiqueta demasiado larga", "blocks": ["catalog"], "icon": "rocket", "intro": format!("  {}  ", "i".repeat(300))},
            {"id": "una-etiqueta-dem", "label": "Otra", "blocks": ["conversations"], "icon": "STAR"},
            {"label": "Caja", "blocks": [{"type": "earnings"}]},
            "not an object",
        ]}), "both").unwrap();
        let tabs = n["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 3, "blank labels are dropped");
        assert_eq!(tabs[0]["label"].as_str().unwrap().chars().count(), MAX_LABEL);
        assert_eq!(tabs[0]["icon"], "catalog", "unknown icons fall back to the first block's");
        assert_eq!(tabs[0]["intro"].as_str().unwrap().chars().count(), MAX_INTRO);
        assert_eq!(tabs[1]["id"], "una-etiqueta-dem2", "duplicate ids are made unique");
        assert_eq!(tabs[1]["icon"], "star");
        assert_eq!(n["home"], "caja");
        // earnings lives on the Caja tab, so only attention is added up front.
        let first: Vec<&str> = tabs[0]["blocks"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(first, vec!["attention", "catalog"]);
    }

    #[test]
    fn nothing_usable_is_none() {
        assert!(normalize(&json!({}), "both").is_none());
        assert!(normalize(&json!({"tabs": "x"}), "both").is_none());
        assert!(normalize(&json!({"tabs": [{"label": "A", "blocks": ["magic"]}]}), "both").is_none());
        // Unknown home id falls back to the first tab.
        let n = normalize(&json!({"home": "nowhere", "tabs": [{"label": "A", "blocks": ["catalog"]}]}), "both").unwrap();
        assert_eq!(n["home"], "a");
    }

    #[test]
    fn icon_for_first_block() {
        let b = |t: &str| vec![json!({"type": t})];
        assert_eq!(icon_for(&b("orders_board")), "orders");
        assert_eq!(icon_for(&b("agenda_week")), "agenda");
        assert_eq!(icon_for(&b("contacts")), "chats");
        assert_eq!(icon_for(&b("earnings")), "money");
        assert_eq!(icon_for(&b("attention")), "home");
        assert_eq!(icon_for(&[]), "home");
    }

    #[test]
    fn catalog_help_names_every_block_and_icon() {
        let h = catalog_help();
        for b in BLOCKS { assert!(h.contains(b), "{b}"); }
        for i in ICONS { assert!(h.contains(i), "{i}"); }
    }
}
