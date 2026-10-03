//! The consumption meter — an account pays for the compute it actually uses.
//!
//! D19 moves the charge from *outcomes* (a booking, a sale) to *consumption*
//! (prompt and completion tokens, seconds of speech recognised, characters
//! spoken, images looked at). The subscription is capacity — Pro and Max buy
//! a daily call allowance — and this is what an account spends once it wants
//! more than the plan carries, on any plan, including free.
//!
//! **The unit does not change.** The ledger stays in minor units of the
//! closed loop (a USD cent today), drawn from the same three buckets in the
//! same order by [`prepaid::draw_in`]. What a unit is *worth* lives entirely
//! in the `METER_*` rates below, so re-pegging later is an env change, never
//! a migration over money that has already moved.
//!
//! **Rounding is the whole problem.** One chat turn costs a small fraction
//! of a cent; rounding each turn up would overcharge by orders of magnitude,
//! and rounding down would charge nothing at all, forever. So consumption
//! accrues in micro-USD in `prepaid_meter` and crosses into the ledger only
//! once it has accumulated a whole cent. The remainder is carried, so the
//! thousandth turn costs exactly what the first one did.
//!
//! **Metering never fails a request.** [`guard`] runs *before* the upstream
//! call and refuses when the balance is already past the grace floor;
//! [`record`] runs after and only ever writes. A metering failure is our
//! problem, not a customer-facing error — the turn the customer is waiting
//! on always completes.

use axum::{http::StatusCode, Json};
use chrono::Utc;
use serde_json::{json, Value};

use crate::{accounts, internal, prepaid, App};

type E = (StatusCode, Json<Value>);

/// Micro-USD — millionths of a dollar — per whole minor unit of the ledger.
/// Ten thousand of them make one cent.
pub const MICROS_PER_CENT: i64 = 10_000;

fn rate(var: &str, default: i64) -> i64 {
    std::env::var(var).ok().and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n >= 0).unwrap_or(default)
}

/// Percent added to every metered amount — one knob to move margin without
/// touching the individual rates. `METER_MARKUP_PERCENT=0` bills at cost.
fn markup() -> i64 {
    std::env::var("METER_MARKUP_PERCENT").ok().and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n >= 0).unwrap_or(0)
}

/// Is the meter live? Off by default so this ships dark and is turned on
/// deliberately, per environment.
pub fn enabled() -> bool {
    matches!(std::env::var("METER_ENABLED").as_deref(), Ok("1") | Ok("true") | Ok("TRUE"))
}

/// One unit of consumption, in the terms the upstream reports it.
#[derive(Debug, Clone, Copy)]
pub enum Use {
    /// A chat turn: tokens in, tokens out.
    Chat { prompt: i64, completion: i64 },
    /// Speech recognised, in seconds of audio. A passthrough only learns
    /// the true duration when the upstream reports one, so [`seconds_of`]
    /// estimates from the upload when it does not.
    Transcribe { seconds: f64 },
    /// Speech rendered, in characters of input text.
    Speak { chars: i64 },
    /// A vision turn: images looked at, plus the tokens around them.
    Vision { images: i64, prompt: i64, completion: i64 },
}

impl Use {
    /// What this consumption costs, in micro-USD, before the markup.
    ///
    /// The token rates are per *thousand* tokens because that is how every
    /// upstream quotes them; dividing at the end keeps the arithmetic in
    /// integers and the rounding in one place.
    fn micros_at_cost(&self) -> i64 {
        match *self {
            Use::Chat { prompt, completion } => {
                (prompt.max(0) * rate("METER_CHAT_IN_UUSD_PER_1K", 300)
                    + completion.max(0) * rate("METER_CHAT_OUT_UUSD_PER_1K", 1_500))
                    / 1_000
            }
            Use::Transcribe { seconds } => {
                let s = if seconds.is_finite() && seconds > 0.0 { seconds } else { 0.0 };
                (s * rate("METER_STT_UUSD_PER_SEC", 100) as f64).round() as i64
            }
            Use::Speak { chars } => chars.max(0) * rate("METER_TTS_UUSD_PER_1K_CHARS", 15_000) / 1_000,
            Use::Vision { images, prompt, completion } => {
                images.max(0) * rate("METER_VISION_UUSD_PER_IMAGE", 2_000)
                    + (prompt.max(0) * rate("METER_CHAT_IN_UUSD_PER_1K", 300)
                        + completion.max(0) * rate("METER_CHAT_OUT_UUSD_PER_1K", 1_500))
                        / 1_000
            }
        }
    }

    /// What the account is charged, in micro-USD, markup included.
    pub fn micros(&self) -> i64 {
        let at_cost = self.micros_at_cost();
        at_cost + at_cost * markup() / 100
    }

