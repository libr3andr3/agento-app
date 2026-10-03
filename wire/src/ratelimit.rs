//! In-memory token buckets for unauthenticated surfaces (registration,
//! login, OTP, lead capture). `burst` calls allowed, refilling completely
//! over `window_secs`. Per-process by design: the cheap brake in front of
//! the database-backed budgets.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct Quota {
    pub burst: f64,
    pub window_secs: f64,
}

impl Quota {
    pub const fn new(burst: f64, window_secs: f64) -> Self {
        Self { burst, window_secs }
    }

    /// `var` overrides the burst; the window is fixed by the caller.
    pub fn from_env(var: &str, default_burst: f64, window_secs: f64) -> Self {
        let burst = std::env::var(var).ok().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0).unwrap_or(default_burst);
        Self { burst, window_secs }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one token from `key`'s bucket under `q`; false when empty.
    pub fn take(&self, key: &str, q: Quota) -> bool {
        let now = Instant::now();
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        // Bound the map: a bucket that has refilled completely carries no
        // state worth keeping, so drop those before inserting anything new.
        if map.len() > 10_000 {
            map.retain(|_, b| b.tokens + now.duration_since(b.last).as_secs_f64() * (q.burst / q.window_secs) < q.burst);
        }
        let bucket = map.entry(key.to_string()).or_insert(Bucket { tokens: q.burst, last: now });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * (q.burst / q.window_secs)).min(q.burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_empty_and_keys_are_independent() {
        let l = Limiter::new();
        let q = Quota::new(2.0, 3600.0);
        assert!(l.take("a", q));
        assert!(l.take("a", q));
        assert!(!l.take("a", q));
        assert!(l.take("b", q));
    }

    #[test]
    fn quota_new_is_const() {
        const Q: Quota = Quota::new(5.0, 60.0);
        assert_eq!((Q.burst, Q.window_secs), (5.0, 60.0));
    }

    #[test]
    fn quota_from_env_overrides_only_valid_positive_bursts() {
        // Unique names: tests run in parallel and share the environment.
        let var = "YAYA_WIRE_TEST_QUOTA_FROM_ENV";
        std::env::remove_var(var);
        assert_eq!(Quota::from_env(var, 3.0, 60.0).burst, 3.0);
        std::env::set_var(var, "9");
        let q = Quota::from_env(var, 3.0, 60.0);
        assert_eq!((q.burst, q.window_secs), (9.0, 60.0));
        for bad in ["0", "-1", "abc", ""] {
            std::env::set_var(var, bad);
            assert_eq!(Quota::from_env(var, 3.0, 60.0).burst, 3.0, "{bad}");
        }
        std::env::remove_var(var);
    }

    #[test]
    fn refills_over_the_window() {
        let l = Limiter::new();
        // One token, refilled fully every 50 ms.
        let q = Quota::new(1.0, 0.05);
        assert!(l.take("k", q));
        assert!(!l.take("k", q));
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(l.take("k", q));
    }

    #[test]
    fn fractional_burst_below_one_never_allows() {
        let l = Limiter::new();
        assert!(!l.take("k", Quota::new(0.5, 3600.0)));
    }

    #[test]
    fn sweeps_refilled_buckets_past_ten_thousand_keys() {
        let l = Limiter::new();
        let fast = Quota::new(1.0, 0.001);
        for i in 0..10_001 {
            l.take(&format!("k{i}"), fast);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        l.take("trigger", fast);
        assert!(l.buckets.lock().unwrap().len() < 100);
        // Buckets still draining survive the sweep.
        let slow = Quota::new(1.0, 3600.0);
        let l = Limiter::new();
        for i in 0..10_001 {
            l.take(&format!("k{i}"), slow);
        }
        l.take("trigger", slow);
        assert!(l.buckets.lock().unwrap().len() > 10_000);
    }
}
