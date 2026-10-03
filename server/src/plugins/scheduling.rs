//! Appointment tools. Bundle-gated: a vertical mounts these by listing them
//! in its bundle.yml `tools:` section.

use anyhow::anyhow;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Scheduling;

fn spec(name: &str, desc: &str, params: Value) -> Value {
    json!({"type": "function",
           "function": {"name": name, "description": desc, "parameters": params}})
}

/// Per-service duration: businesses configure slotDuration as one number OR
/// a map[service -> minutes] ("corte 30, pintado completo 120"). Unknown
/// service on a map falls back to the shortest (grid resolution).
///
/// The 5-minute floor applies to every path, including a scalar: a zero here
/// would make `check_availability`'s slot walk step by zero and never finish.
fn duration_for(values: &Value, service: Option<&str>) -> i64 {
    let field = &values["slotDuration"];
    if let Some(d) = crate::harness::by_service(field, service).and_then(Value::as_i64) {
        return d.max(5);
    }
    field
        .as_object()
        .and_then(|m| m.values().filter_map(Value::as_i64).min())
        .unwrap_or(30)
        .max(5)
}

/// How many customers can be served in parallel.
fn staff_capacity(values: &Value) -> i64 {
    values["staffCount"].as_i64().filter(|c| *c >= 1).unwrap_or(1)
}

/// Appointments overlapping [start, end) — each blocks its own duration.
async fn overlapping(
    ctx: &ToolCtx<'_>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> anyhow::Result<Vec<(Option<String>, DateTime<Utc>, i32)>> {
    // SQLite can't add minutes to an RFC 3339 text in a comparable shape, so
    // the window is widened by the longest plausible appointment (1 day) and
    // the exact end-time check happens here.
    let rows: Vec<(Option<String>, DateTime<Utc>, i32)> = sqlx::query_as(
        "SELECT specialist, starts_at, COALESCE(duration_mins, 30) FROM appointments \
         WHERE business_id = $1 AND status <> 'cancelled' \
           AND starts_at < $2 AND starts_at > $3",
    )
    .bind(ctx.business_id)
    .bind(end)
    .bind(start - Duration::days(1))
    .fetch_all(&ctx.state.db)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, s, d)| *s + Duration::minutes(*d as i64) > start)
        .collect())
}

/// Enough validation to catch typos and dodge garbage; real verification is
/// the reminder email either arriving or not.
fn looks_like_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.contains(' ')
}

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    if let Some((h, m)) = s.split_once(':') {
        NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, 0)
    } else {
        NaiveTime::from_hms_opt(s.parse().ok()?, 0, 0)
    }
}

/// A time of day as owners write it: "9", "09:30", "9h30", "9h", "9am",
/// "9:30 pm", "12pm" (noon), "12am" (midnight). "24" is end of day.
fn parse_clock(s: &str) -> Option<(u32, u32)> {
    let t = s.trim().to_lowercase().replace('.', "").replace(' ', "");
    let (t, pm, am) = if let Some(x) = t.strip_suffix("pm") { (x.to_string(), true, false) }
        else if let Some(x) = t.strip_suffix("am") { (x.to_string(), false, true) }
        else { (t, false, false) };
    let t = t.trim_end_matches("hrs").trim_end_matches("hs").to_string();
    let (h, m) = match t.split_once(|c| c == ':' || c == 'h') {
        Some((h, m)) => (h.parse::<u32>().ok()?, if m.is_empty() { 0 } else { m.parse::<u32>().ok()? }),
        None => (t.parse::<u32>().ok()?, 0),
    };
    if m > 59 || h > 24 || ((am || pm) && !(1..=12).contains(&h)) {
        return None;
    }
    let h = match (am, pm) { (true, _) if h == 12 => 0, (_, true) if h != 12 => h + 12, _ => h };
    (h < 24 || (h == 24 && m == 0)).then_some((h, m))
}

/// The open windows in one day's hours string, as (opening time, minutes
/// open). `Some(vec![])` = closed that day ("cerrado"); `None` = a string
/// this cannot read. Several ranges ("9-13, 15-19"), words ("9 a 18",
/// "18:00 hasta 01:00") and closings past midnight ("18-02") are all fine.
fn open_ranges(hours: &str) -> Option<Vec<(NaiveTime, i64)>> {
    let h = hours.trim().to_lowercase();
    if h.is_empty() || ["cerrado", "closed", "fechado", "no", "-"].contains(&h.as_str()) {
        return Some(vec![]);
    }
    let mut norm = h.replace(['–', '—'], "-");
    for sep in [" a ", " to ", " hasta ", " até ", " ate "] {
        norm = norm.replace(sep, "-");
    }
    for sep in [" y ", " and ", " e ", ";", "/", "|"] {
        norm = norm.replace(sep, ",");
    }
    let mut out = Vec::new();
    for part in norm.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-')?;
        let (oh, om) = parse_clock(a)?;
        let (ch, cm) = parse_clock(b)?;
        let open_min = (oh * 60 + om) as i64;
        let mut len = (ch * 60 + cm) as i64 - open_min;
        if len <= 0 {
            len += 24 * 60; // closes after midnight
        }
        out.push((NaiveTime::from_hms_opt(oh % 24, om, 0)?, len));
    }
    Some(out)
}

