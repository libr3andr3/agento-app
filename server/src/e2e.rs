//! End-to-end encryption between agents. Every agent already holds an
//! Ed25519 identity; the same key, mapped to Curve25519, gives X25519.
//! A message from A to B is sealed with
//!   key = HKDF-SHA256(x25519(a_sk, b_pk), salt="yaya-e2e-v1", info=A||B)
//!   box = ChaCha20-Poly1305(key, nonce, plaintext, aad=A||B)
//! The relay (gateway) sees only `from`, `to`, and ciphertext — it is a
//! courier that cannot read the mail.

use anyhow::{anyhow, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::VerifyingKey;
use hkdf::Hkdf;
use serde_json::{json, Value};
use sha2::Sha256;

pub const ALG: &str = "x25519-hkdf-sha256-chacha20poly1305";
/// Same key agreement, AES-256-GCM as the cipher: what a browser can do with
/// WebCrypto alone (the console at /app). Either side may pick either; a
/// reply uses the alg the message came in.
pub const ALG_AESGCM: &str = "x25519-hkdf-sha256-aes256gcm";

/// The alg a box was sealed with, if we know it.
pub fn alg_of(sealed: &Value) -> Option<&'static str> {
    match sealed["alg"].as_str() {
        Some(a) if a == ALG => Some(ALG),
        Some(a) if a == ALG_AESGCM => Some(ALG_AESGCM),
        _ => None,
    }
}

/// `agent:<hex ed25519 pk>` → X25519 public key.
pub fn x25519_pk_of(agent_id: &str) -> Result<x25519_dalek::PublicKey> {
    let hex_pk = agent_id
        .strip_prefix("agent:")
        .ok_or_else(|| anyhow!("not an agent id"))?;
    let bytes: [u8; 32] = hex::decode(hex_pk)?
        .try_into()
        .map_err(|_| anyhow!("bad agent id length"))?;
    let vk = VerifyingKey::from_bytes(&bytes)?;
    Ok(x25519_dalek::PublicKey::from(vk.to_montgomery().to_bytes()))
}

fn derive(shared: &[u8; 32], from: &str, to: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(b"yaya-e2e-v1"), shared);
    let mut okm = [0u8; 32];
    let info = format!("{from}|{to}");
    hk.expand(info.as_bytes(), &mut okm).expect("32 bytes is a valid hkdf length");
    okm
}

/// Seal `plaintext` from `from` (whose X25519 secret is `my_sk`) to `to`.
pub fn seal(my_sk: &x25519_dalek::StaticSecret, from: &str, to: &str, plaintext: &[u8]) -> Result<Value> {
    seal_with(my_sk, from, to, plaintext, ALG)
}

/// Seal with a chosen cipher (`ALG` or `ALG_AESGCM`).
pub fn seal_with(my_sk: &x25519_dalek::StaticSecret, from: &str, to: &str, plaintext: &[u8], alg: &str) -> Result<Value> {
    let their = x25519_pk_of(to)?;
    let shared = my_sk.diffie_hellman(&their);
    let key = derive(shared.as_bytes(), from, to);
    let mut nonce = [0u8; 12];
    getrandom_fill(&mut nonce)?;
    let aad = format!("{from}|{to}");
    let ct = match alg {
        ALG => ChaCha20Poly1305::new((&key).into())
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: aad.as_bytes() })
            .map_err(|_| anyhow!("encrypt failed"))?,
        ALG_AESGCM => {
            use aes_gcm::{aead::Aead as _, Aes256Gcm, KeyInit as _};
            Aes256Gcm::new((&key).into())
                .encrypt(aes_gcm::Nonce::from_slice(&nonce), aes_gcm::aead::Payload { msg: plaintext, aad: aad.as_bytes() })
                .map_err(|_| anyhow!("encrypt failed"))?
        }
        _ => return Err(anyhow!("unsupported alg")),
    };
    Ok(json!({
        "alg": alg, "from": from, "to": to,
        "nonce": hex::encode(nonce), "ct": hex::encode(ct),
    }))
}

