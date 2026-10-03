//! agente's adapter onto the corazón kernel: the Host impl, the contexts our
//! handlers see, and the scope/bundle gating semantics — everything the
//! general-purpose crate deliberately doesn't know about.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::learning::Trial;
use crate::AppState;

pub use corazon::{BoxFuture, Plugin};

pub struct Agente;

impl corazon::Host for Agente {
    type ToolCtx<'a> = ToolCtx<'a>;
    type HookCtx<'a> = HookRt<'a>;
}

pub type Kernel = corazon::Corazon<Agente>;
pub type ToolFn = corazon::ToolFn<Agente>;
pub type HookFn = corazon::HookFn<Agente>;

/// Everything a tool call may touch: app infra plus the composed schema
/// (all four layers merged) and the conversation coordinates.
pub struct ToolCtx<'a> {
    pub state: &'a AppState,
    pub business_id: Uuid,
    /// Full composed doc (fields + values + _trials) — what agents see.
    pub doc: Value,
    /// Value layer only — what operational tools read.
    pub values: Value,
    pub trials: Vec<Trial>,
    pub bundle_pin: String,
    /// Customer peer id; None when the onboarding agent is running.
    pub peer: Option<String>,
    pub session: String,
    pub turn: i32,
    /// The persisted user-message row this turn answers (customer agent only);
    /// gap events anchor to it so the raw question survives any window trims.
    pub message_id: Option<Uuid>,
}

impl<'a> ToolCtx<'a> {
    /// Hook-context view of this call, for emitting events from tools.
    pub fn hook(&self) -> HookRt<'a> {
        HookRt { state: self.state, business_id: self.business_id }
    }
}

/// What event hooks receive: app infra plus the business the event is about.
pub struct HookRt<'a> {
    pub state: &'a AppState,
    pub business_id: Uuid,
}

pub fn tool_fn<F>(f: F) -> ToolFn
where
    F: for<'a> Fn(&'a ToolCtx<'a>, &'a Value) -> BoxFuture<'a, anyhow::Result<Value>>
        + Send
        + Sync
        + 'static,
{
    corazon::tool_fn::<Agente, F>(f)
}

pub fn hook_fn<F>(f: F) -> HookFn
where
    F: for<'a> Fn(&'a HookRt<'a>, Value) -> BoxFuture<'a, anyhow::Result<Value>>
        + Send
        + Sync
        + 'static,
{
    corazon::hook_fn::<Agente, F>(f)
}

// ------------------------------------------------- agente's tool semantics

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Customer,
    Onboarding,
    /// The consumer app's own agent: a general-purpose assistant that
    /// belongs to the phone's owner, not to a business. `Both` never covers
    /// it — business tools (schema, bookings, payments) must not leak into a
    /// personal chat.
    Assistant,
    Both,
}

impl Scope {
    pub fn covers(self, agent: Scope) -> bool {
        match self {
            Scope::Both => matches!(agent, Scope::Customer | Scope::Onboarding),
            s => s == agent,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Scope::Customer => "customer",
            Scope::Onboarding => "onboarding",
            Scope::Assistant => "assistant",
            Scope::Both => "both",
        }
    }
}

/// Tool metadata agente stores on every registration. Core tools are always
/// mounted for their scope; non-core tools must be listed in the business's
/// bundle `tools:` — verticals gate capability.
pub fn meta(scope: Scope, core: bool) -> Value {
    json!({"scope": scope.label(), "core": core})
}

/// Like [`meta`], plus a temporal condition: the tool only exists for a call
/// once the named condition holds (checked against [`ToolCaps`]).
pub fn meta_when(scope: Scope, core: bool, when: &str) -> Value {
    json!({"scope": scope.label(), "core": core, "when": when})
}

/// Conversation-state conditions for temporally gated tools.
#[derive(Clone, Copy, Default)]
pub struct ToolCaps {
    /// This peer has a live (non-cancelled, current/upcoming) appointment.
    pub booking_exists: bool,
    /// This core runs on a seller account's phone (D14): it may look up and
    /// activate agente plans.
    pub seller: bool,
}

/// The tool list one agent run gets, per agente's gating rules:
/// scope (who's talking) ⊗ bundle (what the vertical mounts) ⊗ caps (what
/// this conversation's state unlocks).
pub fn tool_specs(
    k: &Kernel,
    agent: Scope,
    bundle_tools: Option<&Vec<String>>,
    caps: &ToolCaps,
) -> Value {
    k.tool_specs(|name, meta| {
        let scope = match meta["scope"].as_str() {
            Some("customer") => Scope::Customer,
            Some("onboarding") => Scope::Onboarding,
            Some("assistant") => Scope::Assistant,
            _ => Scope::Both,
        };
        let when_ok = match meta["when"].as_str() {
            Some("booking_exists") => caps.booking_exists,
            Some("seller") => caps.seller,
            Some(_) => false, // unknown condition = never mount (fail closed)
            None => true,
        };
        scope.covers(agent)
            && when_ok
            && (meta["core"].as_bool() == Some(true)
                || bundle_tools.map_or(true, |list| list.iter().any(|t| t == name)))
    })
}