    /// The word that appears on the statement line.
    pub fn kind(&self) -> &'static str {
        match self {
            Use::Chat { .. } => "chat",
            Use::Transcribe { .. } => "voice_in",
            Use::Speak { .. } => "voice_out",
            Use::Vision { .. } => "vision",
        }
    }

    /// What it was made of, kept beside the debit so a statement can say
    /// "1 240 tokens" rather than only "1 cent".
    pub fn detail(&self) -> Value {
        match *self {
            Use::Chat { prompt, completion } => json!({"promptTokens": prompt, "completionTokens": completion}),
            Use::Transcribe { seconds } => json!({"seconds": (seconds * 100.0).round() / 100.0}),
            Use::Speak { chars } => json!({"chars": chars}),
            Use::Vision { images, prompt, completion } => json!({"images": images, "promptTokens": prompt, "completionTokens": completion}),
        }
    }
}

/// Reads `usage` off an OpenAI-shaped response body. Absent or malformed
/// usage means we charge nothing for that call: guessing what a turn cost
/// is worse than missing it, and the upstream invoice is the backstop.
pub fn usage_of(payload: &Value) -> Option<(i64, i64)> {
    let u = payload.get("usage")?;
    let p = u.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0);
    let c = u.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0);
    (p > 0 || c > 0).then_some((p, c))
}

/// Seconds of audio in a transcription call: the upstream's own `duration`
/// when it reports one (`verbose_json`), else the upload's size at an
/// assumed bitrate. The estimate is deliberately on the low side — under-
/// charging on a format we guessed wrong about beats over-charging.
pub fn seconds_of(payload: &Value, uploaded_bytes: usize) -> f64 {
    if let Some(d) = payload.get("duration").and_then(Value::as_f64) {
        if d.is_finite() && d > 0.0 {
            return d;
        }
    }
    let per_sec = rate("METER_STT_BYTES_PER_SEC", 2_000).max(1) as f64;
    (uploaded_bytes as f64 / per_sec).max(0.0)
}

/// Where an account stands, for the headers and for `/v1/credits`.
pub struct Standing {
    pub account: String,
    pub balance: i64,
}

/// The account behind an agent, or `None` for a guest — a phone with no
/// account has nothing to charge and is governed by the free daily cap
/// alone.
async fn account_of(app: &App, agent: &str) -> Result<Option<String>, E> {
    accounts::account_of_agent(app, agent).await
}

/// Refuses the call when the account is already spent past the grace floor.
///
/// Called *before* the upstream, so a spent account stops consuming rather
/// than sinking further; the phone reads the 402 and hands the conversation
/// to the owner (`prepaid::notify_handoff` sends the one WhatsApp about it).
/// A guest, or an account still inside grace, passes through.
pub async fn guard(app: &App, agent: &str) -> Result<Option<Standing>, E> {
    if !enabled() {
        return Ok(None);
    }
    let Some(account) = account_of(app, agent).await? else { return Ok(None) };
    let balance = prepaid::balance(&app.db, &account).await?;
    // One rule for "spent": the same one every screen shows (state_of).
    if refuses(balance) {
        prepaid::notify_handoff(app, &account).await;
        return Err((
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({"error": {
                "message": "saldo agotado",
                "type": "balance",
                "balance": balance,
                "grace": prepaid::GRACE,
                "topup": prepaid::topup_url(),
            }})),
        ));
    }
    Ok(Some(Standing { account, balance }))
}

/// Past the grace floor: the agent hands off. Exactly at the floor is still
/// grace, as `prepaid::state_of` (and so the app's banner) says.
pub fn refuses(balance: i64) -> bool {
    prepaid::state_of(balance) == "manual"
}

