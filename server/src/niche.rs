//! A niche is an industry in a country: `barberia-PE`. Skills learned across
//! the agents of a niche come back to every phone in it as a section of the
//! system prompt — the network's memory, one line at a time.

/// `"Barbería"`, `"PE"` → `"barberia-PE"`. ASCII, lowercase, dashes.
pub fn key(industry: &str, country: &str) -> String {
    let slug: String = industry
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' | 'ã' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o', 'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', 'ç' => 'c', c => c,
        })
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(40).collect();
    format!("{}-{}", if slug.is_empty() { "generic".to_string() } else { slug }, country.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys() {
        assert_eq!(super::key("Barbería", "pe"), "barberia-PE");
        assert_eq!(super::key("Salón de uñas & spa", "PE"), "salon-de-unas-spa-PE");
        assert_eq!(super::key("", "MX"), "generic-MX");
    }

    #[test]
    fn folds_portuguese_and_caps_length() {
        assert_eq!(super::key("Salão de Beleza", "br"), "salao-de-beleza-BR");
        assert_eq!(super::key("Açaí", "BR"), "acai-BR");
        let k = super::key(&"a".repeat(100), "PE");
        assert_eq!(k, format!("{}-PE", "a".repeat(40)));
        assert_eq!(super::key("¡¡!!", "pe"), "generic-PE");
        assert_eq!(super::key("  --Taller--  mecánico ", "PE"), "taller-mecanico-PE");
    }
}
