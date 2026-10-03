//! The private half of an identity. Kept deliberately small: it can tell you
//! who it is, sign bytes, and produce the two signed shapes the network
//! uses (envelopes and request signatures). Persistence is the caller's job.

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

use crate::AgentId;

pub struct Keypair {
    signing: SigningKey,
}

impl Keypair {
    pub fn generate() -> Self {
        Self { signing: SigningKey::generate(&mut rand_core::OsRng) }
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(&seed) }
    }

    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn id(&self) -> AgentId {
        AgentId::from_key(&self.signing.verifying_key())
    }

    pub fn did(&self) -> String {
        self.id().did()
    }

    pub fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.signing.verifying_key()
    }

    /// The Ed25519 scalar, for callers that derive an X25519 secret from it.
    pub fn scalar_bytes(&self) -> [u8; 32] {
        self.signing.to_scalar_bytes()
    }

    /// Hex signature over raw bytes.
    pub fn sign_hex(&self, msg: &[u8]) -> String {
        hex::encode(self.signing.sign(msg).to_bytes())
    }

    /// `{payload, agent, did, sig, alg}` — sig covers the canonical payload.
    pub fn envelope(&self, payload: Value) -> Value {
        let sig = self.sign_hex(&crate::canonical(&payload));
        json!({
            "payload": payload,
            "agent": self.id().to_string(),
            "did": self.did(),
            "sig": sig,
            "alg": "ed25519",
        })
    }
}

impl Clone for Keypair {
    fn clone(&self) -> Self {
        Self { signing: self.signing.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};
    use serde_json::json;

    const SEED: [u8; 32] = [7u8; 32];

    #[test]
    fn from_seed_is_deterministic_and_seed_roundtrips() {
        let a = Keypair::from_seed(SEED);
        let b = Keypair::from_seed(SEED);
        assert_eq!(a.id(), b.id());
        assert_eq!(a.seed(), SEED);
        assert_eq!(Keypair::from_seed(a.seed()).id(), a.id());
    }

    #[test]
    fn generate_gives_distinct_identities() {
        assert_ne!(Keypair::generate().id(), Keypair::generate().id());
    }

    #[test]
    fn id_did_and_verifying_key_agree() {
        let kp = Keypair::from_seed(SEED);
        assert_eq!(kp.id().verifying_key(), kp.verifying_key());
        assert_eq!(kp.did(), kp.id().did());
        assert_eq!(kp.id(), AgentId::from_key(&kp.verifying_key()));
    }

    #[test]
    fn scalar_bytes_are_stable_and_differ_from_seed() {
        let kp = Keypair::from_seed(SEED);
        assert_eq!(kp.scalar_bytes(), Keypair::from_seed(SEED).scalar_bytes());
        assert_ne!(kp.scalar_bytes(), SEED);
        // RFC 8032: the lower half of SHA-512(seed), unclamped (X25519 clamps on use).
        use sha2::Digest;
        let h = sha2::Sha512::digest(SEED);
        assert_eq!(kp.scalar_bytes()[..], h[..32]);
    }

    #[test]
    fn sign_hex_is_deterministic_and_verifies() {
        let kp = Keypair::from_seed(SEED);
        let sig = kp.sign_hex(b"hola");
        assert_eq!(sig.len(), 128);
        assert_eq!(sig, kp.sign_hex(b"hola"));
        let sig = Signature::from_slice(&hex::decode(sig).unwrap()).unwrap();
        assert!(kp.verifying_key().verify(b"hola", &sig).is_ok());
        assert!(kp.verifying_key().verify(b"chau", &sig).is_err());
    }

    #[test]
    fn envelope_has_every_field_and_verifies() {
        let kp = Keypair::from_seed(SEED);
        let env = kp.envelope(json!({"x": 1}));
        assert_eq!(env["payload"], json!({"x": 1}));
        assert_eq!(env["agent"], json!(kp.id().to_string()));
        assert_eq!(env["did"], json!(kp.did()));
        assert_eq!(env["alg"], json!("ed25519"));
        assert_eq!(env["sig"], json!(kp.sign_hex(&crate::canonical(&json!({"x": 1})))));
        assert_eq!(crate::envelope::verify(&env).unwrap(), kp.id());
    }

    #[test]
    fn clone_keeps_the_identity() {
        let kp = Keypair::from_seed(SEED);
        let c = kp.clone();
        assert_eq!(c.id(), kp.id());
        assert_eq!(c.sign_hex(b"m"), kp.sign_hex(b"m"));
    }
}
