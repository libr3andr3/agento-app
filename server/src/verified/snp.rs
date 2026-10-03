//! AMD SEV-SNP attestation reports, verified on the phone.
//!
//! A report is 1184 bytes signed by the chip's VCEK (ECDSA P-384 over bytes
//! `0x000..0x2A0`). AMD's KDS issues the VCEK per chip and TCB, and it chains
//! VCEK ← ASK ← ARK with RSA-PSS/SHA-384. The ARK/ASK pairs in `roots/` were
//! fetched from AMD's KDS (`/vcek/v1/{Genoa,Turin}/cert_chain`) on 2026-09-18;
//! they are the only roots trusted here, so nothing fetched at runtime — the
//! VCEK included — can add trust, only fail to verify.

use anyhow::{anyhow, bail, ensure, Context, Result};
use ring::signature::{self, UnparsedPublicKey};
use x509_parser::prelude::*;

const GENOA_CHAIN: &[u8] = include_bytes!("roots/amd_genoa_cert_chain.pem");
const TURIN_CHAIN: &[u8] = include_bytes!("roots/amd_turin_cert_chain.pem");

pub const REPORT_LEN: usize = 0x4A0;
/// The report body the VCEK signs.
const SIGNED_LEN: usize = 0x2A0;

/// Guest policy bits that must be clear: a debuggable guest can be read by
/// the hypervisor, and a migration agent can export its memory.
const POLICY_MIGRATE_MA: u64 = 1 << 18;
const POLICY_DEBUG: u64 = 1 << 19;

/// VCEK certificate extensions (AMD VCEK certificate spec).
const OID_PRODUCT: &str = "1.3.6.1.4.1.3704.1.2";
const OID_BL_SPL: &str = "1.3.6.1.4.1.3704.1.3.1";
const OID_TEE_SPL: &str = "1.3.6.1.4.1.3704.1.3.2";
const OID_SNP_SPL: &str = "1.3.6.1.4.1.3704.1.3.3";
const OID_UCODE_SPL: &str = "1.3.6.1.4.1.3704.1.3.8";
const OID_FMC_SPL: &str = "1.3.6.1.4.1.3704.1.3.9";
const OID_HWID: &str = "1.3.6.1.4.1.3704.1.4";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Genoa,
    Turin,
}

impl Product {
    /// The product name AMD's KDS and certificates use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Genoa => "Genoa",
            Self::Turin => "Turin",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "Genoa" => Some(Self::Genoa),
            "Turin" => Some(Self::Turin),
            _ => None,
        }
    }

    fn roots(self) -> &'static [u8] {
        match self {
            Self::Genoa => GENOA_CHAIN,
            Self::Turin => TURIN_CHAIN,
        }
    }

    /// Significant chip-id bytes: Turin ids are 8 bytes, the rest zero.
    fn chip_id_len(self) -> usize {
        match self {
            Self::Genoa => 64,
            Self::Turin => 8,
        }
    }
}

/// The platform's security patch levels, as reported and as certified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Tcb {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fmc: Option<u8>,
    pub bootloader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

impl Tcb {
    /// True when every component is at or above `floor`.
    pub fn at_least(&self, floor: &Tcb) -> bool {
        self.bootloader >= floor.bootloader
            && self.tee >= floor.tee
            && self.snp >= floor.snp
            && self.microcode >= floor.microcode
            && floor.fmc.map_or(true, |f| self.fmc.unwrap_or(0) >= f)
    }
}

/// A parsed, not yet verified, attestation report.
pub struct Report<'a> {
    raw: &'a [u8],
}

impl<'a> Report<'a> {
    pub fn parse(raw: &'a [u8]) -> Result<Self> {
        ensure!(raw.len() == REPORT_LEN, "SEV-SNP report is {} bytes, expected {REPORT_LEN}", raw.len());
        Ok(Self { raw })
    }

