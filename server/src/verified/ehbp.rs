//! EHBP — the Encrypted HTTP Body Protocol (tinfoilsh/encrypted-http-body-protocol).
//!
//! The request body is sealed with HPKE (X25519 / HKDF-SHA256 / AES-256-GCM,
//! info `"ehbp request"`) to the enclave's attested key; method, path and
//! headers stay readable so the yaya gateway can still route and meter. The
//! response is sealed under keys derived from the same HPKE context the way
//! OHTTP (RFC 9458) does it, so only this request's sender can open it.
//!
//! Framing, both directions: `u32 big-endian length || ciphertext`, repeated;
//! a zero-length chunk is an empty write and carries no ciphertext.

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use anyhow::{anyhow, bail, ensure, Result};
use hkdf::Hkdf;
use hpke::{aead::AesGcm256, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable, Kem, OpModeS, Serializable};
use sha2::Sha256;

pub const ENC_HEADER: &str = "Ehbp-Encapsulated-Key";
pub const NONCE_HEADER: &str = "Ehbp-Response-Nonce";

const REQUEST_INFO: &[u8] = b"ehbp request";
const EXPORT_LABEL: &[u8] = b"ehbp response";
const RESPONSE_NONCE_LEN: usize = 32;

/// A sealed request, holding what is needed to open its response.
pub struct Sealed {
    enc: [u8; 32],
    exported: [u8; 32],
    pub body: Vec<u8>,
}

impl Sealed {
    /// The `Ehbp-Encapsulated-Key` header value.
    pub fn enc_header(&self) -> String {
        hex::encode(self.enc)
    }

    /// Opens a response. `nonce_header` must be present on anything the
    /// caller treats as authentic — a missing nonce on a 2xx is a downgrade
    /// (someone stripped it and substituted plaintext), not a plaintext reply.
    pub fn open_response(&self, nonce_header: &str, body: &[u8]) -> Result<Vec<u8>> {
        let nonce = hex::decode(nonce_header.trim()).map_err(|_| anyhow!("{NONCE_HEADER} is not hex"))?;
        ensure!(nonce.len() == RESPONSE_NONCE_LEN, "{NONCE_HEADER} must be {RESPONSE_NONCE_LEN} bytes");
        let mut salt = Vec::with_capacity(64);
        salt.extend_from_slice(&self.enc);
        salt.extend_from_slice(&nonce);
        let hk = Hkdf::<Sha256>::new(Some(&salt), &self.exported);
        let (mut key, mut base) = ([0u8; 32], [0u8; 12]);
        hk.expand(b"key", &mut key).map_err(|_| anyhow!("hkdf expand"))?;
        hk.expand(b"nonce", &mut base).map_err(|_| anyhow!("hkdf expand"))?;
        let aead = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("aes key"))?;

        let mut out = Vec::with_capacity(body.len());
        let mut seq: u64 = 0;
        for chunk in frames(body)? {
            let n = chunk_nonce(&base, seq);
            let pt = aead.decrypt(Nonce::from_slice(&n), chunk).map_err(|_| anyhow!("response chunk {seq} failed authentication"))?;
            out.extend_from_slice(&pt);
            seq = seq.checked_add(1).ok_or_else(|| anyhow!("response chunk sequence overflow"))?;
        }
        Ok(out)
    }
}

/// Seals `plaintext` to the enclave's X25519 public key.
pub fn seal(server_pk: &[u8; 32], plaintext: &[u8]) -> Result<Sealed> {
    let pk = <X25519HkdfSha256 as Kem>::PublicKey::from_bytes(server_pk).map_err(|e| anyhow!("HPKE public key: {e:?}"))?;
    let (encapped, mut ctx) = hpke::setup_sender::<AesGcm256, HkdfSha256, X25519HkdfSha256, _>(
        &OpModeS::Base,
        &pk,
        REQUEST_INFO,
        &mut rand::rngs::OsRng,
    )
    .map_err(|e| anyhow!("HPKE setup: {e:?}"))?;
    // The exporter secret comes from the key schedule, not the sealing
    // sequence, so it can be taken now and the context dropped.
    let mut exported = [0u8; 32];
    ctx.export(EXPORT_LABEL, &mut exported).map_err(|e| anyhow!("HPKE export: {e:?}"))?;
    let ct = ctx.seal(plaintext, b"").map_err(|e| anyhow!("HPKE seal: {e:?}"))?;
    let mut body = Vec::with_capacity(4 + ct.len());
    body.extend_from_slice(&u32::try_from(ct.len()).map_err(|_| anyhow!("request too large"))?.to_be_bytes());
    body.extend_from_slice(&ct);
    let enc: [u8; 32] = encapped.to_bytes().as_slice().try_into().map_err(|_| anyhow!("X25519 enc is 32 bytes"))?;
    Ok(Sealed { enc, exported, body })
}

/// `nonce = base XOR seq`, the sequence big-endian in the last 8 bytes.
fn chunk_nonce(base: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut n = *base;
    for (i, b) in seq.to_be_bytes().iter().enumerate() {
        n[4 + i] ^= b;
    }
    n
}

/// Splits a framed body into its non-empty ciphertext chunks, refusing a
/// truncated length prefix or chunk.
fn frames(mut body: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        if body.len() < 4 {
            bail!("truncated EHBP length prefix");
        }
        let len = u32::from_be_bytes(body[..4].try_into().expect("4 bytes")) as usize;
        body = &body[4..];
        if body.len() < len {
            bail!("truncated EHBP chunk ({} of {len} bytes)", body.len());
        }
        if len > 0 {
            out.push(&body[..len]);
        }
        body = &body[len..];
    }
    Ok(out)
}

