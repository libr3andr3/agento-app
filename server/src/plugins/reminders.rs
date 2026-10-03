//! Reminders by calendar invite. agente has no outbound channel by design
//! (it replies inside notifications), so the reminder is delegated to the
//! customer's own calendar: we email an ICS invite whose VALARM fires at the
//! lead time they chose.
//!
//! Spatiotemporal gating, both axes:
//! - space: the plugin only mounts when mail transport is configured
//!   (SMTP_URL, or OUTBOX_DIR for dev). Unmounted → the tool doesn't exist →
//!   capability honesty keeps the agent from ever offering it.
//! - time: the tool is registered with `when: booking_exists`, so it only
//!   appears in a conversation once this customer actually has a booking.

use anyhow::Result;
use chrono::{DateTime, Utc};
use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::harness::{canon_phone, hook_fn, meta_when, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub enum Mailer {
    Smtp(AsyncSmtpTransport<Tokio1Executor>, Mailbox),
    /// Dev transport: writes .eml files instead of sending.
    Outbox(PathBuf, Mailbox),
}

impl Mailer {
    async fn send(&self, msg: Message) -> Result<String> {
        match self {
            Mailer::Smtp(t, _) => {
                t.send(msg).await?;
                Ok("smtp".into())
            }
            Mailer::Outbox(dir, _) => {
                std::fs::create_dir_all(dir)?;
                let path = dir.join(format!("{}.eml", Utc::now().format("%Y%m%dT%H%M%S%f")));
                std::fs::write(&path, msg.formatted())?;
                Ok(format!("outbox:{}", path.display()))
            }
        }
    }
    fn from(&self) -> &Mailbox {
        match self {
            Mailer::Smtp(_, f) | Mailer::Outbox(_, f) => f,
        }
    }
}

/// Mounts only when a transport is configured — spatial gating by config.
pub fn from_env() -> Option<Reminders> {
    let from: Mailbox = std::env::var("MAIL_FROM")
        .unwrap_or_else(|_| "agente <citas@agente.ceo>".into())
        .parse()
        .ok()?;
    if let Ok(url) = std::env::var("SMTP_URL") {
        let t = AsyncSmtpTransport::<Tokio1Executor>::from_url(&url).ok()?.build();
        return Some(Reminders { mailer: Arc::new(Mailer::Smtp(t, from)) });
    }
    if let Ok(dir) = std::env::var("OUTBOX_DIR") {
        return Some(Reminders { mailer: Arc::new(Mailer::Outbox(PathBuf::from(dir), from)) });
    }
    None
}

pub struct Reminders {
    pub mailer: Arc<Mailer>,
}

/// One invite plus two resends. Enough for a genuine "it didn't arrive",
/// far short of anything worth using as a mail relay.
const MAX_REMINDER_SENDS: i32 = 3;

/// ICS TEXT escaping. A carriage return is dropped outright: names come
/// through the model, and a bare CR is a line break to lenient parsers.
fn esc(s: &str) -> String {
    s.replace('\r', "").replace('\\', "\\\\").replace(',', "\\,").replace(';', "\\;").replace('\n', "\\n")
}

#[allow(clippy::too_many_arguments)]
fn build_ics(
    appt_id: Uuid,
    business: &str,
    customer: &str,
    specialist: Option<&str>,
    starts_at: DateTime<Utc>,
    slot_minutes: i64,
    remind_minutes: i64,
    tz: chrono_tz::Tz,
    lang: &str,
) -> String {
    // Stored timestamps are real UTC; the invite declares business-local
    // wall-clock under the business's IANA TZID.
    let start = starts_at.with_timezone(&tz).format("%Y%m%dT%H%M%S");
    let end = (starts_at + chrono::Duration::minutes(slot_minutes))
        .with_timezone(&tz)
        .format("%Y%m%dT%H%M%S");
    let tzid = tz.name();
    let summary = match specialist {
        Some(s) => format!("{} — {} ({})", esc(business), esc(customer), esc(s)),
        None => format!("{} — {}", esc(business), esc(customer)),
    };
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//agente//corazon//ES\r\nMETHOD:REQUEST\r\n\
         BEGIN:VEVENT\r\nUID:{appt_id}@agente.ceo\r\nDTSTAMP:{stamp}\r\n\
         DTSTART;TZID={tzid}:{start}\r\nDTEND;TZID={tzid}:{end}\r\n\
         SUMMARY:{summary}\r\nDESCRIPTION:{confirmed}\r\n\
         BEGIN:VALARM\r\nTRIGGER:-PT{remind_minutes}M\r\nACTION:DISPLAY\r\n\
         DESCRIPTION:{reminder}\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        confirmed = crate::locale::t(lang, "ics_confirmed"),
        reminder = crate::locale::t(lang, "ics_reminder"),
        stamp = Utc::now().format("%Y%m%dT%H%M%SZ"),
    )
}

