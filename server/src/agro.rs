//! The agro marketplace (bundle `agro`): a coordinator business —
//! "coordina, no transporta" — whose WhatsApp line onboards three kinds of
//! people: the productor who sells what they grow, the comprador who buys for
//! a company, and the transportista who moves the cargo between them.
//!
//! The customer agent does the talking; this module owns what it must not
//! improvise: which role a reply names, which answers each role owes, what to
//! ask next, and the record the owner later matches from.
//!
//!   "hola" ──▶ note(): ask the 1/2/3 menu ──▶ agro_register(role)
//!          ──▶ note(): next question ──▶ agro_save(fields) … ──▶ complete

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::db::Db;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Productor,
    Comprador,
    Transportista,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Productor, Role::Comprador, Role::Transportista];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Productor => "productor",
            Role::Comprador => "comprador",
            Role::Transportista => "transportista",
        }
    }

    pub fn from_key(s: &str) -> Option<Role> {
        let s = s.trim().to_lowercase();
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

/// One answer a role owes, and how to ask for it on WhatsApp.
pub struct Field {
    pub key: &'static str,
    pub question: &'static str,
}

const NOMBRE: Field = Field { key: "nombre", question: "¿Cómo te llamas?" };

const PRODUCTOR: &[Field] = &[
    NOMBRE,
    Field { key: "ubicacion", question: "¿Dónde está tu chacra o tu producción? (distrito, provincia y región)" },
    Field { key: "productos", question: "¿Qué produces? (cultivos o productos, y variedades)" },
    Field { key: "volumen", question: "¿Cuánto sueles tener para vender y cada cuánto? (kg o toneladas por cosecha o por semana)" },
    Field { key: "temporada", question: "¿Cuándo cosechas o tienes producto disponible?" },
];

const COMPRADOR: &[Field] = &[
    NOMBRE,
    Field { key: "empresa", question: "¿Para qué empresa o negocio compras? (si tienes RUC, pásamelo también)" },
    Field { key: "ubicacion", question: "¿Dónde recibes la mercadería? (ciudad o dirección de entrega)" },
    Field { key: "productos", question: "¿Qué productos buscas comprar, y con qué calidad o calibre?" },
    Field { key: "volumen", question: "¿Qué volumen necesitas y cada cuánto? (kg o toneladas por semana o por mes)" },
];

const TRANSPORTISTA: &[Field] = &[
    NOMBRE,
    Field { key: "vehiculo", question: "¿Qué vehículo tienes y cuánta carga lleva? (tipo, toneladas, si es refrigerado)" },
    Field { key: "ruta", question: "¿Qué rutas cubres? (por ejemplo Puno → Arequipa → Nazca → Lima)" },
    Field { key: "base", question: "¿En qué ciudad estás normalmente?" },
    Field { key: "disponibilidad", question: "¿Qué días o cada cuánto puedes hacer viajes?" },
];

/// Answers kept when offered but never asked for.
pub const OPTIONAL: &[&str] = &["ruc", "placa", "precio", "calidad", "certificaciones", "email", "notas"];

/// Longest answer stored, in characters.
const MAX_ANSWER: usize = 300;

/// The interview of `role`, in the order it is asked.
pub fn required(role: Role) -> &'static [Field] {
    match role {
        Role::Productor => PRODUCTOR,
        Role::Comprador => COMPRADOR,
        Role::Transportista => TRANSPORTISTA,
    }
}

/// A key some role asks for, or an optional one.
fn known_key(key: &str) -> bool {
    OPTIONAL.contains(&key) || Role::ALL.iter().any(|r| required(*r).iter().any(|f| f.key == key))
}

fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' => 'o', 'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', c => c,
    }
}

