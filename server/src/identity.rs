//! The agent's own identity: one Ed25519 keypair per installation, minted
//! on first boot. Its public key is the agent id on the network (discovery,
//! inter-agent transactions, gateway auth) — see `yaya_wire`.
//!
//! At rest the seed is sealed under a key-encryption key the shell provides
//! (`IDENTITY_KEK_HEX`, 32 bytes; on Android it lives in the Keystore-backed
//! `SecureStore`). Without a KEK the seed is stored plain — the host binary
//! and tests — and a plain seed is sealed in place the first time a KEK
//! shows up, so upgrades never change who the agent is.

use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::VerifyingKey;
use serde_json::Value;
use yaya_wire::{reqsig, Keypair};

use crate::db::Db;

#[derive(Clone)]
pub struct Identity {
    key: Arc<Keypair>,
}

const KEK_VAR: &str = "IDENTITY_KEK_HEX";
const NONCE_LEN: usize = 12;

pub fn kek_from_env() -> anyhow::Result<Option<[u8; 32]>> {
    parse_kek(std::env::var(KEK_VAR).ok())
}

fn parse_kek(v: Option<String>) -> anyhow::Result<Option<[u8; 32]>> {
    match v {
        Some(h) if !h.trim().is_empty() => {
            let bytes: [u8; 32] = hex::decode(h.trim())?.try_into().map_err(|_| anyhow::anyhow!("{KEK_VAR} must be 32 bytes hex"))?;
            Ok(Some(bytes))
        }
        _ => Ok(None),
    }
}

/// Seals a secret at rest under the device key: `nonce || ciphertext`.
pub fn seal(secret: &[u8], kek: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    let ct = ChaCha20Poly1305::new(kek.into())
        .encrypt(Nonce::from_slice(&nonce), secret)
        .map_err(|_| anyhow::anyhow!("seal failed"))?;
    Ok([nonce.as_slice(), &ct].concat())
}

pub fn open(blob: &[u8], kek: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(blob.len() > NONCE_LEN, "sealed secret too short");
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    ChaCha20Poly1305::new(kek.into())
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow::anyhow!("secret cannot be opened with this key"))
}

/// Stores `secret` sealed when a KEK is configured, plain otherwise (host
/// binary, tests) — the same policy the identity seed follows.
pub fn at_rest(secret: &[u8]) -> anyhow::Result<Vec<u8>> {
    at_rest_with(secret, kek_from_env()?)
}

fn at_rest_with(secret: &[u8], kek: Option<[u8; 32]>) -> anyhow::Result<Vec<u8>> {
    match kek {
        Some(k) => seal(secret, &k),
        None => Ok(secret.to_vec()),
    }
}

/// Inverse of [`at_rest`]. A plain value is returned as is.
pub fn from_rest(blob: &[u8], plain_len: Option<usize>) -> anyhow::Result<Vec<u8>> {
    from_rest_with(blob, plain_len, kek_from_env()?)
}

fn from_rest_with(blob: &[u8], plain_len: Option<usize>, kek: Option<[u8; 32]>) -> anyhow::Result<Vec<u8>> {
    match (kek, plain_len) {
        (Some(k), Some(n)) if blob.len() != n => open(blob, &k),
        (Some(k), None) => open(blob, &k),
        _ => Ok(blob.to_vec()),
    }
}

fn seal_seed(seed: &[u8; 32], kek: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    seal(seed, kek)
}

fn open_seed(blob: &[u8], kek: &[u8; 32]) -> anyhow::Result<[u8; 32]> {
    open(blob, kek)?.try_into().map_err(|_| anyhow::anyhow!("sealed identity has the wrong length"))
}

impl Identity {
    pub async fn load_or_create(db: &Db) -> anyhow::Result<Self> {
        Self::load_or_create_with(db, kek_from_env()?).await
    }

