//! Delivery fees, the way informal Lima actually prices them: a flat fee for
//! the districts the owner names, a higher "periferia" fee for the rest of
//! Lima Metropolitana, nothing beyond it — and often free handoff at any
//! Metropolitano / Tren Eléctrico station, because the owner rides it anyway.
//!
//! Config lives in the business schema under `delivery`:
//!   { "zones": {"san borja": 5, "surco": 5, "surquillo": 5},
//!     "default": 10,                     // everything else IN coverage
//!     "coverage": "lima metropolitana",  // descriptive; enforced by the list below
//!     "freeAt": ["metropolitano", "tren electrico"] }  // meeting-point keywords
//! The legacy `deliveryZones` map (zones only) keeps working.

use anyhow::anyhow;
use serde_json::{json, Value};

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Delivery;

/// Lima Metropolitana + Callao, normalized. This IS the "solo Lima" rule:
/// a destination that matches nothing here and no configured zone is either
/// misspelled or outside coverage, and only the owner can say which.
const LIMA: &[&str] = &[
    "ancon", "ate", "barranco", "brena", "carabayllo", "chaclacayo", "chorrillos",
    "cieneguilla", "comas", "el agustino", "independencia", "jesus maria",
    "la molina", "la victoria", "cercado de lima", "lince", "los olivos",
    "lurigancho", "chosica", "lurin", "magdalena", "miraflores", "pachacamac",
    "pucusana", "pueblo libre", "puente piedra", "punta hermosa", "punta negra",
    "rimac", "san bartolo", "san borja", "san isidro", "san juan de lurigancho",
    "san juan de miraflores", "san luis", "san martin de porres", "san miguel",
    "santa anita", "santa maria del mar", "santa rosa", "santiago de surco",
    "surco", "surquillo", "villa el salvador", "villa maria del triunfo",
    "callao", "bellavista", "carmen de la legua", "la perla", "la punta",
    "ventanilla", "mi peru",
];

fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' => 'a', 'é' => 'e', 'í' => 'i', 'ó' => 'o', 'ú' | 'ü' => 'u', 'ñ' => 'n',
            c => c,
        })
        .collect()
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// `phrase` appears in `text` as whole consecutive words ("ate" is in
/// "Av. Central, Ate", not in "Calle Mateo"). Both already `norm`-ed.
fn has_phrase(text: &str, phrase: &str) -> bool {
    let (t, p) = (words(text), words(phrase));
    !p.is_empty() && t.windows(p.len()).any(|w| w == p.as_slice())
}

/// Every meaningful token of a freeAt keyword must appear in the destination:
/// stored "tren electrico" matches "estación Angamos del tren eléctrico", and
/// filler words in the stored phrase don't block it.
fn keyword_hits(keyword: &str, dest: &str) -> bool {
    const FILLER: &[&str] = &[
        "cualquier", "estacion", "estaciones", "punto", "puntos", "del", "de",
        "la", "el", "los", "las", "en", "a", "o", "u", "y",
    ];
    let toks: Vec<&str> = keyword
        .split_whitespace()
        .filter(|t| !FILLER.contains(t))
        .collect();
    !toks.is_empty() && toks.iter().all(|t| dest.contains(t))
}

pub enum Quote {
    /// No delivery configured at all — pickup only.
    NotOffered,
    /// Free handoff at a configured meeting point.
    Free { matched: String },
    /// Priced zone (or the periferia default). `zone` is the display name.
    Fee { zone: String, fee: f64 },
    /// Not a configured zone and not a recognized Lima district: misspelled,
    /// or outside coverage — only the owner can settle it.
    Unknown,
}

/// Resolves a destination against the business's own delivery config. Pure:
/// the customer names the place, the business's stored words set the price.
pub fn fee_for(values: &Value, dest: &str) -> Quote {
    let cfg = &values["delivery"];
    let legacy = &values["deliveryZones"];
    let zones = cfg["zones"]
        .as_object()
        .or_else(|| legacy.as_object())
        .filter(|m| !m.is_empty());
    let has_any = zones.is_some() || cfg["default"].as_f64().is_some();
    if !has_any {
        return Quote::NotOffered;
    }
    let d = norm(dest);

    if let Some(free) = cfg["freeAt"].as_array() {
        for k in free.iter().filter_map(Value::as_str) {
            if keyword_hits(&norm(k), &d) {
                return Quote::Free { matched: k.to_string() };
            }
        }
    }

    if let Some(map) = zones {
        // The most specific zone the destination names wins: "San Juan de
        // Miraflores" is not "Miraflores". A destination that is only part
        // of a zone's name counts when exactly one zone contains it.
        let named = map.iter().filter(|(z, f)| f.as_f64().is_some() && has_phrase(&d, &norm(z))).max_by_key(|(z, _)| words(&norm(z)).len());
        let partial: Vec<_> = map.iter().filter(|(z, f)| f.as_f64().is_some() && has_phrase(&norm(z), &d)).collect();
        let hit = named.or(if partial.len() == 1 { Some(partial[0]) } else { None });
        if let Some((zone, fee)) = hit {
            return Quote::Fee { zone: zone.clone(), fee: fee.as_f64().unwrap_or(0.0) };
        }
    }

    if let Some(default) = cfg["default"].as_f64() {
        // `default` prices "the rest of the city". Knowing what the city IS
        // takes a district list; we have Lima's. For other countries the
        // coverage string is the boundary: a destination that names the
        // configured coverage city gets the default, anything else is unknown
        // and the agent asks the owner (or the owner adds the zone).
        let country = values["country"].as_str().unwrap_or("PE");
        let coverage = norm(cfg["coverage"].as_str().unwrap_or(""));
        let in_city = if country == "PE" {
            LIMA.iter().any(|dist| has_phrase(&d, dist))
        } else {
            !coverage.is_empty() && (has_phrase(&d, &coverage) || has_phrase(&coverage, &d))
        };
        if in_city {
            return Quote::Fee { zone: "periferia".into(), fee: default };
        }
    }
    Quote::Unknown
}

