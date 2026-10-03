//! yaya-wire — what every participant of the yaya.tech agent network has to
//! agree on, in one dependency-light crate:
//!
//! * [`id`] — `agent:<ed25519 hex>` identifiers, `did:key` form, base58.
//! * [`keypair`] — the signing half; mints envelopes and request signatures.
//! * [`envelope`] — `{payload, agent, sig}` signed JSON and its verifier.
//! * [`reqsig`] — proof of possession for HTTP: the `X-Agent-Auth` header.
//! * [`secret`] — constant-time compares and secret-shape validation.
//! * [`net`] — client IP behind a trusted reverse proxy.
//! * [`ratelimit`] — token buckets for unauthenticated surfaces.
//! * [`attest`] — Android Key Attestation chain verification with policy.
//!
//! Nothing here touches a database, a socket or a clock it wasn't handed:
//! every function is pure over its inputs so both ends of the wire (the
//! phone core and the gateway) can share it and test it in isolation.

pub mod attest;
pub mod envelope;
pub mod id;
pub mod keypair;
pub mod net;
pub mod ratelimit;
pub mod reqsig;
pub mod secret;

pub use id::AgentId;
pub use keypair::Keypair;

/// Canonical JSON bytes: what every signature in this crate covers.
/// serde_json serialises objects with sorted keys (no `preserve_order`),
/// so the same value always yields the same bytes on every participant.
pub fn canonical(v: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// Hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_sorts_keys_at_every_depth() {
        let a = json!({"b": 1, "a": {"d": [3, {"z": 0, "y": 1}], "c": null}});
        let b: serde_json::Value = serde_json::from_str(r#"{"a":{"c":null,"d":[3,{"y":1,"z":0}]},"b":1}"#).unwrap();
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(canonical(&a), br#"{"a":{"c":null,"d":[3,{"y":1,"z":0}]},"b":1}"#.to_vec());
    }

    #[test]
    fn canonical_scalars_and_arrays_keep_order() {
        assert_eq!(canonical(&json!(null)), b"null".to_vec());
        assert_eq!(canonical(&json!("ñ")), "\"ñ\"".as_bytes().to_vec());
        assert_eq!(canonical(&json!([2, 1])), b"[2,1]".to_vec());
    }

    #[test]
    fn sha256_hex_known_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
