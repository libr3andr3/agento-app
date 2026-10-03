//! Product orders. Core-mounted (selling products is cross-vertical); the
//! tool itself answers honestly when the business has no product catalog, so
//! a services-only business never fabricates a shop.

use anyhow::anyhow;
use serde_json::{json, Value};

use crate::harness::{meta, tool_fn, Agente, Kernel, Plugin, Scope, ToolCtx};

pub struct Sales;

/// Catalog lookup in the business's words and the customer's. An exact
/// name wins; else the most specific catalog name the request contains
/// ("polo rojo talla M" → "Polo rojo", never the cheaper "Polo"); else the
/// one catalog name containing the request. Two equally good candidates
/// ("polo" among "Polo azul" and "Polo rojo") are no answer: the agent asks.
fn product_price(values: &Value, name: &str) -> Option<(String, f64)> {
    let map = values["products"].as_object()?;
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    let price = |k: &String| {
        let v = &map[k];
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().replace(',', ".").parse::<f64>().ok())).map(|p| (k.clone(), p))
    };
    if let Some(k) = map.keys().find(|k| k.trim().to_lowercase() == n) {
        return price(k);
    }
    let contained: Vec<&String> = map.keys().filter(|k| { let kl = k.trim().to_lowercase(); !kl.is_empty() && n.contains(&kl) }).collect();
    if let Some(best) = contained.iter().map(|k| k.trim().len()).max() {
        let top: Vec<&&String> = contained.iter().filter(|k| k.trim().len() == best).collect();
        return if top.len() == 1 { price(top[0]) } else { None };
    }
    let containing: Vec<&String> = map.keys().filter(|k| k.to_lowercase().contains(&n)).collect();
    match containing.as_slice() {
        [only] => price(only),
        _ => None,
    }
}