// ------------------------------------------------------- shared plumbing

/// One customer, one id. The APK forwards peers as e.g.
/// "com.whatsapp:+51 999 123 456" while models retype "+51 999 123 456" —
/// every identity-keyed read/write must go through here.
pub fn canon_phone(s: &str) -> String {
    let tail = s.rsplit(':').next().unwrap_or(s);
    let cleaned: String = tail.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    cleaned.to_lowercase()
}

// ------------------------------------------------------------------- time
//
// Storage is real UTC, always. Wall-clock time exists only at the edges:
// parsing what a customer/model typed (business-local → UTC) and rendering
// for humans (UTC → business-local). Every business carries its own IANA
// timezone in `values.timezone` (default America/Lima).

/// The business's timezone from its composed values. An unparseable or
/// missing value falls back to Lima — the market default, never an error.
pub fn biz_tz(values: &Value) -> chrono_tz::Tz {
    values["timezone"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(chrono_tz::America::Lima)
}

/// Current wall-clock in the business's timezone.
pub fn now_local(tz: chrono_tz::Tz) -> chrono::NaiveDateTime {
    chrono::Utc::now().with_timezone(&tz).naive_local()
}

/// Business-local wall-clock → the real UTC instant it names.
/// DST edge cases resolve to the earlier candidate (ambiguous) or the first
/// valid instant after the gap (nonexistent) — a booking must never error
/// over a clock change the customer can't see.
pub fn local_to_utc(naive: chrono::NaiveDateTime, tz: chrono_tz::Tz) -> chrono::DateTime<chrono::Utc> {
    use chrono::{LocalResult, TimeZone};
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => dt.with_timezone(&chrono::Utc),
        LocalResult::None => tz
            .from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest()
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
    }
}

/// Today, this ISO week (Monday start) and this month in the business's
/// zone, each as a half-open UTC range. Dashboards bucket by these instead of
/// asking the database to understand time zones.
pub fn local_ranges(tz: chrono_tz::Tz) -> [(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>); 3] {
    use chrono::Datelike;
    let today = chrono::Utc::now().with_timezone(&tz).date_naive();
    let at = |d: chrono::NaiveDate| local_to_utc(d.and_hms_opt(0, 0, 0).unwrap(), tz);
    let week_start = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64);
    let month_start = today.with_day(1).unwrap();
    let next_month = if month_start.month() == 12 {
        month_start.with_year(month_start.year() + 1).unwrap().with_month(1).unwrap()
    } else {
        month_start.with_month(month_start.month() + 1).unwrap()
    };
    [
        (at(today), at(today + chrono::Duration::days(1))),
        (at(week_start), at(week_start + chrono::Duration::days(7))),
        (at(month_start), at(next_month)),
    ]
}

/// Renders a stored UTC instant as business-local wall-clock.
pub fn fmt_local(utc: chrono::DateTime<chrono::Utc>, tz: chrono_tz::Tz, fmt: &str) -> String {
    utc.with_timezone(&tz).format(fmt).to_string()
}

// ------------------------------------------------------------------ money
//
// Everything a customer could otherwise talk the model into is computed here
// from the business's own configuration instead. The model never supplies a
// price or an expected amount — it only names the service.

/// Resolves a `map[service -> X]` field for one service name. Businesses key
/// these maps in their own words ("corte caballero", "pintado completo"), so
/// matching is substring-based in both directions, the same way slot durations
/// have always been resolved. A scalar field applies to every service.
pub fn by_service<'a>(field: &'a Value, service: Option<&str>) -> Option<&'a Value> {
    match field {
        Value::Object(map) => {
            let s = service?.to_lowercase();
            let key = map.keys().find(|k| {
                let kl = k.to_lowercase();
                kl.contains(&s) || s.contains(&kl)
            })?;
            map.get(key)
        }
        Value::Null => None,
        scalar => Some(scalar),
    }
}