async fn schedule_reminder(
    ctx: &ToolCtx<'_>,
    args: &Value,
    mailer: &Mailer,
) -> Result<Value> {
    let Some(peer) = ctx.peer.as_deref() else {
        return Ok(json!({"error": "customer agent only"}));
    };
    let email = args["email"].as_str().unwrap_or("").trim().to_string();
    // Parse with the same type that will actually address the message, rather
    // than guessing with substring checks and discovering the mismatch later.
    let Ok(to) = email.parse::<Mailbox>() else {
        return Ok(json!({"status": "invalid_email",
                         "note": "ask the customer for a valid email address"}));
    };
    let minutes = args["minutes_before"].as_i64().unwrap_or(60).clamp(5, 2880);

    // Newest live appointment belonging to this peer (canonical identity).
    let rows: Vec<(Uuid, String, String, Option<String>, DateTime<Utc>, i32)> = sqlx::query_as(
        "SELECT id, customer_name, phone, specialist, starts_at, \
                COALESCE(duration_mins, 30) FROM appointments \
         WHERE business_id = $1 AND status <> 'cancelled' \
           AND starts_at > $2 \
         ORDER BY created_at DESC LIMIT 30",
    )
    .bind(ctx.business_id)
    .bind(crate::db::ago(chrono::Duration::hours(2)))
    .fetch_all(&ctx.state.db)
    .await?;
    let me = canon_phone(peer);
    let Some((appt_id, customer, _, specialist, starts_at, duration)) =
        rows.into_iter().find(|r| canon_phone(&r.2) == me)
    else {
        return Ok(json!({"status": "no_booking",
                         "note": "no live appointment for this customer — book first"}));
    };

    // Bound outbound mail per appointment. A conversation drives this tool, so
    // without a cap the business's own SMTP identity can be used to send at
    // whatever rate a customer is willing to keep asking — and its sender
    // reputation is the thing that gets spent.
    let sends: (i32,) = sqlx::query_as(
        "UPDATE appointments SET reminder_sends = reminder_sends + 1 \
         WHERE id = $1 RETURNING reminder_sends",
    )
    .bind(appt_id)
    .fetch_one(&ctx.state.db)
    .await?;
    if sends.0 > MAX_REMINDER_SENDS {
        tracing::warn!(appointment = %appt_id, sends = sends.0, "reminder send cap hit");
        return Ok(json!({
            "status": "already_sent",
            "note": "the calendar invite has already been sent for this booking — \
                     tell the customer to check their inbox and spam folder"
        }));
    }

    sqlx::query("UPDATE appointments SET customer_email = $1, remind_minutes = $2 WHERE id = $3")
        .bind(&email)
        .bind(minutes as i32)
        .bind(appt_id)
        .execute(&ctx.state.db)
        .await?;

    let business = ctx.doc["name"].as_str().unwrap_or("agente").to_string();
    let tz = crate::harness::biz_tz(&ctx.values);
    let lang = crate::locale::Locale::from_values(&ctx.values).language;
    let ics = build_ics(appt_id, &business, &customer, specialist.as_deref(),
                        starts_at, duration as i64, minutes, tz, &lang);

    let fecha = crate::harness::fmt_local(starts_at, tz, "%d/%m/%Y %H:%M");
    let human = crate::locale::t(&lang, "mail_body")
        .replace("{customer}", &customer)
        .replace("{business}", &business)
        .replace("{date}", &fecha)
        .replace("{minutes}", &minutes.to_string());
    let subject = crate::locale::t(&lang, "mail_subject")
        .replace("{business}", &business)
        .replace("{date}", &crate::harness::fmt_local(starts_at, tz, "%d/%m %H:%M"));
    let msg = Message::builder()
        .from(mailer.from().clone())
        .to(to)
        .subject(subject)
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(human))
                .singlepart(
                    Attachment::new("invite.ics".into()).body(
                        ics,
                        ContentType::parse("text/calendar; method=REQUEST; charset=utf-8")
                            .expect("static content type"),
                    ),
                ),
        )?;
    let via = mailer.send(msg).await?;
    tracing::info!(appointment = %appt_id, %email, minutes, via, "calendar invite sent");
    Ok(json!({
        "status": "invite_sent",
        "to": email,
        "minutesBefore": minutes,
        "note": "the customer's calendar will fire the reminder; confirm it to them"
    }))
}

const PROMPT_SECTION: &str = "REMINDERS: once this customer has a booking, you gain the \
schedule_reminder tool. Offer it exactly once after confirming a booking: you can email \
them the appointment as a calendar invite whose alarm reminds them — ask for their email \
and how long before they want the alert (default 1 hour). Reminders happen ONLY through \
that calendar invite; never promise to message, call, or remind them any other way.";