/// The day's open windows as UTC instants. `Ok(None)` when the business
/// set no hours at all (nothing to enforce); an unreadable string errors.
fn windows_on(values: &Value, date: NaiveDate, tz: chrono_tz::Tz) -> anyhow::Result<Option<Vec<(DateTime<Utc>, DateTime<Utc>)>>> {
    if values["businessHours"].as_object().is_none_or(|m| m.is_empty()) {
        return Ok(None);
    }
    let idx = date.weekday().num_days_from_monday() as usize;
    let Some(hours) = crate::harness::hours_for_day(values, idx) else { return Ok(Some(vec![])) };
    let ranges = open_ranges(&hours).ok_or_else(|| anyhow!("bad businessHours format: {hours}"))?;
    Ok(Some(ranges.into_iter().map(|(open, len)| {
        let start = crate::harness::local_to_utc(date.and_time(open), tz);
        (start, start + Duration::minutes(len))
    }).collect()))
}

/// One booking at a time per core: the conflict check and the insert must
/// not interleave, or two customers both take the last slot.
static BOOKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn check_availability(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let schema = &ctx.values;
    let date_str = args
        .get("date")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing 'date' (YYYY-MM-DD)"))?;
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")?;

    // Booking window: today .. today + maxAdvanceBookingDays (business-local).
    let tz = crate::harness::biz_tz(schema);
    let today = crate::harness::now_local(tz).date();
    let max_days = schema["maxAdvanceBookingDays"].as_i64().unwrap_or(30);
    if date < today {
        return Ok(json!({"date": date_str, "open": false, "slots": [],
                         "note": "that date is in the past"}));
    }
    if date > today + Duration::days(max_days) {
        return Ok(json!({"date": date_str, "open": false, "slots": [],
                         "note": format!("bookings only open up to {max_days} days ahead")}));
    }

    let weekday_idx = date.weekday().num_days_from_monday() as usize;
    let weekday = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][weekday_idx];
    // Tolerant lookup: the interview stores day keys in the owner's language.
    let windows = windows_on(schema, date, tz)?.unwrap_or_default();
    if windows.is_empty() {
        return Ok(json!({"date": date_str, "open": false, "slots": [],
                         "note": format!("closed on {weekday}")}));
    }
    let service = args["service"].as_str().or_else(|| args["specialty"].as_str());
    let dur = duration_for(schema, service);
    let capacity = staff_capacity(schema);

    let mut slots = Vec::new();
    let step = dur.min(30);
    let now = Utc::now();
    for (day_start, day_end) in windows {
        // Everything booked in the window, each booking with its own duration.
        let busy = overlapping(ctx, day_start, day_end).await?;
        let mut t = day_start;
        while t + Duration::minutes(dur) <= day_end {
            // Never offer a slot that has already passed.
            let in_past = t <= now;
            let t_end = t + Duration::minutes(dur);
            let clashes = busy
                .iter()
                .filter(|(_, s, d)| *s < t_end && *s + Duration::minutes(*d as i64) > t)
                .count() as i64;
            if !in_past && clashes < capacity {
                // Slot strings are business-local: they are read back to the
                // customer and echoed into book_appointment's `timestamp`.
                slots.push(t.with_timezone(&tz).format("%Y-%m-%dT%H:%M").to_string());
            }
            t += Duration::minutes(step);
        }
    }
    Ok(json!({"date": date_str, "open": true,
              "service": service, "durationMinutes": dur,
              "staffCapacity": capacity, "slots": slots}))
}

/// The next free slots over the coming `days`, business-local strings, for
/// the published card — so a client agent can see availability before it
/// even says hello. Same rules as `check_availability` (hours, capacity,
/// default duration), no conversation context.
pub async fn next_free_slots(
    db: &sqlx::SqlitePool,
    business_id: uuid::Uuid,
    values: &Value,
    days: i64,
    max: usize,
) -> Vec<String> {
    let tz = crate::harness::biz_tz(values);
    let today = crate::harness::now_local(tz).date();
    let dur = duration_for(values, None);
    let capacity = staff_capacity(values);
    let now = Utc::now();
    let mut out = Vec::new();
    for d in 0..days {
        let date = today + Duration::days(d);
        let Ok(Some(windows)) = windows_on(values, date, tz) else { continue };
        for (day_start, day_end) in windows {
        let busy: Vec<(DateTime<Utc>, i32)> = sqlx::query_as(
            "SELECT starts_at, COALESCE(duration_mins, 30) FROM appointments \
             WHERE business_id = $1 AND status <> 'cancelled' AND starts_at < $2 AND starts_at > $3",
        )
        .bind(business_id)
        .bind(day_end)
        .bind(day_start - Duration::days(1))
        .fetch_all(db)
        .await
        .unwrap_or_default();
        let mut t = day_start;
        while t + Duration::minutes(dur) <= day_end {
            let t_end = t + Duration::minutes(dur);
            let clashes = busy.iter().filter(|(s, d)| *s < t_end && *s + Duration::minutes(*d as i64) > t).count() as i64;
            if t > now && clashes < capacity {
                out.push(t.with_timezone(&tz).format("%Y-%m-%dT%H:%M").to_string());
                if out.len() >= max {
                    return out;
                }
            }
            t += Duration::minutes(dur.min(30));
        }
        }
    }
    out
}