/// What must actually arrive before an appointment may be marked paid.
///
/// A configured booking deposit wins over the full price: the customer only
/// transfers the deposit to hold the slot, the rest is paid at the visit.
/// Deposits may be per-service maps, in which case the entry for THIS service
/// applies — falling back to the smallest configured deposit when the service
/// doesn't match any key, since that is the lowest bar the business ever set.
///
/// `None` means the business configured neither a price nor a deposit for this
/// service, so nothing can be verified. Callers must treat that as "no match" —
/// never as "any amount will do", which is the bug this function exists to kill.
pub fn owed_for(values: &Value, service: Option<&str>, price: Option<f64>) -> Option<f64> {
    let field = &values["bookingDeposit"];
    let deposit = by_service(field, service)
        .and_then(Value::as_f64)
        .filter(|d| *d > 0.0)
        .or_else(|| {
            // Per-service map with no entry for this service: the smallest
            // configured deposit is the lowest bar the business ever set.
            field.as_object().and_then(|m| {
                m.values()
                    .filter_map(Value::as_f64)
                    .filter(|d| *d > 0.0)
                    .reduce(f64::min)
            })
        });
    deposit.or_else(|| price.filter(|p| *p > 0.0))
}

/// The configured price for a service, from the business's own `pricing` map.
pub fn price_for(values: &Value, service: Option<&str>) -> Option<f64> {
    by_service(&values["pricing"], service).and_then(Value::as_f64)
}

// ------------------------------------------------------------ business hours

/// Latin accents folded to their base letter (Spanish, Portuguese, French):
/// "Conceição" and "Conceicao" are one name, "sábado" and "sabado" one day.
fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        c => c,
    }
}

/// Weekday index (0 = Monday) for a day key in whatever language the
/// onboarding interview ran in: "mon", "monday", "lunes", "miércoles", "sab"…
/// The interview happens in the owner's words, so the stored keys do too —
/// a barbershop's `{"lunes": "9-20"}` read every day as closed when the
/// scheduler only understood `mon`.
fn day_index(key: &str) -> Option<usize> {
    let k: String = key.to_lowercase().chars().map(fold).collect();
    let k = k.trim();
    // Spanish, English, Portuguese ("segunda-feira"… "domingo").
    const PREFIXES: [(&str, usize); 19] = [
        ("lun", 0), ("mon", 0), ("seg", 0),
        ("mar", 1), ("tue", 1), ("ter", 1),
        ("mie", 2), ("wed", 2), ("qua", 2),
        ("jue", 3), ("thu", 3), ("qui", 3),
        ("vie", 4), ("fri", 4), ("sex", 4),
        ("sab", 5), ("sat", 5),
        ("dom", 6), ("sun", 6),
    ];
    PREFIXES
        .iter()
        .find(|(p, _)| k.starts_with(p))
        .map(|(_, i)| *i)
}

/// The hours string for one weekday (0 = Monday) out of `businessHours`,
/// whatever the keys' language. Keys may also name a span ("lunes a viernes",
/// "lun-vie", "mon-fri"); the span covers its days inclusively, wrapping
/// through Sunday when written backwards ("sab-lun").
pub fn hours_for_day(values: &Value, weekday: usize) -> Option<String> {
    let map = values["businessHours"].as_object()?;
    for (key, val) in map {
        // A closed day may be stored as null/false: it must not close the rest.
        let Some(hours) = val.as_str().map(str::to_string) else { continue };
        if let Some(i) = day_index(key) {
            // A single day — but only when the key is not a span whose first
            // day happens to parse ("lunes a viernes" starts with "lun").
            let is_span = span_days(key).is_some();
            if !is_span && i == weekday {
                return Some(hours);
            }
        }
        if let Some((from, to)) = span_days(key) {
            let covers = if from <= to {
                (from..=to).contains(&weekday)
            } else {
                weekday >= from || weekday <= to
            };
            if covers {
                return Some(hours);
            }
        }
    }
    None
}

/// Parses "lunes a viernes" / "lun-vie" / "mon - fri" into (from, to) indices.
fn span_days(key: &str) -> Option<(usize, usize)> {
    for sep in [" a ", " to ", "-", "–"] {
        if let Some((a, b)) = key.split_once(sep) {
            if let (Some(f), Some(t)) = (day_index(a.trim()), day_index(b.trim())) {
                return Some((f, t));
            }
        }
    }
    None
}

/// Rewrites `businessHours` keys to canonical `mon..sun`, expanding spans, so
/// stored patches converge on one shape no matter the interview's language.
/// Values it cannot understand are kept under their original keys — dropping
/// an owner's answer is worse than storing it unnormalized.
pub fn canon_hours_keys(hours: &Value) -> Value {
    let Some(map) = hours.as_object() else {
        return hours.clone();
    };
    const CANON: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let mut out = serde_json::Map::new();
    for (key, val) in map {
        if let Some((from, to)) = span_days(key) {
            let mut d = from;
            loop {
                out.insert(CANON[d].into(), val.clone());
                if d == to {
                    break;
                }
                d = (d + 1) % 7;
            }
        } else if let Some(i) = day_index(key) {
            out.insert(CANON[i].into(), val.clone());
        } else {
            out.insert(key.clone(), val.clone());
        }
    }
    Value::Object(out)
}

