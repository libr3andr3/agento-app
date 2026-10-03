//! Izipay / Micuentaweb (Lyra V4) for Perú. A recarga is one charge
//! (`Charge/CreatePayment`, `formAction: PAYMENT`). A plan is a subscription
//! on the card, in two steps, because the REST API has no single call for
//! both: the form token charges the first period and registers the card
//! (`REGISTER_PAY`); the IPN of that payment carries the card's
//! `paymentMethodToken`, and `Charge/CreateSubscription` schedules every
//! later period on it. Each installment arrives as its own IPN — through the
//! Back Office rule "URL de notificación al crear una recurrencia", which
//! must be switched on — signed, like every IPN, with the REST password.

use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use serde_json::{json, Value};

pub const SCRIPT_URL: &str = "https://static.micuentaweb.pe/static/js/krypton-client/V4.0/stable/kr-payment-form.min.js";

pub struct Izipay {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
    public_key: String,
    /// Signs the browser return (`kr-hash-key = sha256_hmac`); the IPN uses the password.
    hmac_key: String,
    /// Plans register the card and subscribe. Off (`IZIPAY_RECURRING=0`)
    /// sells a plan as a single charge for its months, like a Yape transfer.
    recurring: bool,
    ipn_url: String,
}

impl Izipay {
    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Option<Self>> {
        let user = match std::env::var("IZIPAY_USER") { Ok(u) if !u.trim().is_empty() => u.trim().to_string(), _ => return Ok(None) };
        let need = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).ok_or_else(|| anyhow::anyhow!("{k} must be set with IZIPAY_USER"));
        let z = Self {
            http,
            base: crate::env_or("IZIPAY_BASE_URL", "https://api.micuentaweb.pe/api-payment/V4").trim_end_matches('/').to_string(),
            user,
            password: need("IZIPAY_PASSWORD")?,
            public_key: need("IZIPAY_PUBLIC_KEY")?,
            hmac_key: need("IZIPAY_HMAC_KEY")?,
            recurring: crate::env_or("IZIPAY_RECURRING", "1") != "0",
            ipn_url: crate::env_or("IZIPAY_IPN_URL", "https://agento.ceo/v1/billing/webhook/izipay"),
        };
        // Test and production keys come in pairs; a mixed pair mints forms
        // that can never be paid, so refuse to start with one.
        let test_key = z.public_key.contains(":testpublickey_");
        anyhow::ensure!(test_key == (z.mode() == "TEST"), "IZIPAY_PASSWORD and IZIPAY_PUBLIC_KEY are not from the same mode (test/production)");
        tracing::info!(mode = z.mode(), recurring = z.recurring, "izipay");
        Ok(Some(z))
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    pub fn recurring(&self) -> bool {
        self.recurring
    }