/// Whole words, or word stems, that only one role would say about itself.
/// Plurals like "productores" are left out on purpose: a comprador says
/// "compro a productores".
const WORDS: &[(Role, &[&str], &[&str])] = &[
    (Role::Productor,
     &["productor", "productora", "produzco", "producimos", "agricultor", "agricultora"],
     &["siembr", "sembr", "cultiv", "cosech", "chacr", "ganader", "campesin", "hectar"]),
    (Role::Comprador,
     &["comprador", "compradora", "compro", "comprar", "compramos", "compraria"],
     &["mayorist", "acopiador"]),
    (Role::Transportista,
     &[],
     &["transport", "camion", "flete", "furgon", "chofer", "trailer", "carguer"]),
];

/// The role a free-text reply names: the menu digit ("2", "2)", "2️⃣") or the
/// person's own words ("soy agricultor", "tengo un camión"). `None` when it
/// names none or more than one — the agent asks again instead of guessing.
pub fn parse_role(text: &str) -> Option<Role> {
    let t: String = text.trim().to_lowercase().chars().map(fold).collect();
    let menu = t.trim_end_matches(|c: char| matches!(c, ')' | '.' | '\u{fe0f}' | '\u{20e3}') || c.is_whitespace());
    match menu {
        "1" => return Some(Role::Productor),
        "2" => return Some(Role::Comprador),
        "3" => return Some(Role::Transportista),
        _ => {}
    }
    let words: Vec<&str> = t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let named: Vec<Role> = WORDS
        .iter()
        .filter(|(_, exact, stems)| words.iter().any(|w| exact.contains(w) || stems.iter().any(|s| w.starts_with(s))))
        .map(|(r, _, _)| *r)
        .collect();
    match named.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

fn answered(profile: &Value, key: &str) -> bool {
    profile[key].as_str().is_some_and(|s| !s.trim().is_empty())
}

/// The required fields of `role` that `profile` does not hold yet, in order.
pub fn missing(role: Role, profile: &Value) -> Vec<&'static Field> {
    required(role).iter().filter(|f| !answered(profile, f.key)).collect()
}

#[derive(Debug, Clone)]
pub struct Participant {
    pub peer: String,
    pub phone: Option<String>,
    pub role: Role,
    pub name: Option<String>,
    pub profile: Value,
    pub complete: bool,
    /// Public id the operator's website names the profile by (`{id}`).
    pub id: Option<String>,
    /// The profile link on the operator's site, once complete.
    pub profile_url: Option<String>,
    /// The link already went out to them.
    pub link_sent: bool,
}

impl Participant {
    pub fn to_json(&self) -> Value {
        json!({
            "peer": self.peer,
            "phone": self.phone,
            "role": self.role.as_str(),
            "name": self.name,
            "status": if self.complete { "complete" } else { "onboarding" },
            "profile": self.profile,
            "missing": missing(self.role, &self.profile).iter().map(|f| f.key).collect::<Vec<_>>(),
            "id": self.id,
            "profileUrl": self.profile_url,
        })
    }
}

/// The phone digits behind a WhatsApp peer (`wa:51977000111`).
fn phone_of(peer: &str) -> Option<String> {
    let tail = peer.rsplit(':').next().unwrap_or(peer);
    if !tail.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-')) {
        return None;
    }
    let d = crate::node::digits(tail);
    (8..=15).contains(&d.len()).then_some(d)
}

type Row = (String, Option<String>, String, Option<String>, String, String, Option<String>, Option<String>, Option<String>);

const COLUMNS: &str = "peer, phone, role, name, profile, status, pid, profile_url, link_sent_at";

fn from_row((peer, phone, role, name, profile, status, id, profile_url, link_sent_at): Row) -> Option<Participant> {
    Some(Participant {
        peer,
        phone,
        role: Role::from_key(&role)?,
        name,
        profile: serde_json::from_str(&profile).unwrap_or_else(|_| json!({})),
        complete: status == "complete",
        id,
        profile_url,
        link_sent: link_sent_at.is_some(),
    })
}

