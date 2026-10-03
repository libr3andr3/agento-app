//! Which enclaves this core will talk to.
//!
//! A pin set names the measurements a backend may present and the model each
//! agento alias maps to. The trust rule is the point of the module: **the
//! gateway distributes pins but can never author them.** If the gateway could
//! hand the phone a pin, a compromised gateway could point it at an enclave
//! whose code reads prompts, and "the gateway cannot read your data" would be
//! false. So the baked set in `pins.json` ships inside the APK, and an update
//! is accepted only when it is signed by a key compiled in at build time
//! (`AGENTO_PIN_KEYS`, comma-separated ed25519 public keys in hex) — kept
//! offline, never on the gateway host — and carries a higher `seq`. No key
//! compiled in means no updates: the baked set is final for that build.

use std::collections::BTreeMap;

use anyhow::{anyhow, ensure, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::snp::Tcb;

const BAKED: &str = include_str!("pins.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PinSet {
    pub seq: u64,
    pub issued: String,
    #[serde(default)]
    pub note: String,
    pub tinfoil: TinfoilPins,
    /// `agento:<name>` → backend id → that backend's model id.
    #[serde(default)]
    pub aliases: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TinfoilPins {
    pub host: String,
    pub router_repo: String,
    pub snp: Vec<Pin>,
    /// Refuse a genuine, correctly measured guest on firmware older than this.
    #[serde(default)]
    pub min_tcb: Option<Tcb>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pin {
    pub release: String,
    /// SEV-SNP launch measurement, 96 hex chars.
    pub measurement: String,
}

impl TinfoilPins {
    /// The release a measurement belongs to, if it is pinned.
    pub fn release_of(&self, measurement: &[u8; 48]) -> Option<&str> {
        let m = hex::encode(measurement);
        self.snp.iter().find(|p| p.measurement.eq_ignore_ascii_case(&m)).map(|p| p.release.as_str())
    }
}

impl PinSet {
    /// The model `backend` serves for an agento alias, if mapped.
    pub fn model(&self, alias: &str, backend: &str) -> Option<&str> {
        self.aliases.get(alias).and_then(|m| m.get(backend)).map(String::as_str)
    }

    fn validate(self) -> Result<Self> {
        ensure!(!self.tinfoil.snp.is_empty(), "pin set pins no Tinfoil measurement");
        for p in &self.tinfoil.snp {
            ensure!(
                p.measurement.len() == 96 && p.measurement.bytes().all(|b| b.is_ascii_hexdigit()),
                "pin {} is not a 48-byte hex measurement", p.release
            );
        }
        Ok(self)
    }
}

/// The set shipped in this build.
pub fn baked() -> PinSet {
    serde_json::from_str::<PinSet>(BAKED).expect("baked pins.json parses").validate().expect("baked pins.json is valid")
}

/// Public keys this build accepts pin updates from.
pub fn trusted_keys() -> Vec<[u8; 32]> {
    option_env!("AGENTO_PIN_KEYS")
        .unwrap_or("")
        .split(',')
        .filter_map(|k| hex::decode(k.trim()).ok())
        .filter_map(|k| <[u8; 32]>::try_from(k).ok())
        .collect()
}

/// Checks a distributed update: `{"pins": base64(json), "sig": base64(ed25519), "key": hex}`.
/// The signature covers the exact decoded bytes; the key must be trusted;
/// `seq` must move forward so an old, since-withdrawn set cannot be replayed.
pub fn verify_update(bundle: &Value, keys: &[[u8; 32]], current_seq: u64) -> Result<PinSet> {
    ensure!(!keys.is_empty(), "this build trusts no pin-signing key; updates are disabled");
    let b64 = base64::engine::general_purpose::STANDARD;
    let bytes = b64.decode(bundle["pins"].as_str().ok_or_else(|| anyhow!("bundle has no pins"))?)?;
    let sig: [u8; 64] = b64
        .decode(bundle["sig"].as_str().ok_or_else(|| anyhow!("bundle has no sig"))?)?
        .try_into()
        .map_err(|_| anyhow!("signature is not 64 bytes"))?;
    let key: [u8; 32] = hex::decode(bundle["key"].as_str().unwrap_or(""))?
        .try_into()
        .map_err(|_| anyhow!("key is not 32 bytes"))?;
    ensure!(keys.contains(&key), "pin update signed by an untrusted key");
    ed25519_dalek::VerifyingKey::from_bytes(&key)?
        .verify_strict(&bytes, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| anyhow!("pin update signature does not verify"))?;
    let set: PinSet = serde_json::from_slice(&bytes).context("pin update body")?;
    ensure!(set.seq > current_seq, "pin update seq {} does not advance past {current_seq}", set.seq);
    set.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use serde_json::json;

    fn bundle(key: &ed25519_dalek::SigningKey, set: &Value) -> Value {
        let bytes = serde_json::to_vec(set).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD;
        json!({"pins": b64.encode(&bytes), "sig": b64.encode(key.sign(&bytes).to_bytes()), "key": hex::encode(key.verifying_key().to_bytes())})
    }

    /// Cross-language check with `scripts/verified/sign-pins.py`:
    /// `SIGNED_PINS_BUNDLE=out.json SIGNED_PINS_KEY=<pub hex> cargo test … -- --ignored`
    #[test]
    #[ignore]
    fn verifies_a_bundle_from_sign_pins_py() {
        let bundle: Value = serde_json::from_str(&std::fs::read_to_string(std::env::var("SIGNED_PINS_BUNDLE").unwrap()).unwrap()).unwrap();
        let key: [u8; 32] = hex::decode(std::env::var("SIGNED_PINS_KEY").unwrap()).unwrap().try_into().unwrap();
        let set = verify_update(&bundle, &[key], 1).unwrap();
        assert_eq!(set.seq, 2);
    }

    #[test]
    fn baked_set_pins_the_verified_router() {
        let p = baked();
        let m: [u8; 48] = hex::decode(&p.tinfoil.snp[0].measurement).unwrap().try_into().unwrap();
        assert_eq!(p.tinfoil.release_of(&m), Some("v0.0.150"));
        assert_eq!(p.model("agento:default", "tinfoil"), Some("deepseek-v4-1-flash"));
    }

    #[test]
    fn updates_need_a_trusted_key_and_a_higher_seq() {
        let good = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let evil = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let trusted = [good.verifying_key().to_bytes()];
        let mut set = serde_json::to_value(baked()).unwrap();
        set["seq"] = json!(2);

        assert_eq!(verify_update(&bundle(&good, &set), &trusted, 1).unwrap().seq, 2);
        assert!(verify_update(&bundle(&evil, &set), &trusted, 1).is_err(), "untrusted key");
        assert!(verify_update(&bundle(&good, &set), &trusted, 2).is_err(), "replay");
        assert!(verify_update(&bundle(&good, &set), &[], 1).is_err(), "no key compiled in");

        let mut tampered = bundle(&good, &set);
        let mut forged = set.clone();
        forged["tinfoil"]["snp"][0]["measurement"] = json!("00".repeat(48));
        tampered["pins"] = json!(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&forged).unwrap()));
        assert!(verify_update(&tampered, &trusted, 1).is_err(), "body swapped under a valid signature");
    }
}