async fn create_order(ctx: &ToolCtx<'_>, args: &Value) -> anyhow::Result<Value> {
    let locale = crate::locale::Locale::from_values(&ctx.values);
    let catalog = &ctx.values["products"];
    if !catalog.is_object() || catalog.as_object().is_some_and(|m| m.is_empty()) {
        return Ok(json!({"status": "rejected",
            "note": "this business has no product catalog configured — do not sell products"}));
    }
    let name = args["customer_name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing 'customer_name'"))?;
    let phone = crate::harness::canon_phone(
        ctx.peer
            .as_deref()
            .or_else(|| args["phone"].as_str())
            .ok_or_else(|| anyhow!("missing 'phone'"))?,
    );
    let items = args["items"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("missing 'items' [{{product, qty}}]"))?;

    // Every line is priced from the business's own catalog — the customer is
    // the other party to this conversation, so a price (or a product) the
    // model can be talked into does not exist here. Unknown items reject the
    // whole order with the real catalog, so the agent can correct itself.
    let mut total = 0.0;
    let mut lines = Vec::new();
    for item in items {
        let product = item["product"]
            .as_str()
            .ok_or_else(|| anyhow!("item missing 'product'"))?;
        let qty = item["qty"].as_i64().unwrap_or(1).clamp(1, 999);
        let Some((canonical, price)) = product_price(&ctx.values, product) else {
            return Ok(json!({"status": "rejected",
                "note": format!("'{product}' is not one product in the catalog — if several match, ask which one"),
                "catalog": catalog}));
        };
        total += price * qty as f64;
        lines.push(json!({"product": canonical, "qty": qty, "unitPrice": price}));
    }

    // Delivery rides the order as its own line, priced from the business's
    // zone config — the books must match what the customer was quoted, which
    // they didn't while the fee lived only in the conversation.
    let mut delivery_note: Option<String> = None;
    if let Some(dest) = args["delivery_to"].as_str().filter(|s| !s.trim().is_empty()) {
        use crate::plugins::delivery::{fee_for, Quote};
        match fee_for(&ctx.values, dest) {
            Quote::NotOffered => {
                return Ok(json!({"status": "rejected",
                    "note": "this business has no delivery configured — offer pickup only"}));
            }
            Quote::Unknown => {
                return Ok(json!({"status": "unknown_delivery_zone",
                    "note": format!("'{dest}' is not a configured zone nor a recognized \
                        local area — ask for the area, or say delivery doesn't \
                        reach there; the order was NOT created yet")}));
            }
            Quote::Free { matched } => {
                lines.push(json!({"product": format!("entrega en {matched}"),
                                  "qty": 1, "unitPrice": 0.0}));
                delivery_note = Some(format!("entrega gratis en punto: {dest}"));
            }
            Quote::Fee { zone, fee } => {
                total += fee;
                lines.push(json!({"product": format!("delivery a {zone}"),
                                  "qty": 1, "unitPrice": fee}));
                delivery_note = Some(format!("delivery a: {dest}"));
            }
        }
    }
    let notes = match (args["notes"].as_str().filter(|s| !s.is_empty()), &delivery_note) {
        (Some(n), Some(d)) => Some(format!("{n} · {d}")),
        (Some(n), None) => Some(n.to_string()),
        (None, Some(d)) => Some(d.clone()),
        (None, None) => None,
    };

    // "Ya pagué" back-and-forths made the model re-take the same order — one
    // customer, two identical rows, and the payment could only settle one of
    // them. Same order = same customer + same total, recent, not cancelled.
    // Checked against ANY live status: the first version only matched
    // pending rows, and a Yape that landed fast enough to auto-confirm the
    // original let the duplicate through anyway.
    let dup: Option<(uuid::Uuid, String, bool)> = sqlx::query_as(
        "SELECT id, status, paid FROM orders \
         WHERE business_id = $1 AND phone = $2 AND status <> 'cancelled' \
           AND total = $3 AND created_at > $4 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(ctx.business_id)
    .bind(&phone)
    .bind(total)
    .bind(crate::db::ago(chrono::Duration::hours(2)))
    .fetch_optional(&ctx.state.db)
    .await?;
    if let Some((existing, dup_status, dup_paid)) = dup {
        return Ok(json!({
            "status": if dup_paid { "paid" } else { "already_pending" },
            "orderId": existing,
            "items": lines,
            "total": total,
            "note": if dup_paid {
                "this exact order is ALREADY CONFIRMED AND PAID — do not create \
                 another; tell the customer their order is confirmed"
            } else if dup_status == "confirmed" {
                "this exact order is ALREADY CONFIRMED — do not create another"
            } else {
                "this exact order is ALREADY registered and awaiting payment — \
                 do not create another; ask for the transfer of the same total"
            }
        }));
    }

    // Same payment rule as bookings: upfront/yape businesses collect before
    // the order is confirmed; the Yape notification (payment_event) or
    // collect_payment closes it.
    let upfront = matches!(
        ctx.values["paymentMethod"].as_str(),
        Some("upfront") | Some("yape") | Some("transfer")
    ) && total > 0.0;
    let status = if upfront { "pending_payment" } else { "confirmed" };

    let row: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO orders (id, business_id, customer_name, phone, items, total, status, notes) \
         VALUES ($8,$1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(ctx.business_id)
    .bind(name)
    .bind(&phone)
    .bind(json!(lines))
    .bind(total)
    .bind(status)
    .bind(&notes)
    .bind(uuid::Uuid::new_v4())
    .fetch_one(&ctx.state.db)
    .await?;
    crate::contacts::learn(&ctx.state.db, ctx.business_id, ctx.peer.as_deref(), Some(&phone), Some(name), None).await;
    if status == "confirmed" {
        crate::outcomes::confirm(ctx.state, ctx.business_id, "sale", row.0, &phone, name).await;
    }

    Ok(json!({
        "status": status,
        "orderId": row.0,
        "items": lines,
        "total": total,
        "currency": locale.currency,
        "totalFormatted": locale.money(total),
        "note": if upfront {
            format!("ask the customer to pay {} via {} to confirm; \
                     verification is automatic when the payment arrives", locale.money(total), locale.rails)
        } else {
            "order confirmed — payment on delivery/pickup".into()
        }
    }))
}

impl Plugin<Agente> for Sales {
    fn name(&self) -> &'static str {
        "sales"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.tool(
            meta(Scope::Customer, true),
            json!({"type": "function", "function": {
                "name": "create_order",
                "description": "Take a product order (ONLY when the business has values.products). \
                    Prices come from the catalog server-side; never quote prices not in it. \
                    For businesses that collect via transfer/upfront the order starts pending_payment \
                    and confirms automatically when the payment notification arrives.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "customer_name": {"type": "string"},
                        "items": {"type": "array", "items": {"type": "object", "properties": {
                            "product": {"type": "string"},
                            "qty": {"type": "integer", "minimum": 1}
                        }, "required": ["product"]}},
                        "notes": {"type": "string", "description": "delivery address / pickup time / anything the owner needs"},
                        "delivery_to": {"type": "string", "description": "destination if the customer wants delivery — the fee is resolved from the business's zone config and added to the total; quote_delivery first"}
                    },
                    "required": ["customer_name", "items"]
                }
            }}),
            tool_fn(|c, a| Box::pin(create_order(c, a))),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lookup_is_case_and_direction_insensitive() {
        let v = json!({"products": {"Polo Yaya": 35, "gorra": 25}});
        assert_eq!(product_price(&v, "polo yaya"), Some(("Polo Yaya".into(), 35.0)));
        assert_eq!(product_price(&v, "una GORRA"), Some(("gorra".into(), 25.0)));
        assert_eq!(product_price(&v, "zapatos"), None);
        assert_eq!(product_price(&json!({}), "polo"), None, "no catalog at all");
    }

    use crate::testkit::{self, set_values, tool_ctx};

    #[test]
    fn the_exact_product_wins_and_ambiguity_is_refused() {
        let v = json!({"products": {"Polo": 10, "Polo rojo": 35, "Polo azul": 30}});
        assert_eq!(product_price(&v, "polo rojo"), Some(("Polo rojo".into(), 35.0)), "never the cheaper near-match");
        assert_eq!(product_price(&v, " POLO "), Some(("Polo".into(), 10.0)));
        let v = json!({"products": {"Polo azul": 30, "Polo rojo": 35}});
        assert_eq!(product_price(&v, "polo"), None, "two polos: ask which");
        assert_eq!(product_price(&v, "polo rojo talla M"), Some(("Polo rojo".into(), 35.0)), "the one key it contains");
        assert_eq!(product_price(&json!({"products": {"Inca Kola 1L": "7.50"}}), "inca kola"), Some(("Inca Kola 1L".into(), 7.5)), "prices typed as text");
        assert_eq!(product_price(&json!({"products": {"x": "gratis"}}), "x"), None);
        assert_eq!(product_price(&json!({}), "x"), None);
    }

    async fn shop(values: Value) -> (crate::SharedState, uuid::Uuid) {
        let s = testkit::state().await;
        let b = testkit::business(&s.db).await;
        set_values(&s.db, b, values).await;
        (s, b)
    }

    #[tokio::test]
    async fn orders_are_priced_from_the_catalog() {
        let (s, b) = shop(json!({"products": {"Polo": 30, "Gorra": 15}, "paymentMethod": "yape"})).await;
        let c = tool_ctx(&s, b, Some("com.whatsapp:+51 1")).await;
        let r = create_order(&c, &json!({"customer_name": "Ana", "items": [{"product": "polo", "qty": 2, "price": 1}, {"product": "gorra"}]})).await.unwrap();
        assert_eq!((r["status"].clone(), r["total"].clone(), r["totalFormatted"].clone()), (json!("pending_payment"), json!(75.0), json!("S/ 75")));
        // Same order again: the existing one comes back.
        let again = create_order(&c, &json!({"customer_name": "Ana", "items": [{"product": "polo", "qty": 2}, {"product": "gorra"}]})).await.unwrap();
        assert_eq!((again["status"].clone(), again["orderId"].clone()), (json!("already_pending"), r["orderId"].clone()));
        assert_eq!(create_order(&c, &json!({"customer_name": "Ana", "items": [{"product": "zapatillas"}]})).await.unwrap()["status"], "rejected");
        assert!(create_order(&c, &json!({"customer_name": "Ana", "items": []})).await.is_err());
        assert!(create_order(&c, &json!({"items": [{"product": "polo"}]})).await.is_err());
        let qty: i64 = serde_json::from_str::<Value>(&sqlx::query_scalar::<_, String>("SELECT items FROM orders").fetch_one(&s.db).await.unwrap()).unwrap()[0]["qty"].as_i64().unwrap();
        assert_eq!(qty, 2);
    }

    #[tokio::test]
    async fn pay_on_pickup_confirms_and_bills_at_once() {
        let (s, b) = shop(json!({"products": {"Polo": 30}})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let r = create_order(&c, &json!({"customer_name": "Ana", "items": [{"product": "polo", "qty": 5000}]})).await.unwrap();
        assert_eq!((r["status"].clone(), r["total"].clone()), (json!("confirmed"), json!(29970.0)), "qty clamps to 999");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outcomes").fetch_one(&s.db).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn no_catalog_no_shop_and_delivery_lines() {
        let (s, b) = shop(json!({})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        assert_eq!(create_order(&c, &json!({"customer_name": "A", "items": [{"product": "x"}]})).await.unwrap()["status"], "rejected");
        let (s, b) = shop(json!({"products": {"Polo": 30}, "delivery": {"zones": {"Miraflores": 8}}})).await;
        let c = tool_ctx(&s, b, Some("p")).await;
        let r = create_order(&c, &json!({"customer_name": "A", "items": [{"product": "polo"}], "delivery_to": "Miraflores", "notes": "timbre 2"})).await.unwrap();
        assert_eq!(r["total"], 38.0, "{r}");
        let notes: Option<String> = sqlx::query_scalar("SELECT notes FROM orders").fetch_one(&s.db).await.unwrap();
        assert_eq!(notes.as_deref(), Some("timbre 2 · delivery a: Miraflores"));
        let r = create_order(&c, &json!({"customer_name": "B", "items": [{"product": "polo"}], "delivery_to": "Marte"})).await.unwrap();
        assert_eq!(r["status"], "unknown_delivery_zone");
    }
}