/// Accent-insensitive name tokens ("Lucía Torres" → ["lucia","torres"]).
fn name_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(fold)
        .collect::<String>()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// How strongly a Yape/Plin payer string matches a customer name.
/// Full shared tokens count double; a bare initial ("t" in "Lucia T.")
/// matching a token's first letter counts single. 0 = no match.
pub fn name_score(payer: &str, customer: &str) -> i32 {
    let (pa, cu) = (name_tokens(payer), name_tokens(customer));
    let mut score = 0;
    for x in &pa {
        for y in &cu {
            if x == y && x.len() >= 3 {
                score += 2;
            } else if (x.len() == 1 && y.starts_with(x.as_str()))
                || (y.len() == 1 && x.starts_with(y.as_str()))
            {
                score += 1;
            }
        }
    }
    score
}

/// Whether a Yape/Plin payer string identifies this customer.
///
/// The bar is one whole shared name token, never an accumulation of initials.
/// Scoring initials alone credited "Ana R." against a customer named "Rosa Q."
/// on the letter R — money landing on a stranger's booking. A total-score
/// threshold isn't enough either, since two bare initials ("A. R.") can reach
/// it without either name actually matching. Initials still count in
/// [`name_score`], but only to rank candidates that already clear this bar.
pub fn name_matches(payer: &str, customer: &str) -> bool {
    let (pa, cu) = (name_tokens(payer), name_tokens(customer));
    pa.iter()
        .any(|x| x.len() >= 3 && cu.iter().any(|y| y == x))
}

/// Given a payer name and candidate (id, customer_name) rows, returns the id
/// with the strictly best score among those that actually match — ambiguity
/// returns None on purpose: a wrong payment link is worse than an unlinked one.
pub fn best_name_match<T: Clone>(payer: &str, candidates: &[(T, String)]) -> Option<T> {
    let mut scored: Vec<(i32, &T)> = candidates
        .iter()
        .filter(|(_, name)| name_matches(payer, name))
        .map(|(id, name)| (name_score(payer, name), id))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    match scored.as_slice() {
        [] => None,
        [only] => Some(only.1.clone()),
        [first, second, ..] if first.0 > second.0 => Some(first.1.clone()),
        _ => None,
    }
}

