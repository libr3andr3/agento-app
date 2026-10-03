//! Learning loop v0.1: gap events → candidate patches → three-layer schema.
//!
//! Rules enforced here:
//! 1. Events are append-only; corrections/resolutions are new rows.
//! 2. Every record names its owner and origin event.
//! 3. A candidate covers exactly the field that was asked about.
//! 4. Patterns up, data never: anything read cross-client is redacted at
//!    write time; client values never leave the client's rows.

use anyhow::{anyhow, Result};
use chrono::Utc;
use rand::{distributions::Alphanumeric, Rng};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
#[allow(unused_imports)]
use std::io::Write as _;
use std::sync::OnceLock;
use uuid::Uuid;

use crate::llm::Llm;

// ------------------------------------------------------------------ ids

pub fn gen_id(prefix: &str) -> String {
    let tail: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(|c| (c as char).to_ascii_uppercase())
        .collect();
    format!("{prefix}_{tail}")
}

/// One conversation-day with a peer = one session. The day boundary is the
/// business's local midnight — a UTC boundary split evening conversations
/// into two sessions, quietly diluting trial evidence.
pub fn session_id(peer: &str, tz: chrono_tz::Tz) -> String {
    format!("{}@{}", peer, Utc::now().with_timezone(&tz).format("%Y%m%d"))
}

// ------------------------------------------------------------- redaction

/// The privacy boundary, enforced at write time. LLM-side placeholders
/// (<ZONE>, <PERSON>, ...) pass through; this scrubs what regex can catch
/// even if the model forgot.
pub fn redact(text: &str) -> String {
    static MONEY: OnceLock<regex::Regex> = OnceLock::new();
    static PHONE: OnceLock<regex::Regex> = OnceLock::new();
    static TIME: OnceLock<regex::Regex> = OnceLock::new();
    static DATE: OnceLock<regex::Regex> = OnceLock::new();
    static NUM: OnceLock<regex::Regex> = OnceLock::new();
    static EMAIL: OnceLock<regex::Regex> = OnceLock::new();
    let email = EMAIL.get_or_init(|| regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap());
    let money = MONEY.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:s/\.?|r\$|rd\$|us\$|[$€£¥₹₩₱฿₫₦₺₪₴₲₡৳]|rs\.?|bs|kč|zł|\b[A-Z]{3}\b)\s*\d+(?:[., ]\d+)*|\d+(?:[., ]\d+)*\s*(?:soles|reais|rupees|rupias|euros|dollars|dólares|dolares|pesos|bolivianos|shillings|naira|yen|won|baht|lei|kr|\b[A-Z]{3}\b)").unwrap()
    });
    let phone = PHONE.get_or_init(|| regex::Regex::new(r"\+?\d[\d\s\-]{6,}\d").unwrap());
    let time = TIME.get_or_init(|| regex::Regex::new(r"\b\d{1,2}:\d{2}\b").unwrap());
    let date = DATE.get_or_init(|| {
        regex::Regex::new(r"\b\d{1,2}/\d{1,2}(?:/\d{2,4})?\b").unwrap()
    });
    let num = NUM.get_or_init(|| regex::Regex::new(r"\b\d{3,}\b").unwrap());

    let t = email.replace_all(text, "<EMAIL>");
    let t = money.replace_all(&t, "<MONEY>");
    let t = phone.replace_all(&t, "<PHONE>");
    let t = time.replace_all(&t, "<TIME>");
    let t = date.replace_all(&t, "<DATE>");
    let t = num.replace_all(&t, "<NUM>");
    t.chars().take(300).collect()
}

// ------------------------------------------------------- json path helpers

