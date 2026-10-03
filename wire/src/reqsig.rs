//! Proof of possession for HTTP requests.
//!
//! A bearer token that is a *public* key proves nothing — anyone who has
//! read the registry knows every id. So every authenticated request also
//! carries a signature over what it is doing:
//!
//! ```text
//! Authorization: Bearer agent:<hex>
//! X-Agent-Auth:  v1.<unix ts>.<nonce hex>.<sig hex>
//!
//! sig = ed25519( "yaya-reqsig-v1\n" METHOD "\n" PATH?QUERY "\n" ts "\n" nonce "\n" BODY_HASH )
//! ```
//!
//! `BODY_HASH` is hex SHA-256 of the body, or `-` when the client streams a
//! body it cannot hash up front (multipart uploads). Either way the nonce is
//! single-use within the time window, so a captured header cannot be replayed.

use ed25519_dalek::{Signature, Verifier};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::{AgentId, Keypair};

pub const HEADER: &str = "x-agent-auth";
pub const VERSION: &str = "v1";
pub const UNHASHED_BODY: &str = "-";
/// Accepted clock skew, seconds, in either direction.
pub const DEFAULT_WINDOW_SECS: u64 = 300;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SigError {
    #[error("X-Agent-Auth header malformed")]
    Malformed,
    #[error("request timestamp outside the accepted window")]
    Expired,
    #[error("nonce already used")]
    Replay,
    #[error("request signature does not match agent")]
    Mismatch,
}

fn message(method: &str, path_and_query: &str, ts: u64, nonce: &str, body_hash: &str) -> Vec<u8> {
    format!("yaya-reqsig-{VERSION}\n{}\n{path_and_query}\n{ts}\n{nonce}\n{body_hash}", method.to_ascii_uppercase()).into_bytes()
}

/// What the client sends. `body` is `Some(bytes)` when the whole body is in
/// hand, `None` for streamed bodies.
pub fn sign(key: &Keypair, method: &str, path_and_query: &str, body: Option<&[u8]>, now_unix: u64) -> String {
    let mut nonce = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    let nonce = hex::encode(nonce);
    let body_hash = body.map(crate::sha256_hex).unwrap_or_else(|| UNHASHED_BODY.into());
    let sig = key.sign_hex(&message(method, path_and_query, now_unix, &nonce, &body_hash));
    format!("{VERSION}.{now_unix}.{nonce}.{sig}")
}

/// Parsed header, before verification.
pub struct Presented<'a> {
    pub ts: u64,
    pub nonce: &'a str,
    sig: Signature,
}

pub fn parse(header: &str) -> Result<Presented<'_>, SigError> {
    let mut it = header.trim().splitn(4, '.');
    let (v, ts, nonce, sig) = (it.next(), it.next(), it.next(), it.next());
    let (Some(VERSION), Some(ts), Some(nonce), Some(sig)) = (v, ts, nonce, sig) else {
        return Err(SigError::Malformed);
    };
    if nonce.len() < 16 || nonce.len() > 64 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SigError::Malformed);
    }
    let ts: u64 = ts.parse().map_err(|_| SigError::Malformed)?;
    let sig = hex::decode(sig).ok().and_then(|b| Signature::from_slice(&b).ok()).ok_or(SigError::Malformed)?;
    Ok(Presented { ts, nonce, sig })
}

/// Verifies a presented header against the request it arrived with. The
/// caller supplies the body hash it computed (or `UNHASHED_BODY`); the
/// signature must match either the exact hash or the unhashed marker —
/// never a different hash.
pub fn verify(
    agent: &AgentId,
    presented: &Presented<'_>,
    method: &str,
    path_and_query: &str,
    body_hash: &str,
    now_unix: u64,
    window_secs: u64,
) -> Result<(), SigError> {
    if now_unix.abs_diff(presented.ts) > window_secs {
        return Err(SigError::Expired);
    }
    let vk = agent.verifying_key();
    for h in [body_hash, UNHASHED_BODY] {
        if vk.verify(&message(method, path_and_query, presented.ts, presented.nonce, h), &presented.sig).is_ok() {
            return Ok(());
        }
    }
    Err(SigError::Mismatch)
}