    async fn load_or_create_with(db: &Db, kek: Option<[u8; 32]>) -> anyhow::Result<Self> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT secret_key FROM agent_identity WHERE id = 1").fetch_optional(db).await?;
        let seed: [u8; 32] = match (row, kek) {
            (Some((sk,)), _) if sk.len() == 32 => {
                let seed: [u8; 32] = sk.try_into().expect("checked length");
                if let Some(k) = kek {
                    sqlx::query("UPDATE agent_identity SET secret_key = $1 WHERE id = 1").bind(seal_seed(&seed, &k)?).execute(db).await?;
                    tracing::info!("agent identity sealed under the device key");
                }
                seed
            }
            (Some((sk,)), Some(k)) => open_seed(&sk, &k)?,
            (Some(_), None) => anyhow::bail!("agent identity is sealed but {KEK_VAR} is not set"),
            (None, kek) => {
                let kp = Keypair::generate();
                let seed = kp.seed();
                let stored = match kek { Some(k) => seal_seed(&seed, &k)?, None => seed.to_vec() };
                sqlx::query("INSERT INTO agent_identity (id, public_key, secret_key) VALUES (1, $1, $2)")
                    .bind(kp.id().bytes().to_vec()).bind(stored).execute(db).await?;
                tracing::info!(agent = %kp.id(), sealed = kek.is_some(), "minted agent identity");
                seed
            }
        };
        Ok(Self::from_seed(seed))
    }

    /// Unpersisted identity (CLI clients, tests).
    pub fn ephemeral() -> Self {
        Self { key: Arc::new(Keypair::generate()) }
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { key: Arc::new(Keypair::from_seed(seed)) }
    }

    pub fn seed(&self) -> [u8; 32] {
        self.key.seed()
    }

    /// The same key on Curve25519, for E2E boxes (see e2e.rs).
    pub fn x25519_secret(&self) -> x25519_dalek::StaticSecret {
        x25519_dalek::StaticSecret::from(self.key.scalar_bytes())
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// `agent:<hex pubkey>` — stable, URL-safe, self-certifying.
    pub fn id(&self) -> String {
        self.key.id().to_string()
    }

    pub fn did(&self) -> String {
        self.key.did()
    }

    pub fn sign(&self, msg: &[u8]) -> String {
        self.key.sign_hex(msg)
    }

    /// Signed envelope for anything the agent publishes (cards, reviews).
    pub fn envelope(&self, payload: Value) -> Value {
        self.key.envelope(payload)
    }

    /// The two headers that authenticate an HTTP request as this agent:
    /// the bearer (who) and the request signature (proof).
    pub fn auth_headers(&self, method: &str, path_and_query: &str, body: Option<&[u8]>) -> [(&'static str, String); 2] {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        [
            ("authorization", format!("Bearer {}", self.id())),
            (reqsig::HEADER, reqsig::sign(&self.key, method, path_and_query, body, now)),
        ]
    }
}