async fn quote_delivery(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let dest = args["destination"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("missing 'destination'"))?;
    Ok(match fee_for(&ctx.values, dest) {
        Quote::NotOffered => json!({
            "status": "not_offered",
            "note": "this business has no delivery configured — offer pickup only"
        }),
        Quote::Free { matched } => json!({
            "status": "free",
            "meetingPoint": matched,
            "fee": 0,
            "note": "free handoff at the meeting point — agree on which station and when"
        }),
        Quote::Fee { zone, fee } => json!({
            "status": "priced", "zone": zone, "fee": fee,
            "feeFormatted": crate::locale::Locale::from_values(&ctx.values).money(fee)
        }),
        Quote::Unknown => json!({
            "status": "unknown_zone",
            "coverage": ctx.values["delivery"]["coverage"].as_str().unwrap_or("configured zones only"),
            "note": "not a configured zone nor a recognized local area — ask the \
                     customer for their area, and if it is genuinely outside \
                     coverage say delivery doesn't reach there"
        }),
    })
}

impl Plugin<Agente> for Delivery {
    fn name(&self) -> &'static str {
        "delivery"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, true),
            // No `fee` parameter on purpose, same rule as payments: the
            // business's stored zones set the price, never the conversation.
            json!({"type": "function", "function": {
                "name": "quote_delivery",
                "description": "Delivery fee for a destination, from the business's own zone \
                    config (free meeting points, per-zone fees, periferia default, Lima-only \
                    coverage). Call BEFORE quoting any delivery cost, and pass the same \
                    destination as delivery_to when creating the order so the fee lands in \
                    the total.",
                "parameters": {
                    "type": "object",
                    "properties": {"destination": {"type": "string",
                        "description": "district, address or meeting point the customer named"}},
                    "required": ["destination"]
                }
            }}),
            tool_fn(|c, a| Box::pin(quote_delivery(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Value {
        // The user-story config, verbatim: "5 soles a San Borja, Surco y
        // Surquillo, 10 a la periferia, solo Lima metropolitana, gratis en
        // cualquier estación del Metropolitano o tren eléctrico".
        json!({"delivery": {
            "zones": {"san borja": 5, "surco": 5, "surquillo": 5},
            "default": 10,
            "coverage": "lima metropolitana",
            "freeAt": ["metropolitano", "tren electrico"]
        }})
    }

    #[test]
    fn named_zones_price_flat() {
        assert!(matches!(fee_for(&cfg(), "San Borja"), Quote::Fee { fee, .. } if fee == 5.0));
        assert!(matches!(fee_for(&cfg(), "vivo en Santiago de Surco"),
                         Quote::Fee { fee, .. } if fee == 5.0));
        assert!(matches!(fee_for(&cfg(), "SURQUILLO"), Quote::Fee { fee, .. } if fee == 5.0));
    }

    #[test]
    fn outside_peru_coverage_city_is_the_boundary() {
        let v = json!({"country": "IN", "delivery": {
            "zones": {"koramangala": 40, "indiranagar": 40},
            "default": 80, "coverage": "bangalore"}});
        assert!(matches!(fee_for(&v, "Koramangala"), Quote::Fee { fee, .. } if fee == 40.0));
        assert!(matches!(fee_for(&v, "somewhere in Bangalore"), Quote::Fee { fee, .. } if fee == 80.0));
        assert!(matches!(fee_for(&v, "Ate"), Quote::Unknown)); // Lima list must not leak
        assert!(matches!(fee_for(&v, "Mumbai"), Quote::Unknown));
    }

    #[test]
    fn periferia_default_covers_the_rest_of_lima_only() {
        assert!(matches!(fee_for(&cfg(), "Ate"), Quote::Fee { zone, fee } if zone == "periferia" && fee == 10.0));
        assert!(matches!(fee_for(&cfg(), "Comas"), Quote::Fee { fee, .. } if fee == 10.0));
        // Provinces are not Lima Metropolitana.
        assert!(matches!(fee_for(&cfg(), "Trujillo"), Quote::Unknown));
        assert!(matches!(fee_for(&cfg(), "Huancayo centro"), Quote::Unknown));
    }

    #[test]
    fn meeting_points_are_free_and_win_over_zones() {
        assert!(matches!(fee_for(&cfg(), "estación Angamos del Metropolitano"), Quote::Free { .. }));
        assert!(matches!(fee_for(&cfg(), "tren eléctrico Villa El Salvador"), Quote::Free { .. }));
        // A district alone is not a station.
        assert!(matches!(fee_for(&cfg(), "Villa El Salvador"), Quote::Fee { fee, .. } if fee == 10.0));
    }

    #[test]
    fn legacy_and_absent_configs() {
        let legacy = json!({"deliveryZones": {"miraflores": 8}});
        assert!(matches!(fee_for(&legacy, "Miraflores"), Quote::Fee { fee, .. } if fee == 8.0));
        // Legacy has no default: unlisted district is unknown, not periferia.
        assert!(matches!(fee_for(&legacy, "Comas"), Quote::Unknown));
        assert!(matches!(fee_for(&json!({}), "Surco"), Quote::NotOffered));
    }

    fn fee(v: &Value, d: &str) -> Option<(String, f64)> {
        match fee_for(v, d) { Quote::Fee { zone, fee } => Some((zone, fee)), _ => None }
    }

    #[test]
    fn the_most_specific_zone_prices_the_delivery() {
        let v = json!({"delivery": {"zones": {"miraflores": 5, "san juan de miraflores": 12}}});
        assert_eq!(fee(&v, "Av. Los Héroes, San Juan de Miraflores"), Some(("san juan de miraflores".into(), 12.0)));
        assert_eq!(fee(&v, "Larco 100, Miraflores"), Some(("miraflores".into(), 5.0)));
    }

    #[test]
    fn a_district_name_inside_another_word_is_not_that_district() {
        // "ate" is a Lima district and also inside "Mateo": Arequipa is not Lima.
        let v = json!({"country": "PE", "delivery": {"default": 10}});
        assert!(matches!(fee_for(&v, "Calle Mateo 123, Arequipa"), Quote::Unknown));
        assert_eq!(fee(&v, "Av. Central, Ate"), Some(("periferia".into(), 10.0)));
        let v = json!({"delivery": {"zones": {"lima": 7}}});
        assert!(matches!(fee_for(&v, "Callao"), Quote::Unknown), "zone words match whole words");
        assert!(matches!(fee_for(&v, "a"), Quote::Unknown));
    }

    #[test]
    fn free_points_legacy_zones_and_other_countries() {
        let v = json!({"delivery": {"zones": {"surco": 5}, "freeAt": ["cualquier estación del Metropolitano"]}});
        assert!(matches!(fee_for(&v, "estación Canaval y Moreyra del metropolitano"), Quote::Free { .. }));
        assert!(matches!(fee_for(&json!({}), "surco"), Quote::NotOffered));
        assert_eq!(fee(&json!({"deliveryZones": {"Surco": 6}}), "surco"), Some(("Surco".into(), 6.0)));
        let mx = json!({"country": "MX", "delivery": {"default": 50, "coverage": "Guadalajara"}});
        assert_eq!(fee(&mx, "Col. Americana, Guadalajara"), Some(("periferia".into(), 50.0)));
        assert!(matches!(fee_for(&mx, "Monterrey"), Quote::Unknown));
        assert!(matches!(fee_for(&json!({"country": "MX", "delivery": {"default": 50}}), "x"), Quote::Unknown), "no coverage named: unknown");
    }

    #[tokio::test]
    async fn quote_tool_shapes() {
        let s = crate::testkit::state().await;
        let b = crate::testkit::business(&s.db).await;
        crate::testkit::set_values(&s.db, b, json!({"delivery": {"zones": {"surco": 5}, "freeAt": ["metropolitano"]}})).await;
        let c = crate::testkit::tool_ctx(&s, b, Some("p")).await;
        assert_eq!(quote_delivery(&c, &json!({"destination": "Surco"})).await.unwrap()["feeFormatted"], "S/ 5");
        assert_eq!(quote_delivery(&c, &json!({"destination": "metropolitano"})).await.unwrap()["status"], "free");
        assert_eq!(quote_delivery(&c, &json!({"destination": "Marte"})).await.unwrap()["status"], "unknown_zone");
        assert!(quote_delivery(&c, &json!({"destination": " "})).await.is_err());
        let b2 = crate::testkit::business(&s.db).await;
        let c2 = crate::testkit::tool_ctx(&s, b2, Some("p")).await;
        assert_eq!(quote_delivery(&c2, &json!({"destination": "Surco"})).await.unwrap()["status"], "not_offered");
    }
}