pub async fn get(db: &Db, business_id: Uuid, peer: &str) -> Option<Participant> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM agro_participants WHERE business_id = $1 AND peer = $2",
    )).bind(business_id).bind(peer).fetch_optional(db).await.ok().flatten();
    row.and_then(from_row)
}

/// Writes the profile back and settles the status from it. Returns whether
/// this write is the one that completed the profile.
async fn store(db: &Db, business_id: Uuid, peer: &str, role: Role, profile: &Value, was_complete: bool) -> Result<bool> {
    let complete = missing(role, profile).is_empty();
    let name = profile["nombre"].as_str().map(str::to_string);
    sqlx::query(
        "UPDATE agro_participants SET role = $3, profile = $4, name = $5, status = $6, \
         completed_at = CASE WHEN $7 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00','now') ELSE completed_at END, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE business_id = $1 AND peer = $2",
    )
    .bind(business_id).bind(peer).bind(role.as_str()).bind(profile.to_string()).bind(name)
    .bind(if complete { "complete" } else { "onboarding" })
    .bind(complete && !was_complete)
    .execute(db).await?;
    Ok(complete && !was_complete)
}

/// Records (or changes) the role `peer` plays. The answers already given stay.
pub async fn register(db: &Db, business_id: Uuid, peer: &str, role: Role) -> Result<Participant> {
    sqlx::query("INSERT OR IGNORE INTO agro_participants (business_id, peer, phone, role) VALUES ($1, $2, $3, $4)")
        .bind(business_id).bind(peer).bind(phone_of(peer)).bind(role.as_str())
        .execute(db).await?;
    // Rows from before public ids (migration 023) get theirs here.
    sqlx::query("UPDATE agro_participants SET pid = $3 WHERE business_id = $1 AND peer = $2 AND pid IS NULL")
        .bind(business_id).bind(peer).bind(new_pid()).execute(db).await?;
    let p = get(db, business_id, peer).await.ok_or_else(|| anyhow!("participant vanished"))?;
    store(db, business_id, peer, role, &p.profile, p.complete).await?;
    tracing::info!(business = %business_id, role = role.as_str(), "agro participant registered");
    get(db, business_id, peer).await.ok_or_else(|| anyhow!("participant vanished"))
}

/// Ten lowercase hex characters: short enough for a link, unguessable
/// enough that profiles can't be walked.
fn new_pid() -> String {
    Uuid::new_v4().simple().to_string()[..10].to_string()
}

/// Records the profile link on the operator's site.
pub async fn set_profile_url(db: &Db, business_id: Uuid, peer: &str, url: &str) -> Result<()> {
    sqlx::query("UPDATE agro_participants SET profile_url = $3 WHERE business_id = $1 AND peer = $2")
        .bind(business_id).bind(peer).bind(url).execute(db).await?;
    Ok(())
}

/// The profile link went out (or the owner chose not to send it): never again.
pub async fn mark_link_sent(db: &Db, business_id: Uuid, peer: &str) -> Result<()> {
    sqlx::query("UPDATE agro_participants SET link_sent_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE business_id = $1 AND peer = $2")
        .bind(business_id).bind(peer).execute(db).await?;
    Ok(())
}

/// An answer as stored: trimmed text, at most [`MAX_ANSWER`] characters.
/// Numbers and yes/no become text; blanks and nulls are no answer.
fn answer(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "sí".into() } else { "no".into() },
        Value::Array(a) => a.iter().filter_map(answer).collect::<Vec<_>>().join(", "),
        Value::Object(_) => v.to_string(),
        Value::Null => String::new(),
    };
    (!s.is_empty()).then(|| s.chars().take(MAX_ANSWER).collect())
}

pub struct Saved {
    pub participant: Participant,
    /// Keys offered that no role asks for — not stored.
    pub rejected: Vec<String>,
    /// This save is the one that completed the profile.
    pub completed_now: bool,
}