/// Open a box addressed to `me` (whose X25519 secret is `my_sk`).
pub fn open(my_sk: &x25519_dalek::StaticSecret, me: &str, sealed: &Value) -> Result<Vec<u8>> {
    let alg = alg_of(sealed).ok_or_else(|| anyhow!("unsupported alg"))?;
    let from = sealed["from"].as_str().ok_or_else(|| anyhow!("no from"))?;
    let to = sealed["to"].as_str().ok_or_else(|| anyhow!("no to"))?;
    if to != me {
        return Err(anyhow!("not addressed to me"));
    }
    let their = x25519_pk_of(from)?;
    let shared = my_sk.diffie_hellman(&their);
    let key = derive(shared.as_bytes(), from, to);
    let nonce = hex::decode(sealed["nonce"].as_str().unwrap_or(""))?;
    let ct = hex::decode(sealed["ct"].as_str().unwrap_or(""))?;
    anyhow::ensure!(nonce.len() == 12, "bad nonce length");
    let aad = format!("{from}|{to}");
    match alg {
        ALG_AESGCM => {
            use aes_gcm::{aead::Aead as _, Aes256Gcm, KeyInit as _};
            Aes256Gcm::new((&key).into())
                .decrypt(aes_gcm::Nonce::from_slice(&nonce), aes_gcm::aead::Payload { msg: &ct, aad: aad.as_bytes() })
                .map_err(|_| anyhow!("decrypt failed (wrong key or tampered)"))
        }
        _ => ChaCha20Poly1305::new((&key).into())
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: aad.as_bytes() })
            .map_err(|_| anyhow!("decrypt failed (wrong key or tampered)")),
    }
}