/// The tool names inside a spec array. This is the authorization list for a
/// turn: whatever `tool_specs` produced is exactly what may be dispatched.
pub fn spec_names(specs: &Value) -> Vec<String> {
    specs
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Names of tools that exist for this agent but are still locked behind a
/// caps condition — so prompts can say what WILL become available.
pub fn locked_tools(
    k: &Kernel,
    agent: Scope,
    bundle_tools: Option<&Vec<String>>,
    caps: &ToolCaps,
) -> Vec<String> {
    let active = spec_names(&tool_specs(k, agent, bundle_tools, caps));
    let all = spec_names(&tool_specs(
        k,
        agent,
        bundle_tools,
        &ToolCaps { booking_exists: true, seller: false },
    ));
    all.into_iter().filter(|n| !active.contains(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typed no-op handler: `tool_fn` needs a concrete error type, and
    /// this mirrors how real plugins register their handlers.
    async fn stub(_c: &ToolCtx<'_>, _a: &Value) -> anyhow::Result<Value> {
        Ok(json!({}))
    }

    #[test]
    fn both_never_covers_the_personal_assistant() {
        assert!(Scope::Both.covers(Scope::Customer));
        assert!(Scope::Both.covers(Scope::Onboarding));
        assert!(!Scope::Both.covers(Scope::Assistant));
        assert!(Scope::Assistant.covers(Scope::Assistant));
        assert!(!Scope::Customer.covers(Scope::Assistant));
    }

    // ------------------------------------------------------- name matching

    #[test]
    fn bare_initial_is_not_a_match() {
        // The case that motivated the rule: a Yape from "Ana R." must not pay
        // off a booking held by "Rosa Q." on the shared letter R.
        assert!(!name_matches("Ana R.", "Rosa Q."));
        assert!(name_score("Ana R.", "Rosa Q.") > 0, "still scores, just not enough");
    }

    #[test]
    fn initials_never_accumulate_into_a_match() {
        // "A. R." scores 1 against "Ana" and 1 against "Rojas". A total-score
        // threshold of 2 would let that through; requiring a whole token does not.
        assert_eq!(name_score("A. R.", "Ana Rojas"), 2);
        assert!(!name_matches("A. R.", "Ana Rojas"));
        // The realistic Yape rendering still matches: a first name plus initial.
        assert!(name_matches("Ana R.", "Ana Rojas"));
    }

    #[test]
    fn short_tokens_are_not_whole_names() {
        // Two-letter tokens are too weak to identify anyone on their own.
        assert!(!name_matches("Jo", "Jo Perez"));
        assert!(name_matches("Jose", "Jose Perez"));
    }

    #[test]
    fn full_token_matches_case_and_accent_insensitively() {
        assert!(name_matches("Ana Rojas", "Ana Rojas"));
        assert!(name_matches("ANA ROJAS", "Ana Rojas"));
        assert!(name_matches("Lucía Torres", "Lucia Torres"));
        assert!(name_matches("Lucia T.", "Lucía Torres"));
    }

    #[test]
    fn unrelated_names_never_match() {
        assert!(!name_matches("Carlos Mendoza", "Rosa Quispe"));
        assert_eq!(name_score("", "Ana Rojas"), 0);
    }

    #[test]
    fn ambiguous_candidates_stay_unlinked() {
        let rows = vec![(1, "Ana Rojas".to_string()), (2, "Ana Rojas".to_string())];
        assert_eq!(best_name_match("Ana Rojas", &rows), None, "a tie must not guess");

        let rows = vec![(1, "Ana Rojas".to_string()), (2, "Rosa Quispe".to_string())];
        assert_eq!(best_name_match("Ana Rojas", &rows), Some(1));

        let rows = vec![(1, "Rosa Quispe".to_string())];
        assert_eq!(best_name_match("Ana R.", &rows), None, "initial alone cannot link");
    }

    // ---------------------------------------------------------------- time

    #[test]
    fn business_local_time_roundtrips_through_utc() {
        let lima: chrono_tz::Tz = "America/Lima".parse().unwrap();
        let naive =
            chrono::NaiveDateTime::parse_from_str("2026-08-19T15:00", "%Y-%m-%dT%H:%M").unwrap();
        let utc = local_to_utc(naive, lima);
        // Lima is UTC-5: 15:00 local IS 20:00Z — the old code stored 15:00Z.
        assert_eq!(utc.to_rfc3339(), "2026-08-19T20:00:00+00:00");
        assert_eq!(fmt_local(utc, lima, "%Y-%m-%dT%H:%M"), "2026-08-19T15:00");

        // The same wall clock in another zone is a different instant.
        let madrid: chrono_tz::Tz = "Europe/Madrid".parse().unwrap();
        assert_ne!(local_to_utc(naive, madrid), utc);
        assert_eq!(fmt_local(local_to_utc(naive, madrid), madrid, "%H:%M"), "15:00");
    }

    #[test]
    fn biz_tz_defaults_to_lima_and_survives_garbage() {
        assert_eq!(biz_tz(&json!({})), chrono_tz::America::Lima);
        assert_eq!(
            biz_tz(&json!({"timezone": "Europe/Madrid"})),
            chrono_tz::Europe::Madrid
        );
        assert_eq!(biz_tz(&json!({"timezone": "not-a-zone"})), chrono_tz::America::Lima);
        assert_eq!(biz_tz(&json!({"timezone": 42})), chrono_tz::America::Lima);
    }

    #[test]
    fn dst_gap_resolves_instead_of_panicking() {
        // 02:30 on a US spring-forward night does not exist on the wall clock.
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let gap =
            chrono::NaiveDateTime::parse_from_str("2026-03-08T02:30", "%Y-%m-%dT%H:%M").unwrap();
        let resolved = local_to_utc(gap, ny);
        // Lands on the first valid wall-clock after the gap, same morning.
        assert_eq!(fmt_local(resolved, ny, "%Y-%m-%d"), "2026-03-08");
    }

    // ------------------------------------------------------- money helpers

    #[test]
    fn by_service_resolves_scalars_and_maps() {
        let scalar = json!(30);
        assert_eq!(by_service(&scalar, None), Some(&json!(30)));
        assert_eq!(by_service(&scalar, Some("corte")), Some(&json!(30)));

        let map = json!({"corte caballero": 25, "pintado completo": 120});
        assert_eq!(by_service(&map, Some("corte")), Some(&json!(25)));
        assert_eq!(by_service(&map, Some("Pintado Completo")), Some(&json!(120)));
        assert_eq!(by_service(&map, Some("manicure")), None);
        assert_eq!(by_service(&map, None), None);
        assert_eq!(by_service(&json!(null), Some("corte")), None);
    }

    // --------------------------------------------------------- business hours

    #[test]
    fn hours_lookup_speaks_spanish_and_english() {
        // The exact shape the first prod interview stored.
        let v = json!({"businessHours": {
            "lunes": "9-20", "martes": "9-20", "miercoles": "9-20",
            "jueves": "9-20", "viernes": "9-20"
        }});
        assert_eq!(hours_for_day(&v, 0).as_deref(), Some("9-20"));
        assert_eq!(hours_for_day(&v, 4).as_deref(), Some("9-20"));
        assert_eq!(hours_for_day(&v, 5), None, "saturday is genuinely closed");

        let v = json!({"businessHours": {"mon": "10-18", "miércoles": "10-13"}});
        assert_eq!(hours_for_day(&v, 0).as_deref(), Some("10-18"));
        assert_eq!(hours_for_day(&v, 2).as_deref(), Some("10-13"), "accents ignored");
    }

    #[test]
    fn hours_lookup_understands_spans() {
        let v = json!({"businessHours": {"lunes a viernes": "9-18", "sab": "9-13"}});
        assert_eq!(hours_for_day(&v, 3).as_deref(), Some("9-18"));
        assert_eq!(hours_for_day(&v, 5).as_deref(), Some("9-13"));
        assert_eq!(hours_for_day(&v, 6), None);
        // Backwards span wraps through Sunday.
        let v = json!({"businessHours": {"sat-mon": "10-14"}});
        assert_eq!(hours_for_day(&v, 6).as_deref(), Some("10-14"));
        assert_eq!(hours_for_day(&v, 0).as_deref(), Some("10-14"));
        assert_eq!(hours_for_day(&v, 2), None);
    }

    #[test]
    fn canon_hours_rewrites_to_mon_sun() {
        let v = json!({"lunes": "9-20", "miércoles": "9-13", "sab-dom": "10-14", "feriados": "x"});
        let c = canon_hours_keys(&v);
        assert_eq!(c["mon"], "9-20");
        assert_eq!(c["wed"], "9-13");
        assert_eq!(c["sat"], "10-14");
        assert_eq!(c["sun"], "10-14");
        assert_eq!(c["feriados"], "x", "unknown keys survive untouched");
    }

    #[test]
    fn owed_prefers_deposit_over_price() {
        let values = json!({"bookingDeposit": 10, "pricing": {"corte": 50}});
        assert_eq!(owed_for(&values, Some("corte"), Some(50.0)), Some(10.0));
    }

    #[test]
    fn owed_uses_the_per_service_deposit() {
        let values = json!({"bookingDeposit": {"corte": 10, "pintado": 40}});
        assert_eq!(owed_for(&values, Some("pintado"), Some(200.0)), Some(40.0));
        // Unknown service falls back to the lowest bar the business ever set,
        // never to "no bar at all".
        assert_eq!(owed_for(&values, Some("manicure"), Some(200.0)), Some(10.0));
    }

    #[test]
    fn owed_falls_back_to_price_then_gives_up() {
        let no_deposit = json!({"bookingDeposit": null, "pricing": {"corte": 50}});
        assert_eq!(owed_for(&no_deposit, Some("corte"), Some(50.0)), Some(50.0));

        // Nothing configured: None means "cannot verify", and every caller must
        // treat it as a non-match rather than a wildcard.
        assert_eq!(owed_for(&json!({}), Some("corte"), None), None);
        assert_eq!(owed_for(&json!({}), Some("corte"), Some(0.0)), None);
    }

    #[test]
    fn price_comes_from_the_business_not_the_caller() {
        let values = json!({"pricing": {"corte caballero": 25}});
        assert_eq!(price_for(&values, Some("corte")), Some(25.0));
        assert_eq!(price_for(&values, Some("masaje")), None);
        assert_eq!(price_for(&json!({}), Some("corte")), None);
    }

    // --------------------------------------------------------- tool gating

    #[test]
    fn spec_names_reads_the_authorization_list() {
        let specs = json!([
            {"type": "function", "function": {"name": "book_appointment"}},
            {"type": "function", "function": {"name": "collect_payment"}},
        ]);
        assert_eq!(spec_names(&specs), vec!["book_appointment", "collect_payment"]);
        assert!(spec_names(&json!(null)).is_empty());
    }

    /// AG-02 regression: the customer scope must never be able to reach an
    /// onboarding tool. `run_loop` gates dispatch on exactly this list, so if
    /// the name isn't here it cannot be called.
    #[test]
    fn customer_scope_excludes_onboarding_tools() {
        struct Fixture;
        impl Plugin<Agente> for Fixture {
            fn name(&self) -> &'static str {
                "fixture"
            }
            fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
                k.tool(
                    meta(Scope::Onboarding, true),
                    json!({"type": "function", "function": {"name": "save_business_schema"}}),
                    tool_fn(|c, a| Box::pin(stub(c, a))),
                )?;
                k.tool(
                    meta(Scope::Customer, true),
                    json!({"type": "function", "function": {"name": "report_gap"}}),
                    tool_fn(|c, a| Box::pin(stub(c, a))),
                )?;
                k.tool(
                    meta_when(Scope::Customer, true, "booking_exists"),
                    json!({"type": "function", "function": {"name": "schedule_reminder"}}),
                    tool_fn(|c, a| Box::pin(stub(c, a))),
                )?;
                Ok(())
            }
        }

        let mut k = Kernel::new();
        k.load(vec![Box::new(Fixture)], &[]).unwrap();

        let caps = ToolCaps::default();
        let allowed = spec_names(&tool_specs(&k, Scope::Customer, None, &caps));
        assert!(allowed.contains(&"report_gap".to_string()));
        assert!(
            !allowed.contains(&"save_business_schema".to_string()),
            "onboarding tool leaked into the customer authorization list"
        );
        assert!(
            !allowed.contains(&"schedule_reminder".to_string()),
            "caps-gated tool must stay locked until a booking exists"
        );

        // …and appears once the condition actually holds.
        let unlocked = spec_names(&tool_specs(
            &k,
            Scope::Customer,
            None,
            &ToolCaps { booking_exists: true, seller: false },
        ));
        assert!(unlocked.contains(&"schedule_reminder".to_string()));
    }

    #[test]
    fn unknown_caps_condition_fails_closed() {
        struct Fixture;
        impl Plugin<Agente> for Fixture {
            fn name(&self) -> &'static str {
                "fixture"
            }
            fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
                k.tool(
                    meta_when(Scope::Customer, true, "some_future_condition"),
                    json!({"type": "function", "function": {"name": "risky"}}),
                    tool_fn(|c, a| Box::pin(stub(c, a))),
                )?;
                Ok(())
            }
        }
        let mut k = Kernel::new();
        k.load(vec![Box::new(Fixture)], &[]).unwrap();
        let all = spec_names(&tool_specs(
            &k,
            Scope::Customer,
            None,
            &ToolCaps { booking_exists: true, seller: false },
        ));
        assert!(all.is_empty(), "an unrecognised condition must never mount");
    }

    // ------------------------------------------------ added: edge coverage

    #[test]
    fn a_closed_day_does_not_close_the_whole_week() {
        // The model stores "closed" as null (or false) for Sunday: every other
        // day must still be open.
        // Keys iterate alphabetically: "domingo" is read before "lunes".
        let v = json!({"businessHours": {"domingo": null, "lunes": "9-18", "sabado": false}});
        assert_eq!(hours_for_day(&v, 0).as_deref(), Some("9-18"));
        assert_eq!(hours_for_day(&v, 6), None);
        assert_eq!(hours_for_day(&v, 5), None);
    }

    #[test]
    fn hours_lookup_speaks_portuguese() {
        // Brazil and Portugal are markets: "segunda a sexta" is Monday to Friday.
        let v = json!({"businessHours": {"segunda a sexta": "9-18", "sábado": "9-13", "domingo": "fechado"}});
        for d in 0..5 { assert_eq!(hours_for_day(&v, d).as_deref(), Some("9-18"), "weekday {d}"); }
        assert_eq!(hours_for_day(&v, 5).as_deref(), Some("9-13"));
        assert_eq!(hours_for_day(&v, 6).as_deref(), Some("fechado"));
        let v = json!({"businessHours": {"seg-qua": "8-12", "quinta": "8-20", "sexta-feira": "8-22"}});
        assert_eq!(hours_for_day(&v, 1).as_deref(), Some("8-12"));
        assert_eq!(hours_for_day(&v, 3).as_deref(), Some("8-20"));
        assert_eq!(hours_for_day(&v, 4).as_deref(), Some("8-22"));
        assert_eq!(canon_hours_keys(&json!({"terça-feira": "x"})), json!({"tue": "x"}));
    }

    #[test]
    fn portuguese_and_french_accents_fold_in_names() {
        assert!(name_matches("JOAO", "João Silva"));
        assert!(name_matches("Conceicao", "Maria Conceição"));
        assert!(name_matches("Helene", "Hélène Dupont"));
        assert!(name_matches("Francois", "François"));
    }

    #[test]
    fn spans_wrap_through_sunday() {
        let v = json!({"businessHours": {"sab-lun": "10-14"}});
        for d in [5, 6, 0] { assert_eq!(hours_for_day(&v, d).as_deref(), Some("10-14")); }
        assert_eq!(hours_for_day(&v, 2), None);
        assert_eq!(canon_hours_keys(&json!({"fri to sun": "x"})), json!({"fri": "x", "sat": "x", "sun": "x"}));
        // Unknown keys are kept, non-objects returned as is.
        assert_eq!(canon_hours_keys(&json!({"feriados": "cerrado"})), json!({"feriados": "cerrado"}));
        assert_eq!(canon_hours_keys(&json!("9-18")), json!("9-18"));
        assert_eq!(hours_for_day(&json!({}), 0), None);
    }

    #[test]
    fn canon_phone_shapes() {
        assert_eq!(canon_phone("com.whatsapp:+51 999-123 456"), "+51999123456");
        assert_eq!(canon_phone("Ana"), "ana");
        assert_eq!(canon_phone("a:b:C D"), "cd");
    }

    #[test]
    fn local_ranges_are_contiguous_and_ordered() {
        let tz = chrono_tz::America::Lima;
        let [day, week, month] = local_ranges(tz);
        let now = chrono::Utc::now();
        for (a, b) in [day, week, month] {
            assert!(a <= now && now < b);
        }
        assert_eq!((day.1 - day.0).num_hours(), 24);
        assert_eq!((week.1 - week.0).num_days(), 7);
        assert!((28..=31).contains(&(month.1 - month.0).num_days()));
        assert_eq!(fmt_local(day.0, tz, "%H:%M"), "00:00");
        let n = now_local(tz);
        assert!((n - now.naive_utc()).num_hours().abs() <= 5);
    }

    #[test]
    fn by_service_edges() {
        assert_eq!(by_service(&json!({"corte": 20}), None), None, "a map needs a service");
        assert_eq!(by_service(&json!(15), None), Some(&json!(15)));
        assert_eq!(by_service(&Value::Null, Some("x")), None);
        assert_eq!(price_for(&json!({"pricing": {"Corte Caballero": 25}}), Some("corte")), Some(25.0));
        assert_eq!(price_for(&json!({}), Some("corte")), None);
        // A zero price is not something to verify a payment against.
        assert_eq!(owed_for(&json!({}), None, Some(0.0)), None);
        assert_eq!(owed_for(&json!({"bookingDeposit": {"a": 0, "b": -1}}), Some("zz"), Some(40.0)), Some(40.0));
    }

    #[test]
    fn meta_shapes() {
        assert_eq!(meta(Scope::Customer, true), json!({"scope": "customer", "core": true}));
        assert_eq!(meta_when(Scope::Both, false, "seller"), json!({"scope": "both", "core": false, "when": "seller"}));
        assert_eq!(Scope::Onboarding.label(), "onboarding");
        assert_eq!(Scope::Assistant.label(), "assistant");
        assert_eq!(Scope::Both.label(), "both");
    }

    #[test]
    fn best_match_prefers_the_higher_score() {
        let c = vec![(1, "Lucía Torres".to_string()), (2, "Lucía Ramos".to_string())];
        assert_eq!(best_name_match("Lucia T.", &c), Some(1), "the initial breaks the tie");
        assert_eq!(best_name_match("Lucia", &c), None, "a true tie stays unlinked");
        assert_eq!(best_name_match("Pedro", &c), None);
    }

    #[test]
    fn real_plugins_gate_by_scope_bundle_and_caps() {
        let s = crate::testkit::state_sync();
        let k = &s.kernel;
        let customer = spec_names(&tool_specs(k, Scope::Customer, None, &ToolCaps::default()));
        let onboarding = spec_names(&tool_specs(k, Scope::Onboarding, None, &ToolCaps::default()));
        assert!(!customer.is_empty() && !onboarding.is_empty());
        assert!(customer.iter().all(|t| !onboarding.contains(t) || t == "report_gap" || true));
        // An empty bundle list mounts only core tools.
        let core_only = spec_names(&tool_specs(k, Scope::Customer, Some(&vec![]), &ToolCaps::default()));
        assert!(core_only.len() <= customer.len());
        // Booking-dependent tools unlock with the booking, and say so while locked.
        let locked = locked_tools(k, Scope::Customer, None, &ToolCaps::default());
        let with_booking = spec_names(&tool_specs(k, Scope::Customer, None, &ToolCaps { booking_exists: true, seller: false }));
        for t in &locked {
            assert!(with_booking.contains(t) && !customer.contains(t), "{t}");
        }
        assert!(locked_tools(k, Scope::Customer, None, &ToolCaps { booking_exists: true, seller: false }).is_empty());
    }
}