/// Merges `fields` into `peer`'s profile. Needs a role first.
pub async fn save(db: &Db, business_id: Uuid, peer: &str, fields: &Value) -> Result<Saved> {
    let fields = fields.as_object().ok_or_else(|| anyhow!("fields must be an object of answers, e.g. {{\"nombre\": \"Rosa\"}}"))?;
    let mut p = get(db, business_id, peer).await
        .ok_or_else(|| anyhow!("no role yet: ask productor, comprador or transportista and call agro_register first"))?;
    let mut rejected = Vec::new();
    let profile: &mut Map<String, Value> = match p.profile.as_object_mut() {
        Some(m) => m,
        None => {
            p.profile = json!({});
            p.profile.as_object_mut().unwrap()
        }
    };
    for (k, v) in fields {
        let key = k.trim().to_lowercase();
        if !known_key(&key) {
            rejected.push(k.clone());
            continue;
        }
        if let Some(a) = answer(v) {
            profile.insert(key, Value::String(a));
        }
    }
    let completed_now = store(db, business_id, peer, p.role, &p.profile, p.complete).await?;
    if completed_now {
        tracing::info!(business = %business_id, role = p.role.as_str(), "agro participant complete");
    }
    let participant = get(db, business_id, peer).await.ok_or_else(|| anyhow!("participant vanished"))?;
    Ok(Saved { participant, rejected, completed_now })
}

/// Everyone onboarded or onboarding, newest first, optionally one role.
pub async fn list(db: &Db, business_id: Uuid, role: Option<Role>) -> Result<Vec<Participant>> {
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM agro_participants \
         WHERE business_id = $1 AND ($2 IS NULL OR role = $2) ORDER BY created_at DESC, rowid DESC LIMIT 500",
    )).bind(business_id).bind(role.map(Role::as_str)).fetch_all(db).await?;
    Ok(rows.into_iter().filter_map(from_row).collect())
}

/// The owner's view: counts per role and status, and the newest profiles.
pub async fn directory(db: &Db, business_id: Uuid, role: Option<Role>) -> Result<Value> {
    let people = list(db, business_id, role).await?;
    let count = |r: Role, done: bool| people.iter().filter(|p| p.role == r && p.complete == done).count();
    let counts: Map<String, Value> = Role::ALL.iter()
        .map(|r| (r.as_str().to_string(), json!({"complete": count(*r, true), "onboarding": count(*r, false)})))
        .collect();
    Ok(json!({"counts": counts, "participants": people.iter().take(50).map(Participant::to_json).collect::<Vec<_>>()}))
}

fn collected(p: &Participant) -> String {
    let lines: Vec<String> = p.profile.as_object().into_iter().flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| format!("{k}: {s}")))
        .collect();
    if lines.is_empty() { "nothing yet".into() } else { lines.join("; ") }
}

