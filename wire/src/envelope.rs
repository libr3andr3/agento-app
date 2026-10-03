//! Signed JSON envelopes: `{payload, agent, sig}` where `sig` is an Ed25519
//! signature over the canonical bytes of `payload` by the key named in
//! `agent`. Anyone can verify; only the key holder can produce one.

use ed25519_dalek::{Signature, Verifier};
use serde_json::Value;

use crate::AgentId;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvelopeError {
    #[error("bad agent id in envelope")]
    BadAgent,
    #[error("bad signature encoding")]
    BadSignature,
    #[error("signature does not match agent")]
    Mismatch,
}

/// Verifies `env` and returns the signer.
pub fn verify(env: &Value) -> Result<AgentId, EnvelopeError> {
    let agent: AgentId = env["agent"].as_str().unwrap_or("").parse().map_err(|_| EnvelopeError::BadAgent)?;
    let sig = hex::decode(env["sig"].as_str().unwrap_or(""))
        .ok()
        .and_then(|b| Signature::from_slice(&b).ok())
        .ok_or(EnvelopeError::BadSignature)?;
    agent
        .verifying_key()
        .verify(&crate::canonical(&env["payload"]), &sig)
        .map_err(|_| EnvelopeError::Mismatch)?;
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verifies_and_detects_tampering() {
        let kp = crate::Keypair::generate();
        let env = kp.envelope(json!({"hello": "world", "n": 1}));
        assert_eq!(verify(&env).unwrap(), kp.id());
        let mut t = env.clone();
        t["payload"]["n"] = json!(2);
        assert_eq!(verify(&t), Err(EnvelopeError::Mismatch));
        let mut other = env.clone();
        other["agent"] = json!(crate::Keypair::generate().id().to_string());
        assert_eq!(verify(&other), Err(EnvelopeError::Mismatch));
    }

    #[test]
    fn key_order_does_not_matter() {
        // Canonical form sorts keys, so a re-serialised payload still verifies.
        let kp = crate::Keypair::generate();
        let env = kp.envelope(json!({"b": 1, "a": {"z": true, "y": [1, 2]}}));
        let reparsed: Value = serde_json::from_str(&env.to_string()).unwrap();
        assert!(verify(&reparsed).is_ok());
    }

    #[test]
    fn rejects_missing_or_malformed_agent() {
        let kp = crate::Keypair::generate();
        let mut env = kp.envelope(json!({}));
        env.as_object_mut().unwrap().remove("agent");
        assert_eq!(verify(&env), Err(EnvelopeError::BadAgent));
        env["agent"] = json!("agent:nothex");
        assert_eq!(verify(&env), Err(EnvelopeError::BadAgent));
        env["agent"] = json!(42);
        assert_eq!(verify(&env), Err(EnvelopeError::BadAgent));
    }

    #[test]
    fn rejects_missing_or_malformed_signature() {
        let kp = crate::Keypair::generate();
        let mut env = kp.envelope(json!({"a": 1}));
        for bad in [json!(null), json!("zz"), json!("abcd"), json!("00".repeat(63))] {
            env["sig"] = bad.clone();
            assert_eq!(verify(&env), Err(EnvelopeError::BadSignature), "{bad}");
        }
    }

    #[test]
    fn signature_covers_payload_only() {
        // did/alg are informative; changing them does not change validity.
        let kp = crate::Keypair::generate();
        let mut env = kp.envelope(json!({"a": 1}));
        env["did"] = json!("did:key:zforged");
        env["alg"] = json!("rsa");
        env["extra"] = json!(true);
        assert_eq!(verify(&env).unwrap(), kp.id());
    }

    #[test]
    fn missing_payload_is_a_mismatch_not_a_panic() {
        let kp = crate::Keypair::generate();
        let mut env = kp.envelope(json!({"a": 1}));
        env.as_object_mut().unwrap().remove("payload");
        assert_eq!(verify(&env), Err(EnvelopeError::Mismatch));
        // A payload of null that was actually signed as null verifies.
        let env = kp.envelope(json!(null));
        assert!(verify(&env).is_ok());
    }
}