pub fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        // An absent layer (a bundle with no `defaults:`) is a no-op, not a
        // wipe: merging Null used to replace the whole values object, which
        // silently dropped every core and locale default under it.
        (_, Value::Null) => {}
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(bv) if bv.is_object() && v.is_object() => deep_merge(bv, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Reads a dot-separated path without creating anything along the way.
fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// Whether `candidate` can stand where `existing` stands: containers only
/// yield to containers of the same kind. Scalars may replace scalars freely,
/// and a path nothing occupies yet accepts anything.
fn shape_compatible(existing: Option<&Value>, candidate: &Value) -> bool {
    match existing {
        Some(e) if e.is_object() => candidate.is_object(),
        Some(e) if e.is_array() => candidate.is_array(),
        _ => true,
    }
}

pub fn set_path(root: &mut Value, path: &str, val: Value) {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    let mut cur = root;
    for p in &parts[..parts.len() - 1] {
        if !cur.is_object() {
            *cur = json!({});
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(p.to_string())
            .or_insert(json!({}));
    }
    if !cur.is_object() {
        *cur = json!({});
    }
    cur.as_object_mut()
        .unwrap()
        .insert(parts[parts.len() - 1].to_string(), val);
}

// --------------------------------------------------------- schema layers

fn load_yaml(path: &Path) -> Result<Value> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
    Ok(serde_yaml::from_str(&s)?)
}

/// Resolves a vertical's bundle directory, refusing to leave the schemas tree.
///
/// This is the single chokepoint both `compose` (read) and `promote` (write)
/// route through, which is why the check lives here rather than at each caller:
/// `promote` takes the vertical straight from an admin request body, and a
/// value like `../../..` would otherwise write `bundle.yml` and append to
/// `CHANGELOG.md` outside the schemas directory entirely. An unrecognised name
/// falls back to `generic` rather than erroring, matching how `set_bundle`
/// already treats one.
pub fn bundle_dir(schemas_dir: &Path, vertical: &str) -> PathBuf {
    let safe = is_safe_vertical(vertical) && available_bundles(schemas_dir).iter().any(|b| b == vertical);
    if !safe {
        tracing::warn!(vertical, "unknown or unsafe vertical — falling back to generic");
    }
    schemas_dir
        .join("bundles")
        .join(if safe { vertical } else { "generic" })
}

/// A bundle name is one path component of ordinary identifier characters.
/// Anything that could traverse, absolute-path, or hide a separator is out.
fn is_safe_vertical(vertical: &str) -> bool {
    !vertical.is_empty()
        && vertical.len() <= 64
        && vertical
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Pins `business_id` to the vertical bundle `want` at its current version
/// (`generic` when no such bundle exists). Returns the pin and the bundle.
pub async fn pin_bundle(db: &sqlx::SqlitePool, schemas_dir: &Path, business_id: uuid::Uuid, want: &str) -> anyhow::Result<(String, Value)> {
    let want = want.trim().to_lowercase();
    let vertical = if available_bundles(schemas_dir).contains(&want) { want } else { "generic".to_string() };
    let bundle: Value = serde_yaml::from_str(&std::fs::read_to_string(bundle_dir(schemas_dir, &vertical).join("bundle.yml"))?)?;
    let pin = format!("{vertical}@{}", bundle["version"].as_i64().unwrap_or(1));
    sqlx::query("UPDATE businesses SET bundle = $1 WHERE id = $2").bind(&pin).bind(business_id).execute(db).await?;
    Ok((pin, bundle))
}

pub fn available_bundles(schemas_dir: &Path) -> Vec<String> {
    std::fs::read_dir(schemas_dir.join("bundles"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().join("bundle.yml").exists())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct Trial {
    pub id: String,
    pub field_path: String,
    pub value: Value,
}

/// The composed live schema for one business:
/// core ⊕ vertical bundle ⊕ client patch ⊕ candidates on trial.
pub struct Composed {
    /// Full doc handed to the agents: name/industry/bundle/fields/values/_trials.
    pub doc: Value,
    /// Just the value layer — what the operational tools read.
    pub values: Value,
    pub trials: Vec<Trial>,
    pub bundle_pin: String,
}

pub async fn compose(db: &SqlitePool, schemas_dir: &Path, business_id: Uuid) -> Result<Composed> {
    // Lazy trial bookkeeping first so a decided candidate never mounts again.
    decide_pending(db, business_id).await?;

    let row: (String, String, Value, bool, String, String) = sqlx::query_as(
        "SELECT name, industry, schema_config, onboarded, bundle, country FROM businesses WHERE id = $1",
    )
    .bind(business_id)
    .fetch_one(db)
    .await?;
    let (name, industry, patch, onboarded, bundle_pin, country) = row;

    let core = load_yaml(&schemas_dir.join("core").join("core.yml"))?;
    let vertical = bundle_pin.split('@').next().unwrap_or("generic").to_string();
    let bundle = load_yaml(&bundle_dir(schemas_dir, &vertical).join("bundle.yml"))
        .unwrap_or_else(|_| json!({"bundle": vertical, "version": 0}));

    let mut values = core["defaults"].clone();
    if values.is_null() {
        values = json!({});
    }
    // Locale layer: the country picked at registration sets language,
    // currency, timezone and payment rails. Below bundle and patch so the
    // owner (and a vertical) can override every one of them.
    deep_merge(&mut values, &crate::locale::profile(&country).defaults());
    deep_merge(&mut values, &bundle["defaults"]);
    // Packs bought on the yaya market (`mounted_bundles`): their defaults sit
    // above the vertical's and below the owner's own answers.
    let packs: Vec<(String, String, String)> = sqlx::query_as("SELECT listing, title, doc FROM mounted_bundles ORDER BY mounted_at")
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let packs: Vec<(String, String, Value)> = packs
        .into_iter()
        .filter_map(|(l, t, d)| serde_json::from_str::<Value>(&d).ok().filter(|d| d.is_object()).map(|d| (l, t, d)))
        .collect();
    for (_, _, d) in &packs {
        if d["defaults"].is_object() {
            deep_merge(&mut values, &d["defaults"]);
        }
    }
    deep_merge(&mut values, &patch);

    let mut fields = core["fields"].clone();
    if fields.is_null() {
        fields = json!({});
    }
    deep_merge(&mut fields, &bundle["fields"]);
    for (_, _, d) in &packs {
        if d["fields"].is_object() {
            deep_merge(&mut fields, &d["fields"]);
        }
    }
    // The pack's questions the owner has not answered yet — the manager
    // agent offers to go through them; that is the "ultra-personalised" bit.
    let market_bundles: Vec<Value> = packs
        .iter()
        .map(|(listing, title, d)| {
            let pending: Vec<Value> = d["fields"]
                .as_object()
                .map(|fs| {
                    fs.iter()
                        .filter(|(k, f)| f["ask_in_onboarding"].as_bool() == Some(true) && get_path(&values, k).is_none_or(|v| v.is_null()))
                        .map(|(k, f)| json!({"field": k, "question_es": f["question_es"], "type": f["type"]}))
                        .collect()
                })
                .unwrap_or_default();
            json!({"listing": listing, "title": title, "skill": d["skill"], "pending": pending})
        })
        .collect();
    let bundle_tools: Value = match bundle["tools"].as_array() {
        Some(base) => {
            let mut all: Vec<Value> = base.clone();
            for (_, _, d) in &packs {
                for t in d["tools"].as_array().into_iter().flatten() {
                    if !all.contains(t) {
                        all.push(t.clone());
                    }
                }
            }
            Value::Array(all)
        }
        None => bundle["tools"].clone(),
    };

    // Mount candidates on trial. The agent uses them like real values; they
    // are not in the patch, so unwinding = simply not mounting next time.
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT id, field_path, value FROM candidates \
         WHERE business_id = $1 AND status = 'on_trial'",
    )
    .bind(business_id)
    .fetch_all(db)
    .await?;
    let mut trials = Vec::new();
    let mut trial_map = json!({});
    for (id, field_path, value) in rows {
        // A candidate may refine a field, never change its shape. An owner's
        // free-text gap answer ("martes a las 5") once mounted a bare string
        // over the businessHours map: scheduling read every day as closed and
        // the agent recited the string as the business's entire hours. A
        // shape-conflicting candidate stays in the trial table for the
        // curator, but it never composes — and with zero uses it can never
        // graduate into the patch either.
        if !shape_compatible(get_path(&values, &field_path), &value) {
            tracing::warn!(candidate = %id, field = %field_path,
                "candidate shape conflicts with existing value — not mounting");
            continue;
        }
        set_path(&mut values, &field_path, value.clone());
        set_path(&mut trial_map, &field_path, json!(id));
        trials.push(Trial { id, field_path, value });
    }

    // D15: the vertical's operating knowledge (skill.md) and the owner's UI —
    // the bundle's template unless the agent designed one (patch `ui`).
    let skill = std::fs::read_to_string(bundle_dir(schemas_dir, &vertical).join("skill.md"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let kind = values["businessKind"].as_str().unwrap_or("both").to_string();
    let ui = crate::ui::compose(&bundle["ui"], &patch["ui"], &kind);
    let ui_template = crate::ui::template(&bundle["ui"], &kind);

    let doc = json!({
        "name": name,
        "industry": industry,
        "bundle": bundle_pin,
        "onboarded": onboarded,
        "fields": fields,
        "values": values,
        "_trials": trial_map,
        "marketBundles": market_bundles,
        // The vertical (plus bought packs) decides which non-core tools its agents get.
        "_bundleTools": bundle_tools,
        "_skill": skill,
        "_ui": ui,
        "_uiTemplate": ui_template,
        "_uiDesigned": patch["ui"].is_object(),
    });
    Ok(Composed { doc, values, trials, bundle_pin })
}

// ------------------------------------------------------------- gap events

#[allow(clippy::too_many_arguments)]
pub async fn emit_gap(
    db: &SqlitePool,
    business_id: Uuid,
    bundle_pin: &str,
    session: &str,
    turn: i32,
    kind: &str,
    field_path: Option<&str>,
    utterance: &str,
    fallback: &str,
    message_id: Option<Uuid>,
) -> Result<String> {
    let id = gen_id("gap_evt");
    let kind = match kind {
        "missing_field" | "missing_value" | "conflicting_value" | "unsupported_intent" => kind,
        _ => "missing_field",
    };
    sqlx::query(
        "INSERT INTO gap_events \
           (id, business_id, bundle, session, turn, kind, field_path, utterance_redacted, agent_fallback, message_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(&id)
    .bind(business_id)
    .bind(bundle_pin)
    .bind(session)
    .bind(turn)
    .bind(kind)
    .bind(field_path)
    .bind(redact(utterance))
    .bind(fallback)
    .bind(message_id)
    .execute(db)
    .await?;
    Ok(id)
}

// ---------------------------------------------------------- trial evidence

async fn record_use(db: &SqlitePool, cand_id: &str, session: &str, outcome: &str) -> Result<()> {
    // One row per (candidate, session, outcome); repeats add no evidence.
    sqlx::query(
        "INSERT INTO candidate_uses (id, candidate_id, session, outcome) \
         SELECT $4, $1, $2, $3 WHERE NOT EXISTS \
           (SELECT 1 FROM candidate_uses WHERE candidate_id=$1 AND session=$2 AND outcome=$3)",
    )
    .bind(cand_id)
    .bind(session)
    .bind(outcome)
    .bind(Uuid::new_v4())
    .execute(db)
    .await?;
    Ok(())
}

/// A repeat gap on a field whose candidate is mounted = the value confused
/// the customer. Counted as trial evidence against the candidate.
pub async fn record_confusion(
    db: &SqlitePool,
    trials: &[Trial],
    session: &str,
    gap_field: Option<&str>,
) -> Result<()> {
    let Some(field) = gap_field else { return Ok(()) };
    for t in trials {
        if t.field_path == field || field.starts_with(&format!("{}.", t.field_path)) {
            record_use(db, &t.id, session, "confused").await?;
        }
    }
    Ok(())
}

fn scalar_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.len() >= 2 => out.push(s.to_lowercase()),
        // Single-digit needles are dropped: a candidate value of 5 substring-
        // matched "a las 15:00" and manufactured "resolved" evidence, and
        // three noisy sessions are enough to graduate a candidate for good.
        Value::Number(n) => {
            let s = n.to_string();
            if s.len() >= 2 {
                out.push(s);
            }
        }
        Value::Object(o) => o.values().for_each(|v| scalar_strings(v, out)),
        Value::Array(a) => a.iter().for_each(|v| scalar_strings(v, out)),
        _ => {}
    }
}

/// Whole-word occurrence: `needle` must not sit inside a longer run of
/// letters/digits ("10" inside "2100" is not a use of the value 10). `:` also
/// counts as a word character — booking chats are full of times, and "10"
/// inside "10:30" is a clock, not the deposit.
fn word_hit(haystack: &str, needle: &str) -> bool {
    let boundary = |c: char| !c.is_alphanumeric() && c != ':';
    haystack.match_indices(needle).any(|(i, m)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + m.len()..].chars().next();
        before.map_or(true, boundary) && after.map_or(true, boundary)
    })
}

/// After a customer turn: if a mounted candidate's value (or its field's
/// topic) shows up in the exchange, that session used the candidate.
pub async fn record_trial_uses(
    db: &SqlitePool,
    trials: &[Trial],
    session: &str,
    customer_msg: &str,
    reply: &str,
) -> Result<()> {
    let reply_l = reply.to_lowercase();
    let msg_l = customer_msg.to_lowercase();
    for t in trials {
        let mut needles = Vec::new();
        scalar_strings(&t.value, &mut needles);
        let value_hit = needles.iter().any(|n| word_hit(&reply_l, n));
        let leaf = t
            .field_path
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let topic_hit = leaf.len() >= 4 && (word_hit(&msg_l, &leaf) || word_hit(&reply_l, &leaf));
        if value_hit || topic_hit {
            record_use(db, &t.id, session, "resolved").await?;
        }
    }
    Ok(())
}

// ------------------------------------------------------ trial adjudication

/// Walks every on-trial candidate for the business and applies its own
/// graduate_if / unwind_if. Pure counting — no ML.
pub async fn decide_pending(db: &SqlitePool, business_id: Uuid) -> Result<()> {
    let cands: Vec<(String, String, String, Value, Value, i32, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT id, origin_gap, field_path, value, trial, owner_corrections, created_at \
             FROM candidates WHERE business_id = $1 AND status = 'on_trial'",
        )
        .bind(business_id)
        .fetch_all(db)
        .await?;

    for (id, origin_gap, field_path, value, trial, corrections, created_at) in cands {
        let uses: Vec<(String, String)> = sqlx::query_as(
            "SELECT session, outcome FROM candidate_uses WHERE candidate_id = $1",
        )
        .bind(&id)
        .fetch_all(db)
        .await?;

        // Session outcome = worst signal seen in that session.
        let mut sessions: std::collections::HashMap<String, i32> = Default::default();
        let mut confusion_signals = 0;
        for (sess, outcome) in &uses {
            let rank = match outcome.as_str() {
                "confused" => 2,
                "abandoned" => 1,
                _ => 0,
            };
            if outcome == "confused" {
                confusion_signals += 1;
            }
            let e = sessions.entry(sess.clone()).or_insert(0);
            *e = (*e).max(rank);
        }
        let n = sessions.len() as f64;
        let resolved = sessions.values().filter(|r| **r == 0).count() as f64;
        let resolved_rate = if n > 0.0 { resolved / n } else { 0.0 };

        let window_days = trial["window_days"].as_i64().unwrap_or(7);
        let min_uses = trial["min_uses"].as_i64().unwrap_or(3) as f64;
        let grad_rate = trial["graduate_if"]["resolved_rate_gte"].as_f64().unwrap_or(0.8);
        let grad_corr = trial["graduate_if"]["owner_corrections_lte"].as_i64().unwrap_or(0);
        let un_corr = trial["unwind_if"]["owner_corrections_gte"].as_i64().unwrap_or(1);
        let un_conf = trial["unwind_if"]["confusion_signals_gte"].as_i64().unwrap_or(2);
        let expired_window = Utc::now() > created_at + chrono::Duration::days(window_days);

        let decision = if corrections as i64 >= un_corr || confusion_signals >= un_conf {
            Some(("unwound", format!("corrections={corrections} confusions={confusion_signals}")))
        } else if n >= min_uses && resolved_rate >= grad_rate && (corrections as i64) <= grad_corr {
            Some(("graduated", format!("resolved_rate={resolved_rate:.2} over {n} sessions")))
        } else if expired_window {
            if n < min_uses {
                Some(("expired", format!("window over with only {n} uses")))
            } else if resolved_rate >= grad_rate {
                Some(("graduated", format!("window over, resolved_rate={resolved_rate:.2}")))
            } else {
                Some(("unwound", format!("window over, resolved_rate={resolved_rate:.2}")))
            }
        } else {
            None
        };

        if let Some((status, note)) = decision {
            sqlx::query(
                "UPDATE candidates SET status=$1, decided_note=$2, decided_at=$4 WHERE id=$3",
            )
            .bind(status)
            .bind(&note)
            .bind(&id)
            .bind(Utc::now())
            .execute(db)
            .await?;
            tracing::info!(candidate = %id, field = %field_path, status, %note, "trial decided");

            if status == "graduated" {
                // Graduation is the only path that writes the patch layer.
                let row: (Value, Value) = sqlx::query_as(
                    "SELECT schema_config, provenance FROM businesses WHERE id = $1",
                )
                .bind(business_id)
                .fetch_one(db)
                .await?;
                let (mut patch, mut prov) = row;
                set_path(&mut patch, &field_path, value.clone());
                if !prov.is_object() {
                    prov = json!({});
                }
                prov.as_object_mut()
                    .unwrap()
                    .insert(field_path.clone(), json!(origin_gap));
                sqlx::query(
                    "UPDATE businesses SET schema_config=$1, provenance=$2 WHERE id=$3",
                )
                .bind(&patch)
                .bind(&prov)
                .bind(business_id)
                .execute(db)
                .await?;
            }
        }
    }
    Ok(())
}

// -------------------------------------------------- owner answers → trials

fn default_trial() -> Value {
    json!({
        "window_days": 7,
        "min_uses": 3,
        "graduate_if": { "resolved_rate_gte": 0.8, "owner_corrections_lte": 0 },
        "unwind_if":   { "owner_corrections_gte": 1, "confusion_signals_gte": 2 }
    })
}

/// Which paths a candidate may write. Trial candidates mount straight into
/// live `values`, where a few keys are operational config rather than
/// business knowledge — a misextracted owner answer must never toggle tools
/// (`disabledTools`) or shift the clock (`timezone`). Extraction comes from
/// an LLM, so the gate is structural, not trust-based.
fn candidate_path_allowed(path: &str) -> bool {
    // Where money goes and what access a payment buys are the owner's word
    // only: the payout destination, the price of a question, the links a
    // paid order delivers, network presence. (Prices and deposits are
    // business knowledge a gap may legitimately teach.)
    const DENY: &[&str] = &[
        "disabledTools", "onboarded", "bundle", "timezone",
        "payout", "askPrice", "digital", "networkPublish",
    ];
    let first = path.split('.').next().unwrap_or("");
    path.len() <= 128
        && !DENY.contains(&first)
        && path.split('.').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('_')
                && seg
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ' ')
        })
}