/// Adds this identity's auth headers to a request. `path_and_query` must be
/// exactly what the server will see (including `?query`).
pub fn signed(mut req: reqwest::RequestBuilder, id: &Identity, method: &str, path_and_query: &str, body: Option<&[u8]>) -> reqwest::RequestBuilder {
    for (k, v) in id.auth_headers(method, path_and_query, body) {
        req = req.header(k, v);
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_signature_verifies() {
        let id = Identity::ephemeral();
        let env = id.envelope(json!({"hello": "world"}));
        assert_eq!(yaya_wire::envelope::verify(&env).unwrap().to_string(), id.id());
        assert!(env["did"].as_str().unwrap().starts_with("did:key:z6Mk"));
    }

    #[test]
    fn seed_seals_and_opens() {
        let kek = [7u8; 32];
        let seed = Identity::ephemeral().seed();
        let blob = seal_seed(&seed, &kek).unwrap();
        assert_ne!(blob.len(), 32, "sealed form must not be mistaken for a plain seed");
        assert_eq!(open_seed(&blob, &kek).unwrap(), seed);
        assert!(open_seed(&blob, &[8u8; 32]).is_err());
    }

    #[test]
    fn auth_headers_verify_as_reqsig() {
        let id = Identity::ephemeral();
        let [(_, bearer), (_, sig)] = id.auth_headers("GET", "/v1/inbox?wait=25", None);
        assert_eq!(bearer, format!("Bearer {}", id.id()));
        let p = reqsig::parse(&sig).unwrap();
        let agent: yaya_wire::AgentId = id.id().parse().unwrap();
        assert!(reqsig::verify(&agent, &p, "GET", "/v1/inbox?wait=25", "-", p.ts, 30).is_ok());
    }

    const KEK: [u8; 32] = [9u8; 32];

    #[test]
    fn parse_kek_shapes() {
        assert_eq!(parse_kek(None).unwrap(), None);
        assert_eq!(parse_kek(Some("  ".into())).unwrap(), None);
        assert_eq!(parse_kek(Some(format!(" {} ", "09".repeat(32)))).unwrap(), Some(KEK));
        assert!(parse_kek(Some("zz".into())).is_err());
        assert!(parse_kek(Some("09".repeat(31))).unwrap_err().to_string().contains("32 bytes"));
    }

    #[test]
    fn seal_is_randomised_and_authenticated() {
        let a = seal(b"s", &KEK).unwrap();
        let b = seal(b"s", &KEK).unwrap();
        assert_ne!(a, b, "fresh nonce every time");
        assert_eq!(a.len(), NONCE_LEN + 1 + 16);
        let mut t = a.clone();
        *t.last_mut().unwrap() ^= 1;
        assert!(open(&t, &KEK).unwrap_err().to_string().contains("cannot be opened"));
        assert!(open(&a[..NONCE_LEN], &KEK).unwrap_err().to_string().contains("too short"));
        assert_eq!(open(&a, &KEK).unwrap(), b"s");
    }

    #[test]
    fn at_rest_and_back() {
        assert_eq!(at_rest_with(b"plain", None).unwrap(), b"plain");
        let sealed = at_rest_with(b"0123456789abcdef0123456789abcdef", Some(KEK)).unwrap();
        assert_ne!(sealed.len(), 32);
        assert_eq!(from_rest_with(&sealed, Some(32), Some(KEK)).unwrap(), b"0123456789abcdef0123456789abcdef");
        assert_eq!(from_rest_with(&sealed, None, Some(KEK)).unwrap(), b"0123456789abcdef0123456789abcdef");
        // A value stored before the KEK existed has the plain length: read as is.
        assert_eq!(from_rest_with(&[5u8; 32], Some(32), Some(KEK)).unwrap(), vec![5u8; 32]);
        // No KEK: always plain.
        assert_eq!(from_rest_with(&sealed, None, None).unwrap(), sealed);
        assert!(from_rest_with(b"garbage-that-is-long-enough", None, Some(KEK)).is_err());
    }

    #[test]
    fn open_seed_rejects_wrong_length() {
        let blob = seal(b"short", &KEK).unwrap();
        assert!(open_seed(&blob, &KEK).unwrap_err().to_string().contains("wrong length"));
    }

    #[tokio::test]
    async fn identity_is_minted_once_and_stable() {
        let db = crate::testkit::db().await;
        let a = Identity::load_or_create_with(&db, None).await.unwrap();
        let b = Identity::load_or_create_with(&db, None).await.unwrap();
        assert_eq!(a.id(), b.id());
        let (pk, sk): (Vec<u8>, Vec<u8>) = sqlx::query_as("SELECT public_key, secret_key FROM agent_identity").fetch_one(&db).await.unwrap();
        assert_eq!(hex::encode(pk), a.id().trim_start_matches("agent:"));
        assert_eq!(sk, a.seed().to_vec(), "no KEK: plain");
    }

    #[tokio::test]
    async fn plain_seed_is_sealed_in_place_when_a_kek_appears() {
        let db = crate::testkit::db().await;
        let plain = Identity::load_or_create_with(&db, None).await.unwrap();
        let sealed = Identity::load_or_create_with(&db, Some(KEK)).await.unwrap();
        assert_eq!(plain.id(), sealed.id(), "upgrades never change who the agent is");
        let (sk,): (Vec<u8>,) = sqlx::query_as("SELECT secret_key FROM agent_identity").fetch_one(&db).await.unwrap();
        assert_ne!(sk.len(), 32);
        assert_eq!(Identity::load_or_create_with(&db, Some(KEK)).await.unwrap().id(), plain.id());
        // Sealed but the KEK is gone, or a different one: refuse to boot as someone else.
        assert!(Identity::load_or_create_with(&db, None).await.err().unwrap().to_string().contains("IDENTITY_KEK_HEX"));
        assert!(Identity::load_or_create_with(&db, Some([1; 32])).await.is_err());
    }

    #[tokio::test]
    async fn fresh_identity_under_a_kek_is_sealed_from_the_start() {
        let db = crate::testkit::db().await;
        let id = Identity::load_or_create_with(&db, Some(KEK)).await.unwrap();
        let (sk,): (Vec<u8>,) = sqlx::query_as("SELECT secret_key FROM agent_identity").fetch_one(&db).await.unwrap();
        assert_eq!(open_seed(&sk, &KEK).unwrap(), id.seed());
    }

    #[test]
    fn accessors_agree() {
        let id = Identity::from_seed([3; 32]);
        assert_eq!(Identity::from_seed(id.seed()).id(), id.id());
        assert_eq!(id.did(), yaya_wire::Keypair::from_seed([3; 32]).did());
        assert_eq!(hex::encode(id.public_key().to_bytes()), id.id().trim_start_matches("agent:"));
        let sig = id.sign(b"m");
        let sig = ed25519_dalek::Signature::from_slice(&hex::decode(sig).unwrap()).unwrap();
        use ed25519_dalek::Verifier;
        assert!(id.public_key().verify(b"m", &sig).is_ok());
        assert_ne!(Identity::ephemeral().id(), Identity::ephemeral().id());
        // The X25519 key is the Montgomery form of the Ed25519 one.
        let x_pub = x25519_dalek::PublicKey::from(&id.x25519_secret());
        assert_eq!(x_pub.as_bytes(), &id.public_key().to_montgomery().to_bytes());
    }

    #[tokio::test]
    async fn signed_adds_both_headers() {
        let id = Identity::ephemeral();
        let req = signed(reqwest::Client::new().post("http://x/p?q=1"), &id, "POST", "/p?q=1", Some(b"{}")).build().unwrap();
        assert_eq!(req.headers()["authorization"], format!("Bearer {}", id.id()).as_str());
        let h = req.headers()[reqsig::HEADER].to_str().unwrap();
        let p = reqsig::parse(h).unwrap();
        let agent: yaya_wire::AgentId = id.id().parse().unwrap();
        assert!(reqsig::verify(&agent, &p, "POST", "/p?q=1", &yaya_wire::sha256_hex(b"{}"), p.ts, 30).is_ok());
    }
}