/// Meters one unit of consumption against the account.
///
/// Accrues in micro-USD and flushes whole cents into the ledger through the
/// same lot drawer every other debit uses, all inside one `BEGIN IMMEDIATE`
/// so two phones on one account cannot both spend the last lot. Returns the
/// balance after, when anything was actually flushed.
pub async fn record(app: &App, agent: &str, account: &str, used: Use) -> Result<Option<i64>, E> {
    let micros = used.micros();
    if micros <= 0 {
        return Ok(None);
    }
    let now = Utc::now();
    // Read before the write transaction: a pool read while holding it needs
    // a second connection, and under load every connection may be parked in
    // BEGIN IMMEDIATE waiting for this one — a stall until busy_timeout.
    let cfg = prepaid::country_config_for(&app.db, account).await;
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let (owed,): (i64,) = sqlx::query_as(
        "INSERT INTO prepaid_meter (account, micros, micros_life, updated_at) VALUES ($1, $2, $2, $3) \
         ON CONFLICT (account) DO UPDATE SET micros = micros + $2, micros_life = micros_life + $2, updated_at = $3 \
         RETURNING micros",
    )
    .bind(account).bind(micros).bind(prepaid::stamp(now))
    .fetch_one(&mut *tx).await.map_err(internal)?;

    let cents = owed / MICROS_PER_CENT;
    let detail = used.detail();
    sqlx::query(
        "INSERT INTO prepaid_meter_log (id, account, agent, kind, micros, cents, detail, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(prepaid::id()).bind(account).bind(agent).bind(used.kind()).bind(micros).bind(cents)
    .bind(detail.to_string()).bind(prepaid::stamp(now))
    .execute(&mut *tx).await.map_err(internal)?;

    if cents == 0 {
        tx.commit().await.map_err(internal)?;
        return Ok(None);
    }
    sqlx::query("UPDATE prepaid_meter SET micros = micros - $2 WHERE account = $1")
        .bind(account).bind(cents * MICROS_PER_CENT)
        .execute(&mut *tx).await.map_err(internal)?;

    prepaid::expire_due_in(&mut tx, account, now).await?;
    let before = prepaid::balance_in(&mut tx, account).await?;
    let meta = json!({"micros": micros, "detail": detail}).to_string();
    prepaid::draw_in(&mut tx, account, cents, &cfg, used.kind(), &meta, prepaid::Tag::default(), now).await?;
    let after = before - cents;
    tx.commit().await.map_err(internal)?;
    tracing::debug!(%account, %agent, kind = used.kind(), micros, cents, balance = after, "metered");
    Ok(Some(after))
}

/// Meters without letting a metering failure reach the customer's turn.
/// The turn already succeeded upstream by the time this runs; if the ledger
/// write fails, that is ours to see in the log and reconcile, not theirs to
/// read as an error.
pub async fn record_quietly(app: &App, agent: &str, account: &str, used: Use) -> Option<i64> {
    if !enabled() {
        return None;
    }
    match record(app, agent, account, used).await {
        Ok(balance) => balance,
        Err((status, body)) => {
            tracing::error!(%account, %agent, kind = used.kind(), %status, error = %body.0, "metering failed");
            None
        }
    }
}

/// Adds the balance to a response so the phone can show it without a second
/// round trip, the way `x-yaya-plan` already carries the allowance.
pub fn with_balance(mut r: axum::response::Response, balance: Option<i64>) -> axum::response::Response {
    if let Some(b) = balance {
        if let Ok(v) = b.to_string().parse() {
            r.headers_mut().insert("x-yaya-balance", v);
        }
    }
    r
}

/// The rate card, as the app and the console should show it. Derived from
/// the same env the meter charges by, so what a customer is quoted and what
/// they are charged cannot drift apart.
pub fn rates_json() -> Value {
    json!({
        "enabled": enabled(),
        "unit": "USD_CENT",
        "microsPerUnit": MICROS_PER_CENT,
        "markupPercent": markup(),
        "rates": {
            "chatInPer1kTokens": rate("METER_CHAT_IN_UUSD_PER_1K", 300),
            "chatOutPer1kTokens": rate("METER_CHAT_OUT_UUSD_PER_1K", 1_500),
            "sttPerSecond": rate("METER_STT_UUSD_PER_SEC", 100),
            "ttsPer1kChars": rate("METER_TTS_UUSD_PER_1K_CHARS", 15_000),
            "visionPerImage": rate("METER_VISION_UUSD_PER_IMAGE", 2_000),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// METER_MARKUP_PERCENT is process-wide: every test that sets it holds this.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The carry is the point: a thousand turns each costing a fraction of a
    /// cent must bill the same as one turn costing a thousand fractions.
    #[test]
    fn sub_cent_turns_accumulate_instead_of_rounding() {
        let _env = env_lock();
        std::env::set_var("METER_MARKUP_PERCENT", "0");
        let turn = Use::Chat { prompt: 1_000, completion: 200 };
        // 1000 * 300 / 1000 + 200 * 1500 / 1000 = 300 + 300 = 600 µUSD.
        assert_eq!(turn.micros(), 600);
        // Under a cent on its own; seventeen of them cross the line once.
        assert!(turn.micros() < MICROS_PER_CENT);
        let total: i64 = (0..17).map(|_| turn.micros()).sum();
        assert_eq!(total / MICROS_PER_CENT, 1);
        assert_eq!(total % MICROS_PER_CENT, 200);
    }

    #[test]
    fn markup_is_a_single_knob() {
        let _env = env_lock();
        std::env::set_var("METER_MARKUP_PERCENT", "50");
        assert_eq!(Use::Chat { prompt: 1_000, completion: 0 }.micros(), 450);
        std::env::set_var("METER_MARKUP_PERCENT", "0");
        assert_eq!(Use::Chat { prompt: 1_000, completion: 0 }.micros(), 300);
    }

    /// Missing usage charges nothing rather than guessing a turn's size.
    #[test]
    fn absent_usage_is_not_invented() {
        assert_eq!(usage_of(&json!({})), None);
        assert_eq!(usage_of(&json!({"usage": {"prompt_tokens": 0, "completion_tokens": 0}})), None);
        assert_eq!(usage_of(&json!({"usage": {"prompt_tokens": 12, "completion_tokens": 3}})), Some((12, 3)));
    }

    #[test]
    fn negative_counts_cannot_credit_an_account() {
        let _env = env_lock();
        std::env::set_var("METER_MARKUP_PERCENT", "0");
        assert_eq!(Use::Chat { prompt: -5_000, completion: -5_000 }.micros(), 0);
        assert_eq!(Use::Speak { chars: -100 }.micros(), 0);
        assert_eq!(Use::Transcribe { seconds: f64::NAN }.micros(), 0);
    }

    #[test]
    fn every_kind_prices_and_describes_itself() {
        let _env = env_lock();
        std::env::set_var("METER_MARKUP_PERCENT", "0");
        assert_eq!(Use::Transcribe { seconds: 2.5 }.micros(), 250);
        assert_eq!(Use::Speak { chars: 2_000 }.micros(), 30_000);
        assert_eq!(Use::Vision { images: 2, prompt: 1_000, completion: 0 }.micros(), 4_300);
        for (u, k) in [(Use::Chat { prompt: 1, completion: 1 }, "chat"), (Use::Transcribe { seconds: 1.0 }, "voice_in"), (Use::Speak { chars: 1 }, "voice_out"), (Use::Vision { images: 1, prompt: 0, completion: 0 }, "vision")] {
            assert_eq!(u.kind(), k);
            assert!(u.detail().is_object());
        }
        assert_eq!(seconds_of(&json!({"duration": 3.0}), 1), 3.0);
        assert_eq!(seconds_of(&json!({"duration": -1.0}), 4_000), 2.0, "no duration: estimated from the upload");
        assert!(rates_json().is_object());
        let r = with_balance(axum::response::IntoResponse::into_response("ok"), Some(42));
        assert_eq!(r.headers()["x-yaya-balance"], "42");
    }

    #[tokio::test]
    async fn fractions_carry_and_only_whole_cents_leave_the_balance() {
        let _env = env_lock();
        std::env::set_var("METER_MARKUP_PERCENT", "0");
        let app = crate::testkit::app().await;
        let kp = crate::testkit::Keypair::generate();
        crate::testkit::account_with_agent(&app, "a", "51900000001", &kp).await;
        assert!(prepaid::welcome(&app.db, "a", "51900000001").await.unwrap());
        let start = prepaid::balance(&app.db, "a").await.unwrap();
        let turn = Use::Chat { prompt: 1_000, completion: 200 }; // 600 µUSD
        for _ in 0..16 {
            assert_eq!(record(&app, "ag", "a", turn).await.unwrap(), None, "under a cent: nothing drawn yet");
        }
        assert_eq!(record(&app, "ag", "a", turn).await.unwrap(), Some(start - 1), "the 17th turn crosses one cent");
        assert_eq!(record(&app, "ag", "a", Use::Chat { prompt: 0, completion: 0 }).await.unwrap(), None);
        // Many turns at once on one account: nothing lost, nothing doubled.
        let mut tasks = Vec::new();
        for _ in 0..40 {
            let app = app.clone();
            tasks.push(tokio::spawn(async move { record(&app, "ag", "a", turn).await.unwrap() }));
        }
        for t in tasks { t.await.unwrap(); }
        let (carry, life): (i64, i64) = sqlx::query_as("SELECT micros, micros_life FROM prepaid_meter WHERE account = 'a'").fetch_one(&app.db).await.unwrap();
        assert_eq!(life, 57 * 600);
        let drawn = start - prepaid::balance(&app.db, "a").await.unwrap();
        assert_eq!(drawn * MICROS_PER_CENT + carry, life, "every micro is either drawn or carried");
        let (logs,): (i64,) = sqlx::query_as("SELECT count(*) FROM prepaid_meter_log WHERE account = 'a'").fetch_one(&app.db).await.unwrap();
        assert_eq!(logs, 57);
    }

    #[test]
    fn the_guard_and_the_banner_agree_on_the_floor() {
        assert!(!refuses(prepaid::GRACE), "exactly at the floor is grace, as the banner says");
        assert_eq!(prepaid::state_of(prepaid::GRACE), "grace");
        assert!(refuses(prepaid::GRACE - 1));
        assert!(!refuses(0) && !refuses(10_000));
    }
}

