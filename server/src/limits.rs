//! Throttling, in two layers.
//!
//! **Registration** is the only route that isn't scoped to an existing tenant,
//! and it costs real money to hit: every call writes a business row, mints a
//! device token, runs an LLM turn and synthesizes speech. It is guarded here,
//! in memory, by IP and by owner phone.
//!
//! **Spend** is guarded per business per day, in the database, because the
//! ceiling has to survive a restart and hold across however many processes run.
//!
//! Deliberately dependency-free: this machine has no cargo registry cache, and
//! a token bucket is thirty lines. If this ever needs to be shared across
//! instances, the registration half is the piece to move to Redis.

use anyhow::anyhow;
use axum::http::HeaderMap;
use uuid::Uuid;
use yaya_wire::ratelimit::Quota;

/// In-memory token buckets for the unauthenticated surface.
pub struct Limiter {
    buckets: yaya_wire::ratelimit::Limiter,
    per_ip: Quota,
    per_phone: Quota,
    otp_ip: Quota,
    otp_phone: Quota,
    lead_ip: Quota,
}

impl Limiter {
    pub fn from_env() -> Self {
        Self {
            buckets: yaya_wire::ratelimit::Limiter::new(),
            per_ip: Quota::from_env("REGISTER_PER_IP_PER_HOUR", 5.0, 3600.0),
            per_phone: Quota::from_env("REGISTER_PER_PHONE_PER_DAY", 3.0, 86_400.0),
            otp_ip: Quota::from_env("OTP_PER_IP_PER_HOUR", 10.0, 3600.0),
            otp_phone: Quota::from_env("OTP_PER_PHONE_PER_DAY", 5.0, 86_400.0),
            lead_ip: Quota::from_env("LEAD_PER_IP_PER_HOUR", 20.0, 3600.0),
        }
    }

    fn take(&self, key: String, q: Quota) -> bool {
        self.buckets.take(&key, q)
    }

    /// Both buckets must have room. Checked before any work is done, so a
    /// rejected registration costs nothing but the lookup.
    pub fn allow_registration(&self, ip: &str, phone: &str) -> bool {
        // Evaluate both so neither ordering lets one dimension go uncounted.
        let ip_ok = self.take(format!("ip:{ip}"), self.per_ip);
        let phone_ok = self.take(format!("phone:{}", phone_key(phone)), self.per_phone);
        ip_ok && phone_ok
    }

    /// OTP sends get their own buckets: every send is a paid template message
    /// to a phone the caller chose, so an unthrottled endpoint is both a spend
    /// amplifier and an SMS-bombing primitive against arbitrary numbers.
    pub fn allow_otp(&self, ip: &str, phone: &str) -> bool {
        let ip_ok = self.take(format!("otp-ip:{ip}"), self.otp_ip);
        let phone_ok = self.take(format!("otp-phone:{phone}"), self.otp_phone);
        ip_ok && phone_ok
    }

    /// Landing-page email capture is unauthenticated and inserts a row per new
    /// address; the per-IP bucket is what keeps a bored script from filling
    /// the leads table.
    pub fn allow_lead(&self, ip: &str) -> bool {
        self.take(format!("lead-ip:{ip}"), self.lead_ip)
    }
}

/// One bucket per phone however it is typed: digits only ("+51 999-000-111"
/// and "51999000111" are the same person). Input without digits falls back
/// to the trimmed, lowercased text so it still has a stable key.
fn phone_key(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() { phone.trim().to_lowercase() } else { digits }
}

/// Client address for rate limiting: the proxy's `X-Forwarded-For` is
/// honoured only when the peer is loopback (see `yaya_wire::net`).
pub fn client_ip(headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    yaya_wire::net::client_ip(headers, peer)
}

// ------------------------------------------------------- per-business spend

/// What a daily ceiling covers. Each is counted separately because each bills
/// separately.
#[derive(Clone, Copy)]
pub enum Meter {
    /// Chat completions (the tool loop can spend several per customer turn).
    Llm,
    /// Whisper transcriptions.
    Stt,
    /// Piper syntheses — local CPU rather than an API bill, but not free.
    Tts,
    /// Vision-model catalog extractions. Priciest unit of the four (a whole
    /// photo of tokens per call), hence the lowest ceiling.
    Vision,
}

