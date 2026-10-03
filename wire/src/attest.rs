//! Android Key Attestation — the measured-boot primitive, verified without
//! Google services.
//!
//! A phone mints a key inside its TEE/StrongBox with a challenge; the
//! keystore answers with an X.509 chain rooted in Google's hardware
//! attestation root. The leaf carries, signed by the chip: verified-boot
//! state, bootloader lock, OS/patch levels, and the calling app's package
//! and signing certificate digest.
//!
//! [`verify`] is pure over its inputs. The caller supplies the pinned roots,
//! the policy (which packages and which signing certificates are "our app")
//! and a revocation lookup it fetched itself. What comes back is a
//! [`Verdict`] any third party can recompute from the same public inputs.
//!
//! Two structural rules matter more than they look:
//! * every issuer must be a CA (BasicConstraints), otherwise the app could
//!   sign a forged "leaf" with its own attested key and claim anything;
//! * the app is identified by its signing certificate, not its package
//!   name, otherwise anyone can build an app with our applicationId.

use serde::Serialize;
use x509_parser::asn1_rs::{Any, Class, FromDer, Sequence};
use x509_parser::prelude::*;

pub const ATTESTATION_OID: &str = "1.3.6.1.4.1.11129.2.1.17";
const ATTESTATION_OID_ARCS: &[u64] = &[1, 3, 6, 1, 4, 1, 11129, 2, 1, 17];

/// SHA-256 of a domain-separated agent id: ties the hardware key to exactly
/// one agent, recomputable by any verifier.
pub fn expected_challenge(agent_id: &str) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(format!("agente-attest:v1:{agent_id}").as_bytes()).to_vec()
}

/// Pinned trust anchors (DER).
pub struct Roots {
    ders: Vec<Vec<u8>>,
}

impl Roots {
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let ders = x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
            .map(|p| p.map(|p| p.contents))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("bad root PEM: {e}"))?;
        if ders.is_empty() {
            return Err("no attestation roots".into());
        }
        Ok(Self { ders })
    }
    pub fn from_ders(ders: Vec<Vec<u8>>) -> Self {
        Self { ders }
    }
    fn is_root(&self, der: &[u8]) -> bool {
        self.ders.iter().any(|r| r == der)
    }
}

/// What "our app" means. Digests are lowercase hex SHA-256 of the signing
/// certificate (what `apksigner verify --print-certs` prints).
#[derive(Default, Clone)]
pub struct Policy {
    pub packages: Vec<String>,
    pub signers: Vec<String>,
}