    fn u32_at(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.raw[off..off + 4].try_into().expect("4 bytes"))
    }

    pub fn version(&self) -> u32 {
        self.u32_at(0x00)
    }

    pub fn policy(&self) -> u64 {
        u64::from_le_bytes(self.raw[0x08..0x10].try_into().expect("8 bytes"))
    }

    pub fn vmpl(&self) -> u32 {
        self.u32_at(0x30)
    }

    pub fn signature_algo(&self) -> u32 {
        self.u32_at(0x34)
    }

    pub fn report_data(&self) -> [u8; 64] {
        self.raw[0x50..0x90].try_into().expect("64 bytes")
    }

    pub fn measurement(&self) -> [u8; 48] {
        self.raw[0x90..0xC0].try_into().expect("48 bytes")
    }

    pub fn chip_id(&self) -> &[u8] {
        &self.raw[0x1A0..0x1E0]
    }

    /// Which AMD generation signed this, from the CPUID fields report
    /// version 3 added. Older reports are refused rather than guessed at.
    pub fn product(&self) -> Result<Product> {
        ensure!(self.version() >= 3, "report version {} carries no CPUID; refusing to guess the product", self.version());
        let (family, model) = (self.raw[0x188], self.raw[0x189]);
        match (family, model) {
            (0x19, 0x10..=0x1F) | (0x19, 0xA0..=0xAF) => Ok(Product::Genoa),
            (0x1A, _) => Ok(Product::Turin),
            _ => bail!("unsupported CPU family {family:#x} model {model:#x}"),
        }
    }

    /// The TCB the report claims; the byte layout differs by generation.
    pub fn reported_tcb(&self, p: Product) -> Tcb {
        let t = &self.raw[0x180..0x188];
        match p {
            Product::Genoa => Tcb { fmc: None, bootloader: t[0], tee: t[1], snp: t[6], microcode: t[7] },
            Product::Turin => Tcb { fmc: Some(t[0]), bootloader: t[1], tee: t[2], snp: t[3], microcode: t[7] },
        }
    }

    /// The AMD KDS path for this report's VCEK, relative to `/vcek/v1/`.
    pub fn kds_path(&self) -> Result<String> {
        let p = self.product()?;
        let t = self.reported_tcb(p);
        let chip = hex::encode(&self.chip_id()[..p.chip_id_len()]);
        let mut q = format!("blSPL={}&teeSPL={}&snpSPL={}&ucodeSPL={}", t.bootloader, t.tee, t.snp, t.microcode);
        if let Some(fmc) = t.fmc {
            q = format!("fmcSPL={fmc}&{q}");
        }
        Ok(format!("{}/{chip}?{q}", p.name()))
    }
}

/// What a verified report proves.
#[derive(Clone, Debug)]
pub struct Verified {
    pub product: Product,
    pub measurement: [u8; 48],
    pub report_data: [u8; 64],
    pub tcb: Tcb,
}

