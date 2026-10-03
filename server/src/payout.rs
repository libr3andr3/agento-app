//! Where this business's customers send money. Typed by the owner, in
//! their own words: the wallet's name ("Yape", "Pix", "Mercado Pago",
//! "Nequi"…) and the destination (a number, a key, an alias — whatever that
//! wallet uses). Nothing here knows any wallet; the names the network has
//! seen are offered as suggestions, that is all.

use serde_json::{json, Value};

pub const MAX_DESTINATIONS: usize = 5;

fn clean(s: Option<&str>, max: usize) -> Option<String> {
    s.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty()).map(|s| s.chars().take(max).collect())
}

/// Normalises what the form sent: trimmed strings, bounded lengths, empty
/// rows dropped. `Err` names the first row that is half-filled.
pub fn normalise(input: &Value) -> Result<Value, (usize, String)> {
    let cash_only = input["cashOnly"].as_bool().unwrap_or(false);
    let holder = clean(input["holder"].as_str(), 80);
    let mut dests = Vec::new();
    for (i, d) in input["destinations"].as_array().into_iter().flatten().enumerate().take(MAX_DESTINATIONS) {
        let wallet = clean(d["wallet"].as_str(), 40);
        let handle = clean(d["handle"].as_str(), 160);
        match (wallet, handle) {
            (None, None) => continue,
            (Some(w), Some(h)) => dests.push(json!({"wallet": w, "handle": h})),
            (Some(_), None) => return Err((i, "missing the number / key to pay to".into())),
            (None, Some(_)) => return Err((i, "missing the wallet's name".into())),
        }
    }
    if !cash_only && dests.is_empty() {
        return Err((0, "add at least one way to pay, or mark cash only".into()));
    }
    Ok(json!({"holder": holder, "cashOnly": cash_only, "destinations": dests, "updatedAt": chrono::Utc::now().to_rfc3339()}))
}

/// One line for the prompts: "Yape 999 888 777 (a nombre de Tito Pérez); Pix …".
pub fn describe(payout: &Value) -> Option<String> {
    if payout["cashOnly"].as_bool() == Some(true) {
        return Some("cash only".into());
    }
    let holder = payout["holder"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let lines: Vec<String> = payout["destinations"].as_array()?.iter().filter_map(|d| {
        let (w, h) = (d["wallet"].as_str()?, d["handle"].as_str()?);
        Some(match holder { Some(n) => format!("{w} {h} (a nombre de {n})"), None => format!("{w} {h}") })
    }).collect();
    if lines.is_empty() { None } else { Some(lines.join("; ")) }
}

/// The wallet names the owner uses, for the network's suggestions.
pub fn wallets(payout: &Value) -> Vec<String> {
    payout["destinations"].as_array().into_iter().flatten().filter_map(|d| d["wallet"].as_str().map(String::from)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_and_describes() {
        let p = normalise(&json!({"holder": "  Tito  Pérez ", "destinations": [{"wallet": "Yape", "handle": " 999 888 777 "}, {"wallet": "", "handle": ""}]})).unwrap();
        assert_eq!(p["destinations"].as_array().unwrap().len(), 1);
        assert_eq!(describe(&p).unwrap(), "Yape 999 888 777 (a nombre de Tito Pérez)");
        assert_eq!(wallets(&p), vec!["Yape"]);
        assert_eq!(normalise(&json!({"destinations": [{"wallet": "Pix"}]})).unwrap_err().0, 0);
        assert!(normalise(&json!({"destinations": []})).is_err());
        assert_eq!(describe(&normalise(&json!({"cashOnly": true})).unwrap()).unwrap(), "cash only");
    }

    #[test]
    fn clean_collapses_whitespace_and_bounds_length() {
        assert_eq!(clean(Some("  a \t b\n c "), 10).as_deref(), Some("a b c"));
        assert_eq!(clean(Some("   "), 10), None);
        assert_eq!(clean(None, 10), None);
        // Counts characters, not bytes.
        assert_eq!(clean(Some("ñññññ"), 3).as_deref(), Some("ñññ"));
    }

    #[test]
    fn missing_wallet_name_is_reported_with_its_row() {
        let e = normalise(&json!({"destinations": [{"wallet": "Yape", "handle": "1"}, {"handle": "999"}]})).unwrap_err();
        assert_eq!(e, (1, "missing the wallet's name".to_string()));
        let e = normalise(&json!({"destinations": [{"wallet": "Pix", "handle": "  "}]})).unwrap_err();
        assert_eq!(e, (0, "missing the number / key to pay to".to_string()));
    }

    #[test]
    fn cash_only_needs_no_destinations_but_keeps_any_given() {
        let p = normalise(&json!({"cashOnly": true, "destinations": [{"wallet": "Yape", "handle": "1"}]})).unwrap();
        assert_eq!(p["cashOnly"], true);
        assert_eq!(p["destinations"].as_array().unwrap().len(), 1);
        // Described as cash only regardless.
        assert_eq!(describe(&p).as_deref(), Some("cash only"));
    }

    #[test]
    fn at_most_five_rows_are_read() {
        let rows: Vec<Value> = (0..8).map(|i| json!({"wallet": format!("W{i}"), "handle": format!("{i}")})).collect();
        let p = normalise(&json!({"destinations": rows})).unwrap();
        assert_eq!(p["destinations"].as_array().unwrap().len(), MAX_DESTINATIONS);
        // Half-filled rows past the limit are ignored, not errors.
        let mut rows: Vec<Value> = (0..5).map(|i| json!({"wallet": format!("W{i}"), "handle": "1"})).collect();
        rows.push(json!({"wallet": "only name"}));
        assert!(normalise(&json!({"destinations": rows})).is_ok());
    }

    #[test]
    fn lengths_are_bounded() {
        let p = normalise(&json!({"holder": "h".repeat(200), "destinations": [{"wallet": "w".repeat(100), "handle": "x".repeat(500)}]})).unwrap();
        assert_eq!(p["holder"].as_str().unwrap().len(), 80);
        assert_eq!(p["destinations"][0]["wallet"].as_str().unwrap().len(), 40);
        assert_eq!(p["destinations"][0]["handle"].as_str().unwrap().len(), 160);
    }

    #[test]
    fn non_array_destinations_and_missing_holder() {
        assert!(normalise(&json!({"destinations": "Yape 999"})).is_err());
        let p = normalise(&json!({"destinations": [{"wallet": "Yape", "handle": "9"}]})).unwrap();
        assert!(p["holder"].is_null());
        assert!(chrono::DateTime::parse_from_rfc3339(p["updatedAt"].as_str().unwrap()).is_ok());
    }

    #[test]
    fn describe_joins_rows_and_skips_broken_ones() {
        let p = json!({"holder": " ", "destinations": [
            {"wallet": "Yape", "handle": "9"}, {"wallet": "Pix"}, {"wallet": "Plin", "handle": "8"}
        ]});
        assert_eq!(describe(&p).as_deref(), Some("Yape 9; Plin 8"));
        assert_eq!(describe(&json!({"destinations": []})), None);
        assert_eq!(describe(&json!({})), None);
        assert_eq!(describe(&json!({"destinations": [{"wallet": "Pix"}]})), None);
    }

    #[test]
    fn wallets_lists_names_in_order() {
        assert_eq!(wallets(&json!({"destinations": [{"wallet": "Pix"}, {"handle": "x"}, {"wallet": "Nequi"}]})), vec!["Pix", "Nequi"]);
        assert!(wallets(&json!({})).is_empty());
    }
}
