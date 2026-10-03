//! Agent identifiers. An agent *is* its Ed25519 public key; the id is the
//! hex of that key with a fixed prefix, so it is self-certifying: whoever
//! can sign for it, owns it.

use ed25519_dalek::VerifyingKey;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AgentId([u8; 32]);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("agent id must look like agent:<64 hex chars>")]
    Shape,
    #[error("agent id is not a valid ed25519 public key")]
    NotAKey,
}

impl AgentId {
    pub const PREFIX: &'static str = "agent:";

    pub fn from_key(key: &VerifyingKey) -> Self {
        Self(key.to_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, IdError> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| IdError::NotAKey)?;
        Ok(Self(bytes))
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        // Validated at construction.
        VerifyingKey::from_bytes(&self.0).expect("AgentId holds a valid key")
    }

    /// `did:key:z6Mk…` — multicodec ed25519-pub (0xed01) in base58btc.
    pub fn did(&self) -> String {
        let mut bytes = vec![0xed, 0x01];
        bytes.extend_from_slice(&self.0);
        format!("did:key:z{}", bs58(&bytes))
    }

    /// True for anything that parses; cheap shape check for untrusted input.
    pub fn looks_valid(s: &str) -> bool {
        s.parse::<AgentId>().is_ok()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", Self::PREFIX, hex::encode(self.0))
    }
}

impl fmt::Debug for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgentId({self})")
    }
}

impl FromStr for AgentId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex_part = s.strip_prefix(Self::PREFIX).ok_or(IdError::Shape)?;
        if hex_part.len() != 64 || !hex_part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(IdError::Shape);
        }
        let bytes: [u8; 32] = hex::decode(hex_part).map_err(|_| IdError::Shape)?.try_into().map_err(|_| IdError::Shape)?;
        Self::from_bytes(bytes)
    }
}

impl serde::Serialize for AgentId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for AgentId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Bitcoin-alphabet base58 (as used by did:key).
pub fn bs58(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut digits: Vec<u8> = vec![0];
    for &byte in input {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let zeros = input.iter().take_while(|b| **b == 0).count();
    let mut out = String::with_capacity(zeros + digits.len());
    out.extend(std::iter::repeat_n('1', zeros));
    out.extend(digits.iter().rev().map(|d| ALPHABET[*d as usize] as char));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_matches_known_vectors() {
        assert_eq!(bs58(b"hello"), "Cn8eVZg");
        assert_eq!(bs58(&[0, 0, 1]), "112");
    }

    #[test]
    fn roundtrips_and_rejects_shapes() {
        let kp = crate::Keypair::generate();
        let id = kp.id();
        let s = id.to_string();
        assert!(s.starts_with("agent:") && s.len() == 70);
        assert_eq!(s.parse::<AgentId>().unwrap(), id);
        assert!(id.did().starts_with("did:key:z6Mk"));
        assert_eq!("agent:zz".parse::<AgentId>(), Err(IdError::Shape));
        assert_eq!("agent:".to_string().parse::<AgentId>(), Err(IdError::Shape));
        // Uppercase hex is not canonical.
        assert!(s.to_uppercase().parse::<AgentId>().is_err());
    }

    #[test]
    fn from_bytes_rejects_non_points() {
        // y = 2 does not decompress to a curve point.
        let mut b = [0u8; 32];
        b[0] = 2;
        assert_eq!(AgentId::from_bytes(b), Err(IdError::NotAKey));
        let kp = crate::Keypair::generate();
        assert_eq!(AgentId::from_bytes(*kp.id().bytes()).unwrap(), kp.id());
    }

    #[test]
    fn parse_rejects_wrong_prefix_and_lengths() {
        let hex64 = hex::encode(crate::Keypair::generate().id().bytes());
        assert_eq!(format!("did:{hex64}").parse::<AgentId>(), Err(IdError::Shape));
        assert_eq!(hex64.parse::<AgentId>(), Err(IdError::Shape));
        assert_eq!(format!("agent:{}", &hex64[..62]).parse::<AgentId>(), Err(IdError::Shape));
        assert_eq!(format!("agent:{hex64}00").parse::<AgentId>(), Err(IdError::Shape));
        assert_eq!(format!(" agent:{hex64}").parse::<AgentId>(), Err(IdError::Shape));
    }

    #[test]
    fn parse_well_shaped_non_key_is_not_a_key() {
        let s = format!("agent:02{}", "0".repeat(62));
        assert_eq!(s.parse::<AgentId>(), Err(IdError::NotAKey));
        assert!(!AgentId::looks_valid(&s));
    }

    #[test]
    fn looks_valid() {
        assert!(AgentId::looks_valid(&crate::Keypair::generate().id().to_string()));
        assert!(!AgentId::looks_valid(""));
        assert!(!AgentId::looks_valid("agent:"));
    }

    #[test]
    fn did_is_known_vector() {
        // RFC 8032 test 1 public key -> its well-known did:key.
        let pk: [u8; 32] = hex::decode("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a").unwrap().try_into().unwrap();
        let id = AgentId::from_bytes(pk).unwrap();
        assert_eq!(id.did(), "did:key:z6MktwupdmLXVVqTzCw4i46r4uGyosGXRnR3XjN4Zq7oMMsw");
    }

    #[test]
    fn debug_and_display() {
        let id = crate::Keypair::from_seed([1; 32]).id();
        assert_eq!(format!("{id:?}"), format!("AgentId({id})"));
        assert!(id.to_string().starts_with(AgentId::PREFIX));
    }

    #[test]
    fn serde_roundtrip_and_rejects_garbage() {
        let id = crate::Keypair::from_seed([3; 32]).id();
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<AgentId>(&s).unwrap(), id);
        assert!(serde_json::from_str::<AgentId>("\"agent:xyz\"").is_err());
        assert!(serde_json::from_str::<AgentId>("5").is_err());
    }

    #[test]
    fn base58_edge_cases() {
        assert_eq!(bs58(&[]), "1");
        assert_eq!(bs58(&[0]), "11");
        assert_eq!(bs58(&[57]), "z");
        assert_eq!(bs58(&[58]), "21");
        assert_eq!(bs58(&[0xff; 4]), "7YXq9G");
    }

    #[test]
    fn hash_eq_usable_as_map_key() {
        let a = crate::Keypair::from_seed([4; 32]).id();
        let mut set = std::collections::HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&a));
    }
}
