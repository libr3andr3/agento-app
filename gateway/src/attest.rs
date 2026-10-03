//! The gateway's side of key attestation: pinned Google roots, Google's
//! revocation list (cached), and the policy from the environment. The
//! verification itself lives in `yaya_wire::attest` so any verifier can
//! recompute our verdicts.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use yaya_wire::attest::{Policy, Revocation, Roots, Verdict};

const STATUS_URL: &str = "https://android.googleapis.com/attestation/status";

pub struct Attestation {
    roots: Roots,
    policy: Policy,
    http: reqwest::Client,
    status_url: String,
    revocations: Mutex<Option<(Instant, Value)>>,
}

impl Attestation {
    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Self> {
        let roots = Roots::from_pem(include_str!("../roots/google_attestation_roots.pem")).map_err(|e| anyhow::anyhow!(e))?;
        let policy = Policy::from_lists(
            &crate::env_or("EXPECTED_PACKAGE", "yaya.tech.agento,yaya.tech.agento.business,yaya.tech.agente,yaya.tech.agente.business"),
            &crate::env_or("EXPECTED_SIGNERS", ""),
        );
        if policy.signers.is_empty() {
            tracing::warn!("EXPECTED_SIGNERS is empty: attested apps are matched by package name only");
        }
        Ok(Self { roots, policy, http, status_url: crate::env_or("ATTEST_STATUS_URL", STATUS_URL), revocations: Mutex::new(None) })
    }

    /// Google's list, cached for an hour. Unreachable = `None`, which the
    /// verifier reports as unknown and never treats as OK.
    async fn revocation_list(&self) -> Option<Value> {
        {
            let c = self.revocations.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((t, v)) = c.as_ref() {
                if t.elapsed() < Duration::from_secs(3600) {
                    return Some(v.clone());
                }
            }
        }
        let v = self.http.get(&self.status_url).timeout(Duration::from_secs(10)).send().await.ok()?.json::<Value>().await.ok()?;
        *self.revocations.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        Some(v)
    }

    pub async fn verify(&self, agent_id: &str, chain_b64: &[String]) -> Verdict {
        let list = self.revocation_list().await;
        yaya_wire::attest::verify(&self.roots, agent_id, chain_b64, &self.policy, |serial| revocation_of(list.as_ref(), serial))
    }
}

/// A serial against Google's list: absent from a list we hold is OK, any
/// status other than OK is revoked, and no list at all is unknown.
fn revocation_of(list: Option<&Value>, serial: &str) -> Revocation {
    match list {
        None => Revocation::Unknown,
        Some(l) => match l["entries"][serial]["status"].as_str() {
            None | Some("OK") => Revocation::Ok,
            Some(_) => Revocation::Revoked,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Mock;
    use serde_json::json;

    #[test]
    fn serials_map_to_ok_revoked_or_unknown() {
        let l = json!({"entries": {"abc": {"status": "REVOKED"}, "def": {"status": "OK"}, "ghi": {"status": "SUSPENDED"}}});
        assert!(matches!(revocation_of(Some(&l), "abc"), Revocation::Revoked));
        assert!(matches!(revocation_of(Some(&l), "ghi"), Revocation::Revoked));
        assert!(matches!(revocation_of(Some(&l), "def"), Revocation::Ok));
        assert!(matches!(revocation_of(Some(&l), "zzz"), Revocation::Ok), "not listed = not revoked");
        assert!(matches!(revocation_of(None, "abc"), Revocation::Unknown), "no list is never OK");
    }

    #[tokio::test]
    async fn the_list_is_cached_and_junk_chains_are_unverified() {
        let m = Mock::start().await;
        m.on("/status", json!({"entries": {}}));
        let mut a = Attestation::from_env(reqwest::Client::new()).unwrap();
        a.status_url = format!("{}/status", m.base);
        let v = a.verify("agent:x", &["not base64!".into()]).await;
        assert!(!v.verified && v.reason.is_some());
        a.verify("agent:x", &[]).await;
        assert_eq!(m.seen_path("/status").len(), 1, "fetched once, then cached for the hour");
        a.status_url = "http://127.0.0.1:9/status".into();
        *a.revocations.lock().unwrap() = None;
        assert!(a.revocation_list().await.is_none(), "unreachable list is None, not an empty OK list");
    }
}

pub use yaya_wire::attest::summary;