/// Single-use nonces within the time window. Memory-bounded by sweeping
/// entries older than the window on every insert past a threshold.
pub struct NonceCache {
    window_secs: u64,
    seen: Mutex<HashMap<String, u64>>,
}

impl NonceCache {
    pub fn new(window_secs: u64) -> Self {
        Self { window_secs, seen: Mutex::new(HashMap::new()) }
    }

    /// Records `(agent, nonce)`; `Err(Replay)` if it was already there.
    pub fn claim(&self, agent: &AgentId, nonce: &str, now_unix: u64) -> Result<(), SigError> {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() > 10_000 {
            let cutoff = now_unix.saturating_sub(self.window_secs * 2);
            seen.retain(|_, t| *t >= cutoff);
        }
        let key = format!("{agent}:{nonce}");
        if seen.contains_key(&key) {
            return Err(SigError::Replay);
        }
        seen.insert(key, now_unix);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_and_without_body() {
        let kp = Keypair::generate();
        let body = br#"{"a":1}"#;
        let h = sign(&kp, "post", "/v1/chat/completions", Some(body), 1_000);
        let p = parse(&h).unwrap();
        assert!(verify(&kp.id(), &p, "POST", "/v1/chat/completions", &crate::sha256_hex(body), 1_100, 300).is_ok());
        // Different body, same header: rejected.
        assert_eq!(verify(&kp.id(), &p, "POST", "/v1/chat/completions", &crate::sha256_hex(b"{}"), 1_100, 300), Err(SigError::Mismatch));
        // Different path: rejected.
        assert_eq!(verify(&kp.id(), &p, "POST", "/v1/inbox", &crate::sha256_hex(body), 1_100, 300), Err(SigError::Mismatch));
        // Other agent: rejected.
        assert_eq!(verify(&Keypair::generate().id(), &p, "POST", "/v1/chat/completions", &crate::sha256_hex(body), 1_100, 300), Err(SigError::Mismatch));
        // Too old.
        assert_eq!(verify(&kp.id(), &p, "POST", "/v1/chat/completions", &crate::sha256_hex(body), 2_000, 300), Err(SigError::Expired));
        // Streamed body: signed with the marker, verifies whatever the server hashed.
        let h = sign(&kp, "POST", "/v1/audio/transcriptions", None, 1_000);
        let p = parse(&h).unwrap();
        assert!(verify(&kp.id(), &p, "POST", "/v1/audio/transcriptions", &crate::sha256_hex(b"multipart..."), 1_000, 300).is_ok());
    }

    #[test]
    fn nonces_are_single_use() {
        let kp = Keypair::generate();
        let cache = NonceCache::new(300);
        assert!(cache.claim(&kp.id(), "abcd1234abcd1234", 10).is_ok());
        assert_eq!(cache.claim(&kp.id(), "abcd1234abcd1234", 11), Err(SigError::Replay));
        // Same nonce from another agent is a different key.
        assert!(cache.claim(&Keypair::generate().id(), "abcd1234abcd1234", 11).is_ok());
    }

    #[test]
    fn malformed_headers() {
        assert_eq!(parse("v2.1.abcd.ff").err(), Some(SigError::Malformed));
        assert_eq!(parse("v1.x.abcd1234abcd1234.ff").err(), Some(SigError::Malformed));
        assert_eq!(parse("v1.1.zz.ff").err(), Some(SigError::Malformed));
    }

    #[test]
    fn header_shape() {
        let kp = Keypair::generate();
        let h = sign(&kp, "GET", "/x", None, 42);
        let parts: Vec<&str> = h.split('.').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], VERSION);
        assert_eq!(parts[1], "42");
        assert_eq!(parts[2].len(), 32);
        assert_eq!(parts[3].len(), 128);
        // Fresh nonce every time.
        assert_ne!(h, sign(&kp, "GET", "/x", None, 42));
    }

    #[test]
    fn method_is_case_insensitive() {
        let kp = Keypair::generate();
        let h = sign(&kp, "get", "/x?a=1", Some(b""), 10);
        let p = parse(&h).unwrap();
        assert!(verify(&kp.id(), &p, "GET", "/x?a=1", &crate::sha256_hex(b""), 10, 300).is_ok());
        assert_eq!(verify(&kp.id(), &p, "POST", "/x?a=1", &crate::sha256_hex(b""), 10, 300), Err(SigError::Mismatch));
    }

    #[test]
    fn query_string_is_covered() {
        let kp = Keypair::generate();
        let p_h = sign(&kp, "GET", "/x?a=1", None, 10);
        let p = parse(&p_h).unwrap();
        assert_eq!(verify(&kp.id(), &p, "GET", "/x?a=2", "-", 10, 300), Err(SigError::Mismatch));
    }

    #[test]
    fn window_boundaries_both_directions() {
        let kp = Keypair::generate();
        let h = sign(&kp, "GET", "/", None, 1_000);
        let p = parse(&h).unwrap();
        assert!(verify(&kp.id(), &p, "GET", "/", "-", 1_300, 300).is_ok());
        assert_eq!(verify(&kp.id(), &p, "GET", "/", "-", 1_301, 300), Err(SigError::Expired));
        assert!(verify(&kp.id(), &p, "GET", "/", "-", 700, 300).is_ok());
        assert_eq!(verify(&kp.id(), &p, "GET", "/", "-", 699, 300), Err(SigError::Expired));
    }

    #[test]
    fn hashed_signature_does_not_verify_as_unhashed_for_other_body() {
        // A client that hashed body A cannot be replayed against body B, even
        // though the server also tries the unhashed marker.
        let kp = Keypair::generate();
        let h = sign(&kp, "POST", "/p", Some(b"A"), 5);
        let p = parse(&h).unwrap();
        assert_eq!(verify(&kp.id(), &p, "POST", "/p", UNHASHED_BODY, 5, 300), Err(SigError::Mismatch));
    }

    #[test]
    fn parse_accepts_surrounding_whitespace_and_exposes_fields() {
        let kp = Keypair::generate();
        let h = sign(&kp, "GET", "/", None, 77);
        let padded = format!("  {h}\n");
        let p = parse(&padded).unwrap();
        assert_eq!(p.ts, 77);
        assert_eq!(p.nonce, h.split('.').nth(2).unwrap());
    }

    #[test]
    fn parse_rejects_more_shapes() {
        let sig = "00".repeat(64);
        let n16 = "a".repeat(16);
        assert!(parse("").is_err());
        assert!(parse("v1").is_err());
        assert!(parse(&format!("v1.1.{n16}")).is_err());
        // Nonce length bounds.
        assert!(parse(&format!("v1.1.{}.{sig}", "a".repeat(15))).is_err());
        assert!(parse(&format!("v1.1.{}.{sig}", "a".repeat(65))).is_err());
        assert!(parse(&format!("v1.1.{}.{sig}", "a".repeat(64))).is_ok());
        assert!(parse(&format!("v1.1.{n16}.{sig}")).is_ok());
        // Negative / overflow timestamps.
        assert!(parse(&format!("v1.-1.{n16}.{sig}")).is_err());
        assert!(parse(&format!("v1.99999999999999999999.{n16}.{sig}")).is_err());
        // Signature of the wrong length.
        assert!(parse(&format!("v1.1.{n16}.{}", "00".repeat(63))).is_err());
        // Extra dots end up in the signature and fail to decode.
        assert!(parse(&format!("v1.1.{n16}.{sig}.x")).is_err());
    }

    #[test]
    fn nonce_cache_sweeps_old_entries_but_keeps_recent() {
        let kp = Keypair::generate();
        let cache = NonceCache::new(10);
        for i in 0..10_001u64 {
            cache.claim(&kp.id(), &format!("{i:016x}"), 0).unwrap();
        }
        // Next insert, far in the future, sweeps everything older than 2 windows.
        cache.claim(&kp.id(), "ffffffffffffffff", 1_000).unwrap();
        assert_eq!(cache.seen.lock().unwrap().len(), 1);
        // A swept nonce can be claimed again (its timestamp is outside the window anyway).
        assert!(cache.claim(&kp.id(), &format!("{:016x}", 0), 1_000).is_ok());
        // Recent ones are kept and still replay-protected.
        assert_eq!(cache.claim(&kp.id(), "ffffffffffffffff", 1_001), Err(SigError::Replay));
    }
}