async fn book_appointment(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let name = args["customer_name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing 'customer_name'"))?;
    // Identity comes from the conversation, not from what the model retyped:
    // the same customer must always land under one canonical phone id.
    let phone = crate::harness::canon_phone(
        ctx.peer
            .as_deref()
            .or_else(|| args["phone"].as_str())
            .ok_or_else(|| anyhow!("missing 'phone'"))?,
    );
    let ts = args["timestamp"]
        .as_str()
        .ok_or_else(|| anyhow!("missing 'timestamp' (YYYY-MM-DDTHH:MM)"))?;
    // The model passes business-local wall-clock (it echoes check_availability
    // slots); storage is the real UTC instant that wall-clock names.
    let tz = crate::harness::biz_tz(&ctx.values);
    let starts_at = crate::harness::local_to_utc(
        chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S"))?,
        tz,
    );
    let specialist = args["specialist"].as_str();
    let service = args["service"].as_str();
    // Price comes from the business's own `pricing` map, never from the tool
    // arguments: the customer is the other party to this conversation, and a
    // price they can talk the model into is a price they set for themselves.
    // An unpriced service yields None, exactly as an omitted argument used to,
    // so bookings for services with no configured price still go through.
    let price = crate::harness::price_for(&ctx.values, service);
    let dur = duration_for(&ctx.values, service);

    // Email, when the business demands it (values.requireEmail — remote/
    // virtual businesses need somewhere to send the meeting details). The
    // requirement is enforced here, not in the prompt: a prompt-only rule is
    // one persuasive customer away from an appointment nobody can reach.
    let email = args["customer_email"]
        .as_str()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    if ctx.values["requireEmail"].as_bool() == Some(true) {
        match email {
            Some(e) if looks_like_email(e) => {}
            Some(e) => {
                return Ok(json!({"status": "rejected",
                    "note": format!("'{e}' does not look like a valid email — ask again")}));
            }
            None => {
                return Ok(json!({"status": "need_email",
                    "note": "this business requires the customer's email to book — \
                             ask for it, then book with customer_email set"}));
            }
        }
    }

    // Time sanity: no past bookings, no bookings beyond the business window.
    let now = Utc::now();
    if starts_at < now - Duration::minutes(5) {
        return Ok(json!({"status": "rejected",
            "note": format!("that time is in the past (now for this business: {})",
                            crate::harness::fmt_local(now, tz, "%Y-%m-%d %H:%M"))}));
    }
    let max_days = ctx.values["maxAdvanceBookingDays"].as_i64().unwrap_or(30);
    if starts_at > now + Duration::days(max_days) {
        return Ok(json!({"status": "rejected",
            "note": format!("bookings only open up to {max_days} days ahead")}));
    }

    // Inside opening hours: the booking must fit one of the day's windows,
    // or last night's when that one runs past midnight. A prompt-only rule
    // is one persuasive customer away from a 3 a.m. booking.
    let ends_at = starts_at + Duration::minutes(dur);
    let local_date = starts_at.with_timezone(&tz).date_naive();
    let mut windows = Vec::new();
    let mut hours_set = false;
    for d in [local_date - Duration::days(1), local_date] {
        if let Some(w) = windows_on(&ctx.values, d, tz)? {
            hours_set = true;
            windows.extend(w);
        }
    }
    if hours_set && !windows.iter().any(|(a, b)| *a <= starts_at && ends_at <= *b) {
        return Ok(json!({"status": "rejected",
            "note": "outside opening hours — offer a time from check_availability"}));
    }

    // Payment is owed when the business collects upfront/yape OR configured a
    // booking deposit — but never for a free service (price 0).
    let upfront = matches!(
        ctx.values["paymentMethod"].as_str(),
        Some("upfront") | Some("yape") | Some("transfer")
    );
    let deposit_num = ctx.values["bookingDeposit"].as_f64().filter(|d| *d > 0.0);
    let deposit_configured = deposit_num.is_some()
        || ctx.values["bookingDeposit"]
            .as_object()
            .is_some_and(|o| !o.is_empty());
    let free = price == Some(0.0);
    // Where deposits are not enabled for the business's country (gateway
    // `country_config`, cached by outcomes.rs) the agent books without one:
    // the held slot is the confirmed outcome (agente/docs/CREDITS.md § 5).
    let deposits_enabled = crate::outcomes::deposits_enabled(&crate::outcomes::cached(ctx.state).await);
    let status = if !free && deposits_enabled && (upfront || deposit_configured) {
        "pending_payment"
    } else {
        "confirmed"
    };

    // Idempotency before conflicts: if THIS customer already holds a booking
    // overlapping the requested window, hand it back instead of colliding
    // with it. The model re-books after "ya pagué" exchanges, and the old
    // capacity check then counted the customer's own confirmed appointment
    // as the slot being taken — the agent told a paid-up customer their
    // hora "se acaba de ocupar".
    let _booking = BOOKING.lock().await;
    let own: Vec<(Uuid, String, bool, String, DateTime<Utc>, i32)> = sqlx::query_as(
        "SELECT id, status, paid, phone, starts_at, COALESCE(duration_mins, 30) FROM appointments \
         WHERE business_id = $1 AND status <> 'cancelled' \
           AND starts_at < $2 AND starts_at > $3",
    )
    .bind(ctx.business_id)
    .bind(ends_at)
    .bind(starts_at - Duration::days(1))
    .fetch_all(&ctx.state.db)
    .await?;
    if let Some((id, st, paid, ..)) = own
        .iter()
        .filter(|r| r.4 + Duration::minutes(r.5 as i64) > starts_at)
        .find(|r| crate::harness::canon_phone(&r.3) == phone)
    {
        return Ok(json!({
            "status": "already_booked",
            "appointment_id": id,
            "bookingStatus": st,
            "paid": paid,
            "note": if *paid {
                "this customer ALREADY HAS this appointment, confirmed and paid — \
                 never say the slot filled; confirm their booking stands"
            } else {
                "this customer ALREADY HAS this appointment (awaiting payment) — \
                 do not book again; verify with collect_payment"
            }
        }));
    }

    // Duration-aware conflicts: this booking blocks [start, start+dur), the
    // requested specialist can't be double-booked, and total simultaneous
    // customers can't exceed staff capacity.
    let busy = overlapping(ctx, starts_at, ends_at).await?;
    if let Some(sp) = specialist {
        if busy.iter().any(|(s, ..)| s.as_deref() == Some(sp)) {
            return Ok(json!({"status": "conflict",
                "note": format!("{sp} is already booked at that time")}));
        }
    }
    if busy.len() as i64 >= staff_capacity(&ctx.values) {
        return Ok(json!({"status": "conflict",
            "note": "all staff are busy in that window; offer another time"}));
    }

    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO appointments \
           (id, business_id, customer_name, phone, specialist, starts_at, status, price, service, duration_mins, customer_email) \
         VALUES ($11,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id",
    )
    .bind(ctx.business_id)
    .bind(name)
    .bind(&phone)
    .bind(specialist)
    .bind(starts_at)
    .bind(status)
    .bind(price)
    .bind(service)
    .bind(dur as i32)
    .bind(email)
    .bind(Uuid::new_v4())
    .fetch_one(&ctx.state.db)
    .await?;
    // The CRM learns who this is from the booking.
    crate::contacts::learn(&ctx.state.db, ctx.business_id, ctx.peer.as_deref(), Some(&phone), Some(name), email.as_deref()).await;

    // Customers sometimes yape BEFORE the booking exists (seen in prod): if
    // an unlinked payment already arrived FROM THIS CUSTOMER'S NAME, confirm
    // right away. Nameless payments only count when no other bill is open.
    let mut status = status.to_string();
    // A pre-existing payment can only confirm this booking if it actually
    // covers what is owed. `None` means nothing is configured to check against,
    // so nothing gets confirmed — an unverifiable payment stays unlinked.
    let owed = crate::harness::owed_for(&ctx.values, service, price);
    if status == "pending_payment" && owed.is_some() {
        let candidates: Vec<(Uuid, Option<f64>, Option<String>)> = sqlx::query_as(
            "SELECT id, amount, payer FROM payments \
             WHERE business_id = $1 AND appointment_id IS NULL AND order_id IS NULL \
               AND received_at > $3 \
               AND amount IS NOT NULL AND amount >= $2 \
             ORDER BY received_at DESC LIMIT 20",
        )
        .bind(ctx.business_id)
        .bind(owed)
        .bind(crate::db::ago(Duration::hours(2)))
        .fetch_all(&ctx.state.db)
        .await?;
        // Open bills business-wide, orders included — the nameless shortcut
        // must not fire while an order somewhere could own this money.
        let other_pending: (i64,) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM appointments \
                      WHERE business_id = $1 AND status = 'pending_payment' AND id <> $2) \
                  + (SELECT count(*) FROM orders \
                      WHERE business_id = $1 AND status = 'pending_payment')",
        )
        .bind(ctx.business_id)
        .bind(row.0)
        .fetch_one(&ctx.state.db)
        .await?;
        let pre = crate::settlement::pick_payment_for_bill(
            &candidates,
            name,
            other_pending.0 as usize + 1, // + this brand-new bill
        )
        .payment;
        if let Some(pay_id) = pre {
            // Claimed only while still unlinked: one transfer, one bill.
            let claimed = sqlx::query("UPDATE payments SET appointment_id = $1 WHERE id = $2 AND appointment_id IS NULL AND order_id IS NULL")
                .bind(row.0)
                .bind(pay_id)
                .execute(&ctx.state.db)
                .await?
                .rows_affected();
            if claimed == 1 {
                sqlx::query("UPDATE appointments SET paid = TRUE, status = 'confirmed' WHERE id = $1")
                    .bind(row.0)
                    .execute(&ctx.state.db)
                    .await?;
                status = "confirmed".into();
            }
        }
    }
    if status == "confirmed" {
        crate::outcomes::confirm(ctx.state, ctx.business_id, "booking", row.0, &phone, name).await;
    }
    Ok(json!({"status": status, "appointment_id": row.0, "starts_at": ts,
              "customer": name, "specialist": specialist, "price": price,
              "service": service, "durationMinutes": dur,
              "depositRequired": deposits_enabled && (upfront || deposit_configured) && !free,
              "paid": status == "confirmed" && deposits_enabled && (upfront || deposit_configured) && !free}))
}