fn getrandom_fill(buf: &mut [u8]) -> Result<()> {
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(buf);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    #[test]
    fn roundtrip_between_two_identities() {
        let a = Identity::ephemeral();
        let b = Identity::ephemeral();
        let boxed = seal(&a.x25519_secret(), &a.id(), &b.id(), b"hola, quiero una cita").unwrap();
        let got = open(&b.x25519_secret(), &b.id(), &boxed).unwrap();
        assert_eq!(got, b"hola, quiero una cita");
        // A third party cannot open it.
        let c = Identity::ephemeral();
        assert!(open(&c.x25519_secret(), &c.id(), &boxed).is_err());
        // Tampering fails.
        let mut t = boxed.clone();
        t["ct"] = json!("00".repeat(40));
        assert!(open(&b.x25519_secret(), &b.id(), &t).is_err());
    }

    /// A box sealed by the web console's WebCrypto code (Ed25519 → X25519
    /// by the birational map, HKDF-SHA256, AES-256-GCM) opens here, and its
    /// envelope verifies. Fixture from `scripts/dev/console-interop.mjs`.
    #[test]
    fn js_console_vector_opens_and_verifies() {
        let fx: Value = serde_json::from_str(include_str!("../tests/fixtures/console-box.json")).unwrap();
        let seed: [u8; 32] = hex::decode(fx["seedB"].as_str().unwrap()).unwrap().try_into().unwrap();
        let b = yaya_wire::Keypair::from_seed(seed);
        assert_eq!(b.id().to_string(), fx["b"].as_str().unwrap(), "same seed, same agent id on both sides");
        let sk = x25519_dalek::StaticSecret::from(b.scalar_bytes());
        let signer = yaya_wire::envelope::verify(&fx["envelope"]).unwrap();
        assert_eq!(signer.to_string(), fx["a"].as_str().unwrap());
        let pt = open(&sk, &b.id().to_string(), &fx["envelope"]["payload"]).unwrap();
        let got: Value = serde_json::from_slice(&pt).unwrap();
        assert_eq!(got, fx["plaintext"]);
        // The phone's reply direction (B → A) is what the console decrypts;
        // the fixture proves the console's own open() with `selfcheck`.
        assert_eq!(fx["selfcheck"].as_str().unwrap(), "{\"ok\":true}");
    }

    #[test]
    fn aes_gcm_variant_roundtrips() {
        let a = Identity::ephemeral();
        let b = Identity::ephemeral();
        let boxed = seal_with(&a.x25519_secret(), &a.id(), &b.id(), b"desde el navegador", ALG_AESGCM).unwrap();
        assert_eq!(alg_of(&boxed), Some(ALG_AESGCM));
        assert_eq!(open(&b.x25519_secret(), &b.id(), &boxed).unwrap(), b"desde el navegador");
        let c = Identity::ephemeral();
        assert!(open(&c.x25519_secret(), &c.id(), &boxed).is_err());
    }

    #[test]
    fn alg_of_known_and_unknown() {
        assert_eq!(alg_of(&json!({"alg": ALG})), Some(ALG));
        assert_eq!(alg_of(&json!({"alg": ALG_AESGCM})), Some(ALG_AESGCM));
        assert_eq!(alg_of(&json!({"alg": "rot13"})), None);
        assert_eq!(alg_of(&json!({})), None);
    }

    #[test]
    fn x25519_pk_of_rejects_bad_ids() {
        assert!(x25519_pk_of("did:key:z6Mk").unwrap_err().to_string().contains("not an agent id"));
        assert!(x25519_pk_of("agent:zz").is_err());
        assert!(x25519_pk_of("agent:abcd").unwrap_err().to_string().contains("length"));
        let a = Identity::ephemeral();
        assert_eq!(x25519_pk_of(&a.id()).unwrap(), x25519_dalek::PublicKey::from(&a.x25519_secret()));
    }

    #[test]
    fn derive_binds_direction() {
        let s = [1u8; 32];
        assert_ne!(derive(&s, "a", "b"), derive(&s, "b", "a"));
        assert_eq!(derive(&s, "a", "b"), derive(&s, "a", "b"));
    }

    #[test]
    fn seal_shape_and_fresh_nonces() {
        let (a, b) = (Identity::ephemeral(), Identity::ephemeral());
        let x = seal(&a.x25519_secret(), &a.id(), &b.id(), b"hi").unwrap();
        let y = seal(&a.x25519_secret(), &a.id(), &b.id(), b"hi").unwrap();
        assert_eq!((x["alg"].as_str(), x["from"].as_str(), x["to"].as_str()), (Some(ALG), Some(a.id().as_str()), Some(b.id().as_str())));
        assert_eq!(x["nonce"].as_str().unwrap().len(), 24);
        assert_ne!(x["nonce"], y["nonce"]);
        assert_ne!(x["ct"], y["ct"]);
        assert!(!x.to_string().contains("hi\""));
        assert!(seal_with(&a.x25519_secret(), &a.id(), &b.id(), b"hi", "rot13").unwrap_err().to_string().contains("unsupported"));
        assert!(seal(&a.x25519_secret(), &a.id(), "agent:nope", b"hi").is_err());
    }

    #[test]
    fn the_sender_can_open_nothing_it_sealed_to_someone_else_as_itself() {
        let (a, b) = (Identity::ephemeral(), Identity::ephemeral());
        let boxed = seal(&a.x25519_secret(), &a.id(), &b.id(), b"x").unwrap();
        assert!(open(&a.x25519_secret(), &a.id(), &boxed).unwrap_err().to_string().contains("not addressed to me"));
    }

    #[test]
    fn open_rejects_every_malformed_field() {
        let (a, b) = (Identity::ephemeral(), Identity::ephemeral());
        let good = seal(&a.x25519_secret(), &a.id(), &b.id(), b"x").unwrap();
        let open_b = |v: &Value| open(&b.x25519_secret(), &b.id(), v);
        let with = |k: &str, v: Value| { let mut t = good.clone(); t[k] = v; t };
        assert!(open_b(&with("alg", json!("x"))).unwrap_err().to_string().contains("unsupported"));
        assert!(open_b(&with("from", Value::Null)).unwrap_err().to_string().contains("no from"));
        assert!(open_b(&with("to", Value::Null)).unwrap_err().to_string().contains("no to"));
        assert!(open_b(&with("nonce", json!("abcd"))).unwrap_err().to_string().contains("nonce length"));
        assert!(open_b(&with("nonce", json!("zz"))).is_err());
        assert!(open_b(&with("ct", json!("zz"))).is_err());
        // Claiming another sender changes the key: it no longer opens.
        let c = Identity::ephemeral();
        assert!(open_b(&with("from", json!(c.id()))).unwrap_err().to_string().contains("decrypt failed"));
        // Switching the cipher label on a ChaCha box fails authentication.
        assert!(open_b(&with("alg", json!(ALG_AESGCM))).is_err());
        assert_eq!(open_b(&good).unwrap(), b"x");
    }

    #[test]
    fn empty_and_large_payloads() {
        let (a, b) = (Identity::ephemeral(), Identity::ephemeral());
        for pt in [vec![], vec![7u8; 1 << 20]] {
            for alg in [ALG, ALG_AESGCM] {
                let boxed = seal_with(&a.x25519_secret(), &a.id(), &b.id(), &pt, alg).unwrap();
                assert_eq!(open(&b.x25519_secret(), &b.id(), &boxed).unwrap(), pt);
            }
        }
    }

    #[test]
    fn getrandom_fill_fills() {
        let mut buf = [0u8; 32];
        getrandom_fill(&mut buf).unwrap();
        assert_ne!(buf, [0u8; 32]);
    }
}