/// What the customer agent must do next with `peer`: open with the role menu,
/// ask the next missing answer, or — profile complete — keep it current.
pub async fn note(db: &Db, business_id: Uuid, peer: &str) -> String {
    const RULES: &str = "The business COORDINATES: it connects productores, compradores and transportistas \
        and follows up on the deals; it does not transport, and you never promise a price, a buyer, a load or \
        a truck you do not have — say the team will connect them. Short WhatsApp messages, plain words, \
        ONE question per message.";
    let Some(p) = get(db, business_id, peer).await else {
        return format!(
            "AGRO ONBOARDING — this person is new. {RULES}\n\
             Your reply must ask, in one short message, which of these they are, as this menu:\n\
             1️⃣ Productor — vendo lo que produzco\n\
             2️⃣ Comprador — compro para mi empresa o negocio\n\
             3️⃣ Transportista — llevo carga\n\
             As soon as they answer (a number or their own words), call agro_register with the role. \
             If what they say fits two roles, ask which one they want to register as first."
        );
    };
    let role = p.role.as_str();
    let todo = missing(p.role, &p.profile);
    match todo.first() {
        Some(next) => format!(
            "AGRO ONBOARDING — this person registered as {role}. {RULES}\n\
             Collected so far: {have}.\n\
             Ask next, in your own warm words: \"{q}\" ({left} question(s) left after this one).\n\
             Every time they answer, call agro_save with the answers (several fields from one message is fine; \
             keys: {keys}). If they say they are a different role, call agro_register again.",
            have = collected(&p),
            q = next.question,
            left = todo.len() - 1,
            keys = todo.iter().map(|f| f.key).chain(OPTIONAL.iter().copied()).collect::<Vec<_>>().join(", "),
        ),
        None => format!(
            "AGRO PROFILE COMPLETE — {role}: {have}. {RULES}\n\
             Do not interview them again. Help with what they ask, and when they tell you something new about \
             their offer, needs, route or availability, call agro_save to keep the profile current.",
            have = collected(&p),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    #[test]
    fn roles_round_trip() {
        for r in Role::ALL {
            assert_eq!(Role::from_key(r.as_str()), Some(r));
        }
        assert_eq!(Role::from_key(" Comprador "), Some(Role::Comprador));
        assert_eq!(Role::from_key("acopiador"), None);
    }

    #[test]
    fn menu_digits_name_a_role() {
        assert_eq!(parse_role("1"), Some(Role::Productor));
        assert_eq!(parse_role(" 2) "), Some(Role::Comprador));
        assert_eq!(parse_role("3."), Some(Role::Transportista));
        assert_eq!(parse_role("2️⃣"), Some(Role::Comprador));
        assert_eq!(parse_role("4"), None);
        assert_eq!(parse_role("tengo 3 hectáreas"), Some(Role::Productor), "a digit inside a sentence is not the menu");
    }

    #[test]
    fn own_words_name_a_role() {
        assert_eq!(parse_role("Soy agricultor de Puno"), Some(Role::Productor));
        assert_eq!(parse_role("siembro papa nativa y quinua"), Some(Role::Productor));
        assert_eq!(parse_role("PRODUCTORA de palta"), Some(Role::Productor));
        assert_eq!(parse_role("quiero comprar a productores"), Some(Role::Comprador));
        assert_eq!(parse_role("compro cebolla para mi restaurante"), Some(Role::Comprador));
        assert_eq!(parse_role("somos mayoristas en Lima"), Some(Role::Comprador));
        assert_eq!(parse_role("Tengo un camión de 10 toneladas"), Some(Role::Transportista));
        assert_eq!(parse_role("hago fletes Arequipa-Lima"), Some(Role::Transportista));
        assert_eq!(parse_role("soy transportista"), Some(Role::Transportista));
    }

    #[test]
    fn nothing_or_two_roles_is_no_answer() {
        assert_eq!(parse_role("hola"), None);
        assert_eq!(parse_role(""), None);
        assert_eq!(parse_role("¿qué es esto?"), None);
        assert_eq!(parse_role("busco transporte para mi cosecha"), None, "two roles: ask, don't guess");
        assert_eq!(parse_role("me comprometí a llamar"), None, "'comprometí' is not 'compro'");
        assert_eq!(parse_role("vendo productos"), None, "'productos' is what anyone sells or buys");
    }

    #[test]
    fn every_role_asks_the_name_first_and_never_twice() {
        for r in Role::ALL {
            let f = required(r);
            assert_eq!(f[0].key, "nombre", "{r:?}");
            assert!(f.len() >= 4, "{r:?}");
            let mut keys: Vec<_> = f.iter().map(|f| f.key).collect();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), f.len(), "{r:?} repeats a field");
            assert!(f.iter().all(|f| f.question.starts_with('¿') && f.question.contains('?')), "{r:?}");
            assert!(f.iter().all(|f| !OPTIONAL.contains(&f.key)), "{r:?}");
        }
        let keys = |r| required(r).iter().map(|f| f.key).collect::<Vec<_>>();
        assert!(keys(Role::Productor).contains(&"productos") && keys(Role::Productor).contains(&"volumen"));
        assert!(keys(Role::Comprador).contains(&"empresa"));
        assert!(keys(Role::Transportista).contains(&"vehiculo") && keys(Role::Transportista).contains(&"ruta"));
    }

    #[test]
    fn missing_is_in_order_and_ignores_blanks() {
        let m = missing(Role::Transportista, &json!({"nombre": "Juan", "ruta": "  ", "vehiculo": "Volvo 20 t"}));
        let keys: Vec<_> = m.iter().map(|f| f.key).collect();
        assert_eq!(keys[0], "ruta", "blank counts as missing, order is the interview's");
        assert!(!keys.contains(&"nombre") && !keys.contains(&"vehiculo"));
        assert_eq!(missing(Role::Productor, &json!({})).len(), required(Role::Productor).len());
    }

    const PEER: &str = "wa:51977000111";

    #[tokio::test]
    async fn register_then_save_until_complete() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        assert!(get(&s.db, b, PEER).await.is_none());
        let err = save(&s.db, b, PEER, &json!({"nombre": "Rosa"})).await.err().unwrap().to_string();
        assert!(err.contains("agro_register"), "{err}");

        let p = register(&s.db, b, PEER, Role::Productor).await.unwrap();
        assert_eq!((p.role, p.complete, p.phone.as_deref()), (Role::Productor, false, Some("51977000111")));

        let r = save(&s.db, b, PEER, &json!({"nombre": " Rosa Quispe ", "ubicacion": "Juliaca, Puno", "precio_pasaje": "x"})).await.unwrap();
        assert_eq!(r.rejected, vec!["precio_pasaje".to_string()]);
        assert!(!r.completed_now);
        assert_eq!(r.participant.name.as_deref(), Some("Rosa Quispe"));
        assert_eq!(r.participant.profile["nombre"], "Rosa Quispe", "trimmed");

        let mut last = None;
        for f in missing(Role::Productor, &r.participant.profile) {
            last = Some(save(&s.db, b, PEER, &json!({ f.key: format!("respuesta {}", f.key) })).await.unwrap());
        }
        let last = last.unwrap();
        assert!(last.completed_now && last.participant.complete);
        assert_eq!(last.participant.profile["ubicacion"], "Juliaca, Puno", "earlier answers kept");
        // Saving again updates, but does not "complete" twice.
        let again = save(&s.db, b, PEER, &json!({"volumen": "2 t por semana"})).await.unwrap();
        assert!(again.participant.complete && !again.completed_now);
        assert_eq!(get(&s.db, b, PEER).await.unwrap().profile["volumen"], "2 t por semana");
    }

    #[tokio::test]
    async fn save_bounds_what_it_stores() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        register(&s.db, b, PEER, Role::Comprador).await.unwrap();
        let r = save(&s.db, b, PEER, &json!({"nombre": "", "empresa": "x".repeat(2000), "ruc": 20123456789u64, "notas": null})).await.unwrap();
        assert!(r.participant.profile.get("nombre").is_none(), "blank answers are not answers");
        assert_eq!(r.participant.profile["empresa"].as_str().unwrap().chars().count(), 300);
        assert_eq!(r.participant.profile["ruc"], "20123456789", "numbers kept as text");
        assert!(r.participant.profile.get("notas").is_none());
        assert!(save(&s.db, b, PEER, &json!("nombre: Ana")).await.is_err(), "fields is an object");
    }

    #[tokio::test]
    async fn changing_role_keeps_answers_and_rechecks_completion() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        register(&s.db, b, PEER, Role::Transportista).await.unwrap();
        let all: Map<String, Value> = required(Role::Transportista).iter().map(|f| (f.key.to_string(), json!("ok"))).collect();
        assert!(save(&s.db, b, PEER, &Value::Object(all)).await.unwrap().completed_now);
        let p = register(&s.db, b, PEER, Role::Productor).await.unwrap();
        assert_eq!(p.role, Role::Productor);
        assert!(!p.complete, "a productor owes other answers");
        assert_eq!(p.profile["nombre"], "ok");
    }

    #[tokio::test]
    async fn participants_are_per_business_and_listed_by_role() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let other = testkit::business(&s.db).await;
        register(&s.db, b, "wa:51900000001", Role::Productor).await.unwrap();
        register(&s.db, b, "wa:51900000002", Role::Comprador).await.unwrap();
        register(&s.db, b, "wa:51900000003", Role::Productor).await.unwrap();
        register(&s.db, other, "wa:51900000004", Role::Productor).await.unwrap();
        assert_eq!(list(&s.db, b, None).await.unwrap().len(), 3);
        let prods = list(&s.db, b, Some(Role::Productor)).await.unwrap();
        assert_eq!(prods.iter().map(|p| p.peer.as_str()).collect::<Vec<_>>(), vec!["wa:51900000003", "wa:51900000001"], "newest first");
        assert!(get(&s.db, other, "wa:51900000001").await.is_none());
    }

    #[tokio::test]
    async fn note_walks_the_interview() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let n = note(&s.db, b, PEER).await;
        assert!(n.contains("1️⃣ Productor") && n.contains("2️⃣ Comprador") && n.contains("3️⃣ Transportista"), "{n}");
        assert!(n.contains("agro_register"), "{n}");

        register(&s.db, b, PEER, Role::Comprador).await.unwrap();
        let n = note(&s.db, b, PEER).await;
        assert!(n.contains("comprador") && n.contains(required(Role::Comprador)[0].question), "{n}");
        assert!(n.contains("agro_save"), "{n}");
        assert!(!n.contains("1️⃣"), "the menu is asked once: {n}");

        save(&s.db, b, PEER, &json!({"nombre": "Luis", "empresa": "Hoteles Sur SAC"})).await.unwrap();
        let n = note(&s.db, b, PEER).await;
        assert!(n.contains("Hoteles Sur SAC"), "what was said is shown: {n}");
        assert!(n.contains(required(Role::Comprador)[2].question) && !n.contains(required(Role::Comprador)[1].question), "{n}");

        let rest: Map<String, Value> = required(Role::Comprador).iter().map(|f| (f.key.to_string(), json!("ok"))).collect();
        save(&s.db, b, PEER, &Value::Object(rest)).await.unwrap();
        let n = note(&s.db, b, PEER).await;
        assert!(n.contains("COMPLETE") && !n.contains("¿"), "no more questions: {n}");
    }

    #[test]
    fn participant_json_is_what_the_owner_reads() {
        let p = Participant { peer: PEER.into(), phone: Some("51977000111".into()), role: Role::Transportista, name: Some("Juan".into()), profile: json!({"ruta": "Puno-Lima"}), complete: false, id: Some("a1b2c3d4e5".into()), profile_url: None, link_sent: false };
        let v = p.to_json();
        assert_eq!(v["role"], "transportista");
        assert_eq!(v["phone"], "51977000111");
        assert_eq!(v["status"], "onboarding");
        assert_eq!(v["profile"]["ruta"], "Puno-Lima");
        assert!(v["missing"].as_array().unwrap().iter().any(|k| k == "vehiculo"));
        assert_eq!(v["id"], "a1b2c3d4e5");
        assert!(v["profileUrl"].is_null());
    }

    #[tokio::test]
    async fn every_participant_gets_a_public_id_once() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let id = register(&s.db, b, PEER, Role::Productor).await.unwrap().id.unwrap();
        assert_eq!(id.len(), 10);
        assert_eq!(register(&s.db, b, PEER, Role::Comprador).await.unwrap().id, Some(id.clone()), "stable across role changes");
        assert_ne!(register(&s.db, b, "wa:51900000009", Role::Productor).await.unwrap().id, Some(id));
    }
}