    /// `TEST` or `PRODUCTION`, as Izipay writes it in `orderDetails.mode`.
    pub fn mode(&self) -> &'static str {
        if self.password.starts_with("testpassword_") { "TEST" } else { "PRODUCTION" }
    }

    /// The form token for one checkout. `register` (a plan) also keeps the
    /// card, so the IPN brings back the token the subscription is built on.
    #[allow(clippy::too_many_arguments)]
    pub async fn form_token(&self, order_id: &str, amount_minor: i64, email: &str, name: Option<&str>, account: &str, plan: &str, register: bool) -> anyhow::Result<String> {
        let mut billing = json!({"country": "PE"});
        if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
            let (first, last) = n.split_once(' ').unwrap_or((n, ""));
            billing["firstName"] = json!(clip(first, 60));
            if !last.trim().is_empty() {
                billing["lastName"] = json!(clip(last.trim(), 60));
            }
        }
        let body = json!({
            "amount": amount_minor,
            "currency": "PEN",
            "orderId": order_id,
            // A token needs the buyer's email (Izipay asks for it on the form otherwise).
            "formAction": if register { "REGISTER_PAY" } else { "PAYMENT" },
            "ipnTargetUrl": self.ipn_url,
            "metadata": {"checkout": order_id, "account": account, "plan": plan},
            "customer": {"email": email, "reference": clip(account, 80), "billingDetails": billing},
        });
        let v = self.call("Charge/CreatePayment", &body).await?;
        v["answer"]["formToken"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("no formToken: {v}"))
    }

    /// Schedules the card behind `token` for `plan.amount` every period from
    /// `schedule.first` on. The first period was paid with the form; this is
    /// only what comes after it. Answers the provider's subscription id.
    pub async fn create_subscription(&self, token: &str, amount_minor: i64, schedule: &Schedule, order_id: &str, description: &str, metadata: Value) -> anyhow::Result<String> {
        let body = json!({
            "amount": amount_minor,
            "currency": "PEN",
            "effectDate": schedule.effect_date(),
            "rrule": schedule.rrule,
            "paymentMethodToken": token,
            "orderId": order_id,
            "description": clip(description, 255),
            "metadata": metadata,
        });
        let v = self.call("Charge/CreateSubscription", &body).await?;
        v["answer"]["subscriptionId"].as_str().filter(|s| !s.is_empty()).map(String::from).ok_or_else(|| anyhow::anyhow!("no subscriptionId: {v}"))
    }

    /// Stops the future installments. Charges already made stand.
    pub async fn cancel_subscription(&self, token: &str, subscription_id: &str) -> anyhow::Result<()> {
        let v = self.call("Subscription/Cancel", &json!({"paymentMethodToken": token, "subscriptionId": subscription_id})).await?;
        // 0 = cancelled; 32 = no such subscription (already gone at Izipay).
        match v["answer"]["responseCode"].as_i64() {
            Some(0) | Some(32) | None => Ok(()),
            Some(c) => anyhow::bail!("izipay cancel {subscription_id}: responseCode {c}: {v}"),
        }
    }

    async fn call(&self, op: &str, body: &Value) -> anyhow::Result<Value> {
        let r = self.http.post(format!("{}/{op}", self.base)).basic_auth(&self.user, Some(&self.password)).json(body).send().await?;
        let v: Value = r.json().await?;
        // Izipay answers 200 with `status: ERROR` and the reason in `answer`.
        anyhow::ensure!(v["status"] == "SUCCESS", "izipay {op}: {} {} {}", v["answer"]["errorCode"], v["answer"]["errorMessage"], v["answer"]["detailedErrorMessage"]);
        Ok(v)
    }

    /// `kr-hash` = HMAC-SHA256(key, kr-answer) hex. IPNs sign with the
    /// password; the browser return signs with the HMAC-SHA-256 key. The
    /// answer is also tried with `\/` unescaped, as Izipay's own PHP sample
    /// does — either way only the key holder could have produced the hash.
    pub fn verify(&self, kr_answer: &str, kr_hash: &str, from_browser: bool) -> bool {
        let key = if from_browser { &self.hmac_key } else { &self.password };
        if key.is_empty() || kr_hash.is_empty() {
            return false;
        }
        let want = kr_hash.trim().to_lowercase();
        let ok = |s: &str| yaya_wire::secret::ct_eq(&hmac_hex(key, s), &want);
        ok(kr_answer) || (kr_answer.contains("\\/") && ok(&kr_answer.replace("\\/", "/")))
    }
}

fn hmac_hex(key: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).expect("hmac takes any key");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// When a subscription charges: `first` is the date of the first
/// installment after the period just paid, and `rrule` repeats it. Izipay
/// charges in a daily batch at 00:00–05:00 UTC, on the rrule's dates only.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub first: NaiveDate,
    pub rrule: String,
}

impl Schedule {
    /// The period after one paid at `paid`: monthly for a 1-month plan,
    /// yearly for an annual one. A month started on the 28th or later
    /// charges on the last day of each month, so February is not skipped
    /// (RFC 5545 drops months that lack the 30th).
    pub fn after(paid: DateTime<Utc>, months: i64) -> Self {
        let day = paid.date_naive();
        if months >= 12 {
            // Feb 29 lands on Feb 28 (chrono clamps), and so does every year after.
            let first = day.checked_add_months(Months::new(12)).expect("in range");
            return Self { first, rrule: format!("RRULE:FREQ=YEARLY;BYMONTH={};BYMONTHDAY={}", first.month(), first.day()) };
        }
        let next = day.checked_add_months(Months::new(1)).expect("in range");
        if day.day() >= 28 {
            let last = last_day_of_month(next);
            return Self { first: last, rrule: "RRULE:FREQ=MONTHLY;BYMONTHDAY=28,29,30,31;BYSETPOS=-1".into() };
        }
        Self { first: next, rrule: format!("RRULE:FREQ=MONTHLY;BYMONTHDAY={}", day.day()) }
    }