/// The server half, for tests: what an EHBP server does with a request.
#[cfg(test)]
pub(crate) mod server {
    use super::*;
    use hpke::OpModeR;

    pub fn keypair() -> ([u8; 32], [u8; 32]) {
        let (sk, pk) = X25519HkdfSha256::gen_keypair(&mut rand::rngs::OsRng);
        (sk.to_bytes().as_slice().try_into().unwrap(), pk.to_bytes().as_slice().try_into().unwrap())
    }

    /// Returns the plaintext and the exporter secret the response keys use.
    pub fn open_request(sk: &[u8; 32], enc_hex: &str, body: &[u8]) -> Result<(Vec<u8>, [u8; 32])> {
        let sk = <X25519HkdfSha256 as Kem>::PrivateKey::from_bytes(sk).map_err(|e| anyhow!("{e:?}"))?;
        let enc_bytes = hex::decode(enc_hex)?;
        let enc = <X25519HkdfSha256 as Kem>::EncappedKey::from_bytes(&enc_bytes).map_err(|e| anyhow!("{e:?}"))?;
        let mut ctx = hpke::setup_receiver::<AesGcm256, HkdfSha256, X25519HkdfSha256>(&OpModeR::Base, &sk, &enc, REQUEST_INFO)
            .map_err(|e| anyhow!("{e:?}"))?;
        let mut pt = Vec::new();
        for chunk in frames(body)? {
            pt.extend(ctx.open(chunk, b"").map_err(|e| anyhow!("{e:?}"))?);
        }
        let mut exported = [0u8; 32];
        ctx.export(EXPORT_LABEL, &mut exported).map_err(|e| anyhow!("{e:?}"))?;
        Ok((pt, exported))
    }

    /// Seals a response in `pieces`, with an empty write between each, the
    /// way a streaming server would.
    pub fn seal_response(exported: &[u8; 32], enc_hex: &str, pieces: &[&[u8]]) -> (String, Vec<u8>) {
        let nonce: [u8; 32] = rand::random();
        let mut salt = hex::decode(enc_hex).unwrap();
        salt.extend_from_slice(&nonce);
        let hk = Hkdf::<Sha256>::new(Some(&salt), exported);
        let (mut key, mut base) = ([0u8; 32], [0u8; 12]);
        hk.expand(b"key", &mut key).unwrap();
        hk.expand(b"nonce", &mut base).unwrap();
        let aead = Aes256Gcm::new_from_slice(&key).unwrap();
        let mut body = Vec::new();
        for (seq, p) in pieces.iter().enumerate() {
            let ct = aead.encrypt(Nonce::from_slice(&chunk_nonce(&base, seq as u64)), *p).unwrap();
            body.extend_from_slice(&(ct.len() as u32).to_be_bytes());
            body.extend_from_slice(&ct);
            body.extend_from_slice(&0u32.to_be_bytes());
        }
        (hex::encode(nonce), body)
    }
}

#[cfg(test)]
mod tests {
    use super::server::*;
    use super::*;

    #[test]
    fn round_trip_through_a_streaming_server() {
        let (sk, pk) = keypair();
        let req = br#"{"model":"deepseek-v4-1-flash","messages":[{"role":"user","content":"hola"}]}"#;
        let sealed = seal(&pk, req).unwrap();
        assert!(!sealed.body.windows(4).any(|w| w == b"hola"), "plaintext leaked into the sealed body");
        let (pt, exported) = open_request(&sk, &sealed.enc_header(), &sealed.body).unwrap();
        assert_eq!(pt, req);
        let (nonce, body) = seal_response(&exported, &sealed.enc_header(), &[b"{\"choices\":", b"[]}"]);
        assert_eq!(sealed.open_response(&nonce, &body).unwrap(), b"{\"choices\":[]}");
    }

    #[test]
    fn another_requests_response_does_not_open() {
        let (sk, pk) = keypair();
        let a = seal(&pk, b"a").unwrap();
        let b = seal(&pk, b"b").unwrap();
        let (_, exported_b) = open_request(&sk, &b.enc_header(), &b.body).unwrap();
        let (nonce, body) = seal_response(&exported_b, &b.enc_header(), &[b"for b"]);
        assert!(a.open_response(&nonce, &body).is_err());
    }

    #[test]
    fn tampering_and_truncation_fail_closed() {
        let (sk, pk) = keypair();
        let s = seal(&pk, b"x").unwrap();
        let (_, exported) = open_request(&sk, &s.enc_header(), &s.body).unwrap();
        let (nonce, mut body) = seal_response(&exported, &s.enc_header(), &[b"one", b"two"]);
        let full = body.clone();
        body[6] ^= 1;
        assert!(s.open_response(&nonce, &body).is_err(), "flipped bit");
        assert!(s.open_response(&nonce, &full[..full.len() - 7]).is_err(), "truncated");
        assert!(s.open_response("00", &full).is_err(), "short nonce");
    }

    #[test]
    fn chunk_nonce_matches_the_reference_layout() {
        // Go reference: nonce[11-i] ^= byte(seq >> (i*8)) for i in 0..8.
        let base = [0u8; 12];
        assert_eq!(chunk_nonce(&base, 1)[11], 1);
        assert_eq!(chunk_nonce(&base, 0x0102)[10..], [1, 2]);
        assert_eq!(chunk_nonce(&base, 0)[..4], [0, 0, 0, 0]);
    }
}