fn extract_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(&text[start..=end]).ok()
}

/// Owner answered a gap: extract narrow facts, unwind contradicted trials
/// (that's a correction — evidence too), mount new candidates immediately.
pub async fn answer_gap(
    db: &SqlitePool,
    llm: &Llm,
    business_id: Uuid,
    gap_id: &str,
    answer: &str,
) -> Result<Vec<Value>> {
    let gap: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT kind, field_path, utterance_redacted FROM gap_events \
         WHERE id = $1 AND business_id = $2",
    )
    .bind(gap_id)
    .bind(business_id)
    .fetch_optional(db)
    .await?;
    let Some((kind, field_hint, utterance)) = gap else {
        return Err(anyhow!("gap not found"));
    };

    let sys = "You turn a business owner's answer into schema facts. \
        Scope rule: one candidate per exact fact the owner stated — never broader \
        than what was said; if they volunteer several facts, emit several candidates. \
        field_path is dot-separated camelCase (e.g. deliveryZones.surco, bookingDeposit, \
        pricing.corteCaballero). Reuse the suggested field_path when it fits. \
        value is the JSON value only (number for money in the business currency, string, bool, or small object). \
        SHAPE RULE: when the target field holds a map, address the ENTRY \
        (businessHours.tue = \"17-20\", never businessHours = \"martes a las 5\") — \
        a free-text value over a structured field is always wrong and will be dropped. \
        businessHours entries use keys mon..sun and \"H-H\" ranges. \
        Respond with ONLY JSON: {\"candidates\":[{\"field_path\":\"...\",\"value\":...,\"confidence\":0.0}]}";
    let user = json!({
        "gap_kind": kind,
        "suggested_field_path": field_hint,
        "customer_question_redacted": utterance,
        "owner_answer": answer,
    });
    crate::limits::charge(db, business_id, crate::limits::Meter::Llm).await?;
    let msg = llm
        .chat(
            &[
                json!({"role": "system", "content": sys}),
                json!({"role": "user", "content": user.to_string()}),
            ],
            None,
        )
        .await?;
    let parsed = msg["content"]
        .as_str()
        .and_then(extract_json)
        .unwrap_or(json!({"candidates": []}));
    let mut extracted: Vec<Value> = parsed["candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // Unmappable answer still becomes knowledge the agent can quote.
    if extracted.is_empty() {
        let fp = field_hint.clone().unwrap_or_else(|| {
            format!("notes.{}", gap_id.trim_start_matches("gap_evt_").to_lowercase())
        });
        extracted.push(json!({"field_path": fp, "value": answer, "confidence": 0.5}));
    }

    let mut created = Vec::new();
    for c in extracted.iter().take(5) {
        let Some(field_path) = c["field_path"].as_str() else { continue };
        if !candidate_path_allowed(field_path) {
            tracing::warn!(field = %field_path, "extracted candidate path denied");
            continue;
        }
        let value = c["value"].clone();
        if value.is_null() {
            continue;
        }

        // A different value for a field already on trial = owner correction:
        // count it against the old candidate, then let its own unwind_if fire.
        let olds: Vec<(String, Value)> = sqlx::query_as(
            "SELECT id, value FROM candidates \
             WHERE business_id=$1 AND field_path=$2 AND status='on_trial'",
        )
        .bind(business_id)
        .bind(field_path)
        .fetch_all(db)
        .await?;
        for (old_id, old_val) in olds {
            if old_val != value {
                sqlx::query(
                    "UPDATE candidates SET owner_corrections = owner_corrections + 1 WHERE id=$1",
                )
                .bind(&old_id)
                .execute(db)
                .await?;
            }
        }

        let id = gen_id("cand");
        sqlx::query(
            "INSERT INTO candidates \
               (id, origin_gap, business_id, field_path, value, source, scope, trial) \
             VALUES ($1,$2,$3,$4,$5,$6,'exact_field',$7)",
        )
        .bind(&id)
        .bind(gap_id)
        .bind(business_id)
        .bind(field_path)
        .bind(&value)
        .bind(json!({
            "channel": "app_owner",
            "owner_utterance": answer,
            "confidence": c["confidence"].as_f64().unwrap_or(0.7),
        }))
        .bind(default_trial())
        .execute(db)
        .await?;
        created.push(json!({"id": id, "fieldPath": field_path, "value": value}));
    }
    // Corrections may have tripped unwind thresholds — settle now.
    decide_pending(db, business_id).await?;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_names_are_single_safe_components() {
        assert!(is_safe_vertical("peluqueria"));
        assert!(is_safe_vertical("food_truck"));
        assert!(is_safe_vertical("spa-2"));

        assert!(!is_safe_vertical(""));
        assert!(!is_safe_vertical(".."));
        assert!(!is_safe_vertical("../../etc"));
        assert!(!is_safe_vertical("a/b"));
        assert!(!is_safe_vertical("a\\b"));
        assert!(!is_safe_vertical("/etc/passwd"));
        assert!(!is_safe_vertical("peluqueria/../../.."));
        assert!(!is_safe_vertical(&"x".repeat(65)));
    }

    /// AG-10: a traversing vertical must never escape the schemas tree, no
    /// matter which of the two callers reaches `bundle_dir` first.
    #[test]
    fn bundle_dir_never_escapes_the_schemas_tree() {
        let root = Path::new("/srv/agente/schemas");
        for attempt in ["../../etc", "..", "a/b", "/etc", "peluqueria/../../.."] {
            let dir = bundle_dir(root, attempt);
            assert!(
                dir.starts_with(root.join("bundles")),
                "{attempt:?} escaped to {}",
                dir.display()
            );
            assert_eq!(dir, root.join("bundles").join("generic"));
        }
    }

    #[test]
    fn redaction_scrubs_what_the_model_might_miss() {
        let out = redact("Pago S/ 50 al 987654321 el 12/03 a las 14:30");
        assert!(!out.contains("50"), "money survived: {out}");
        assert!(!out.contains("987654321"), "phone survived: {out}");
        assert!(out.contains("<MONEY>") && out.contains("<PHONE>"));
    }

    #[test]
    fn shape_guard_blocks_free_text_over_maps() {
        // The prod incident: string candidate at a map-valued field.
        let values = json!({"businessHours": {"mon": "9-20"}});
        let existing = get_path(&values, "businessHours");
        assert!(!shape_compatible(existing, &json!("martes a las 5")));
        assert!(shape_compatible(existing, &json!({"tue": "17-20"})));
        // Entries inside the map are scalars and may change freely.
        assert!(shape_compatible(get_path(&values, "businessHours.mon"), &json!("10-18")));
        // Unoccupied paths accept anything.
        assert!(shape_compatible(get_path(&values, "deliveryZones.surco"), &json!(15)));
    }

    #[test]
    fn trial_evidence_needs_whole_words() {
        // The noise this kills: single digits and digits inside times/amounts.
        assert!(word_hit("el adelanto es 10 soles", "10"));
        assert!(word_hit("son s/10, se paga por yape", "10"));
        assert!(!word_hit("te espero a las 10:30", "10"), "clock time is not the deposit");
        assert!(!word_hit("cuesta 2100 soles", "10"), "digits inside longer numbers");
        assert!(!word_hit("a las 21:10 cerramos", "10"), "minute part of a time");
        assert!(word_hit("delivery a surco cuesta 8", "surco"));
        assert!(!word_hit("surcolombiano", "surco"));

        // Single-digit values produce no needles at all.
        let mut needles = Vec::new();
        scalar_strings(&json!({"deposit": 5, "zone": "ate"}), &mut needles);
        assert_eq!(needles, vec!["ate".to_string()]);
    }

    #[test]
    fn candidate_paths_cannot_touch_operational_config() {
        assert!(candidate_path_allowed("bookingDeposit"));
        assert!(candidate_path_allowed("pricing.corteCaballero"));
        assert!(candidate_path_allowed("delivery.zones.san borja"));
        assert!(candidate_path_allowed("notes.abc123"));

        assert!(!candidate_path_allowed("disabledTools"));
        assert!(!candidate_path_allowed("timezone"));
        assert!(!candidate_path_allowed("onboarded"));
        assert!(!candidate_path_allowed("bundle"));
        assert!(!candidate_path_allowed("_trials.x"), "meta layers are off limits");
        assert!(!candidate_path_allowed("pricing._x"));
        assert!(!candidate_path_allowed("pricing..corte"), "empty segments");
        assert!(!candidate_path_allowed("a.b/c"), "path-ish characters");
        assert!(!candidate_path_allowed(&"x".repeat(200)));
    }

    #[test]
    fn set_path_builds_missing_levels() {
        let mut v = json!({});
        set_path(&mut v, "pricing.corte", json!(25));
        assert_eq!(v["pricing"]["corte"], json!(25));
        // A scalar in the way is replaced rather than panicking.
        let mut v = json!({"pricing": 5});
        set_path(&mut v, "pricing.corte", json!(25));
        assert_eq!(v["pricing"]["corte"], json!(25));
    }
}