    /// `effectDate` as the API wants it: 25 characters, midnight UTC.
    pub fn effect_date(&self) -> String {
        format!("{}T00:00:00+00:00", self.first.format("%Y-%m-%d"))
    }

    /// The first date of this rrule strictly after the day `charged` fell
    /// on: the installment after the one just paid, however late its IPN
    /// arrived and whether or not an earlier one was refused.
    pub fn next_after(&self, charged: DateTime<Utc>) -> NaiveDate {
        let today = charged.date_naive();
        let part = |k: &str| self.rrule.split(|c| c == ';' || c == ':').find_map(|p| p.strip_prefix(k)).and_then(|v| v.split(',').next()?.parse::<u32>().ok());
        let on = |y: i32, m: u32, d: u32| NaiveDate::from_ymd_opt(y, m, d).unwrap_or_else(|| last_day_of_month(NaiveDate::from_ymd_opt(y, m, 1).expect("valid month")));
        if self.rrule.contains("FREQ=YEARLY") {
            let (m, d) = (part("BYMONTH=").unwrap_or(today.month()), part("BYMONTHDAY=").unwrap_or(today.day()));
            let this = on(today.year(), m, d);
            return if this > today { this } else { on(today.year() + 1, m, d) };
        }
        let in_month = |first_of: NaiveDate| {
            if self.rrule.contains("BYSETPOS=-1") { last_day_of_month(first_of) } else { on(first_of.year(), first_of.month(), part("BYMONTHDAY=").unwrap_or(today.day())) }
        };
        let this = in_month(today.with_day(1).expect("day 1"));
        if this > today { this } else { in_month(today.with_day(1).expect("day 1").checked_add_months(Months::new(1)).expect("in range")) }
    }
}

fn last_day_of_month(d: NaiveDate) -> NaiveDate {
    let first_next = if d.month() == 12 { NaiveDate::from_ymd_opt(d.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1) };
    first_next.expect("valid").pred_opt().expect("valid")
}