impl Policy {
    /// From the comma-separated env shapes: `pkg.a,pkg.b` / `hex1,hex2`.
    pub fn from_lists(packages: &str, signers: &str) -> Self {
        let split = |s: &str| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase().replace(':', "")).collect::<Vec<_>>();
        Self { packages: split(packages).into_iter().map(|p| p.to_string()).collect(), signers: split(signers) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revocation {
    Ok,
    Revoked,
    /// The list could not be consulted. Reported, and never treated as OK.
    Unknown,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct Verdict {
    /// Every link verifies, every issuer is a CA, every cert is in validity,
    /// and the root is pinned.
    pub chain_ok: bool,
    pub challenge_ok: bool,
    /// software | tee | strongbox
    pub security_level: String,
    /// verified | self_signed | unlocked | failed | unknown
    pub verified_boot: String,
    pub device_locked: bool,
    pub os_version: Option<i64>,
    pub os_patch_level: Option<i64>,
    pub vendor_patch_level: Option<i64>,
    pub boot_patch_level: Option<i64>,
    pub verified_boot_hash: Option<String>,
    pub package: Option<String>,
    pub package_version: Option<i64>,
    pub signer_digest: Option<String>,
    pub package_ok: bool,
    pub signer_ok: bool,
    /// OK | REVOKED | unknown
    pub revocation: String,
    pub attestation_version: Option<i64>,
    pub leaf_expired: bool,
    /// The one word policies key on.
    pub verified: bool,
    pub reason: Option<String>,
}

/// Verifies a base64-DER chain (leaf first) minted for `agent_id`.
/// `revocation` is consulted with each non-root serial (lowercase hex).
pub fn verify(
    roots: &Roots,
    agent_id: &str,
    chain_b64: &[String],
    policy: &Policy,
    revocation: impl Fn(&str) -> Revocation,
) -> Verdict {
    use base64::Engine;
    let mut v = Verdict { security_level: "unknown".into(), verified_boot: "unknown".into(), revocation: "unknown".into(), ..Default::default() };
    let ders: Vec<Vec<u8>> = match chain_b64.iter().map(|s| base64::engine::general_purpose::STANDARD.decode(s.trim())).collect::<Result<Vec<_>, _>>() {
        Ok(d) if (2..=8).contains(&d.len()) => d,
        _ => {
            v.reason = Some("chain is not a base64 DER list of 2–8 certificates".into());
            return v;
        }
    };
    let certs: Vec<X509Certificate> = match ders.iter().map(|d| X509Certificate::from_der(d).map(|(_, c)| c)).collect::<Result<Vec<_>, _>>() {
        Ok(c) => c,
        Err(e) => {
            v.reason = Some(format!("certificate parse error: {e}"));
            return v;
        }
    };
    let mut structure = Ok(());
    for i in 0..certs.len() - 1 {
        let issuer = &certs[i + 1];
        if certs[i].verify_signature(Some(issuer.public_key())).is_err() {
            structure = Err(format!("certificate {i} is not signed by certificate {}", i + 1));
            break;
        }
        // The forgery guard: an end-entity key (the app's own attested key)
        // must never act as an issuer.
        let is_ca = matches!(issuer.basic_constraints(), Ok(Some(bc)) if bc.value.ca);
        if !is_ca {
            structure = Err(format!("certificate {} is not a CA and cannot issue certificate {i}", i + 1));
            break;
        }
        if !issuer.validity().is_valid() {
            structure = Err(format!("certificate {} is outside its validity period", i + 1));
            break;
        }
    }
    let leaf = &certs[0];
    v.leaf_expired = !leaf.validity().is_valid();
    let root_ok = roots.is_root(ders.last().unwrap());
    if structure.is_ok() && !root_ok {
        structure = Err("chain does not end in a pinned hardware attestation root".into());
    }
    if structure.is_ok() && v.leaf_expired {
        structure = Err("leaf certificate is outside its validity period".into());
    }
    match structure {
        Ok(()) => v.chain_ok = true,
        Err(r) => v.reason = Some(r),
    }

    let oid = x509_parser::der_parser::oid::Oid::from(ATTESTATION_OID_ARCS).unwrap();
    let Ok(Some(ext)) = leaf.get_extension_unique(&oid) else {
        v.reason.get_or_insert(format!("leaf has no attestation extension {ATTESTATION_OID}"));
        return v;
    };
    let challenge = match parse_key_description(ext.value, &mut v) {
        Ok(c) => c,
        Err(e) => {
            v.reason.get_or_insert(format!("attestation extension: {e}"));
            return v;
        }
    };
    v.challenge_ok = challenge == expected_challenge(agent_id);

    let mut status = Revocation::Ok;
    for c in &certs[..certs.len() - 1] {
        let serial = c.raw_serial_as_string().replace(':', "").to_lowercase();
        match revocation(&serial) {
            Revocation::Ok => {}
            other => {
                status = other;
                break;
            }
        }
    }
    v.revocation = match status { Revocation::Ok => "OK", Revocation::Revoked => "REVOKED", Revocation::Unknown => "unknown" }.into();

    v.package_ok = v.package.as_deref().is_some_and(|p| policy.packages.iter().any(|e| e == p));
    v.signer_ok = match (&v.signer_digest, policy.signers.is_empty()) {
        (_, true) => true, // no pin configured: package name is all we have
        (Some(d), false) => policy.signers.iter().any(|s| s == d),
        (None, false) => false,
    };
    v.verified = v.chain_ok
        && v.challenge_ok
        && v.verified_boot == "verified"
        && v.device_locked
        && v.security_level != "software"
        && status == Revocation::Ok
        && v.package_ok
        && v.signer_ok;
    if v.reason.is_none() && !v.verified {
        v.reason = Some(
            if !v.challenge_ok { "challenge does not bind this key to the agent" }
            else if v.verified_boot != "verified" { "verified boot did not pass" }
            else if !v.device_locked { "bootloader unlocked" }
            else if v.security_level == "software" { "software-only attestation" }
            else if status != Revocation::Ok { "attestation key revoked or status unknown" }
            else if !v.package_ok { "attested app is not an allowed package" }
            else if !v.signer_ok { "attested app is not signed by an allowed certificate" }
            else { "unverified" }
            .into(),
        );
    }
    v
}

fn level(n: u32) -> &'static str {
    match n { 0 => "software", 1 => "tee", 2 => "strongbox", _ => "unknown" }
}

/// KeyDescription ::= SEQUENCE { attestationVersion, attestationSecurityLevel,
/// keymasterVersion, keymasterSecurityLevel, attestationChallenge, uniqueId,
/// softwareEnforced AuthorizationList, teeEnforced AuthorizationList }
fn parse_key_description(der: &[u8], v: &mut Verdict) -> Result<Vec<u8>, String> {
    let e = |e: x509_parser::asn1_rs::Error| e.to_string();
    let (_, seq) = Sequence::from_der(der).map_err(|e| e.to_string())?;
    let items: Vec<Any> = seq.der_iter::<Any, x509_parser::asn1_rs::Error>().collect::<Result<_, _>>().map_err(e)?;
    if items.len() < 8 {
        return Err("KeyDescription too short".into());
    }
    v.attestation_version = items[0].as_integer().ok().and_then(|i| i.as_i64().ok());
    v.security_level = level(items[1].as_enumerated().map_err(e)?.0).into();
    let challenge = items[4].as_octetstring().map_err(e)?.as_ref().to_vec();
    parse_auth_list(&items[6], v, false)?;
    parse_auth_list(&items[7], v, true)?;
    Ok(challenge)
}

fn parse_auth_list(any: &Any, v: &mut Verdict, hw: bool) -> Result<(), String> {
    let e = |e: x509_parser::asn1_rs::Error| e.to_string();
    let seq = any.as_sequence().map_err(e)?;
    for item in seq.der_iter::<Any, x509_parser::asn1_rs::Error>() {
        let item = item.map_err(e)?;
        if item.class() != Class::ContextSpecific {
            continue;
        }
        let tag = item.tag().0;
        let (_, inner) = Any::from_der(item.data).map_err(|e| e.to_string())?;
        match tag {
            704 if hw => {
                // RootOfTrust ::= SEQUENCE { verifiedBootKey OCTET, deviceLocked BOOL,
                //   verifiedBootState ENUM, verifiedBootHash OCTET }
                let rot = inner.as_sequence().map_err(e)?;
                let parts: Vec<Any> = rot.der_iter::<Any, x509_parser::asn1_rs::Error>().collect::<Result<_, _>>().map_err(e)?;
                if parts.len() >= 3 {
                    v.device_locked = parts[1].as_bool().unwrap_or(false);
                    v.verified_boot = match parts[2].as_enumerated().map(|x| x.0) {
                        Ok(0) => "verified", Ok(1) => "self_signed", Ok(2) => "unlocked", Ok(3) => "failed", _ => "unknown",
                    }.into();
                }
                if parts.len() >= 4 {
                    v.verified_boot_hash = parts[3].as_octetstring().ok().map(|o| hex::encode(o.as_ref()));
                }
            }
            705 if hw => v.os_version = inner.as_integer().ok().and_then(|i| i.as_i64().ok()),
            706 if hw => v.os_patch_level = inner.as_integer().ok().and_then(|i| i.as_i64().ok()),
            718 if hw => v.vendor_patch_level = inner.as_integer().ok().and_then(|i| i.as_i64().ok()),
            719 if hw => v.boot_patch_level = inner.as_integer().ok().and_then(|i| i.as_i64().ok()),
            709 => {
                // AttestationApplicationId ::= SEQUENCE { packageInfos SET OF
                //   SEQUENCE { packageName OCTET, version INT }, signatureDigests SET OF OCTET }
                let bytes = inner.as_octetstring().map_err(e)?.as_ref().to_vec();
                let (_, app) = Sequence::from_der(&bytes).map_err(|e| e.to_string())?;
                let parts: Vec<Any> = app.der_iter::<Any, x509_parser::asn1_rs::Error>().collect::<Result<_, _>>().map_err(e)?;
                if let Some(pkgs) = parts.first() {
                    let (_, first_pkg) = Any::from_der(pkgs.data).map_err(|e| e.to_string())?;
                    let pkg = first_pkg.as_sequence().map_err(e)?;
                    let f: Vec<Any> = pkg.der_iter::<Any, x509_parser::asn1_rs::Error>().collect::<Result<_, _>>().map_err(e)?;
                    if f.len() >= 2 {
                        v.package = f[0].as_octetstring().ok().map(|o| String::from_utf8_lossy(o.as_ref()).to_string());
                        v.package_version = f[1].as_integer().ok().and_then(|i| i.as_i64().ok());
                    }
                }
                if let Some(digests) = parts.get(1) {
                    let (_, first) = Any::from_der(digests.data).map_err(|e| e.to_string())?;
                    v.signer_digest = first.as_octetstring().ok().map(|o| hex::encode(o.as_ref()));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The public subset embedded in facts and discovery.
pub fn summary(v: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "verified": v["verified"],
        "verifiedBoot": v["verified_boot"],
        "deviceLocked": v["device_locked"],
        "securityLevel": v["security_level"],
        "osVersion": v["os_version"],
        "patchLevel": v["os_patch_level"],
        "package": v["package"],
        "signerPinned": v["signer_ok"],
        "reason": v["reason"],
        "checkedAt": v["checked_at"],
    })
}

#[cfg(test)]
mod tests {
    //! A synthetic attestation PKI: root CA → intermediate CA → leaf with a
    //! hand-encoded KeyDescription, plus a forged leaf signed by the leaf's
    //! own key. Same shapes a real device produces, minus Google's key.
    use super::*;
    use base64::Engine;
    use rcgen::{BasicConstraints, Certificate, CertificateParams, CustomExtension, IsCa, KeyPair};

    // ---- minimal DER writer
    fn der(tag: &[u8], content: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        let n = content.len();
        if n < 128 { out.push(n as u8); } else if n < 256 { out.extend([0x81, n as u8]); } else { out.extend([0x82, (n >> 8) as u8, n as u8]); }
        out.extend_from_slice(content);
        out
    }
    fn seq(parts: &[Vec<u8>]) -> Vec<u8> { der(&[0x30], &parts.concat()) }
    fn set(parts: &[Vec<u8>]) -> Vec<u8> { der(&[0x31], &parts.concat()) }
    fn int(n: u8) -> Vec<u8> { der(&[0x02], &[n]) }
    fn enm(n: u8) -> Vec<u8> { der(&[0x0A], &[n]) }
    fn oct(b: &[u8]) -> Vec<u8> { der(&[0x04], b) }
    fn boolean(b: bool) -> Vec<u8> { der(&[0x01], &[if b { 0xff } else { 0 }]) }
    /// EXPLICIT context tag with a high tag number (704 → BF 85 40).
    fn ctx(tag: u32, inner: &[u8]) -> Vec<u8> {
        let mut t = vec![0xBF];
        let hi = (tag >> 7) as u8;
        let lo = (tag & 0x7f) as u8;
        if hi > 0 { t.push(0x80 | hi); }
        t.push(lo);
        der(&t, inner)
    }

    fn key_description(challenge: &[u8], level: u8, boot_state: u8, locked: bool, package: &str, signer: &[u8]) -> Vec<u8> {
        let app_id = seq(&[
            set(&[seq(&[oct(package.as_bytes()), int(36)])]),
            set(&[oct(signer)]),
        ]);
        let software = seq(&[ctx(709, &oct(&app_id))]);
        let root_of_trust = seq(&[oct(&[1; 32]), boolean(locked), enm(boot_state), oct(&[2; 32])]);
        let hardware = seq(&[ctx(704, &root_of_trust), ctx(705, &der(&[0x02], &[0x02, 0x22, 0xe0])), ctx(706, &der(&[0x02], &[0x03, 0x17, 0x3f]))]);
        seq(&[int(100), enm(level), int(100), enm(level), oct(challenge), oct(b""), software, hardware])
    }

    struct Pki { root: Certificate, root_key: KeyPair, inter: Certificate, inter_key: KeyPair }

    fn pki() -> Pki {
        let mut rp = CertificateParams::new(Vec::<String>::new()).unwrap();
        rp.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let root_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let root = rp.self_signed(&root_key).unwrap();
        let mut ip = CertificateParams::new(Vec::<String>::new()).unwrap();
        ip.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        let inter_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let inter = ip.signed_by(&inter_key, &root, &root_key).unwrap();
        Pki { root, root_key, inter, inter_key }
    }

    fn leaf(issuer: &Certificate, issuer_key: &KeyPair, kd: Vec<u8>) -> (Certificate, KeyPair) {
        let mut lp = CertificateParams::new(Vec::<String>::new()).unwrap();
        lp.is_ca = IsCa::NoCa;
        lp.custom_extensions.push(CustomExtension::from_oid_content(ATTESTATION_OID_ARCS, kd));
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        (lp.signed_by(&key, issuer, issuer_key).unwrap(), key)
    }

    fn b64(c: &Certificate) -> String { base64::engine::general_purpose::STANDARD.encode(c.der()) }

    const AGENT: &str = "agent:7f52000000000000000000000000000000000000000000000000000000000000";
    const PKG: &str = "yaya.tech.agente";
    const SIGNER: [u8; 32] = [0xab; 32];

    fn policy() -> Policy { Policy::from_lists(PKG, &hex::encode(SIGNER)) }

    #[test]
    fn genuine_chain_verifies() {
        let p = pki();
        let (l, _) = leaf(&p.inter, &p.inter_key, key_description(&expected_challenge(AGENT), 1, 0, true, PKG, &SIGNER));
        let roots = Roots::from_ders(vec![p.root.der().to_vec()]);
        let v = verify(&roots, AGENT, &[b64(&l), b64(&p.inter), b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert!(v.verified, "{v:?}");
        assert_eq!(v.security_level, "tee");
        assert_eq!(v.package.as_deref(), Some(PKG));
        assert_eq!(v.os_version, Some(140000));
        assert_eq!(v.os_patch_level, Some(202559));
    }

    #[test]
    fn forged_leaf_signed_by_the_attested_key_is_rejected() {
        let p = pki();
        // The device's real leaf: honest about an unlocked bootloader.
        let (real, real_key) = leaf(&p.inter, &p.inter_key, key_description(b"someone else", 1, 2, false, "com.evil", &[0; 32]));
        // What the attacker signs with that key: everything a verifier wants to see.
        let (forged, _) = leaf(&real, &real_key, key_description(&expected_challenge(AGENT), 2, 0, true, PKG, &SIGNER));
        let roots = Roots::from_ders(vec![p.root.der().to_vec()]);
        let v = verify(&roots, AGENT, &[b64(&forged), b64(&real), b64(&p.inter), b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert!(!v.verified);
        assert!(!v.chain_ok);
        assert!(v.reason.as_deref().unwrap().contains("not a CA"), "{v:?}");
    }

    #[test]
    fn policy_pins_package_and_signer() {
        let p = pki();
        let roots = Roots::from_ders(vec![p.root.der().to_vec()]);
        let (l, _) = leaf(&p.inter, &p.inter_key, key_description(&expected_challenge(AGENT), 1, 0, true, PKG, &[0xcd; 32]));
        let v = verify(&roots, AGENT, &[b64(&l), b64(&p.inter), b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert!(v.chain_ok && v.package_ok && !v.signer_ok && !v.verified);
        let (l, _) = leaf(&p.inter, &p.inter_key, key_description(&expected_challenge(AGENT), 1, 0, true, "yaya.tech.agent0", &SIGNER));
        let v = verify(&roots, AGENT, &[b64(&l), b64(&p.inter), b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert!(!v.package_ok && !v.verified);
        // Unknown revocation status fails closed.
        let (l, _) = leaf(&p.inter, &p.inter_key, key_description(&expected_challenge(AGENT), 1, 0, true, PKG, &SIGNER));
        let v = verify(&roots, AGENT, &[b64(&l), b64(&p.inter), b64(&p.root)], &policy(), |_| Revocation::Unknown);
        assert!(!v.verified && v.revocation == "unknown");
    }

    #[test]
    fn wrong_root_or_challenge() {
        let p = pki();
        let other = pki();
        let (l, _) = leaf(&p.inter, &p.inter_key, key_description(&expected_challenge("agent:00"), 1, 0, true, PKG, &SIGNER));
        let roots = Roots::from_ders(vec![other.root.der().to_vec()]);
        let v = verify(&roots, AGENT, &[b64(&l), b64(&p.inter), b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert!(!v.chain_ok && !v.challenge_ok);
        assert!(v.reason.as_deref().unwrap().contains("pinned"));
        let _ = &p.root_key;
    }

    // ---- helpers for the edge-case tests below
    fn roots_of(p: &Pki) -> Roots { Roots::from_ders(vec![p.root.der().to_vec()]) }
    fn good_kd() -> Vec<u8> { key_description(&expected_challenge(AGENT), 1, 0, true, PKG, &SIGNER) }
    fn chain(p: &Pki, l: &Certificate) -> Vec<String> { vec![b64(l), b64(&p.inter), b64(&p.root)] }
    fn run(p: &Pki, kd: Vec<u8>, rev: Revocation) -> Verdict {
        let (l, _) = leaf(&p.inter, &p.inter_key, kd);
        verify(&roots_of(p), AGENT, &chain(p, &l), &policy(), |_| rev)
    }

    #[test]
    fn expected_challenge_is_domain_separated_sha256() {
        use sha2::Digest;
        let c = expected_challenge("agent:ab");
        assert_eq!(c.len(), 32);
        assert_eq!(c, sha2::Sha256::digest(b"agente-attest:v1:agent:ab").to_vec());
        assert_ne!(c, expected_challenge("agent:ac"));
    }

    #[test]
    fn roots_from_pem() {
        let p = pki();
        let pem = format!("{}{}", p.root.pem(), p.inter.pem());
        let r = Roots::from_pem(&pem).unwrap();
        assert!(r.is_root(p.root.der()));
        assert!(r.is_root(p.inter.der()));
        assert!(!r.is_root(b"nope"));
        assert_eq!(Roots::from_pem("").err().unwrap(), "no attestation roots");
        assert!(Roots::from_pem("-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n").is_err());
    }

    #[test]
    fn policy_from_lists_normalises() {
        let pol = Policy::from_lists(" a.b , ,C.D,", "AB:CD, ef01 ,");
        assert_eq!(pol.packages, vec!["a.b", "c.d"]);
        assert_eq!(pol.signers, vec!["abcd", "ef01"]);
        let empty = Policy::from_lists("", "");
        assert!(empty.packages.is_empty() && empty.signers.is_empty());
    }

    #[test]
    fn level_names() {
        assert_eq!(level(0), "software");
        assert_eq!(level(1), "tee");
        assert_eq!(level(2), "strongbox");
        assert_eq!(level(3), "unknown");
    }

    #[test]
    fn chain_length_and_encoding_are_checked_first() {
        let p = pki();
        let roots = roots_of(&p);
        for bad in [vec![], vec![b64(&p.root)], vec!["%%%".to_string(), b64(&p.root)], vec![b64(&p.root); 9]] {
            let v = verify(&roots, AGENT, &bad, &policy(), |_| Revocation::Ok);
            assert!(!v.verified && !v.chain_ok);
            assert!(v.reason.as_deref().unwrap().contains("2–8"), "{v:?}");
            assert_eq!((v.security_level.as_str(), v.verified_boot.as_str(), v.revocation.as_str()), ("unknown", "unknown", "unknown"));
        }
        use base64::Engine;
        let junk = base64::engine::general_purpose::STANDARD.encode(b"not a certificate");
        let v = verify(&roots, AGENT, &[junk, b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert!(v.reason.as_deref().unwrap().starts_with("certificate parse error"), "{v:?}");
    }

    #[test]
    fn whitespace_around_base64_is_tolerated() {
        let p = pki();
        let (l, _) = leaf(&p.inter, &p.inter_key, good_kd());
        let c: Vec<String> = chain(&p, &l).into_iter().map(|s| format!(" {s}\n")).collect();
        assert!(verify(&roots_of(&p), AGENT, &c, &policy(), |_| Revocation::Ok).verified);
    }

    #[test]
    fn broken_link_is_reported() {
        let p = pki();
        let other = pki();
        // Leaf issued by another intermediate, presented with ours.
        let (l, _) = leaf(&other.inter, &other.inter_key, good_kd());
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &policy(), |_| Revocation::Ok);
        assert!(!v.chain_ok && !v.verified);
        assert_eq!(v.reason.as_deref(), Some("certificate 0 is not signed by certificate 1"));
    }

    #[test]
    fn expired_leaf_and_expired_issuer() {
        let p = pki();
        let mut lp = CertificateParams::new(Vec::<String>::new()).unwrap();
        lp.is_ca = IsCa::NoCa;
        lp.not_before = rcgen::date_time_ymd(2000, 1, 1);
        lp.not_after = rcgen::date_time_ymd(2001, 1, 1);
        lp.custom_extensions.push(CustomExtension::from_oid_content(ATTESTATION_OID_ARCS, good_kd()));
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let l = lp.signed_by(&key, &p.inter, &p.inter_key).unwrap();
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &policy(), |_| Revocation::Ok);
        assert!(v.leaf_expired && !v.chain_ok && !v.verified);
        assert_eq!(v.reason.as_deref(), Some("leaf certificate is outside its validity period"));
        // Claims are still parsed so the verdict is informative.
        assert!(v.challenge_ok);

        let mut ip = CertificateParams::new(Vec::<String>::new()).unwrap();
        ip.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        ip.not_before = rcgen::date_time_ymd(2000, 1, 1);
        ip.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let ik = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let old_inter = ip.signed_by(&ik, &p.root, &p.root_key).unwrap();
        let (l, _) = leaf(&old_inter, &ik, good_kd());
        let v = verify(&roots_of(&p), AGENT, &[b64(&l), b64(&old_inter), b64(&p.root)], &policy(), |_| Revocation::Ok);
        assert_eq!(v.reason.as_deref(), Some("certificate 1 is outside its validity period"));
    }

    #[test]
    fn leaf_without_extension() {
        let p = pki();
        let mut lp = CertificateParams::new(Vec::<String>::new()).unwrap();
        lp.is_ca = IsCa::NoCa;
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let l = lp.signed_by(&key, &p.inter, &p.inter_key).unwrap();
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &policy(), |_| Revocation::Ok);
        assert!(v.chain_ok && !v.verified);
        assert!(v.reason.as_deref().unwrap().contains(ATTESTATION_OID));
    }

    #[test]
    fn malformed_key_description() {
        let p = pki();
        let v = run(&p, seq(&[int(1), int(2)]), Revocation::Ok);
        assert!(!v.verified);
        assert_eq!(v.reason.as_deref(), Some("attestation extension: KeyDescription too short"));
        let v = run(&p, oct(b"x"), Revocation::Ok);
        assert!(v.reason.as_deref().unwrap().starts_with("attestation extension:"));
    }

    #[test]
    fn each_failure_has_its_own_reason() {
        let p = pki();
        let cases: Vec<(Vec<u8>, Revocation, &str)> = vec![
            (key_description(b"wrong", 1, 0, true, PKG, &SIGNER), Revocation::Ok, "challenge does not bind this key to the agent"),
            (key_description(&expected_challenge(AGENT), 1, 1, true, PKG, &SIGNER), Revocation::Ok, "verified boot did not pass"),
            (key_description(&expected_challenge(AGENT), 1, 0, false, PKG, &SIGNER), Revocation::Ok, "bootloader unlocked"),
            (key_description(&expected_challenge(AGENT), 0, 0, true, PKG, &SIGNER), Revocation::Ok, "software-only attestation"),
            (good_kd(), Revocation::Revoked, "attestation key revoked or status unknown"),
            (key_description(&expected_challenge(AGENT), 1, 0, true, "com.other", &SIGNER), Revocation::Ok, "attested app is not an allowed package"),
            (key_description(&expected_challenge(AGENT), 1, 0, true, PKG, &[1; 32]), Revocation::Ok, "attested app is not signed by an allowed certificate"),
        ];
        for (kd, rev, reason) in cases {
            let v = run(&p, kd, rev);
            assert!(v.chain_ok && !v.verified, "{reason}");
            assert_eq!(v.reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn boot_states_and_levels_are_decoded() {
        let p = pki();
        for (state, name) in [(0, "verified"), (1, "self_signed"), (2, "unlocked"), (3, "failed"), (9, "unknown")] {
            let v = run(&p, key_description(&expected_challenge(AGENT), 2, state, true, PKG, &SIGNER), Revocation::Ok);
            assert_eq!(v.verified_boot, name);
            assert_eq!(v.security_level, "strongbox");
        }
        let v = run(&p, good_kd(), Revocation::Ok);
        assert_eq!(v.verified_boot_hash.as_deref(), Some("02".repeat(32).as_str()));
        assert_eq!(v.signer_digest.as_deref(), Some(hex::encode(SIGNER).as_str()));
        assert_eq!(v.package_version, Some(36));
        assert_eq!(v.attestation_version, Some(100));
        assert!(v.device_locked);
        assert_eq!(v.revocation, "OK");
        assert!(v.reason.is_none());
    }

    #[test]
    fn revocation_is_asked_for_every_non_root_serial_until_a_miss() {
        let p = pki();
        let (l, _) = leaf(&p.inter, &p.inter_key, good_kd());
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &policy(), |s| { asked.borrow_mut().push(s.to_string()); Revocation::Ok });
        assert!(v.verified);
        let asked = asked.into_inner();
        assert_eq!(asked.len(), 2, "leaf + intermediate, never the root");
        assert!(asked.iter().all(|s| !s.contains(':') && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())));
        let n = std::cell::Cell::new(0);
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &policy(), |_| { n.set(n.get() + 1); Revocation::Revoked });
        assert_eq!((n.get(), v.revocation.as_str(), v.verified), (1, "REVOKED", false));
    }

    #[test]
    fn signer_pin_is_optional() {
        let p = pki();
        let (l, _) = leaf(&p.inter, &p.inter_key, key_description(&expected_challenge(AGENT), 1, 0, true, PKG, &[9; 32]));
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &Policy::from_lists(PKG, ""), |_| Revocation::Ok);
        assert!(v.signer_ok && v.verified);
        let v = verify(&roots_of(&p), AGENT, &chain(&p, &l), &Policy::default(), |_| Revocation::Ok);
        assert!(!v.package_ok && !v.verified, "an empty package list allows nothing");
    }

    #[test]
    fn summary_projects_public_fields() {
        let p = pki();
        let v = run(&p, good_kd(), Revocation::Ok);
        let mut j = serde_json::to_value(&v).unwrap();
        j["checked_at"] = serde_json::json!(123);
        let s = summary(&j);
        assert_eq!(s["verified"], true);
        assert_eq!(s["verifiedBoot"], "verified");
        assert_eq!(s["deviceLocked"], true);
        assert_eq!(s["securityLevel"], "tee");
        assert_eq!(s["osVersion"], 140000);
        assert_eq!(s["patchLevel"], 202559);
        assert_eq!(s["package"], PKG);
        assert_eq!(s["signerPinned"], true);
        assert_eq!(s["checkedAt"], 123);
        assert!(s.get("verified_boot_hash").is_none() && s.get("signerDigest").is_none());
        assert_eq!(s.as_object().unwrap().len(), 10);
        // Missing input fields project to null rather than panicking.
        assert_eq!(summary(&serde_json::json!({}))["verified"], serde_json::Value::Null);
    }
}