#[cfg(test)]
mod merge_tests {
    use super::deep_merge;
    use serde_json::json;

    #[test]
    fn null_layer_does_not_wipe_defaults() {
        let mut v = json!({"slotDuration": 30, "timezone": "America/Lima"});
        deep_merge(&mut v, &serde_json::Value::Null);
        assert_eq!(v["slotDuration"], 30);
        deep_merge(&mut v, &json!({"pricing": {"corte": 25}}));
        assert_eq!(v["timezone"], "America/Lima");
        assert_eq!(v["pricing"]["corte"], 25);
    }

}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::testkit::{self, Mock};

    #[test]
    fn money_routing_is_never_learned_from_a_gap() {
        // Candidates mount into live values; where customers send money and
        // what they are charged or delivered is set by the owner, never by an
        // extraction that also read the customer's own question.
        for p in ["payout", "payout.destinations", "askPrice", "digital.curso", "networkPublish"] {
            assert!(!candidate_path_allowed(p), "{p} must not be writable by a trial candidate");
        }
        assert!(candidate_path_allowed("pricing.corteCaballero"));
        assert!(candidate_path_allowed("notes.parking"));
    }

    #[test]
    fn ids_sessions_and_json_extraction() {
        let a = gen_id("gap_evt");
        assert!(a.starts_with("gap_evt_") && a.len() == 8 + 12 && a[8..].chars().all(|c| !c.is_ascii_lowercase()));
        assert_ne!(gen_id("x"), gen_id("x"));
        let s = session_id("+51 1", chrono_tz::America::Lima);
        assert!(s.starts_with("+51 1@") && s.len() == "+51 1@".len() + 8);
        assert_eq!(extract_json("Claro: {\"a\": 1} listo"), Some(json!({"a": 1})));
        assert_eq!(extract_json("nada"), None);
        assert_eq!(extract_json("{rota"), None);
    }

    #[test]
    fn redaction_covers_emails_times_dates_and_long_numbers() {
        let out = redact("escríbeme a ana@x.pe, cita 10:30 del 3/4, pedido 12345, pago 20 soles");
        for leak in ["ana@x.pe", "10:30", "3/4", "12345", "20 soles"] {
            assert!(!out.contains(leak), "{leak} in {out}");
        }
        assert!(redact(&"x".repeat(500)).chars().count() <= 300);
    }

    #[test]
    fn deep_merge_and_paths() {
        let mut a = json!({"p": {"x": 1, "y": 2}, "k": 1});
        deep_merge(&mut a, &json!({"p": {"y": 3, "z": 4}, "n": true}));
        assert_eq!(a, json!({"p": {"x": 1, "y": 3, "z": 4}, "k": 1, "n": true}));
        deep_merge(&mut a, &json!({"p": "flat"}));
        assert_eq!(a["p"], "flat", "a scalar replaces a map");
        let mut v = json!({});
        set_path(&mut v, "a.b.c", json!(5));
        assert_eq!(get_path(&v, "a.b.c"), Some(&json!(5)));
        assert_eq!(get_path(&v, "a.x"), None);
        set_path(&mut v, "a", json!("scalar"));
        set_path(&mut v, "a.b", json!(1));
        assert_eq!(v, json!({"a": {"b": 1}}), "a scalar on the way becomes a map");
        assert!(shape_compatible(None, &json!("x")));
        assert!(shape_compatible(Some(&json!("a")), &json!(3)));
    }

    #[test]
    fn bundles_on_disk() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas");
        let b = available_bundles(&dir);
        for v in ["generic", "peluqueria", "restaurante"] {
            assert!(b.contains(&v.to_string()), "{v}");
        }
        assert!(load_yaml(&bundle_dir(&dir, "generic").join("bundle.yml")).unwrap().is_object());
        assert!(load_yaml(&dir.join("nope.yml")).is_err());
    }

    #[test]
    fn word_hits_are_whole_words_and_not_clocks() {
        assert!(word_hit("el deposito es 10 soles", "10"));
        assert!(!word_hit("a las 10:30", "10"));
        assert!(!word_hit("2100", "10"));
        let mut out = vec![];
        scalar_strings(&json!({"a": "Surco", "b": [5, 15], "c": "x"}), &mut out);
        assert_eq!(out, vec!["surco", "15"]);
    }

    #[tokio::test]
    async fn compose_layers_core_locale_bundle_patch_and_trials() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        sqlx::query("UPDATE businesses SET country = 'BR', schema_config = '{\"pricing\": {\"corte\": 40}}' WHERE id = $1").bind(b).execute(&s.db).await.unwrap();
        let c = compose(&s.db, &s.schemas_dir, b).await.unwrap();
        assert_eq!(c.values["currency"], "BRL", "locale layer");
        assert_eq!(c.values["pricing"]["corte"], 40, "owner patch");
        assert_eq!(c.doc["name"], "Tito");
        assert!(c.doc["fields"].is_object());
        assert!(c.trials.is_empty());
        // A candidate on trial is visible in values, flagged in _trials.
        let gap = emit_gap(&s.db, b, "generic@1", "s", 1, "weird_kind", Some("pricing.barba"), "¿cuánto la barba? 987654321", "asked_owner", None).await.unwrap();
        let (kind, utt): (String, String) = sqlx::query_as("SELECT kind, utterance_redacted FROM gap_events WHERE id = $1").bind(&gap).fetch_one(&s.db).await.unwrap();
        assert_eq!(kind, "missing_field", "unknown kinds are normalised");
        assert!(!utt.contains("987654321"));
        sqlx::query("INSERT INTO candidates (id, origin_gap, business_id, field_path, value, source, trial) VALUES ('cand_1', $1, $2, 'pricing.barba', '20', '{}', $3)")
            .bind(&gap).bind(b).bind(default_trial()).execute(&s.db).await.unwrap();
        let c = compose(&s.db, &s.schemas_dir, b).await.unwrap();
        assert_eq!(c.values["pricing"]["barba"], 20);
        assert_eq!(c.trials.len(), 1);
    }

    async fn candidate(s: &crate::AppState, b: Uuid, gap: &str, id: &str, path: &str, value: Value, age_days: i64) {
        sqlx::query("INSERT INTO candidates (id, origin_gap, business_id, field_path, value, source, trial, created_at) VALUES ($1,$2,$3,$4,$5,'{}',$6,$7)")
            .bind(id).bind(gap).bind(b).bind(path).bind(value).bind(default_trial()).bind(crate::db::ago(chrono::Duration::days(age_days)))
            .execute(&s.db).await.unwrap();
    }

    async fn status(s: &crate::AppState, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM candidates WHERE id = $1").bind(id).fetch_one(&s.db).await.unwrap()
    }

    #[tokio::test]
    async fn trials_graduate_unwind_or_expire_on_evidence() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let gap = emit_gap(&s.db, b, "g", "s", 1, "missing_field", None, "x", "f", None).await.unwrap();
        candidate(&s, b, &gap, "good", "pricing.barba", json!(20), 0).await;
        candidate(&s, b, &gap, "bad", "pricing.tinte", json!(80), 0).await;
        candidate(&s, b, &gap, "old", "notes.x", json!("y"), 30).await;
        let t = |id: &str, p: &str, v: Value| Trial { id: id.into(), field_path: p.into(), value: v };
        for sess in ["s1", "s2", "s3"] {
            record_trial_uses(&s.db, &[t("good", "pricing.barba", json!(20))], sess, "¿la barba?", "la barba cuesta 20").await.unwrap();
            record_trial_uses(&s.db, &[t("good", "pricing.barba", json!(20))], sess, "x", "la barba cuesta 20").await.unwrap(); // no double count
        }
        record_confusion(&s.db, &[t("bad", "pricing.tinte", json!(80))], "s1", Some("pricing.tinte")).await.unwrap();
        record_confusion(&s.db, &[t("bad", "pricing.tinte", json!(80))], "s2", Some("pricing.tinte.color")).await.unwrap();
        record_confusion(&s.db, &[t("bad", "pricing.tinte", json!(80))], "s3", None).await.unwrap();
        decide_pending(&s.db, b).await.unwrap();
        assert_eq!(status(&s, "good").await, "graduated");
        assert_eq!(status(&s, "bad").await, "unwound");
        assert_eq!(status(&s, "old").await, "expired");
        let (patch, prov): (Value, Value) = sqlx::query_as("SELECT schema_config, provenance FROM businesses WHERE id = $1").bind(b).fetch_one(&s.db).await.unwrap();
        assert_eq!(patch["pricing"]["barba"], 20, "graduation writes the patch");
        assert!(patch["pricing"].get("tinte").is_none());
        assert_eq!(prov["pricing.barba"], json!(gap));
    }

    #[tokio::test]
    async fn owner_answers_become_candidates_and_corrections() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let b = testkit::business(&s.db).await;
        let gap = emit_gap(&s.db, b, "g", "s", 1, "missing_field", Some("pricing.barba"), "¿la barba?", "asked_owner", None).await.unwrap();
        m.say(r#"{"candidates": [{"field_path": "pricing.barba", "value": 20, "confidence": 0.9}, {"field_path": "disabledTools", "value": ["book_appointment"]}, {"field_path": "payout.holder", "value": "Estafador"}]}"#);
        let made = answer_gap(&s.db, &s.llm, b, &gap, "la barba 20 soles").await.unwrap();
        assert_eq!(made.len(), 1, "operational and money paths are dropped: {made:?}");
        // A different value for the same field corrects (and unwinds) the old one.
        m.say(r#"{"candidates": [{"field_path": "pricing.barba", "value": 25}]}"#);
        answer_gap(&s.db, &s.llm, b, &gap, "no, 25").await.unwrap();
        let old = made[0]["id"].as_str().unwrap();
        assert_eq!(status(&s, old).await, "unwound");
        // Nothing extractable: the answer itself is kept as a note.
        let gap2 = emit_gap(&s.db, b, "g", "s", 1, "unsupported_intent", None, "¿hay parqueo?", "f", None).await.unwrap();
        m.say("no entendí");
        let made = answer_gap(&s.db, &s.llm, b, &gap2, "sí, en la esquina").await.unwrap();
        assert!(made[0]["fieldPath"].as_str().unwrap().starts_with("notes."));
        assert!(answer_gap(&s.db, &s.llm, b, "gap_evt_NOPE", "x").await.is_err());
    }
}