#[cfg(test)]
impl Izipay {
    pub fn for_test(password: &str) -> Self {
        Self { http: reqwest::Client::new(), base: String::new(), user: String::new(), password: password.into(), public_key: String::new(), hmac_key: "browserkey".into(), recurring: true, ipn_url: String::new() }
    }
    pub fn at_for_test(base: &str, password: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_string(), user: "shop".into(), public_key: "pk".into(), ipn_url: "https://agento.ceo/v1/billing/webhook/izipay".into(), ..Self::for_test(password) }
    }
    pub fn hash_for_test(&self, kr_answer: &str) -> String {
        hmac_hex(&self.password, kr_answer)
    }
    pub fn browser_hash_for_test(&self, kr_answer: &str) -> String {
        hmac_hex(&self.hmac_key, kr_answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_php_reference() {
        // hash_hmac("sha256", "hello", "secret")
        let z = Izipay { password: "secret".into(), ..Izipay::for_test("x") };
        assert!(z.verify("hello", "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b", false));
        assert!(!z.verify("hello", "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b", true));
        assert!(!z.verify("hello", "", false));
    }

    #[test]
    fn an_escaped_answer_verifies_either_way() {
        let z = Izipay::for_test("pw");
        let plain = r#"{"url":"https://x/y"}"#;
        let escaped = r#"{"url":"https:\/\/x\/y"}"#;
        assert!(z.verify(escaped, &z.hash_for_test(plain), false));
        assert!(z.verify(escaped, &z.hash_for_test(escaped), false));
        assert!(!z.verify(plain, &z.hash_for_test(escaped), false));
    }

    #[test]
    fn mode_follows_the_password() {
        assert_eq!(Izipay::for_test("testpassword_abc").mode(), "TEST");
        assert_eq!(Izipay::for_test("prodpassword_abc").mode(), "PRODUCTION");
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn monthly_schedules_keep_the_day_and_never_skip_february() {
        let s = Schedule::after(at("2026-09-25T18:30:00Z"), 1);
        assert_eq!(s, Schedule { first: NaiveDate::from_ymd_opt(2026, 10, 25).unwrap(), rrule: "RRULE:FREQ=MONTHLY;BYMONTHDAY=25".into() });
        assert_eq!(s.effect_date(), "2026-10-25T00:00:00+00:00");
        assert_eq!(s.effect_date().len(), 25);
        assert_eq!(s.next_after(at("2026-10-25T03:00:00Z")), NaiveDate::from_ymd_opt(2026, 11, 25).unwrap());

        // Bought on the 31st of January: the last day of every month.
        let s = Schedule::after(at("2027-01-31T12:00:00Z"), 1);
        assert_eq!(s.first, NaiveDate::from_ymd_opt(2027, 2, 28).unwrap());
        assert_eq!(s.rrule, "RRULE:FREQ=MONTHLY;BYMONTHDAY=28,29,30,31;BYSETPOS=-1");
        assert_eq!(s.next_after(at("2027-02-28T02:00:00Z")), NaiveDate::from_ymd_opt(2027, 3, 31).unwrap());
        // The 28th itself also goes to month-end: Feb 28 → Mar 31.
        assert_eq!(Schedule::after(at("2027-02-28T12:00:00Z"), 1).first, NaiveDate::from_ymd_opt(2027, 3, 31).unwrap());
    }

    #[test]
    fn the_next_installment_follows_the_rrule_not_the_ipn_clock() {
        let s = Schedule { first: NaiveDate::from_ymd_opt(2026, 10, 25).unwrap(), rrule: "RRULE:FREQ=MONTHLY;BYMONTHDAY=25".into() };
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(s.next_after(at("2026-10-25T02:00:00Z")), d(2026, 11, 25), "on time");
        assert_eq!(s.next_after(at("2026-10-27T09:00:00Z")), d(2026, 11, 25), "a late IPN does not shift the calendar");
        assert_eq!(s.next_after(at("2026-12-25T02:00:00Z")), d(2027, 1, 25), "after a refused month");
        let end = Schedule { first: d(2027, 2, 28), rrule: "RRULE:FREQ=MONTHLY;BYMONTHDAY=28,29,30,31;BYSETPOS=-1".into() };
        assert_eq!(end.next_after(at("2027-02-28T01:00:00Z")), d(2027, 3, 31));
        assert_eq!(end.next_after(at("2027-03-01T01:00:00Z")), d(2027, 3, 31), "the Feb charge reported on Mar 1");
        let y = Schedule::after(at("2026-09-25T12:00:00Z"), 12);
        assert_eq!(y.next_after(at("2027-09-26T01:00:00Z")), d(2028, 9, 25));
    }

    #[test]
    fn yearly_schedules_repeat_the_date() {
        let s = Schedule::after(at("2026-09-25T12:00:00Z"), 12);
        assert_eq!(s, Schedule { first: NaiveDate::from_ymd_opt(2027, 9, 25).unwrap(), rrule: "RRULE:FREQ=YEARLY;BYMONTH=9;BYMONTHDAY=25".into() });
        assert_eq!(s.next_after(at("2027-09-25T01:00:00Z")), NaiveDate::from_ymd_opt(2028, 9, 25).unwrap());
        let s = Schedule::after(at("2028-02-29T12:00:00Z"), 12);
        assert_eq!(s.first, NaiveDate::from_ymd_opt(2029, 2, 28).unwrap());
        assert_eq!(s.rrule, "RRULE:FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=28");
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn ipn_and_browser_returns_use_different_keys() {
        let z = Izipay::for_test("pw");
        let h = z.hash_for_test("{\"orderStatus\":\"PAID\"}");
        assert!(z.verify("{\"orderStatus\":\"PAID\"}", &h.to_uppercase(), false), "hex case does not matter");
        assert!(!z.verify("{\"orderStatus\":\"PAID\"}", &h, true));
        assert!(!z.verify("{\"orderStatus\":\"UNPAID\"}", &h, false));
        let mut empty = Izipay::for_test("");
        empty.hmac_key.clear();
        assert!(!empty.verify("x", "", false) && !empty.verify("x", "", true), "no key never verifies");
    }
}