impl Plugin<Agente> for Reminders {
    fn name(&self) -> &'static str {
        "reminders"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.provide("mailer", self.mailer.clone());
        let mailer = self.mailer.clone();
        k.tool(
            meta_when(Scope::Customer, true, "booking_exists"),
            json!({"type": "function", "function": {
                "name": "schedule_reminder",
                "description": "Email the customer their confirmed appointment as a calendar invite with a reminder alarm. Only after a booking exists; ask for their email and preferred lead time first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "email": {"type": "string", "description": "customer's email address"},
                        "minutes_before": {"type": "number", "description": "how many minutes before the appointment the alarm should fire (default 60)"}
                    },
                    "required": ["email"]
                }
            }}),
            tool_fn(move |c, a| {
                let mailer = mailer.clone();
                Box::pin(async move { schedule_reminder(c, a, &mailer).await })
            }),
        )?;
        k.on(
            "prompt/customer",
            hook_fn(|_rt, mut payload| {
                Box::pin(async move {
                    payload["sections"]
                        .as_array_mut()
                        .map(|s| s.push(json!(PROMPT_SECTION)));
                    Ok(payload)
                })
            }),
        );
        k.on(
            "prompt/onboarding",
            hook_fn(|_rt, mut payload| {
                Box::pin(async move {
                    payload["sections"].as_array_mut().map(|s| {
                        s.push(json!(
                            "Selling point to mention once during the interview: after \
                             each confirmed booking, the agent can email the customer a \
                             calendar invite whose alarm reminds them before the visit."
                        ))
                    });
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

    use crate::testkit::{self, tool_ctx};

    fn outbox() -> (Mailer, PathBuf) {
        let dir = std::env::temp_dir().join(format!("outbox-{}", Uuid::new_v4()));
        (Mailer::Outbox(dir.clone(), "agente <citas@agente.ceo>".parse().unwrap()), dir)
    }

    #[test]
    fn ics_text_cannot_add_lines() {
        assert_eq!(esc("a,b;c\\d\ne"), "a\\,b\\;c\\\\d\\ne");
        assert!(!esc("Ana\rATTENDEE:mailto:x@y.z").contains('\r'), "a bare CR must not survive either");
    }

    #[test]
    fn invite_is_local_time_with_an_alarm() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-22T15:00:00Z").unwrap().with_timezone(&Utc);
        let ics = build_ics(Uuid::nil(), "Barbería, Tito", "Ana", Some("Luis"), at, 45, 90, chrono_tz::America::Lima, "es");
        assert!(ics.contains("DTSTART;TZID=America/Lima:20260922T100000"));
        assert!(ics.contains("DTEND;TZID=America/Lima:20260922T104500"));
        assert!(ics.contains("TRIGGER:-PT90M"));
        assert!(ics.contains("SUMMARY:Barbería\\, Tito — Ana (Luis)"));
        assert!(ics.contains("Reserva confirmada"));
        assert!(ics.ends_with("END:VCALENDAR\r\n"));
    }

    #[test]
    fn unconfigured_without_smtp_or_outbox() {
        assert!(from_env().is_none());
    }

    #[tokio::test]
    async fn invites_are_sent_for_the_customers_own_booking_and_capped() {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        let (mailer, dir) = outbox();
        let owner = tool_ctx(&s, b, None).await;
        assert_eq!(schedule_reminder(&owner, &json!({"email": "a@b.pe"}), &mailer).await.unwrap()["error"], "customer agent only");
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 977 000 111")).await;
        assert_eq!(schedule_reminder(&c, &json!({"email": "not an email"}), &mailer).await.unwrap()["status"], "invalid_email");
        assert_eq!(schedule_reminder(&c, &json!({"email": "a@b.pe"}), &mailer).await.unwrap()["status"], "no_booking");
        testkit::appointment(&s.db, b, "Ana", "confirmed", false, None, 24).await; // phone '+51 977 000 111'
        for _ in 0..3 {
            let r = schedule_reminder(&c, &json!({"email": "ana@x.pe", "minutes_before": 1}), &mailer).await.unwrap();
            assert_eq!((r["status"].clone(), r["minutesBefore"].clone()), (json!("invite_sent"), json!(5)), "clamped to 5 minutes");
        }
        assert_eq!(schedule_reminder(&c, &json!({"email": "ana@x.pe"}), &mailer).await.unwrap()["status"], "already_sent");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 3);
        let eml = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        assert!(eml.contains("ana@x.pe") && eml.contains("text/calendar"));
        let (email, mins): (Option<String>, Option<i32>) = sqlx::query_as("SELECT customer_email, remind_minutes FROM appointments").fetch_one(&s.db).await.unwrap();
        assert_eq!((email.as_deref(), mins), (Some("ana@x.pe"), Some(5)));
        // Someone else asking gets nothing about Ana's booking.
        let other = tool_ctx(&s, b, Some("com.whatsapp:+51 988")).await;
        assert_eq!(schedule_reminder(&other, &json!({"email": "x@y.pe"}), &mailer).await.unwrap()["status"], "no_booking");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
