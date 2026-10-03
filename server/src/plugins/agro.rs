//! The agro coordinator's agent (bundle `agro`, see crate::agro). On the
//! customer side, every WhatsApp contact is asked whether they are productor,
//! comprador or transportista and interviewed for that role; the owner's
//! manager agent reads the resulting directory. The tools are not core: only
//! a business pinned to the agro bundle mounts them, and the prompt section
//! follows the tools — no agro tools, no agro interview.

use anyhow::anyhow;
use serde_json::{json, Value};

use crate::agro::{self, Role};
use crate::ops;
use crate::harness::{hook_fn, meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Agro;

fn customer_peer<'a>(ctx: &'a ToolCtx<'_>) -> anyhow::Result<&'a str> {
    ctx.peer.as_deref().ok_or_else(|| anyhow!("customer conversations only"))
}

async fn agro_register(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let peer = customer_peer(ctx)?;
    let said = args["role"].as_str().unwrap_or("");
    let Some(role) = Role::from_key(said).or_else(|| agro::parse_role(said)) else {
        return Ok(json!({"status": "unknown_role",
            "note": "ask again: productor (1), comprador (2) or transportista (3)"}));
    };
    let before = agro::get(&ctx.state.db, ctx.business_id, peer).await.map(|p| p.role);
    let p = agro::register(&ctx.state.db, ctx.business_id, peer, role).await?;
    if before != Some(role) {
        ops::emit(ctx.state, ctx.business_id, "participant.registered", json!({"participant": p.to_json()})).await?;
    }
    Ok(step(&p, format!("registered as {}", role.as_str())))
}

async fn agro_save(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let peer = customer_peer(ctx)?;
    let saved = match agro::save(&ctx.state.db, ctx.business_id, peer, &args["fields"]).await {
        Ok(s) => s,
        Err(e) => return Ok(json!({"status": "error", "note": e.to_string()})),
    };
    let offered = args["fields"].as_object().map_or(0, |m| m.len());
    let mut p = saved.participant;
    if saved.completed_now {
        p.profile_url = profile_link(ctx, &p).await?;
    } else if offered > saved.rejected.len() {
        ops::emit(ctx.state, ctx.business_id, "participant.updated", json!({"participant": p.to_json()})).await?;
    }
    let mut out = step(&p, if saved.completed_now { "profile complete".into() } else { "saved".into() });
    if !saved.rejected.is_empty() {
        out["rejected"] = json!(saved.rejected);
    }
    if saved.completed_now {
        let send = ops::config(&ctx.state.db, ctx.business_id).await.send_profile_link;
        let mut note = String::from("thank them by name, recap in one line what you registered, and tell them the team will \
                                     contact them to connect them — no promises of prices, buyers or loads");
        if let (Some(url), true) = (&p.profile_url, send) {
            out["profileUrl"] = json!(url);
            note.push_str(&format!("; then give them their profile link on the website, exactly as is: {url}"));
        }
        out["note"] = json!(note);
    }
    Ok(out)
}

/// The link to a completed profile: the operator's webhook may answer with
/// its own (on their domain only); otherwise their template. Their systems
/// hear `participant.completed` either way.
async fn profile_link(ctx: &ToolCtx<'_>, p: &agro::Participant) -> anyhow::Result<Option<String>> {
    let cfg = ops::config(&ctx.state.db, ctx.business_id).await;
    let mut p = p.clone();
    p.profile_url = cfg.profile_url.as_deref().map(|t| {
        let name = p.name.clone().unwrap_or_default();
        ops::render(t, &[
            ("id", p.id.as_deref().unwrap_or("")),
            ("role", p.role.as_str()),
            ("phone", p.phone.as_deref().unwrap_or("")),
            ("slug", &ops::slug(&name)),
        ])
    });
    let reply = ops::emit(ctx.state, ctx.business_id, "participant.completed", json!({"participant": p.to_json()})).await?;
    let theirs = reply
        .and_then(|r| r["profileUrl"].as_str().map(str::to_string))
        .filter(|u| cfg.domain.as_deref().is_some_and(|d| ops::on_domain(u, d)));
    let url = theirs.or(p.profile_url);
    if let Some(u) = &url {
        agro::set_profile_url(&ctx.state.db, ctx.business_id, &p.peer, u).await?;
    }
    Ok(url)
}

async fn ops_configure(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if ctx.peer.is_some() {
        return Ok(json!({"error": "owner only"}));
    }
    Ok(match ops::configure(&ctx.state.db, ctx.business_id, args).await {
        Ok(c) => {
            let mut v = c.to_json(false);
            v["example"] = json!(c.profile_url.as_deref().map(ops::example));
            v["note"] = json!("saved. The webhook signing secret is never shown here: it is in the app, Ajustes → Sitio web e integración.");
            v
        }
        Err(e) => json!({"error": e.to_string()}),
    })
}

async fn ops_test(ctx: &ToolCtx<'_>, _args: &Value) -> anyhow::Result<Value> {
    if ctx.peer.is_some() {
        return Ok(json!({"error": "owner only"}));
    }
    Ok(ops::ping(ctx.state, ctx.business_id).await.unwrap_or_else(|e| json!({"delivered": false, "error": e.to_string()})))
}

/// The tool result: where the interview stands and what to ask next.
fn step(p: &agro::Participant, status: String) -> Value {
    let todo = agro::missing(p.role, &p.profile);
    let mut v = json!({"status": status, "role": p.role.as_str(), "profile": p.profile, "complete": p.complete});
    if let Some(next) = todo.first() {
        v["nextField"] = json!(next.key);
        v["nextQuestion"] = json!(next.question);
    }
    v
}

async fn agro_directory(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    if ctx.peer.is_some() {
        return Ok(json!({"error": "owner only"}));
    }
    let role = args["role"].as_str().and_then(Role::from_key);
    agro::directory(&ctx.state.db, ctx.business_id, role).await
}

impl Plugin<Agente> for Agro {
    fn name(&self) -> &'static str {
        "agro"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, false),
            json!({"type": "function", "function": {
                "name": "agro_register",
                "description": "Register the role this WhatsApp contact plays in the agro network, as soon as they say it: productor (sells what they grow), comprador (buys for a company or business) or transportista (moves cargo). Accepts the menu number or their words. Call again if they correct it.",
                "parameters": {"type": "object", "properties": {
                    "role": {"type": "string", "description": "productor | comprador | transportista, or what they answered (\"1\", \"tengo un camión\")"}
                }, "required": ["role"]}
            }}),
            tool_fn(|c, a| Box::pin(agro_register(c, a))),
        )?;
        k.tool(
            meta(Scope::Customer, false),
            json!({"type": "function", "function": {
                "name": "agro_save",
                "description": "Save the contact's answers to their agro profile after every answer. Returns the next question to ask, or that the profile is complete.",
                "parameters": {"type": "object", "properties": {
                    "fields": {"type": "object", "description": "answers by key: nombre, ubicacion, productos, volumen, temporada, empresa, vehiculo, ruta, base, disponibilidad, ruc, placa, precio, calidad, certificaciones, email, notas",
                        "additionalProperties": {"type": "string"}}
                }, "required": ["fields"]}
            }}),
            tool_fn(|c, a| Box::pin(agro_save(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, false),
            json!({"type": "function", "function": {
                "name": "agro_directory",
                "description": "The owner's agro directory: how many productores, compradores and transportistas registered over WhatsApp (complete or still onboarding) and their profiles, newest first.",
                "parameters": {"type": "object", "properties": {
                    "role": {"type": "string", "enum": ["productor", "comprador", "transportista"]}
                }}
            }}),
            tool_fn(|c, a| Box::pin(agro_directory(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, false),
            json!({"type": "function", "function": {
                "name": "ops_configure",
                "description": "Connect the business's own website and systems. domain: their website (example.com) — profile links live on it. profileUrl: link template for a participant's profile, placeholders {id} {role} {phone} {slug}, default https://{domain}/perfil/{id}. webhookUrl: their server endpoint that receives signed events (participant.registered/updated/completed) and writes to their databases. sendProfileLink: send each participant their link when onboarding completes. Send only what the owner gave; an empty string clears.",
                "parameters": {"type": "object", "properties": {
                    "domain": {"type": "string"},
                    "profileUrl": {"type": "string"},
                    "webhookUrl": {"type": "string"},
                    "sendProfileLink": {"type": "boolean"}
                }}
            }}),
            tool_fn(|c, a| Box::pin(ops_configure(c, a))),
        )?;
        k.tool(
            meta(Scope::Onboarding, false),
            json!({"type": "function", "function": {
                "name": "ops_test_webhook",
                "description": "Send a test event (ping) to the business's webhook and report whether their server accepted it.",
                "parameters": {"type": "object", "properties": {}}
            }}),
            tool_fn(|c, a| Box::pin(ops_test(c, a))),
        )?;
        // The profile link is a promise: when the model forgets it, the
        // reply carries it anyway — once.
        k.on(
            "reply/customer",
            hook_fn(|rt, mut payload| {
                Box::pin(async move {
                    let Some(peer) = payload["peer"].as_str().map(str::to_string) else { return Ok(payload) };
                    let Some(p) = agro::get(&rt.state.db, rt.business_id, &peer).await else { return Ok(payload) };
                    let (Some(url), false) = (p.profile_url.clone(), p.link_sent) else { return Ok(payload) };
                    let reply = payload["reply"].as_str().unwrap_or("").to_string();
                    if reply.trim().is_empty() {
                        return Ok(payload);
                    }
                    if ops::config(&rt.state.db, rt.business_id).await.send_profile_link && !reply.contains(&url) {
                        payload["reply"] = json!(format!("{}\n\n👉 Tu perfil: {url}", reply.trim_end()));
                    }
                    agro::mark_link_sent(&rt.state.db, rt.business_id, &peer).await?;
                    Ok(payload)
                })
            }),
        );
        k.on(
            "prompt/customer",
            hook_fn(|rt, mut payload| {
                Box::pin(async move {
                    let mounted = payload["tools"].as_array().is_some_and(|t| t.iter().any(|n| n == "agro_register"));
                    let peer = payload["peer"].as_str().map(str::to_string);
                    if let (true, Some(peer)) = (mounted, peer) {
                        let section = agro::note(&rt.state.db, rt.business_id, &peer).await;
                        if let Some(s) = payload["sections"].as_array_mut() {
                            s.push(json!(section));
                        }
                    }
                    Ok(payload)
                })
            }),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, api, tool_ctx, Mock};

    const PEER: &str = "wa:51977000111";

    #[tokio::test]
    async fn register_takes_the_menu_or_own_words() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let c = tool_ctx(&s, b, Some(PEER)).await;
        let r = agro_register(&c, &json!({"role": "hola"})).await.unwrap();
        assert_eq!(r["status"], "unknown_role");
        assert!(agro::get(&s.db, b, PEER).await.is_none());

        let r = agro_register(&c, &json!({"role": "tengo un camión"})).await.unwrap();
        assert_eq!(r["role"], "transportista");
        assert_eq!(r["nextField"], "nombre");
        let r = agro_register(&c, &json!({"role": "Comprador"})).await.unwrap();
        assert_eq!(r["role"], "comprador", "a correction re-registers");

        let owner = tool_ctx(&s, b, None).await;
        assert!(agro_register(&owner, &json!({"role": "1"})).await.is_err(), "customer side only");
    }

    #[tokio::test]
    async fn save_answers_with_the_next_question_until_complete() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let c = tool_ctx(&s, b, Some(PEER)).await;
        let r = agro_save(&c, &json!({"fields": {"nombre": "Rosa"}})).await.unwrap();
        assert_eq!(r["status"], "error", "no role yet is told to the model, not thrown: {r}");

        agro_register(&c, &json!({"role": "1"})).await.unwrap();
        let r = agro_save(&c, &json!({"fields": {"nombre": "Rosa", "ubicacion": "Juliaca", "color": "rojo"}})).await.unwrap();
        assert_eq!(r["status"], "saved");
        assert_eq!(r["nextField"], "productos");
        assert_eq!(r["rejected"], json!(["color"]));

        let r = agro_save(&c, &json!({"fields": {"productos": "papa nativa", "volumen": "3 t/semana", "temporada": "abril-junio"}})).await.unwrap();
        assert_eq!(r["status"], "profile complete");
        assert_eq!(r["complete"], true);
        assert!(r.get("nextQuestion").is_none());
        assert!(r["note"].as_str().unwrap().contains("team"));
    }

    #[tokio::test]
    async fn the_directory_is_the_owners() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        agro::register(&s.db, b, "wa:51900000001", Role::Productor).await.unwrap();
        agro::register(&s.db, b, "wa:51900000002", Role::Transportista).await.unwrap();
        let customer = tool_ctx(&s, b, Some(PEER)).await;
        assert_eq!(agro_directory(&customer, &json!({})).await.unwrap()["error"], "owner only");
        let owner = tool_ctx(&s, b, None).await;
        let d = agro_directory(&owner, &json!({})).await.unwrap();
        assert_eq!(d["counts"]["productor"]["onboarding"], 1);
        assert_eq!(d["counts"]["comprador"]["onboarding"], 0);
        assert_eq!(d["participants"].as_array().unwrap().len(), 2);
        let d = agro_directory(&owner, &json!({"role": "transportista"})).await.unwrap();
        assert_eq!(d["participants"][0]["phone"], "51900000002");
    }

    fn system_prompt(m: &Mock) -> String {
        m.seen_path("/chat/completions").last().unwrap().body["messages"][0]["content"].as_str().unwrap().to_string()
    }

    fn tool_names(m: &Mock) -> String {
        m.seen_path("/chat/completions").last().unwrap().body["tools"].to_string()
    }

    #[tokio::test]
    async fn other_verticals_never_see_the_agro_interview() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"}));
        m.say("Hola, ¿en qué te ayudo?");
        let msg = json!({"from": "51977000111", "text": "hola"});
        assert_eq!(api(&s, "POST", "/api/node/message", None, Some(msg)).await.0, 200);
        assert!(!system_prompt(&m).contains("AGRO"));
        assert!(!tool_names(&m).contains("agro_register"));
    }

    /// The whole WhatsApp flow on a node pinned to the agro bundle: menu,
    /// role, interview, complete — the model's moves scripted, the state real.
    #[tokio::test]
    async fn whatsapp_onboards_a_transportista_end_to_end() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (_, b) = testkit::onboard(&s, &m).await;
        sqlx::query("UPDATE businesses SET bundle = 'agro@1' WHERE id = $1").bind(b).execute(&s.db).await.unwrap();
        m.on("/v1/credits", json!({"state": "ok"}));
        let wa = |text: &str| json!({"from": "+51 977 000 111", "name": "Juan", "text": text});

        m.say("Hola 👋 ¿Eres 1️⃣ Productor, 2️⃣ Comprador o 3️⃣ Transportista?");
        let (st, v) = api(&s, "POST", "/api/node/message", None, Some(wa("hola"))).await;
        assert_eq!(st, 200, "{v}");
        let sys = system_prompt(&m);
        assert!(sys.contains("AGRO ONBOARDING — this person is new") && sys.contains("3️⃣ Transportista"), "{sys}");
        assert!(tool_names(&m).contains("agro_register") && tool_names(&m).contains("agro_save"));
        assert!(!tool_names(&m).contains("agro_directory"), "the directory is the owner's");

        m.call_tool("agro_register", json!({"role": "3"}));
        m.say("¡Genial! ¿Cómo te llamas?");
        api(&s, "POST", "/api/node/message", None, Some(wa("3"))).await;
        let p = agro::get(&s.db, b, PEER).await.expect("registered from WhatsApp");
        assert_eq!(p.role, Role::Transportista);

        m.call_tool("agro_save", json!({"fields": {"nombre": "Juan Mamani", "vehiculo": "camión Volvo 20 t, no refrigerado"}}));
        m.say("Gracias Juan. ¿Qué rutas cubres?");
        api(&s, "POST", "/api/node/message", None, Some(wa("Juan Mamani, tengo un Volvo de 20 t"))).await;
        assert_eq!(agro::get(&s.db, b, PEER).await.unwrap().name.as_deref(), Some("Juan Mamani"));

        m.call_tool("agro_save", json!({"fields": {"ruta": "Puno → Arequipa → Nazca → Lima", "base": "Juliaca", "disponibilidad": "martes y viernes"}}));
        m.say("¡Listo Juan! Te registré como transportista. El equipo te contactará.");
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(wa("Puno a Lima, vivo en Juliaca, salgo martes y viernes"))).await;
        // This turn opened knowing what the last one saved, asking the route next.
        let sys = system_prompt(&m);
        assert!(sys.contains("registered as transportista") && sys.contains("Juan Mamani"), "{sys}");
        assert!(sys.contains(agro::required(Role::Transportista)[2].question), "{sys}");
        assert!(v["text"].as_str().unwrap().contains("Te registré"), "{v}");
        let p = agro::get(&s.db, b, PEER).await.unwrap();
        assert!(p.complete, "{:?}", p.profile);
        assert_eq!(p.name.as_deref(), Some("Juan Mamani"));

        m.say("¿En qué más te ayudo?");
        api(&s, "POST", "/api/node/message", None, Some(wa("gracias"))).await;
        assert!(system_prompt(&m).contains("AGRO PROFILE COMPLETE"));
    }

    #[tokio::test]
    async fn the_owners_app_lists_the_directory() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (token, b) = testkit::onboard(&s, &m).await;
        agro::register(&s.db, b, "wa:51900000001", Role::Productor).await.unwrap();
        agro::register(&s.db, b, "wa:51900000002", Role::Comprador).await.unwrap();
        assert_eq!(api(&s, "GET", "/api/agro/participants", None, None).await.0, 401, "owner's device only");
        let (st, v) = api(&s, "GET", "/api/agro/participants", Some(&token), None).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["participants"].as_array().unwrap().len(), 2);
        assert_eq!(v["counts"]["comprador"]["onboarding"], 1);
        let (_, v) = api(&s, "GET", "/api/agro/participants?role=productor", Some(&token), None).await;
        assert_eq!(v["participants"].as_array().unwrap().len(), 1);
        assert_eq!(v["participants"][0]["missing"][0], "nombre");
        assert_eq!(api(&s, "GET", "/api/agro/participants?role=chofer", Some(&token), None).await.0, 400);
    }

    // ---------------------------------------------- the operator's systems

    async fn complete_productor(c: &ToolCtx<'_>) -> Value {
        agro_register(c, &json!({"role": "productor"})).await.unwrap();
        agro_save(c, &json!({"fields": {"nombre": "Rosa Quispe", "ubicacion": "Juliaca", "productos": "papa", "volumen": "3 t"}})).await.unwrap();
        agro_save(c, &json!({"fields": {"temporada": "mayo"}})).await.unwrap()
    }

    #[tokio::test]
    async fn completion_links_to_the_operators_site() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        crate::ops::configure(&s.db, b, &json!({"domain": "example.com"})).await.unwrap();
        let c = tool_ctx(&s, b, Some(PEER)).await;
        let r = complete_productor(&c).await;
        let p = agro::get(&s.db, b, PEER).await.unwrap();
        let id = p.id.clone().unwrap();
        assert_eq!(r["profileUrl"], format!("https://example.com/perfil/{id}"));
        assert_eq!(p.profile_url.as_deref(), r["profileUrl"].as_str());
        assert!(r["note"].as_str().unwrap().contains(r["profileUrl"].as_str().unwrap()), "the model is told to send it: {r}");

        crate::ops::configure(&s.db, b, &json!({"profileUrl": "https://example.com/{role}/{slug}-{phone}"})).await.unwrap();
        let c2 = tool_ctx(&s, b, Some("wa:51977000222")).await;
        assert_eq!(complete_productor(&c2).await["profileUrl"], "https://example.com/productor/rosa-quispe-51977000222");
    }

    #[tokio::test]
    async fn no_domain_no_link() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let c = tool_ctx(&s, b, Some(PEER)).await;
        let r = complete_productor(&c).await;
        assert_eq!(r["complete"], true);
        assert!(r.get("profileUrl").is_none(), "{r}");
        assert!(!r["note"].as_str().unwrap().contains("http"));
    }

    #[tokio::test]
    async fn their_webhook_hears_every_step_and_may_name_the_link() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        crate::ops::configure(&s.db, b, &json!({"domain": "example.com", "webhookUrl": format!("{}/hook", m.base)})).await.unwrap();
        m.on_fn("/hook", |seen| match seen.body["type"].as_str() {
            Some("participant.completed") => (200, json!({"profileUrl": "https://app.example.com/u/9f3"})),
            _ => (200, json!({"ok": true})),
        });
        let c = tool_ctx(&s, b, Some(PEER)).await;
        let r = complete_productor(&c).await;
        assert_eq!(r["profileUrl"], "https://app.example.com/u/9f3", "their id, their link");
        let kinds: Vec<String> = m.seen_path("/hook").iter().map(|x| x.body["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["participant.registered", "participant.updated", "participant.completed"]);
        let done = m.seen_path("/hook").pop().unwrap().body;
        let p = &done["data"]["participant"];
        assert_eq!((p["role"].as_str(), p["phone"].as_str(), p["status"].as_str()), (Some("productor"), Some("51977000111"), Some("complete")));
        assert_eq!(p["profile"]["nombre"], "Rosa Quispe");
        assert!(p["id"].as_str().is_some_and(|id| !id.is_empty()));
    }

    #[tokio::test]
    async fn a_link_off_their_domain_is_not_sent() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        crate::ops::configure(&s.db, b, &json!({"domain": "example.com", "webhookUrl": format!("{}/hook", m.base)})).await.unwrap();
        m.on("/hook", json!({"profileUrl": "https://phish.example/u/1"}));
        let c = tool_ctx(&s, b, Some(PEER)).await;
        let r = complete_productor(&c).await;
        assert!(r["profileUrl"].as_str().unwrap().starts_with("https://example.com/perfil/"), "template instead: {r}");
    }

    #[tokio::test]
    async fn the_link_reaches_whatsapp_even_if_the_model_forgets_it() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (_, b) = testkit::onboard(&s, &m).await;
        sqlx::query("UPDATE businesses SET bundle = 'agro@1' WHERE id = $1").bind(b).execute(&s.db).await.unwrap();
        crate::ops::configure(&s.db, b, &json!({"domain": "example.com"})).await.unwrap();
        m.on("/v1/credits", json!({"state": "ok"}));
        agro::register(&s.db, b, PEER, Role::Comprador).await.unwrap();
        agro::save(&s.db, b, PEER, &json!({"nombre": "Luis", "empresa": "Hoteles Sur", "ubicacion": "Arequipa", "productos": "quinua"})).await.unwrap();
        let wa = |text: &str| json!({"from": "51977000111", "text": text});

        m.call_tool("agro_save", json!({"fields": {"volumen": "500 kg al mes"}}));
        m.say("¡Listo Luis, quedaste registrado!");
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(wa("500 kg al mes"))).await;
        let url = agro::get(&s.db, b, PEER).await.unwrap().profile_url.unwrap();
        let text = v["text"].as_str().unwrap();
        assert!(text.contains("¡Listo Luis") && text.ends_with(&url), "{text}");
        assert_eq!(text.matches(&url).count(), 1);

        m.say("De nada 🙌");
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(wa("gracias"))).await;
        assert_eq!(v["text"], "De nada 🙌", "sent once");
    }

    #[tokio::test]
    async fn the_model_sending_the_link_itself_is_not_doubled_and_owners_can_turn_it_off() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let (_, b) = testkit::onboard(&s, &m).await;
        sqlx::query("UPDATE businesses SET bundle = 'agro@1' WHERE id = $1").bind(b).execute(&s.db).await.unwrap();
        m.on("/v1/credits", json!({"state": "ok"}));
        crate::ops::configure(&s.db, b, &json!({"domain": "example.com", "sendProfileLink": false})).await.unwrap();
        agro::register(&s.db, b, PEER, Role::Comprador).await.unwrap();
        agro::save(&s.db, b, PEER, &json!({"nombre": "Luis", "empresa": "Hoteles Sur", "ubicacion": "Arequipa", "productos": "quinua"})).await.unwrap();
        m.call_tool("agro_save", json!({"fields": {"volumen": "500 kg"}}));
        m.say("¡Listo!");
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(json!({"from": "51977000111", "text": "500 kg"}))).await;
        let text = v["text"].as_str().unwrap();
        assert!(text.ends_with("¡Listo!") && !text.contains("http"), "links off: nothing appended: {text}");
        assert!(agro::get(&s.db, b, PEER).await.unwrap().profile_url.is_some(), "the owner still has the link");

        crate::ops::configure(&s.db, b, &json!({"sendProfileLink": true})).await.unwrap();
        let other = "wa:51977000333";
        agro::register(&s.db, b, other, Role::Comprador).await.unwrap();
        agro::save(&s.db, b, other, &json!({"nombre": "Ana", "empresa": "X", "ubicacion": "Lima", "productos": "palta"})).await.unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        m.on_fn("/chat/completions", move |seen| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return (200, json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "agro_save", "arguments": json!({"fields": {"volumen": "1 t"}}).to_string()}}]}}]}));
            }
            // The model read the tool result and wrote the link itself.
            let url = seen.body["messages"].as_array().unwrap().iter().rev()
                .find_map(|x| x["content"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok()).and_then(|v| v["profileUrl"].as_str().map(str::to_string)))
                .unwrap_or_default();
            (200, json!({"choices": [{"message": {"role": "assistant", "content": format!("Tu perfil: {url}")}}]}))
        });
        let (_, v) = api(&s, "POST", "/api/node/message", None, Some(json!({"from": "51977000333", "text": "1 t"}))).await;
        let url = agro::get(&s.db, b, other).await.unwrap().profile_url.unwrap();
        assert_eq!(v["text"].as_str().unwrap().matches(&url).count(), 1, "{v}");
    }

    #[tokio::test]
    async fn the_owner_configures_the_site_from_the_manager_chat() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        let customer = tool_ctx(&s, b, Some(PEER)).await;
        assert_eq!(ops_configure(&customer, &json!({"domain": "evil.com"})).await.unwrap()["error"], "owner only");
        let owner = tool_ctx(&s, b, None).await;
        let r = ops_configure(&owner, &json!({"domain": "example.com", "webhookUrl": format!("{}/hook", m.base)})).await.unwrap();
        assert_eq!(r["domain"], "example.com");
        assert_eq!(r["profileUrl"], "https://example.com/perfil/{id}");
        let secret = crate::ops::config(&s.db, b).await.secret.unwrap();
        assert!(!r.to_string().contains(&secret), "the secret never goes through the model");
        let r = ops_configure(&owner, &json!({"profileUrl": "https://otro.com/{id}"})).await.unwrap();
        assert!(r["error"].as_str().unwrap().contains("example.com"), "the reason is told to the model: {r}");

        m.on("/hook", json!({}));
        assert_eq!(ops_test(&owner, &json!({})).await.unwrap()["delivered"], true);
        ops_configure(&owner, &json!({"webhookUrl": format!("{}/down", m.base)})).await.unwrap();
        m.on_status("/down", 404, json!({}));
        let r = ops_test(&owner, &json!({})).await.unwrap();
        assert_eq!(r["delivered"], false);
        assert!(r["error"].as_str().unwrap().contains("404"), "{r}");
    }
}
