//! Verified inference: model calls that only reach an enclave this phone
//! has checked itself.
//!
//! A [`VerifiedBackend`] attests a provider's enclave against pinned
//! measurements and seals every request to a key that enclave committed to,
//! so the yaya gateway in the middle routes and meters ciphertext. Each
//! provider does both differently — Tinfoil: SEV-SNP + EHBP; Phala: TDX +
//! E2EE v2; Confidential AI: TDX + an X-Wing tunnel — so sealing stays
//! inside the backend and the trait exposes the sealed round trip.
//!
//! Off unless `LLM_VERIFIED` names a backend. When it is on there is no
//! fallback: a call that cannot be verified fails rather than going out in
//! the clear.

pub mod ehbp;
pub mod pins;
pub mod snp;
pub mod tinfoil;

use std::future::Future;
use std::io::Read;
use std::pin::Pin;

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a backend can carry on its verified path.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Caps {
    pub streaming: bool,
    pub sealed_bodies: bool,
    pub audio: bool,
    pub tools: bool,
    pub embeddings: bool,
}

/// What was verified, for logs and for the app's "verified" indicator.
#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    pub backend: &'static str,
    pub host: String,
    pub platform: &'static str,
    pub product: String,
    pub measurement: String,
    pub release: String,
    pub tcb: snp::Tcb,
    pub verified_at: chrono::DateTime<chrono::Utc>,
}

/// A response off the verified path. `authenticated` is false only for
/// plaintext errors produced along the path (gateway plan limits, auth);
/// anything the enclave said is authenticated.
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub authenticated: bool,
}

pub trait VerifiedBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn caps(&self) -> Caps;
    /// The backend's model for an `agento:*` alias, from the pin set.
    fn model_for<'a>(&'a self, alias: &'a str) -> BoxFut<'a, Option<String>>;
    /// Attest (or reuse a fresh attestation) and report what was verified.
    fn verify(&self) -> BoxFut<'_, Result<Evidence>>;
    /// One chat-completions call, sealed end to end to the verified enclave.
    fn chat<'a>(&'a self, body: &'a Value) -> BoxFut<'a, Result<Reply>>;
}

/// `LLM_VERIFIED=tinfoil` turns verified inference on; unset or `off` leaves
/// the existing path untouched.
pub fn from_env(identity: &crate::identity::Identity) -> Result<Option<Box<dyn VerifiedBackend>>> {
    match std::env::var("LLM_VERIFIED").unwrap_or_default().trim() {
        "" | "0" | "off" | "false" => Ok(None),
        "tinfoil" => Ok(Some(Box::new(tinfoil::Tinfoil::from_env(identity)?))),
        other => bail!("LLM_VERIFIED={other:?} names no verified backend (known: tinfoil)"),
    }
}

/// Inflates a gzip attestation body, refusing anything implausibly large.
pub(crate) fn gunzip(gz: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz).take(64 * 1024).read_to_end(&mut out)?;
    Ok(out)
}