impl Meter {
    fn key(self) -> &'static str {
        match self {
            Meter::Llm => "llm",
            Meter::Stt => "stt",
            Meter::Tts => "tts",
            Meter::Vision => "vision",
        }
    }

    fn ceiling(self) -> i64 {
        let (var, default) = match self {
            Meter::Llm => ("MAX_LLM_CALLS_PER_DAY", 2_000),
            Meter::Stt => ("MAX_STT_CALLS_PER_DAY", 300),
            Meter::Tts => ("MAX_TTS_CALLS_PER_DAY", 2_000),
            Meter::Vision => ("MAX_VISION_CALLS_PER_DAY", 50),
        };
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default)
    }
}

/// Counts one unit against a business's daily allowance and fails once it is
/// spent. The increment and the check are the same statement, so concurrent
/// turns can't both slip past the last unit.
pub async fn charge(db: &sqlx::SqlitePool, business_id: Uuid, meter: Meter) -> anyhow::Result<()> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO usage_counters (business_id, day, kind, n) \
         VALUES ($1, $2, $3, 1) \
         ON CONFLICT (business_id, day, kind) \
         DO UPDATE SET n = n + 1 \
         RETURNING n",
    )
    .bind(business_id)
    .bind(chrono::Utc::now().with_timezone(&chrono_tz::America::Lima).date_naive())
    .bind(meter.key())
    .fetch_one(db)
    .await?;

    let ceiling = meter.ceiling();
    if row.0 > ceiling {
        tracing::warn!(
            business = %business_id, meter = meter.key(), used = row.0, ceiling,
            "daily ceiling exceeded"
        );
        return Err(anyhow!(
            "daily {} limit reached for this business ({ceiling})",
            meter.key()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(per_ip: Quota, per_phone: Quota) -> Limiter {
        let lax = Quota::new(100.0, 3600.0);
        Limiter { buckets: yaya_wire::ratelimit::Limiter::new(), per_ip, per_phone, otp_ip: lax, otp_phone: lax, lead_ip: lax }
    }

    #[test]
    fn bucket_allows_the_burst_then_stops() {
        let l = limiter(Quota::new(3.0, 3600.0), Quota::new(100.0, 3600.0));
        for i in 0..3 {
            assert!(l.allow_registration("1.2.3.4", "+51999000111"), "call {i} should pass");
        }
        assert!(!l.allow_registration("1.2.3.4", "+51999000111"), "burst is spent");
        // A different address has its own bucket.
        assert!(l.allow_registration("5.6.7.8", "+51999000222"));
    }

    #[test]
    fn phone_is_limited_independently_of_address() {
        let l = limiter(Quota::new(100.0, 3600.0), Quota::new(2.0, 86_400.0));
        assert!(l.allow_registration("1.1.1.1", "+51999000111"));
        assert!(l.allow_registration("2.2.2.2", "+51999000111"));
        assert!(
            !l.allow_registration("3.3.3.3", "+51999000111"),
            "rotating the address must not reset the phone's allowance"
        );
    }

    #[test]
    fn phone_matching_ignores_case_and_padding() {
        let l = limiter(Quota::new(100.0, 3600.0), Quota::new(1.0, 86_400.0));
        assert!(l.allow_registration("1.1.1.1", "  +51999000111 "));
        assert!(!l.allow_registration("1.1.1.1", "+51999000111"));
    }

    #[test]
    fn forwarded_for_is_trusted_only_from_the_local_proxy() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        // Our own reverse proxy on loopback: the first hop is the client.
        assert_eq!(client_ip(&h, Some("127.0.0.1:5555".parse().unwrap())), "203.0.113.9");
        // A remote peer setting the header itself does not get to pick its IP.
        assert_eq!(client_ip(&h, Some("10.0.0.7:5555".parse().unwrap())), "10.0.0.7");
        assert_eq!(client_ip(&HeaderMap::new(), Some("10.0.0.7:5555".parse().unwrap())), "10.0.0.7");
        assert_eq!(client_ip(&HeaderMap::new(), None), "unknown");
    }

    #[test]
    fn registration_phone_key_ignores_formatting() {
        // "+51 999-000-111" and "+51999000111" are one phone: spacing must not
        // mint a fresh per-phone allowance.
        let l = limiter(Quota::new(100.0, 3600.0), Quota::new(1.0, 86_400.0));
        assert!(l.allow_registration("1.1.1.1", "+51 999-000-111"));
        assert!(!l.allow_registration("1.1.1.1", "+51999000111"));
        assert!(!l.allow_registration("1.1.1.1", "51 999 000 111"));
    }

    #[test]
    fn phone_key_shapes() {
        assert_eq!(phone_key("+51 999-000-111"), "51999000111");
        assert_eq!(phone_key("  ABC "), "abc");
    }

    #[test]
    fn otp_and_lead_buckets() {
        let tight = Quota::new(1.0, 3600.0);
        let lax = Quota::new(100.0, 3600.0);
        let l = Limiter { buckets: yaya_wire::ratelimit::Limiter::new(), per_ip: lax, per_phone: lax, otp_ip: lax, otp_phone: tight, lead_ip: tight };
        assert!(l.allow_otp("1.1.1.1", "+51999"));
        assert!(!l.allow_otp("2.2.2.2", "+51999"), "per-phone OTP bucket spans addresses");
        assert!(l.allow_otp("2.2.2.2", "+51888"));
        assert!(l.allow_lead("1.1.1.1"));
        assert!(!l.allow_lead("1.1.1.1"));
        assert!(l.allow_lead("9.9.9.9"));
        // Registration and OTP buckets are separate namespaces.
        assert!(l.allow_registration("1.1.1.1", "+51999"));
        let l = Limiter { buckets: yaya_wire::ratelimit::Limiter::new(), per_ip: lax, per_phone: lax, otp_ip: tight, otp_phone: lax, lead_ip: lax };
        assert!(l.allow_otp("3.3.3.3", "+51111"));
        assert!(!l.allow_otp("3.3.3.3", "+51222"), "per-IP OTP bucket spans phones");
    }

    #[test]
    fn from_env_defaults() {
        let l = Limiter::from_env();
        assert!(l.allow_registration("1.1.1.1", "+51000"));
        assert!(l.allow_otp("1.1.1.1", "+51000"));
        assert!(l.allow_lead("1.1.1.1"));
    }

    #[test]
    fn meter_keys_and_default_ceilings() {
        assert_eq!([Meter::Llm, Meter::Stt, Meter::Tts, Meter::Vision].map(|m| m.key()), ["llm", "stt", "tts", "vision"]);
        // Defaults (env unset in tests): vision is the tightest.
        assert!(Meter::Vision.ceiling() < Meter::Stt.ceiling());
        assert!(Meter::Stt.ceiling() < Meter::Llm.ceiling());
    }

    #[tokio::test]
    async fn charge_counts_per_business_meter_and_stops_at_the_ceiling() {
        let db = crate::testkit::db().await;
        let (a, b) = (crate::testkit::business(&db).await, crate::testkit::business(&db).await);
        let ceiling = Meter::Vision.ceiling();
        for _ in 0..ceiling {
            charge(&db, a, Meter::Vision).await.unwrap();
        }
        let e = charge(&db, a, Meter::Vision).await.unwrap_err().to_string();
        assert_eq!(e, format!("daily vision limit reached for this business ({ceiling})"));
        // Other meters and other businesses are untouched.
        charge(&db, a, Meter::Stt).await.unwrap();
        charge(&db, b, Meter::Vision).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT n FROM usage_counters WHERE kind = 'vision' AND business_id = $1").bind(a).fetch_one(&db).await.unwrap();
        assert_eq!(n, ceiling + 1, "the refused call is counted too");
    }

    #[tokio::test]
    async fn concurrent_charges_never_exceed_the_ceiling() {
        let db = crate::testkit::db().await;
        let a = crate::testkit::business(&db).await;
        let ceiling = Meter::Vision.ceiling();
        let tasks: Vec<_> = (0..ceiling + 20).map(|_| { let db = db.clone(); tokio::spawn(async move { charge(&db, a, Meter::Vision).await.is_ok() }) }).collect();
        let mut ok = 0;
        for t in tasks { ok += t.await.unwrap() as i64; }
        assert_eq!(ok, ceiling);
    }
}