async fn handle_cancellation(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let notice_min = ctx.values["cancellationNoticeMins"].as_i64().unwrap_or(0);

    let appt: Option<(Uuid, DateTime<Utc>, bool)> = if let Some(id) =
        args["appointment_id"].as_str().and_then(|s| Uuid::parse_str(s).ok())
    {
        // Belonging to the business is not the same as belonging to the person
        // asking. Without this, any customer holding another customer's
        // appointment id could cancel their booking. Canonicalization has to
        // happen in Rust, so the row is fetched and then filtered.
        let me = ctx
            .peer
            .as_deref()
            .or_else(|| args["phone"].as_str())
            .map(crate::harness::canon_phone);
        let row: Option<(Uuid, DateTime<Utc>, bool, String)> = sqlx::query_as(
            "SELECT id, starts_at, paid, phone FROM appointments \
             WHERE business_id = $1 AND id = $2 AND status <> 'cancelled'",
        )
        .bind(ctx.business_id)
        .bind(id)
        .fetch_optional(&ctx.state.db)
        .await?;
        // Someone else's appointment answers exactly like a nonexistent one, so
        // the result can't be used to probe for valid ids.
        row.filter(|r| me.as_deref() == Some(crate::harness::canon_phone(&r.3).as_str()))
            .map(|r| (r.0, r.1, r.2))
    } else if let Some(peer) = ctx.peer.as_deref().or_else(|| args["phone"].as_str()) {
        // Canonical identity match (stored rows may predate normalization).
        let me = crate::harness::canon_phone(peer);
        let rows: Vec<(Uuid, DateTime<Utc>, bool, String)> = sqlx::query_as(
            "SELECT id, starts_at, paid, phone FROM appointments \
             WHERE business_id = $1 AND status <> 'cancelled' \
               AND starts_at > $2 ORDER BY starts_at LIMIT 30",
        )
        .bind(ctx.business_id)
        .bind(crate::db::now())
        .fetch_all(&ctx.state.db)
        .await?;
        rows.into_iter()
            .find(|r| crate::harness::canon_phone(&r.3) == me)
            .map(|r| (r.0, r.1, r.2))
    } else {
        return Err(anyhow!("need 'appointment_id' or 'phone'"));
    };

    let Some((id, starts_at, paid)) = appt else {
        return Ok(json!({"status": "not_found"}));
    };
    let mins_left = (starts_at - Utc::now()).num_minutes();
    if mins_left < notice_min {
        return Ok(json!({"status": "rejected",
            "note": format!("policy requires {notice_min} min notice; only {mins_left} left")}));
    }
    sqlx::query("UPDATE appointments SET status = 'cancelled' WHERE id = $1")
        .bind(id)
        .execute(&ctx.state.db)
        .await?;
    // A cancelled outcome within 24 h of its charge gets the credit back.
    crate::outcomes::reverse(ctx.state, id).await;
    Ok(json!({"status": "cancelled", "appointment_id": id,
              "refund": if paid { "due (manual for now)" } else { "n/a" }}))
}