/// Verifies `raw` against `vcek_der`, which may have come from anywhere:
/// it is trusted only if it chains to the baked AMD roots for the product
/// the report names, and its certified TCB and chip id match the report.
pub fn verify(raw: &[u8], vcek_der: &[u8], now_unix: i64) -> Result<Verified> {
    let r = Report::parse(raw)?;
    ensure!(r.signature_algo() == 1, "report signature algorithm {} is not ECDSA P-384/SHA-384", r.signature_algo());
    ensure!(r.vmpl() == 0, "report requested at VMPL {}, expected 0", r.vmpl());
    ensure!(r.policy() & POLICY_DEBUG == 0, "guest policy allows debugging ({:#x})", r.policy());
    ensure!(r.policy() & POLICY_MIGRATE_MA == 0, "guest policy allows a migration agent ({:#x})", r.policy());
    let product = r.product()?;
    let tcb = r.reported_tcb(product);

    // AMD roots: ARK (self-signed) → ASK → VCEK, all RSA-PSS/SHA-384.
    let (ask_der, ark_der) = root_pair(product)?;
    let (_, ark) = X509Certificate::from_der(&ark_der).map_err(|e| anyhow!("ARK: {e}"))?;
    let (_, ask) = X509Certificate::from_der(&ask_der).map_err(|e| anyhow!("ASK: {e}"))?;
    rsa_pss_verified_by(&ark, &ark).context("ARK self-signature")?;
    rsa_pss_verified_by(&ask, &ark).context("ASK signed by ARK")?;

    let (_, vcek) = X509Certificate::from_der(vcek_der).map_err(|e| anyhow!("VCEK: {e}"))?;
    let issuer = common_name(vcek.issuer()).unwrap_or_default();
    ensure!(issuer == format!("SEV-{}", product.name()), "VCEK issuer {issuer:?} does not match {}", product.name());
    rsa_pss_verified_by(&vcek, &ask).context("VCEK signed by ASK")?;
    let v = vcek.validity();
    ensure!(
        v.not_before.timestamp() <= now_unix && now_unix <= v.not_after.timestamp(),
        "VCEK not valid now ({} .. {})", v.not_before, v.not_after
    );

    // The VCEK certifies one chip at one TCB; the report must claim exactly that.
    let ext = |oid: &str| vcek.extensions().iter().find(|e| e.oid.to_id_string() == oid).map(|e| e.value);
    if let Some(name) = ext(OID_PRODUCT) {
        ensure!(der_ia5(name).as_deref() == Some(product.name()), "VCEK product extension does not name {}", product.name());
    }
    let spl = |oid: &str| -> Result<u8> {
        let v = ext(oid).ok_or_else(|| anyhow!("VCEK lacks extension {oid}"))?;
        der_small_int(v).ok_or_else(|| anyhow!("VCEK extension {oid} is not a small INTEGER"))
    };
    let certified = Tcb {
        fmc: if product == Product::Turin { Some(spl(OID_FMC_SPL)?) } else { None },
        bootloader: spl(OID_BL_SPL)?,
        tee: spl(OID_TEE_SPL)?,
        snp: spl(OID_SNP_SPL)?,
        microcode: spl(OID_UCODE_SPL)?,
    };
    ensure!(certified == tcb, "VCEK certifies TCB {certified:?} but the report claims {tcb:?}");
    let hwid = ext(OID_HWID).ok_or_else(|| anyhow!("VCEK lacks the hwID extension"))?;
    let chip = r.chip_id();
    ensure!(
        chip.starts_with(hwid) && chip[hwid.len()..].iter().all(|b| *b == 0),
        "VCEK was issued for a different chip"
    );

    // Finally the report itself: r and s are little-endian 72-byte fields.
    let sig = &raw[SIGNED_LEN..SIGNED_LEN + 144];
    let mut fixed = [0u8; 96];
    let (r_be, s_be) = fixed.split_at_mut(48);
    for (dst, src) in [(r_be, &sig[..72]), (s_be, &sig[72..])] {
        ensure!(src[48..].iter().all(|b| *b == 0), "signature component exceeds P-384 size");
        dst.copy_from_slice(&src[..48]);
        dst.reverse();
    }
    let point = &vcek.public_key().subject_public_key.data;
    UnparsedPublicKey::new(&signature::ECDSA_P384_SHA384_FIXED, point.as_ref())
        .verify(&raw[..SIGNED_LEN], &fixed)
        .map_err(|_| anyhow!("report signature does not verify under the VCEK"))?;

    Ok(Verified { product, measurement: r.measurement(), report_data: r.report_data(), tcb })
}

/// `(ASK, ARK)` DER for a product, from the baked PEM chain.
fn root_pair(p: Product) -> Result<(Vec<u8>, Vec<u8>)> {
    let (mut ask, mut ark) = (None, None);
    for pem in x509_parser::pem::Pem::iter_from_buffer(p.roots()) {
        let pem = pem.map_err(|e| anyhow!("baked AMD chain: {e}"))?;
        let (_, cert) = X509Certificate::from_der(&pem.contents).map_err(|e| anyhow!("baked AMD chain: {e}"))?;
        match common_name(cert.subject()).as_deref() {
            Some(cn) if cn == format!("ARK-{}", p.name()) => ark = Some(pem.contents.clone()),
            Some(cn) if cn == format!("SEV-{}", p.name()) => ask = Some(pem.contents.clone()),
            _ => {}
        }
    }
    Ok((ask.ok_or_else(|| anyhow!("baked chain lacks ASK"))?, ark.ok_or_else(|| anyhow!("baked chain lacks ARK"))?))
}

