//! Shared secrets: comparing them without timing leaks, and refusing the
//! placeholders that end up in production when nobody generates a real one.

/// Constant-time equality. Length is not secret (formats are public); the
/// bytes are, so the fold runs over the whole slice either way.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SecretError {
    #[error("{0} must be set (no default is provided)")]
    Missing(String),
    #[error("{0} is too short ({1} chars); use at least {2} random characters")]
    Short(String, usize, usize),
    #[error("{0} is still a placeholder value — generate a real one")]
    Placeholder(String),
}

/// Validates a secret read from configuration: present, long enough, and not
/// one of the placeholder shapes that have shipped by accident before.
pub fn validate(name: &str, value: Option<&str>, min_len: usize) -> Result<String, SecretError> {
    let v = value.map(str::trim).filter(|v| !v.is_empty()).ok_or_else(|| SecretError::Missing(name.into()))?;
    if v.len() < min_len {
        return Err(SecretError::Short(name.into(), v.len(), min_len));
    }
    let lower = v.to_ascii_lowercase();
    if lower.starts_with("agente-") || lower.starts_with("agento-") || lower.contains("change_me") || lower.contains("changeme") || lower == "secret" {
        return Err(SecretError::Placeholder(name.into()));
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }

    #[test]
    fn validates_shapes() {
        assert!(matches!(validate("K", None, 32), Err(SecretError::Missing(_))));
        assert!(matches!(validate("K", Some("short"), 32), Err(SecretError::Short(..))));
        assert!(matches!(validate("K", Some("agente-admin-dev-xxxxxxxxxxxxxxxxxxxxxxxx"), 32), Err(SecretError::Placeholder(_))));
        assert!(validate("K", Some(&"x".repeat(40)), 32).is_ok());
    }

    #[test]
    fn ct_eq_edge_cases() {
        assert!(ct_eq("", ""));
        assert!(!ct_eq("", "a"));
        assert!(!ct_eq("a", ""));
        // Same length, differs only in the last byte.
        assert!(!ct_eq("aaaaaaab", "aaaaaaaa"));
        // Multibyte UTF-8 compares by bytes.
        assert!(ct_eq("ñandú", "ñandú"));
        assert!(!ct_eq("ñandú", "nandu"));
    }

    #[test]
    fn missing_covers_empty_and_whitespace() {
        assert_eq!(validate("K", Some(""), 1), Err(SecretError::Missing("K".into())));
        assert_eq!(validate("K", Some("   \t\n"), 1), Err(SecretError::Missing("K".into())));
    }

    #[test]
    fn short_reports_trimmed_length_and_minimum() {
        assert_eq!(validate("K", Some("  abc  "), 5), Err(SecretError::Short("K".into(), 3, 5)));
        // Exactly the minimum is enough.
        assert_eq!(validate("K", Some("abcde"), 5), Ok("abcde".into()));
    }

    #[test]
    fn value_is_returned_trimmed() {
        assert_eq!(validate("K", Some("  0123456789abcdef  "), 16), Ok("0123456789abcdef".into()));
    }

    #[test]
    fn rejects_every_placeholder_shape() {
        let pad = "x".repeat(40);
        for v in [
            format!("agente-admin-{pad}"),
            format!("AGENTE-admin-{pad}"),
            // The brand is still agento: its placeholders shipped before the rename.
            format!("agento-admin-dev-{pad}"),
            format!("{pad}change_me"),
            format!("{pad}CHANGEME{pad}"),
        ] {
            assert_eq!(validate("K", Some(&v), 32), Err(SecretError::Placeholder("K".into())), "{v}");
        }
        assert_eq!(validate("K", Some("secret"), 1), Err(SecretError::Placeholder("K".into())));
        assert_eq!(validate("K", Some("SECRET"), 1), Err(SecretError::Placeholder("K".into())));
    }

    #[test]
    fn accepts_real_looking_secrets_that_merely_mention_words() {
        // "agent" alone, or "secret" inside a longer value, is fine.
        assert!(validate("K", Some(&format!("agent{}", "7".repeat(40))), 32).is_ok());
        assert!(validate("K", Some(&format!("mysecret{}", "7".repeat(40))), 32).is_ok());
    }

    #[test]
    fn error_messages_name_the_variable() {
        assert_eq!(SecretError::Missing("DB_KEY".into()).to_string(), "DB_KEY must be set (no default is provided)");
        assert!(SecretError::Short("DB_KEY".into(), 3, 32).to_string().contains("3 chars"));
        assert!(SecretError::Placeholder("DB_KEY".into()).to_string().contains("placeholder"));
    }
}