impl Plugin<Agente> for Scheduling {
    fn name(&self) -> &'static str {
        "scheduling"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, false),
            spec("check_availability", "Free appointment slots on a date. Pass the service so slots reflect its real duration.", json!({
                "type": "object",
                "properties": {
                    "date": {"type": "string", "description": "YYYY-MM-DD"},
                    "service": {"type": "string", "description": "which service the customer wants (matches pricing/slotDuration keys)"}
                },
                "required": ["date"]
            })),
            tool_fn(|c, a| Box::pin(check_availability(c, a))),
        )?;
        k.tool(
            meta(Scope::Customer, false),
            // No `price` parameter: the price is looked up from the business's
            // own `pricing` map by service name. The model names the service;
            // it does not get to name the amount.
            spec("book_appointment", "Book a slot for a customer. Always pass the service — it determines both how long the booking blocks the calendar and what the booking costs. If values.requireEmail is true, collect customer_email BEFORE booking. Re-booking a slot the customer already holds returns already_booked — their appointment stands; never tell them it filled.", json!({
                "type": "object",
                "properties": {
                    "customer_name": {"type": "string"},
                    "phone": {"type": "string"},
                    "specialist": {"type": "string"},
                    "service": {"type": "string", "description": "which service (matches pricing keys)"},
                    "timestamp": {"type": "string", "description": "YYYY-MM-DDTHH:MM"},
                    "customer_email": {"type": "string", "description": "customer's email — mandatory when values.requireEmail is true (meeting details are sent there)"}
                },
                "required": ["customer_name", "phone", "timestamp", "service"]
            })),
            tool_fn(|c, a| Box::pin(book_appointment(c, a))),
        )?;
        k.tool(
            meta(Scope::Customer, false),
            spec("handle_cancellation", "Cancel an appointment, enforcing the notice policy", json!({
                "type": "object",
                "properties": {"appointment_id": {"type": "string"}, "phone": {"type": "string"}},
                "required": []
            })),
            tool_fn(|c, a| Box::pin(handle_cancellation(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation_is_permissive_but_not_gullible() {
        assert!(looks_like_email("andre@yaya.tech"));
        assert!(looks_like_email("a.b+tag@sub.dominio.pe"));
        assert!(!looks_like_email("no-arroba.com"));
        assert!(!looks_like_email("@dominio.com"));
        assert!(!looks_like_email("a@sindominio"));
        assert!(!looks_like_email("a@.com"));
        assert!(!looks_like_email("con espacios@x.com"));
    }

    use crate::testkit::{self, day, set_values, tool_ctx};

    const ALL_WEEK: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

    fn hours(h: &str) -> Value {
        Value::Object(ALL_WEEK.iter().map(|d| (d.to_string(), json!(h))).collect())
    }

    async fn biz(values: Value) -> (crate::SharedState, Uuid) {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        set_values(&s.db, b, values).await;
        (s, b)
    }

    fn slots_of(v: &Value) -> Vec<String> {
        v["slots"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()[11..].to_string()).collect()
    }

    #[test]
    fn hours_strings_as_owners_write_them() {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!(open_ranges("9-18"), Some(vec![(t(9, 0), 540)]));
        assert_eq!(open_ranges("09:00 - 13:00, 15:00 - 19:30"), Some(vec![(t(9, 0), 240), (t(15, 0), 270)]));
        assert_eq!(open_ranges("9 a 13 y 15 a 19"), Some(vec![(t(9, 0), 240), (t(15, 0), 240)]));
        assert_eq!(open_ranges("18:00 hasta 02:00"), Some(vec![(t(18, 0), 480)]));
        assert_eq!(open_ranges("9am–6pm"), Some(vec![(t(9, 0), 540)]));
        assert_eq!(open_ranges("12pm-12am"), Some(vec![(t(12, 0), 720)]));
        assert_eq!(open_ranges("8h30-17h"), Some(vec![(t(8, 30), 510)]));
        assert_eq!(open_ranges("0-24"), Some(vec![(t(0, 0), 1440)]));
        assert_eq!(open_ranges("Cerrado"), Some(vec![]));
        assert_eq!(open_ranges("todo el día"), None);
        assert_eq!(open_ranges("9-25"), None);
        assert_eq!(parse_clock("13pm"), None);
        assert_eq!(parse_clock("9:75"), None);
    }

    #[test]
    fn durations_and_capacity() {
        assert_eq!(duration_for(&json!({}), None), 30);
        assert_eq!(duration_for(&json!({"slotDuration": 0}), None), 5, "never a zero step");
        assert_eq!(duration_for(&json!({"slotDuration": {"corte": 30, "tinte": 120}}), Some("tinte completo")), 120);
        assert_eq!(duration_for(&json!({"slotDuration": {"corte": 30, "tinte": 120}}), Some("masaje")), 30, "unknown service: the shortest");
        assert_eq!(staff_capacity(&json!({})), 1);
        assert_eq!(staff_capacity(&json!({"staffCount": 0})), 1);
        assert_eq!(staff_capacity(&json!({"staffCount": 3})), 3);
        assert_eq!(parse_hhmm(" 9 "), NaiveTime::from_hms_opt(9, 0, 0));
        assert_eq!(parse_hhmm("09:30"), NaiveTime::from_hms_opt(9, 30, 0));
        assert_eq!(parse_hhmm("25"), None);
    }

    #[tokio::test]
    async fn availability_follows_hours_and_bookings() {
        let (s, b) = biz(json!({"businessHours": hours("9-12"), "slotDuration": 60})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let v = check_availability(&c, &json!({"date": day(1)})).await.unwrap();
        assert_eq!(slots_of(&v), vec!["09:00", "09:30", "10:00", "10:30", "11:00"]);
        book_appointment(&c, &json!({"customer_name": "Ana", "timestamp": format!("{}T10:00", day(1)), "service": "corte"})).await.unwrap();
        let v = check_availability(&c, &json!({"date": day(1)})).await.unwrap();
        assert_eq!(slots_of(&v), vec!["09:00", "11:00"], "a 60-min booking at 10 blocks 9:30–10:30 starts");
        assert_eq!(check_availability(&c, &json!({"date": day(-1)})).await.unwrap()["note"], "that date is in the past");
        assert!(check_availability(&c, &json!({"date": day(400)})).await.unwrap()["note"].as_str().unwrap().contains("30 days"));
        assert!(check_availability(&c, &json!({})).await.is_err());
        assert!(check_availability(&c, &json!({"date": "mañana"})).await.is_err());
    }

    #[tokio::test]
    async fn split_shifts_offer_both_halves() {
        // Closed at lunch: "9-13, 15-19" — the most common hours in Lima.
        let (s, b) = biz(json!({"businessHours": hours("9-11, 15-17"), "slotDuration": 60})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let v = check_availability(&c, &json!({"date": day(1)})).await.expect("split hours must not error");
        assert_eq!(slots_of(&v), vec!["09:00", "09:30", "10:00", "15:00", "15:30", "16:00"]);
    }

    #[tokio::test]
    async fn hours_in_words_and_past_midnight() {
        let (s, b) = biz(json!({"businessHours": hours("18:00 a 01:00"), "slotDuration": 60})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let v = check_availability(&c, &json!({"date": day(1)})).await.unwrap();
        let got = slots_of(&v);
        assert_eq!(got.first().map(String::as_str), Some("18:00"));
        assert!(got.contains(&"23:30".to_string()) && got.contains(&"00:00".to_string()), "{got:?}");
        let (s, b) = biz(json!({"businessHours": hours("9am-12pm"), "slotDuration": 60})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        assert_eq!(slots_of(&check_availability(&c, &json!({"date": day(1)})).await.unwrap()).first().map(String::as_str), Some("09:00"));
        let (s, b) = biz(json!({"businessHours": {"sun": "cerrado"}})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let sunday = (1..=7).map(day).find(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap().weekday() == chrono::Weekday::Sun).unwrap();
        assert_eq!(check_availability(&c, &json!({"date": sunday})).await.unwrap()["open"], false);
    }

    #[tokio::test]
    async fn bookings_outside_opening_hours_are_refused() {
        let (s, b) = biz(json!({"businessHours": hours("9-18")})).await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 977 000 111")).await;
        let r = book_appointment(&c, &json!({"customer_name": "Ana", "timestamp": format!("{}T03:00", day(1)), "service": "corte"})).await.unwrap();
        assert_eq!(r["status"], "rejected", "{r}");
        let r = book_appointment(&c, &json!({"customer_name": "Ana", "timestamp": format!("{}T17:45", day(1)), "service": "corte"})).await.unwrap();
        assert_eq!(r["status"], "rejected", "a 30-min booking at 17:45 ends after closing: {r}");
        assert_eq!(book_appointment(&c, &json!({"customer_name": "Ana", "timestamp": format!("{}T10:00", day(1)), "service": "corte"})).await.unwrap()["status"], "confirmed");
        // No hours configured at all: nothing to enforce.
        let (s, b) = biz(json!({})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        assert_eq!(book_appointment(&c, &json!({"customer_name": "X", "timestamp": format!("{}T03:00", day(1))})).await.unwrap()["status"], "confirmed");
    }

    #[tokio::test]
    async fn booking_prices_deposits_and_idempotency() {
        let (s, b) = biz(json!({"businessHours": hours("8-20"), "pricing": {"corte": 25}, "bookingDeposit": 10})).await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 977 000 111")).await;
        let ts = format!("{}T10:00", day(1));
        // The model cannot set the price.
        let r = book_appointment(&c, &json!({"customer_name": "Ana", "timestamp": ts, "service": "corte", "price": 1})).await.unwrap();
        assert_eq!((r["status"].clone(), r["price"].clone(), r["depositRequired"].clone()), (json!("pending_payment"), json!(25.0), json!(true)));
        let again = book_appointment(&c, &json!({"customer_name": "Ana", "timestamp": ts, "service": "corte"})).await.unwrap();
        assert_eq!((again["status"].clone(), again["appointment_id"].clone()), (json!("already_booked"), r["appointment_id"].clone()));
        // Someone else, same slot, capacity 1: conflict.
        let other = tool_ctx(&s, b, Some("com.whatsapp:+51 977 000 222")).await;
        assert_eq!(book_appointment(&other, &json!({"customer_name": "Rosa", "timestamp": ts, "service": "corte"})).await.unwrap()["status"], "conflict");
        // Missing fields and garbage times.
        assert!(book_appointment(&other, &json!({"timestamp": ts})).await.is_err());
        assert!(book_appointment(&other, &json!({"customer_name": "R"})).await.is_err());
        assert!(book_appointment(&other, &json!({"customer_name": "R", "timestamp": "mañana a las 3"})).await.is_err());
        assert_eq!(book_appointment(&other, &json!({"customer_name": "R", "timestamp": format!("{}T10:00", day(-1))})).await.unwrap()["status"], "rejected");
    }

    #[tokio::test]
    async fn a_payment_that_arrived_first_confirms_the_new_booking() {
        let (s, b) = biz(json!({"businessHours": hours("8-20"), "pricing": {"corte": 25}, "paymentMethod": "yape"})).await;
        sqlx::query("INSERT INTO payments (id, business_id, source, payer, amount, raw_text) VALUES ($1, $2, 'Yape', 'Ana Rojas', 25, 'x')")
            .bind(Uuid::new_v4()).bind(b).execute(&s.db).await.unwrap();
        let c = tool_ctx(&s, b, Some("p1")).await;
        let r = book_appointment(&c, &json!({"customer_name": "Ana Rojas", "timestamp": format!("{}T10:00", day(1)), "service": "corte"})).await.unwrap();
        assert_eq!((r["status"].clone(), r["paid"].clone()), (json!("confirmed"), json!(true)));
        let linked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE appointment_id IS NOT NULL").fetch_one(&s.db).await.unwrap();
        assert_eq!(linked, 1);
    }

    #[tokio::test]
    async fn email_when_the_business_requires_it() {
        let (s, b) = biz(json!({"requireEmail": true})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let ts = format!("{}T10:00", day(1));
        assert_eq!(book_appointment(&c, &json!({"customer_name": "A", "timestamp": ts})).await.unwrap()["status"], "need_email");
        assert_eq!(book_appointment(&c, &json!({"customer_name": "A", "timestamp": ts, "customer_email": "a@b"})).await.unwrap()["status"], "rejected");
        assert_eq!(book_appointment(&c, &json!({"customer_name": "A", "timestamp": ts, "customer_email": "a@b.pe"})).await.unwrap()["status"], "confirmed");
    }

    #[tokio::test]
    async fn the_last_slot_goes_to_exactly_one_customer() {
        let (s, b) = biz(json!({})).await;
        let ts = format!("{}T10:00", day(1));
        let mut tasks = vec![];
        for i in 0..6 {
            let s = s.clone();
            let ts = ts.clone();
            tasks.push(tokio::spawn(async move {
                let c = tool_ctx(&s, b, Some(&format!("peer{i}"))).await;
                book_appointment(&c, &json!({"customer_name": format!("C{i}"), "timestamp": ts})).await.unwrap()["status"].as_str().unwrap().to_string()
            }));
        }
        let mut ok = 0;
        for t in tasks { if t.await.unwrap() == "confirmed" { ok += 1; } }
        assert_eq!(ok, 1, "capacity 1: one booking, the rest conflict");
    }

    #[tokio::test]
    async fn cancellation_is_own_bookings_only_and_respects_notice() {
        let (s, b) = biz(json!({"cancellationNoticeMins": 120})).await;
        let ana = tool_ctx(&s, b, Some("com.whatsapp:+51 1")).await;
        let r = book_appointment(&ana, &json!({"customer_name": "Ana", "timestamp": format!("{}T10:00", day(2))})).await.unwrap();
        let id = r["appointment_id"].as_str().unwrap().to_string();
        let mallory = tool_ctx(&s, b, Some("com.whatsapp:+51 2")).await;
        assert_eq!(handle_cancellation(&mallory, &json!({"appointment_id": id})).await.unwrap()["status"], "not_found", "someone else's booking looks nonexistent");
        assert_eq!(handle_cancellation(&ana, &json!({"appointment_id": id})).await.unwrap()["status"], "cancelled");
        assert_eq!(handle_cancellation(&ana, &json!({})).await.unwrap()["status"], "not_found");
        // Too late to cancel.
        let soon = testkit::appointment(&s.db, b, "Ana", "confirmed", false, None, 1).await;
        sqlx::query("UPDATE appointments SET phone = 'com.whatsapp:+51 1' WHERE id = $1").bind(soon).execute(&s.db).await.unwrap();
        let r = handle_cancellation(&ana, &json!({"appointment_id": soon.to_string()})).await.unwrap();
        assert_eq!(r["status"], "rejected");
        let owner = tool_ctx(&s, b, None).await;
        assert!(handle_cancellation(&owner, &json!({})).await.is_err());
    }

    #[tokio::test]
    async fn published_free_slots() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let v = json!({"businessHours": hours("9-11, 15-16"), "slotDuration": 60});
        let slots = next_free_slots(&s.db, b, &v, 3, 50).await;
        assert!(slots.iter().any(|x| x.ends_with("T15:00")), "{slots:?}");
        assert_eq!(next_free_slots(&s.db, b, &v, 3, 2).await.len(), 2);
        assert!(next_free_slots(&s.db, b, &json!({}), 3, 5).await.is_empty(), "no hours, no slots");
    }
}