fn rsa_pss_verified_by(cert: &X509Certificate, issuer: &X509Certificate) -> Result<()> {
    let key = &issuer.public_key().subject_public_key.data;
    UnparsedPublicKey::new(&signature::RSA_PSS_2048_8192_SHA384, key.as_ref())
        .verify(cert.tbs_certificate.as_ref(), cert.signature_value.data.as_ref())
        .map_err(|_| anyhow!("RSA-PSS/SHA-384 signature does not verify"))
}

fn common_name(name: &X509Name) -> Option<String> {
    name.iter_common_name().next().and_then(|cn| cn.as_str().ok()).map(String::from)
}

/// A DER INTEGER small enough for a patch level.
fn der_small_int(v: &[u8]) -> Option<u8> {
    match v {
        [0x02, 1, b] => Some(*b),
        [0x02, 2, 0, b] if *b >= 0x80 => Some(*b),
        _ => None,
    }
}

fn der_ia5(v: &[u8]) -> Option<String> {
    match v {
        [0x16, n, rest @ ..] if rest.len() == *n as usize => String::from_utf8(rest.to_vec()).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Evidence pulled from inference.tinfoil.sh on 2026-09-18 and verified
    /// independently at the time (AMD KDS + Sigstore/Rekor).
    fn fixture() -> (Vec<u8>, Vec<u8>) {
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/tinfoil/attestation.json")).unwrap();
        let gz = base64::engine::general_purpose::STANDARD.decode(doc["body"].as_str().unwrap()).unwrap();
        let raw = super::super::gunzip(&gz).unwrap();
        (raw, include_bytes!("../../tests/fixtures/tinfoil/vcek_genoa.der").to_vec())
    }

    const WHEN: i64 = 1_789_776_000; // 2026-09-18

    #[test]
    fn live_report_verifies() {
        let (raw, vcek) = fixture();
        let v = verify(&raw, &vcek, WHEN).unwrap();
        assert_eq!(v.product, Product::Genoa);
        assert_eq!(hex::encode(&v.measurement[..8]), "27e0b90db95a9a31");
        assert_eq!(v.tcb, Tcb { fmc: None, bootloader: 10, tee: 0, snp: 23, microcode: 84 });
    }

    #[test]
    fn kds_path_matches_what_amd_served() {
        let (raw, _) = fixture();
        let p = Report::parse(&raw).unwrap().kds_path().unwrap();
        assert!(p.starts_with("Genoa/1af1aa6c1f56037a"), "{p}");
        assert!(p.ends_with("?blSPL=10&teeSPL=0&snpSPL=23&ucodeSPL=84"), "{p}");
    }

    #[test]
    fn any_flipped_bit_in_the_signed_body_fails() {
        let (mut raw, vcek) = fixture();
        raw[0x90] ^= 1; // the measurement
        assert!(verify(&raw, &vcek, WHEN).is_err());
    }

    #[test]
    fn debug_policy_is_refused_before_anything_else() {
        let (mut raw, vcek) = fixture();
        raw[0x0A] |= 0x08; // policy bit 19
        let e = verify(&raw, &vcek, WHEN).unwrap_err().to_string();
        assert!(e.contains("debugging"), "{e}");
    }

    #[test]
    fn a_vcek_for_another_tcb_is_refused() {
        let (mut raw, vcek) = fixture();
        raw[0x186] = 22; // claim snpSPL 22; the VCEK certifies 23
        let e = verify(&raw, &vcek, WHEN).unwrap_err().to_string();
        assert!(e.contains("certifies TCB"), "{e}");
    }

    #[test]
    fn an_expired_window_is_refused() {
        let (raw, vcek) = fixture();
        assert!(verify(&raw, &vcek, 1_600_000_000).is_err());
    }

    #[test]
    fn tcb_floor() {
        let t = Tcb { fmc: None, bootloader: 10, tee: 0, snp: 23, microcode: 84 };
        assert!(t.at_least(&Tcb { fmc: None, bootloader: 10, tee: 0, snp: 22, microcode: 84 }));
        assert!(!t.at_least(&Tcb { fmc: None, bootloader: 10, tee: 0, snp: 24, microcode: 84 }));
    }
}
