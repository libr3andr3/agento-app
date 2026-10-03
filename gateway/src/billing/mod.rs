//! Money in, on the web. A checkout opens with the provider that fits the
//! buyer — Dodo Payments (merchant of record, worldwide) or Izipay /
//! Micuentaweb (Perú, a monthly subscription on the card) — and closes on
//! the provider's webhook/IPN, which is the only thing that activates a
//! plan. Every Peruvian charge gets a boleta electrónica from NubeFact.
//! Each provider is a module that is simply absent when its keys are.

pub mod dodo;
pub mod izipay;
pub mod nubefact;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use serde_json::{json, Value};

use crate::{accounts, err, internal, plans, ApiResult, App, Auth, Shared};

pub struct Billing {
    pub dodo: Option<dodo::Dodo>,
    pub izipay: Option<izipay::Izipay>,
    pub nubefact: Option<nubefact::Nubefact>,
}

impl Billing {
    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Self> {
        let b = Self { dodo: dodo::Dodo::from_env(http.clone())?, izipay: izipay::Izipay::from_env(http.clone())?, nubefact: nubefact::Nubefact::from_env(http)? };
        tracing::info!(dodo = b.dodo.is_some(), izipay = b.izipay.is_some(), nubefact = b.nubefact.is_some(), "billing providers");
        Ok(b)
    }

    /// What the web app may offer this account.
    pub fn methods(&self, country: &str) -> Value {
        let mut m = Vec::new();
        if country == "PE" {
            if self.izipay.is_some() { m.push(json!({"id": "izipay", "label": "Tarjeta (Izipay)", "recurring": true, "boleta": self.nubefact.is_some()})); }
            m.push(json!({"id": "yape", "label": "Yape / Plin", "recurring": false, "boleta": self.nubefact.is_some()}));
        } else if self.dodo.is_some() {
            m.push(json!({"id": "dodo", "label": "Card / PayPal (Dodo Payments)", "recurring": true, "boleta": false}));
        }
        json!(m)
    }
}

impl Billing {
    /// True when a card checkout can be opened for this country right now.
    pub fn card_available(&self, country: &str) -> bool {
        if country == "PE" { self.izipay.is_some() || self.dodo.is_some() } else { self.dodo.is_some() }
    }

    /// The provider a checkout defaults to when the client names none.
    fn default_method(&self, country: &str) -> Option<&'static str> {
        if country == "PE" && self.izipay.is_some() { return Some("izipay"); }
        if self.dodo.is_some() { return Some("dodo"); }
        if self.izipay.is_some() { return Some("izipay"); }
        None
    }
}

/// Country of an account: from its verified phone's dialling code (51 = PE),
/// else the `country` the web app sends.
pub fn country_of(phone: Option<&str>, hint: Option<&str>) -> String {
    if let Some(p) = phone {
        if p.starts_with("51") { return "PE".into(); }
    }
    hint.map(|c| c.trim().to_ascii_uppercase()).filter(|c| c.len() == 2).unwrap_or_else(|| "PE".into())
}

#[derive(serde::Deserialize)]
pub struct CheckoutReq {
    /// pro | max | enterprise — or absent for a recarga (`amountMinor`).
    #[serde(default)] plan: Option<String>,
    #[serde(default)] months: Option<i64>,
    /// A recarga in céntimos: whole soles within `plans::recarga_check`.
    #[serde(default, rename = "amountMinor")] amount_minor: Option<i64>,
    /// dodo | izipay; absent = whichever fits the buyer's country.
    #[serde(default)] method: Option<String>,
    #[serde(default)] country: Option<String>,
    /// "en" → dollars, anything else → soles (D12). Absent: Accept-Language.
    #[serde(default)] lang: Option<String>,
    /// DNI for the boleta (optional; below the SUNAT threshold a boleta may go without one).
    #[serde(default)] dni: Option<String>,
    /// RUC — 11 digits. Present means the buyer wants a factura (D18).
    #[serde(default)] ruc: Option<String>,
    #[serde(default)] name: Option<String>,
    /// Where the comprobante is sent. Required in Perú: an account that
    /// signed in by phone alone has no real address of its own.
    #[serde(default)] email: Option<String>,
}

/// A document number keeps only its alphanumerics; empty becomes absent.
fn clean_doc(v: Option<&String>) -> Option<String> {
    v.map(|d| d.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()).filter(|d| !d.is_empty())
}

/// The address a comprobante is sent to. An account that signed in by phone
/// alone carries a synthesized `<phone>@phone.yaya.tech`, which is not an
/// inbox — never send a boleta there.
pub fn billing_email(given: Option<&str>, account_email: &str) -> Option<String> {
    let ok = |e: &str| {
        let e = e.trim();
        !e.is_empty() && !e.ends_with("@phone.yaya.tech") && e.contains('@') && !e.starts_with('@') && !e.ends_with('@')
    };
    given.filter(|e| ok(e)).map(|e| e.trim().to_ascii_lowercase())
        .or_else(|| Some(account_email).filter(|e| ok(e)).map(|e| e.trim().to_ascii_lowercase()))
}

/// `POST /v1/account/billing/checkout` — opens a card checkout for a plan
/// (`{plan, months}`) or a recarga (`{amountMinor}`). Dodo answers with a
/// hosted URL; Izipay with a form token the page embeds. The plan or the
/// credits only turn on from the provider's webhook (`settle`).
pub async fn checkout(State(app): State<Shared>, Extension(auth): Extension<Auth>, headers: HeaderMap, Json(req): Json<CheckoutReq>) -> ApiResult {
    let acct = accounts::session_of(&app, &auth).await?;
    let country = country_of(acct.phone.as_deref(), req.country.as_deref());
    let cur = plans::currency_of(&headers, req.lang.as_deref());
    // What is being bought: a tier for some months, or credits.
    let (plan, months, amount, label) = match (req.plan.as_deref().map(str::trim).filter(|p| !p.is_empty()), req.amount_minor) {
        (Some(p), _) => {
            let tier = plans::tier(p);
            if tier.name == "free" || tier.name == "trial" {
                return Err(err(StatusCode::BAD_REQUEST, "choose pro, max or custom"));
            }
            let months = req.months.unwrap_or(1).clamp(1, 12);
            if plans::quoted(tier.name) {
                return Err(err(StatusCode::BAD_REQUEST, "enterprise is quoted in a discovery call — write to sales"));
            }
            // The amount is priced below, in the currency the chosen provider charges.
            (tier.name.to_string(), months, 0, tier.name.to_uppercase())
        }
        (None, Some(a)) => {
            plans::recarga_check(a)?;
            // `months` carries the céntimos bought, exactly as `plan_requests` does.
            ("credits".to_string(), a, a, "RECARGA".to_string())
        }
        (None, None) => return Err(err(StatusCode::BAD_REQUEST, "send {plan, months} or {amountMinor}")),
    };
    let method = match req.method.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        // Dollar prices are only sold through Dodo (USD, merchant of record).
        None if cur == "USD" => "dodo".to_string(),
        None => app.billing.default_method(&country).ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "card payments are not available yet; pay by Yape/Plin"))?.to_string(),
    };
    // Price in the currency the provider actually charges, never the one
    // the browser speaks: Izipay charges soles, Dodo dollars. Pricing a plan
    // in USD and handing that number to Izipay sold Pro for S/ 29.
    let amount = match method.as_str() {
        _ if plan == "credits" => amount,
        "izipay" => plans::amount_minor_in(&plan, months, "PEN"),
        "dodo" => plans::amount_minor_in(&plan, months, "USD"),
        _ => amount,
    };
    let id = format!("YAYA-{}-{}", label, uuid::Uuid::new_v4().simple().to_string()[..8].to_uppercase());
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from).or(acct.name.clone());
    // A RUC wins over a DNI: it is the buyer asking for a factura (D18 §6).
    let ruc = clean_doc(req.ruc.as_ref()).filter(|r| r.len() == 11 && r.chars().all(|c| c.is_ascii_digit()));
    if req.ruc.is_some() && ruc.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "a RUC is 11 digits"));
    }
    let (dni, doc_type) = match ruc {
        Some(r) => (Some(r), Some("RUC")),
        None => match clean_doc(req.dni.as_ref()) {
            Some(d) => (Some(d), Some("DNI")),
            None => (None, None),
        },
    };
    let email = billing_email(req.email.as_deref(), &acct.email);
    // In Perú the charge produces a comprobante, and a comprobante needs
    // somewhere to arrive. Ask now rather than issue one nobody receives.
    if cur == "PEN" && app.billing.nubefact.is_some() && email.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "an email is required for the boleta or factura"));
    }
    let provider_email = email.clone().unwrap_or_else(|| acct.email.clone());
    let (provider, currency, answer) = match method.as_str() {
        "dodo" => {
            let d = app.billing.dodo.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "card payments are not configured for your region yet"))?;
            let url = d.checkout_url(&plan, months, &provider_email, name.as_deref(), &country, &id, &acct.id).await.map_err(|e| { tracing::error!("dodo: {e}"); err(StatusCode::BAD_GATEWAY, "payment provider unavailable") })?;
            // Dodo prices its products in USD and is the merchant of record:
            // no boleta from us, and the webhook amount is not compared to soles.
            ("dodo", "USD".to_string(), json!({"url": url}))
        }
        "izipay" => {
            let z = app.billing.izipay.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "card payments are not configured yet"))?;
            // A plan is a subscription on the card; a recarga is one charge.
            let register = z.recurring() && plan != "credits";
            if register {
                let same: Option<(String,)> = sqlx::query_as(
                    "SELECT checkout FROM card_subscriptions WHERE account = $1 AND plan = $2 AND months = $3 AND status IN ('active','pending')")
                    .bind(&acct.id).bind(&plan).bind(months).fetch_optional(&app.db).await.map_err(internal)?;
                if same.is_some() {
                    return Err(err(StatusCode::CONFLICT, "ya tienes esta suscripción activa: se renueva sola cada periodo"));
                }
            }
            let form = z.form_token(&id, amount, &provider_email, name.as_deref(), &acct.id, &plan, register).await.map_err(|e| { tracing::error!("izipay: {e}"); err(StatusCode::BAD_GATEWAY, "payment provider unavailable") })?;
            ("izipay", "PEN".to_string(), json!({"formToken": form, "publicKey": z.public_key(), "script": izipay::SCRIPT_URL, "mode": z.mode(),
                                                 "recurring": register, "interval": if !register { Value::Null } else if months >= 12 { json!("year") } else { json!("month") }}))
        }
        _ => return Err(err(StatusCode::BAD_REQUEST, "method must be dodo or izipay")),
    };
    sqlx::query(
        "INSERT INTO checkouts (id, account, provider, plan, months, amount_minor, currency, customer_doc, customer_name, customer_email, customer_doc_type) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    ).bind(&id).bind(&acct.id).bind(provider).bind(&plan).bind(months).bind(amount).bind(&currency).bind(&dni).bind(&name).bind(&email).bind(doc_type)
    .execute(&app.db).await.map_err(internal)?;
    tracing::info!(account = %acct.id, %id, provider, %plan, months, "checkout opened");
    Ok(Json(json!({"id": id, "provider": provider, "plan": plan, "months": if plan == "credits" { 0 } else { months },
                   "comprobante": if currency == "PEN" && app.billing.nubefact.is_some() { Some(nubefact::Doc::for_doc(dni.as_deref(), doc_type).as_str()) } else { None },
                   "creditsMinor": if plan == "credits" { Some(amount) } else { None },
                   "amount": amount as f64 / 100.0, "currency": currency, "answer": answer})).into_response())
}

/// `GET /v1/account/billing/checkout/{id}` — the client polls this after the
/// provider redirects back; `paid` flips when the webhook has landed.
pub async fn status(State(app): State<Shared>, Extension(auth): Extension<Auth>, Path(id): Path<String>) -> ApiResult {
    let acct = accounts::session_of(&app, &auth).await?;
    let row: Option<(String, String, String, i64, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT provider, plan, status, months, amount_minor, currency, paid_at FROM checkouts WHERE id = $1 AND account = $2",
    ).bind(&id).bind(&acct.id).fetch_optional(&app.db).await.map_err(internal)?;
    let Some((provider, plan, status, months, amount, currency, paid_at)) = row else { return Err(err(StatusCode::NOT_FOUND, "unknown checkout")) };
    let invoice: Option<(String, i64, Option<String>, String, String)> = sqlx::query_as("SELECT serie, numero, pdf_url, status, kind FROM invoices WHERE checkout = $1 ORDER BY created_at DESC LIMIT 1")
        .bind(&id).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(Json(json!({"id": id, "provider": provider, "plan": plan, "status": status, "months": months, "amount": amount as f64 / 100.0, "currency": currency, "paidAt": paid_at,
                   "invoice": invoice.map(|(s, n, pdf, st, kind)| json!({"number": format!("{s}-{n}"), "pdf": pdf, "status": st, "kind": kind}))})).into_response())
}

/// The one place a plan turns on from a card: idempotent on the provider's
/// event id, extends the plan by the checkout's months (renewals extend it
/// again), grants credits through `plans::set`, and issues the boleta.
/// A recarga (`plan = credits`, `months` = céntimos) lands as a topup —
/// with the launch bonus, like a Yape recarga: the bonus is a promotion on
/// buying credits, not on the rail they came through. The account ends in
/// the same state a manual `/admin/plan {ref}` confirmation produces, and
/// any Yape/Plin request still open for the same thing is closed so the
/// app stops asking for a transfer that was paid by card.
pub async fn settle(app: &App, checkout_id: &str, provider: &str, event_id: &str, paid_amount_minor: Option<i64>, paid_currency: Option<&str>) -> anyhow::Result<bool> {
    let fresh = sqlx::query("INSERT OR IGNORE INTO billing_events (id, provider, checkout, kind) VALUES ($1, $2, $3, 'paid')")
        .bind(event_id).bind(provider).bind(checkout_id).execute(&app.db).await?.rows_affected();
    if fresh == 0 {
        return Ok(false);
    }
    let row: Option<(String, String, i64, i64, String, Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT account, plan, months, amount_minor, currency, customer_doc, customer_name, customer_email, customer_doc_type FROM checkouts WHERE id = $1",
    ).bind(checkout_id).fetch_optional(&app.db).await?;
    let Some((account, plan, months, amount, currency, doc, name, doc_email, doc_type)) = row else { anyhow::bail!("unknown checkout {checkout_id}") };
    // A card recarga through Dodo is `amount` céntimos of soles sold as that
    // many one-sol units of a Dodo product priced in dollars: the dollar
    // total is Dodo's to set, so only its currency is checked. Every other
    // checkout is recorded in the currency its provider charges.
    let comparable = !(plan == "credits" && provider == "dodo");
    if let Some(paid) = paid_amount_minor.filter(|_| comparable) {
        if paid < amount { anyhow::bail!("paid {paid} < expected {amount} for {checkout_id}"); }
    }
    if let Some(cur) = paid_currency.map(str::trim).filter(|c| !c.is_empty()) {
        if !cur.eq_ignore_ascii_case(&currency) { anyhow::bail!("paid in {cur}, checkout is in {currency} for {checkout_id}"); }
    }
    let marked = sqlx::query("UPDATE checkouts SET status = 'paid', paid_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = $1 AND (status <> 'paid' OR plan <> 'credits')")
        .bind(checkout_id).execute(&app.db).await?.rows_affected();
    // A recarga is one-time money: another event id for a paid one is the same payment.
    if marked == 0 {
        tracing::warn!(%checkout_id, %event_id, provider, "recarga already paid; event ignored");
        return Ok(false);
    }
    let subject = accounts::subject(&account);
    if plan == "credits" {
        crate::credits::add(app, &account, months, "topup", Some(checkout_id), Some("recarga con tarjeta")).await.map_err(|e| anyhow::anyhow!("topup: {:?}", e.1))?;
        let bonus = crate::wallet::bonus_for(months);
        if bonus > 0 {
            if let Some((pct, _)) = crate::wallet::launch_bonus() {
                let _ = crate::credits::add(app, &account, bonus, "bonus", Some(checkout_id), Some(&format!("lanzamiento: +{pct}% en tu recarga"))).await;
            }
        }
        sqlx::query("UPDATE plan_requests SET status = 'cancelled' WHERE agent = $1 AND status = 'pending' AND plan = 'credits' AND months = $2")
            .bind(&subject).bind(months).execute(&app.db).await?;
        tracing::info!(%account, %checkout_id, provider, credits = months, "recarga paid by card");
        return Ok(true);
    }
    plans::set(app, &subject, &plan, months, provider, Some(checkout_id)).await.map_err(|e| anyhow::anyhow!("plan set: {:?}", e.1))?;
    sqlx::query("UPDATE plan_requests SET status = 'cancelled' WHERE agent = $1 AND status = 'pending' AND plan <> 'credits'")
        .bind(&subject).execute(&app.db).await?;
    tracing::info!(%account, %checkout_id, provider, %plan, months, "plan activated by card");
    if currency == "PEN" {
        let desc = format!("Plan {} agente — {} mes(es)", capitalize(&plan), months);
        if let Err(e) = issue_comprobante(app, &account, checkout_id, amount, &desc,
                                          doc.as_deref(), doc_type.as_deref(), name.as_deref(), doc_email.as_deref()).await {
            tracing::error!(%account, %checkout_id, error = %e, "comprobante not issued");
        }
    }

    Ok(true)
}

/// Issue the SUNAT comprobante for money we have actually received, and file
/// it. Every channel lands here — a card through `settle()`, a Yape transfer
/// through `plans::confirm_request` — because the document SUNAT wants does
/// not depend on how the customer paid. `order` is whatever reference the
/// payment is known by: a checkout id for cards, a `plan_requests.ref` for
/// Yape.
#[allow(clippy::too_many_arguments)]
pub async fn issue_comprobante(
    app: &App,
    account: &str,
    order: &str,
    amount_minor: i64,
    desc: &str,
    doc: Option<&str>,
    doc_type: Option<&str>,
    name: Option<&str>,
    doc_email: Option<&str>,
) -> anyhow::Result<()> {
    let Some(nf) = app.billing.nubefact.as_ref() else { return Ok(()) };
    // D18: a RUC means the buyer wants to deduct this, and only a factura
    // lets them. What the order itself carries wins; the account's fiscal
    // profile fills in whatever the order never asked for, so a business
    // that gave us its RUC once gets a factura for every charge after
    // without being asked again.
    let profile: Option<(String, String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT doc_type, doc, name, address, email FROM billing_profiles WHERE account = $1")
        .bind(account).fetch_optional(&app.db).await?;
    let (doc, doc_type, name, address, profile_email) = match (&profile, doc) {
        (Some((p_type, p_doc, p_name, p_addr, p_mail)), None) => (
            Some(p_doc.clone()), Some(p_type.clone()), Some(p_name.clone()), p_addr.clone(), p_mail.clone(),
        ),
        (p, given) => (
            given.map(str::to_string), doc_type.map(str::to_string), name.map(str::to_string),
            p.as_ref().and_then(|x| x.3.clone()), p.as_ref().and_then(|x| x.4.clone()),
        ),
    };
    // The address on the comprobante is the one the buyer gave; an account's
    // own `<phone>@phone.yaya.tech` is not an inbox.
    let account_email: Option<(String,)> = sqlx::query_as("SELECT email FROM accounts WHERE id = $1")
        .bind(account).fetch_optional(&app.db).await?;
    let email = billing_email(doc_email.or(profile_email.as_deref()), account_email.as_ref().map(|e| e.0.as_str()).unwrap_or(""));
    let (doc, doc_type) = (doc.as_deref(), doc_type.as_deref());
    let kind = nubefact::Doc::for_doc(doc, doc_type);
    // SUNAT wants the buyer identified on a boleta once the total passes
    // S/700. Issuing "sin documento" above that is a defective document, so
    // refuse before a correlative is spent on one: the sale stands, the
    // comprobante is issued by hand once we have the DNI.
    if kind == nubefact::Doc::Boleta && doc.is_none() && amount_minor > BOLETA_DNI_FLOOR_MINOR {
        anyhow::bail!("a boleta over S/700 needs the buyer's document; none was given");
    }
    let serie = nf.serie_of(kind).to_string();
    let numero = next_invoice_number(app, &serie).await?;
    match nf.emit(kind, numero, amount_minor, desc, doc, name.as_deref(), address.as_deref(), email.as_deref()).await {
        Ok(r) => {
            sqlx::query("INSERT INTO invoices (id, account, checkout, serie, numero, total_minor, currency, status, kind, pdf_url, xml_url, response) VALUES ($1,$2,$3,$4,$5,$6,'PEN',$7,$8,$9,$10,$11)")
                .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(order).bind(&serie).bind(numero).bind(amount_minor)
                .bind(if r.accepted { "accepted" } else { "issued" }).bind(kind.as_str()).bind(&r.pdf_url).bind(&r.xml_url).bind(r.raw.to_string())
                .execute(&app.db).await?;
            tracing::info!(%account, %serie, numero, kind = kind.as_str(), accepted = r.accepted, "comprobante issued");
        }
        Err(e) => {
            // The money is real and the plan is on; the document is not.
            // Record the failure against its number so the correlative stays
            // honest and it can be reissued.
            tracing::error!(%account, %serie, numero, kind = kind.as_str(), error = %e, "comprobante failed");
            sqlx::query("INSERT INTO invoices (id, account, checkout, serie, numero, total_minor, currency, status, kind, response) VALUES ($1,$2,$3,$4,$5,$6,'PEN','error',$7,$8)")
                .bind(uuid::Uuid::new_v4().to_string()).bind(account).bind(order).bind(&serie).bind(numero).bind(amount_minor).bind(kind.as_str()).bind(e.to_string())
                .execute(&app.db).await?;
        }
    }
    Ok(())
}

/// Past this total a boleta must name its buyer (SUNAT, in céntimos).
pub const BOLETA_DNI_FLOOR_MINOR: i64 = 70000;

/// The business's fiscal identity, asked once. A RUC here turns every later
/// charge into a factura the owner can deduct — which is most of why a
/// Peruvian business buys software in the company's name at all.
#[derive(serde::Deserialize)]
pub struct ProfileReq {
    /// DNI | RUC. A RUC is what makes the comprobante a factura.
    #[serde(default)] doc_type: Option<String>,
    #[serde(default)] doc: Option<String>,
    /// Razón social, spelled the way SUNAT spells it.
    #[serde(default)] name: Option<String>,
    /// Dirección fiscal.
    #[serde(default)] address: Option<String>,
    #[serde(default)] email: Option<String>,
}

pub async fn get_profile(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let acct = accounts::session_of(&app, &auth).await?;
    Ok(Json(profile_json(&app, &acct.id).await?).into_response())
}

pub async fn put_profile(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<ProfileReq>) -> ApiResult {
    let acct = accounts::session_of(&app, &auth).await?;
    let doc = clean_doc(req.doc.as_ref()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "falta el número de documento"))?;
    let kind = nubefact::Doc::for_doc(Some(&doc), req.doc_type.as_deref());
    let name = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // SUNAT will not take a factura addressed to "CLIENTE": a factura names
    // the business that is deducting it. A boleta may go without.
    if kind == nubefact::Doc::Factura && name.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "una factura necesita la razón social"));
    }
    let doc_type = if kind == nubefact::Doc::Factura { "RUC" } else { "DNI" };
    sqlx::query(
        "INSERT INTO billing_profiles (account, doc_type, doc, name, address, email) VALUES ($1,$2,$3,$4,$5,$6) \
         ON CONFLICT (account) DO UPDATE SET doc_type = $2, doc = $3, name = $4, address = $5, email = $6, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(&acct.id).bind(doc_type).bind(&doc).bind(name.unwrap_or("")).bind(req.address.as_deref()).bind(req.email.as_deref())
    .execute(&app.db).await.map_err(internal)?;
    tracing::info!(account = %acct.id, doc_type, "fiscal profile saved");
    Ok(Json(profile_json(&app, &acct.id).await?).into_response())
}

/// What the app renders on the billing screen. `comprobante` is the promise:
/// what this account will actually receive the next time it pays.
async fn profile_json(app: &App, account: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let p: Option<(String, String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT doc_type, doc, name, address, email FROM billing_profiles WHERE account = $1")
        .bind(account).fetch_optional(&app.db).await.map_err(internal)?;
    Ok(match p {
        Some((t, d, n, a, e)) => json!({
            "docType": t, "doc": d, "name": n, "address": a, "email": e,
            "comprobante": if t == "RUC" { "factura" } else { "boleta" },
        }),
        None => json!({
            "docType": Value::Null, "doc": Value::Null, "name": Value::Null,
            "address": Value::Null, "email": Value::Null, "comprobante": "boleta",
        }),
    })
}

async fn next_invoice_number(app: &App, serie: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "INSERT INTO invoice_counters (serie, last) VALUES ($1, COALESCE((SELECT MAX(numero) FROM invoices WHERE serie = $1), 0) + 1) \
         ON CONFLICT (serie) DO UPDATE SET last = invoice_counters.last + 1 RETURNING last",
    ).bind(serie).fetch_one(&app.db).await?;
    Ok(n)
}

pub(crate) fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() { Some(f) => f.to_uppercase().collect::<String>() + c.as_str(), None => String::new() }
}

/// For `/v1/account`: methods for this account, open/paid checkouts, boletas.
pub async fn summary(app: &App, acct: &accounts::Account) -> Result<Value, (StatusCode, Json<Value>)> {
    let country = country_of(acct.phone.as_deref(), None);
    let checkouts: Vec<(String, String, String, String, i64, i64, String, String)> = sqlx::query_as(
        "SELECT id, provider, plan, status, months, amount_minor, currency, created_at FROM checkouts WHERE account = $1 ORDER BY created_at DESC LIMIT 20",
    ).bind(&acct.id).fetch_all(&app.db).await.map_err(internal)?;
    let invoices: Vec<(String, i64, i64, String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT serie, numero, total_minor, status, pdf_url, created_at, kind FROM invoices WHERE account = $1 ORDER BY created_at DESC LIMIT 20",
    ).bind(&acct.id).fetch_all(&app.db).await.map_err(internal)?;
    let channels = plans::payment_channels();
    Ok(json!({
        "subscriptions": subscriptions_json(app, &acct.id).await?,
        "country": country,
        "card": app.billing.card_available(&country),
        "yape": channels["yape"].is_string(),
        "plin": channels["plin"].is_string(),
        "methods": app.billing.methods(&country),
        "checkouts": checkouts.into_iter().map(|(id, p, pl, st, m, a, c, at)| json!({"id": id, "provider": p, "plan": pl, "status": st, "months": m, "amount": a as f64 / 100.0, "currency": c, "createdAt": at})).collect::<Vec<_>>(),
        "invoices": invoices.into_iter().map(|(s, n, t, st, pdf, at, kind)| json!({"number": format!("{s}-{n}"), "total": t as f64 / 100.0, "status": st, "pdf": pdf, "createdAt": at, "kind": kind})).collect::<Vec<_>>(),
    }))
}

/// `POST /v1/billing/webhook/dodo` — Standard Webhooks envelope
/// (`webhook-id`, `webhook-timestamp`, `webhook-signature`). Anything that
/// does not verify is refused; a verified event we do not act on is
/// acknowledged so Dodo stops retrying it. `payment.succeeded` and
/// subscription renewals settle the checkout named in `metadata.checkout`.
pub async fn dodo_webhook(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let Some(d) = app.billing.dodo.as_ref() else { return Err(err(StatusCode::NOT_FOUND, "dodo is not configured")) };
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string();
    let (id, ts, sig) = (h("webhook-id"), h("webhook-timestamp"), h("webhook-signature"));
    if id.is_empty() || ts.is_empty() || sig.is_empty() {
        return Err(err(StatusCode::UNAUTHORIZED, "missing webhook signature headers"));
    }
    let now = chrono::Utc::now().timestamp();
    match ts.parse::<i64>() {
        Ok(t) if (now - t).abs() <= 300 => {}
        _ => return Err(err(StatusCode::UNAUTHORIZED, "webhook timestamp out of tolerance")),
    }
    if !d.verify(&id, &ts, &sig, &body) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad webhook signature"));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| err(StatusCode::BAD_REQUEST, "webhook body is not JSON"))?;
    let kind = v["type"].as_str().unwrap_or("");
    let data = &v["data"];
    let settles = matches!(kind, "payment.succeeded" | "subscription.active" | "subscription.renewed");
    if !settles {
        tracing::info!(%id, kind, "dodo event ignored");
        return Ok(Json(json!({"ok": true, "ignored": kind})).into_response());
    }
    let Some(checkout) = data["metadata"]["checkout"].as_str().filter(|c| !c.is_empty()) else {
        tracing::warn!(%id, kind, "dodo event without metadata.checkout");
        return Ok(Json(json!({"ok": true, "ignored": "no checkout"})).into_response());
    };
    // Renewals carry a new payment id each month; the webhook id is the fallback.
    let event_id = data["payment_id"].as_str().filter(|p| !p.is_empty()).map(|p| format!("dodo:{p}")).unwrap_or_else(|| format!("dodo:{id}"));
    // Dodo reports what it charged: verified like the IPN's amount.
    match settle(&app, checkout, "dodo", &event_id, data["total_amount"].as_i64(), data["currency"].as_str()).await {
        Ok(fresh) => Ok(Json(json!({"ok": true, "checkout": checkout, "settled": fresh})).into_response()),
        Err(e) => {
            tracing::error!(%checkout, %event_id, error = %e, "dodo settle failed");
            if e.to_string().starts_with("unknown checkout") {
                Err(err(StatusCode::NOT_FOUND, "unknown checkout"))
            } else {
                Err(internal(e))
            }
        }
    }
}

/// `POST /v1/billing/webhook/izipay` — the Micuentaweb IPN: a form with
/// `kr-answer` (JSON) and `kr-hash` (HMAC-SHA256 with the REST password).
/// Two Back Office rules send here: "al final del pago" (every checkout,
/// also named per payment by `ipnTargetUrl`) and "al crear una recurrencia"
/// (each subscription installment). Izipay expects a 200 to stop retrying,
/// so an answer we could not fully act on — a subscription Izipay refused
/// to create — is a 500, and the retry picks it up again.
pub async fn izipay_ipn(State(app): State<Shared>, axum::Form(form): axum::Form<std::collections::HashMap<String, String>>) -> ApiResult {
    let Some(z) = app.billing.izipay.as_ref() else { return Err(err(StatusCode::NOT_FOUND, "izipay is not configured")) };
    let answer = form.get("kr-answer").map(String::as_str).unwrap_or("");
    let hash = form.get("kr-hash").map(String::as_str).unwrap_or("");
    if answer.is_empty() || hash.is_empty() || !z.verify(answer, hash, false) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad IPN signature"));
    }
    let v: Value = serde_json::from_str(answer).map_err(|_| err(StatusCode::BAD_REQUEST, "kr-answer is not JSON"))?;
    match apply_izipay(&app, &v).await {
        Ok(done) => Ok(done.into_response()),
        Err(e) if e.to_string().starts_with("unknown checkout") => Err(err(StatusCode::NOT_FOUND, "unknown checkout")),
        Err(e) if e.to_string().starts_with("no orderId") => Err(err(StatusCode::BAD_REQUEST, "no orderId")),
        Err(e) => { tracing::error!(error = %e, "izipay IPN not applied"); Err(internal(e)) }
    }
}

#[derive(serde::Deserialize)]
pub struct IzipayReturn {
    #[serde(rename = "krAnswer")] kr_answer: String,
    #[serde(rename = "krHash")] kr_hash: String,
}

/// `POST /v1/account/billing/izipay/return` — what the embedded form hands
/// the page when the card is accepted, signed with the HMAC-SHA-256 key.
/// It settles exactly like the IPN (same transaction id, so whichever lands
/// second is a no-op): the buyer sees the plan on in seconds even while the
/// IPN is still in flight — or was never configured.
pub async fn izipay_return(State(app): State<Shared>, Extension(auth): Extension<Auth>, Json(req): Json<IzipayReturn>) -> ApiResult {
    let acct = accounts::session_of(&app, &auth).await?;
    let Some(z) = app.billing.izipay.as_ref() else { return Err(err(StatusCode::NOT_FOUND, "izipay is not configured")) };
    if !z.verify(&req.kr_answer, &req.kr_hash, true) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad kr-hash"));
    }
    let v: Value = serde_json::from_str(&req.kr_answer).map_err(|_| err(StatusCode::BAD_REQUEST, "kr-answer is not JSON"))?;
    let order = v["orderDetails"]["orderId"].as_str().unwrap_or("").to_string();
    let owner: Option<(String,)> = sqlx::query_as("SELECT account FROM checkouts WHERE id = $1").bind(&order).fetch_optional(&app.db).await.map_err(internal)?;
    if owner.map(|o| o.0) != Some(acct.id.clone()) {
        return Err(err(StatusCode::NOT_FOUND, "unknown checkout"));
    }
    // The payment is what the buyer waits for; a subscription that could
    // not be scheduled yet is retried by the IPN, not shown as a failure.
    if let Err(e) = apply_izipay(&app, &v).await {
        tracing::error!(%order, error = %e, "izipay browser return not fully applied");
    }
    status(State(app), Extension(auth), Path(order)).await
}

/// Days a subscription plan stays on past the date its renewal is due:
/// Izipay charges in a night batch, and the IPN follows it.
pub const RENEWAL_GRACE_DAYS: i64 = 3;

/// One verified Izipay answer (IPN or browser return), applied once. The
/// first payment of a plan activates it and schedules the subscription on
/// the card it registered; each installment after that is found by its
/// subscription id and renews the same checkout.
pub async fn apply_izipay(app: &App, v: &Value) -> anyhow::Result<String> {
    let z = app.billing.izipay.as_ref().ok_or_else(|| anyhow::anyhow!("izipay is not configured"))?;
    // A test-mode payment never buys anything with production keys (and the
    // other way round), whatever else it claims.
    let mode = v["orderDetails"]["mode"].as_str().unwrap_or("");
    if !mode.is_empty() && mode != z.mode() {
        tracing::warn!(mode, ours = z.mode(), "izipay answer from the other mode ignored");
        return Ok(format!("ignored {mode}"));
    }
    let tx = &v["transactions"][0];
    let sub_id = tx["transactionDetails"]["subscriptionDetails"]["subscriptionId"].as_str().filter(|s| !s.is_empty());
    let renewal_of: Option<(String,)> = match sub_id {
        Some(s) => sqlx::query_as("SELECT checkout FROM card_subscriptions WHERE provider = 'izipay' AND external_id = $1").bind(s).fetch_optional(&app.db).await?,
        None => None,
    };
    let is_renewal = renewal_of.is_some();
    let order = match renewal_of {
        Some((c,)) => c,
        None => v["orderDetails"]["orderId"].as_str().filter(|o| !o.is_empty()).map(String::from).ok_or_else(|| anyhow::anyhow!("no orderId"))?,
    };
    let status = v["orderStatus"].as_str().unwrap_or("");
    if status != "PAID" {
        if is_renewal {
            // Izipay does not retry a refused installment: the plan runs out
            // at the end of its grace, and next period's charge may revive it.
            tracing::warn!(%order, status, "izipay installment not paid");
            sqlx::query("UPDATE card_subscriptions SET last_error = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE checkout = $1")
                .bind(&order).bind(format!("installment {status} {}", tx["detailedErrorCode"].as_str().unwrap_or(""))).execute(&app.db).await?;
        } else {
            tracing::info!(%order, status, "izipay payment not paid");
        }
        return Ok(format!("ignored {status}"));
    }
    let tx_id = tx["uuid"].as_str().filter(|u| !u.is_empty()).map(|u| format!("izipay:{u}"))
        .unwrap_or_else(|| format!("izipay:{order}:{}:{}", v["orderDetails"]["orderTotalAmount"], v["serverDate"].as_str().unwrap_or("")));
    let fresh = settle(app, &order, "izipay", &tx_id, v["orderDetails"]["orderTotalAmount"].as_i64(), v["orderDetails"]["orderCurrency"].as_str()).await?;

    let (account, plan, months, amount, paid_at): (String, String, i64, i64, Option<String>) =
        sqlx::query_as("SELECT account, plan, months, amount_minor, paid_at FROM checkouts WHERE id = $1").bind(&order).fetch_one(&app.db).await?;
    if plan == "credits" || !z.recurring() {
        return Ok("OK".into());
    }
    if is_renewal {
        if fresh {
            let rrule: Option<(Option<String>,)> = sqlx::query_as("SELECT rrule FROM card_subscriptions WHERE checkout = $1").bind(&order).fetch_optional(&app.db).await?;
            let rrule = rrule.and_then(|r| r.0).unwrap_or_else(|| if months >= 12 { "RRULE:FREQ=YEARLY".into() } else { "RRULE:FREQ=MONTHLY".into() });
            let now = chrono::Utc::now();
            let next = izipay::Schedule { first: now.date_naive(), rrule }.next_after(now);
            sqlx::query("UPDATE card_subscriptions SET next_charge_on = $2, last_error = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE checkout = $1")
                .bind(&order).bind(next.to_string()).execute(&app.db).await?;
            hold_plan(app, &account, &plan, next).await?;
            tracing::info!(%order, %account, %plan, %next, "izipay installment paid; plan renewed");
        }
        return Ok("OK".into());
    }
    // The first payment: the card it registered carries every later period.
    let Some(token) = tx["paymentMethodToken"].as_str().filter(|t| !t.is_empty()) else {
        tracing::error!(%order, "izipay plan paid without a card token: no subscription, the plan runs its months");
        return Ok("OK".into());
    };
    let paid = paid_at.as_deref().and_then(|p| chrono::DateTime::parse_from_rfc3339(p).ok()).map(|t| t.with_timezone(&chrono::Utc)).unwrap_or_else(chrono::Utc::now);
    let sched = izipay::Schedule::after(paid, months);
    ensure_subscription(app, z, &order, &account, &plan, months, amount, token, &sched).await?;
    Ok("OK".into())
}

/// Schedules the subscription for a paid plan checkout, once. The row is
/// claimed before Izipay is called, so the IPN and the browser return
/// racing each other create one subscription; a failed attempt is left as
/// `error` for the next IPN delivery to retry.
#[allow(clippy::too_many_arguments)]
async fn ensure_subscription(app: &App, z: &izipay::Izipay, checkout: &str, account: &str, plan: &str, months: i64, amount: i64, token: &str, sched: &izipay::Schedule) -> anyhow::Result<()> {
    let claimed = sqlx::query(
        "INSERT INTO card_subscriptions (checkout, account, provider, plan, months, amount_minor, currency, token, rrule, next_charge_on, status) \
         VALUES ($1,$2,'izipay',$3,$4,$5,'PEN',$6,$7,$8,'pending') \
         ON CONFLICT (checkout) DO UPDATE SET status = 'pending', token = excluded.token, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') \
         WHERE card_subscriptions.status = 'error' \
            OR (card_subscriptions.status = 'pending' AND card_subscriptions.updated_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-10 minutes'))",
    ).bind(checkout).bind(account).bind(plan).bind(months).bind(amount).bind(token).bind(&sched.rrule).bind(sched.first.to_string())
    .execute(&app.db).await?.rows_affected();
    if claimed == 0 {
        return Ok(()); // already scheduled (or being scheduled, or cancelled)
    }
    let desc = format!("Plan {} agento — {}", capitalize(plan), if months >= 12 { "anual" } else { "mensual" });
    match z.create_subscription(token, amount, sched, checkout, &desc, json!({"checkout": checkout, "account": account, "plan": plan})).await {
        Ok(id) => {
            sqlx::query("UPDATE card_subscriptions SET status = 'active', external_id = $2, last_error = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE checkout = $1")
                .bind(checkout).bind(&id).execute(&app.db).await?;
            tracing::info!(%checkout, %account, %plan, subscription = %id, first = %sched.first, "izipay subscription scheduled");
            hold_plan(app, account, plan, sched.first).await?;
            // One subscription per account: a new plan replaces the old one.
            let older: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT checkout, token, external_id FROM card_subscriptions WHERE account = $1 AND status = 'active' AND checkout <> $2 AND external_id IS NOT NULL")
                .bind(account).bind(checkout).fetch_all(&app.db).await?;
            for (c, t, ext) in older {
                match z.cancel_subscription(&t, &ext).await {
                    Ok(()) => {
                        sqlx::query("UPDATE card_subscriptions SET status = 'cancelled', cancelled_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE checkout = $1")
                            .bind(&c).execute(&app.db).await?;
                        tracing::info!(%account, replaced = %c, by = %checkout, "older subscription cancelled");
                    }
                    Err(e) => tracing::error!(%account, subscription = %ext, error = %e, "older subscription NOT cancelled — cancel it in the Izipay Back Office"),
                }
            }
            Ok(())
        }
        Err(e) => {
            sqlx::query("UPDATE card_subscriptions SET status = 'error', last_error = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE checkout = $1")
                .bind(checkout).bind(e.to_string()).execute(&app.db).await?;
            Err(e.context(format!("subscription for {checkout} not created")))
        }
    }
}

/// Keeps a subscription plan on until `RENEWAL_GRACE_DAYS` past its next
/// charge. Only ever lengthens the plan, and only the plan it was bought for.
async fn hold_plan(app: &App, account: &str, plan: &str, next_charge: chrono::NaiveDate) -> anyhow::Result<()> {
    let until = (next_charge + chrono::Duration::days(RENEWAL_GRACE_DAYS)).and_hms_opt(0, 0, 0).expect("midnight").and_utc().to_rfc3339();
    sqlx::query("UPDATE plans SET expires_at = $3, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE agent = $1 AND plan = $2 AND (expires_at IS NULL OR expires_at < $3)")
        .bind(accounts::subject(account)).bind(plan).bind(&until).execute(&app.db).await?;
    Ok(())
}

/// `POST /v1/account/billing/subscription/cancel` — no more charges. The
/// plan stays on to the end of the period already paid (the day the next
/// charge would have been), without the grace days kept for that charge.
pub async fn cancel_subscription(State(app): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let acct = accounts::session_of(&app, &auth).await?;
    let rows: Vec<(String, String, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT checkout, token, external_id, plan, next_charge_on FROM card_subscriptions WHERE account = $1 AND status IN ('active','pending','error')")
        .bind(&acct.id).fetch_all(&app.db).await.map_err(internal)?;
    if rows.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no tienes una suscripción activa"));
    }
    for (checkout, token, ext, plan, next) in rows {
        if let Some(ext) = ext.as_deref() {
            let z = app.billing.izipay.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no pudimos cancelar ahora; escríbenos y lo hacemos a mano"))?;
            z.cancel_subscription(&token, ext).await.map_err(|e| {
                tracing::error!(account = %acct.id, subscription = %ext, error = %e, "subscription cancel failed");
                err(StatusCode::BAD_GATEWAY, "no pudimos cancelar ahora; inténtalo en un minuto")
            })?;
        }
        sqlx::query("UPDATE card_subscriptions SET status = 'cancelled', cancelled_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE checkout = $1")
            .bind(&checkout).execute(&app.db).await.map_err(internal)?;
        if let Some(end) = next.as_deref().and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) {
            let until = (end + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).expect("midnight").and_utc().to_rfc3339();
            sqlx::query("UPDATE plans SET expires_at = $3, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE agent = $1 AND plan = $2 AND expires_at > $3")
                .bind(accounts::subject(&acct.id)).bind(&plan).bind(&until).execute(&app.db).await.map_err(internal)?;
        }
        tracing::info!(account = %acct.id, %checkout, "subscription cancelled by the owner");
    }
    Ok(Json(json!({"ok": true, "subscriptions": subscriptions_json(&app, &acct.id).await?})).into_response())
}

/// The account's card subscriptions, newest first, as the web app shows them.
async fn subscriptions_json(app: &App, account: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let rows: Vec<(String, String, i64, i64, String, String, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT checkout, plan, months, amount_minor, currency, status, next_charge_on, created_at, cancelled_at FROM card_subscriptions \
         WHERE account = $1 ORDER BY created_at DESC LIMIT 10")
        .bind(account).fetch_all(&app.db).await.map_err(internal)?;
    Ok(json!(rows.into_iter().map(|(c, p, m, a, cur, st, next, at, cancelled)| json!({
        "checkout": c, "plan": p, "interval": if m >= 12 { "year" } else { "month" }, "amount": a as f64 / 100.0, "currency": cur,
        "status": st, "nextChargeOn": if st == "cancelled" { None } else { next }, "createdAt": at, "cancelledAt": cancelled,
    })).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The billing screen promises what the next comprobante will be, and an
    /// owner deciding whether to buy in the company's name is reading exactly
    /// that. No profile is a boleta; a RUC is a factura they can deduct.
    /// Also pins migration 029 actually running.
    #[tokio::test]
    async fn a_ruc_profile_promises_a_deductible_factura() {
        let app = crate::test_app().await;
        assert_eq!(profile_json(&app, "acct-b2b").await.unwrap()["comprobante"], "boleta");

        sqlx::query(
            "INSERT INTO billing_profiles (account, doc_type, doc, name, address) \
             VALUES ('acct-b2b', 'RUC', '20616460031', 'YAYA TECH S.A.C.', 'Av. Ejemplo 123')",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let p = profile_json(&app, "acct-b2b").await.unwrap();
        assert_eq!(p["comprobante"], "factura");
        assert_eq!(p["doc"], "20616460031");
        assert_eq!(p["name"], "YAYA TECH S.A.C.");
        assert_eq!(p["address"], "Av. Ejemplo 123");
    }

    /// A RUC on the profile must reach the comprobante even when the order
    /// itself carried nothing — that is the point of asking once. Without a
    /// NubeFact token nothing is emitted, so what this pins is the merge:
    /// the profile decides the document kind when the order is silent.
    #[test]
    fn the_profile_decides_when_the_order_is_silent() {
        assert_eq!(nubefact::Doc::for_doc(Some("20616460031"), Some("RUC")), nubefact::Doc::Factura);
        assert_eq!(nubefact::Doc::for_doc(None, None), nubefact::Doc::Boleta);
    }
    use std::sync::Arc;

    /// The Izipay IPN: unsigned → 401; PAID with the right hash settles
    /// (and checks the amount); the same transaction twice settles once.
    #[tokio::test]
    async fn izipay_ipn_settles_a_paid_order() {
        let mut app = crate::test_app().await;
        app.billing.izipay = Some(izipay::Izipay::for_test("pw"));
        let app = Arc::new(app);
        open(&app, "YAYA-PRO-IZI", "a4", "pro", 1, plans::amount_minor("pro", 1), "PEN").await;
        let answer = json!({"orderStatus": "PAID", "orderDetails": {"orderId": "YAYA-PRO-IZI", "orderTotalAmount": plans::amount_minor("pro", 1)}, "transactions": [{"uuid": "tx1"}]}).to_string();
        let form = |a: &str, h: &str| axum::Form(std::collections::HashMap::from([("kr-answer".to_string(), a.to_string()), ("kr-hash".to_string(), h.to_string())]));
        assert_eq!(izipay_ipn(State(app.clone()), form(&answer, "deadbeef")).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
        let good = app.billing.izipay.as_ref().unwrap().hash_for_test(&answer);
        assert_eq!(izipay_ipn(State(app.clone()), form(&answer, &good)).await.unwrap().status(), StatusCode::OK);
        assert_eq!(plans::effective_for(&app, &accounts::subject("a4")).await.unwrap().plan, "pro");
        let b = balance(&app, "a4").await;
        assert_eq!(izipay_ipn(State(app.clone()), form(&answer, &good)).await.unwrap().status(), StatusCode::OK);
        assert_eq!(balance(&app, "a4").await, b);
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM billing_events").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1);
    }

    /// A phone-only account carries a synthesized `<phone>@phone.yaya.tech`
    /// address that nobody reads. A comprobante sent there is a comprobante
    /// nobody receives, so it never counts as a billing address — the one
    /// given at checkout does, and a real account address is the fallback.
    #[test]
    fn the_synthesized_phone_address_is_never_a_billing_address() {
        assert_eq!(billing_email(Some("Ana@Negocio.pe "), "51999@phone.yaya.tech").as_deref(), Some("ana@negocio.pe"));
        assert_eq!(billing_email(None, "51999@phone.yaya.tech"), None);
        assert_eq!(billing_email(Some(""), "51999@phone.yaya.tech"), None);
        assert_eq!(billing_email(Some("nope"), "real@negocio.pe").as_deref(), Some("real@negocio.pe"));
        assert_eq!(billing_email(None, "real@negocio.pe").as_deref(), Some("real@negocio.pe"));
        assert_eq!(billing_email(None, ""), None);
    }

    /// The checkout row is what `settle()` reads long after the browser is
    /// gone, so the document kind and the billing address have to survive
    /// in it — not be re-guessed from the account when the webhook lands.
    #[tokio::test]
    async fn a_checkout_remembers_the_document_and_the_address() {
        let app = crate::test_app().await;
        sqlx::query("INSERT INTO checkouts (id, account, provider, plan, months, amount_minor, currency, customer_doc, customer_doc_type, customer_email) \
                     VALUES ('YAYA-PRO-RUC','a9','izipay','pro',1,15000,'PEN','20512345678','RUC','conta@negocio.pe')")
            .execute(&app.db).await.unwrap();
        let (doc, kind, email): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT customer_doc, customer_doc_type, customer_email FROM checkouts WHERE id = 'YAYA-PRO-RUC'")
                .fetch_one(&app.db).await.unwrap();
        assert_eq!(nubefact::Doc::for_doc(doc.as_deref(), kind.as_deref()), nubefact::Doc::Factura);
        assert_eq!(billing_email(email.as_deref(), "51999@phone.yaya.tech").as_deref(), Some("conta@negocio.pe"));
    }

    async fn open(app: &App, id: &str, account: &str, plan: &str, months: i64, amount: i64, currency: &str) {
        sqlx::query("INSERT INTO checkouts (id, account, provider, plan, months, amount_minor, currency) VALUES ($1,$2,'dodo',$3,$4,$5,$6)")
            .bind(id).bind(account).bind(plan).bind(months).bind(amount).bind(currency).execute(&app.db).await.unwrap();
    }

    async fn balance(app: &App, account: &str) -> i64 {
        crate::credits::balance(app, account).await.unwrap()
    }

    /// A paid card checkout leaves the account exactly where a manual
    /// Yape confirmation would: plan on, half the price in credits, the
    /// open Yape request closed. Settling the same event twice is a no-op.
    #[tokio::test]
    async fn settle_activates_a_plan_once() {
        let app = crate::test_app().await;
        let subject = accounts::subject("a1");
        // The owner had also opened a Yape request for the same plan.
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months) VALUES ('YAYA-PRO-YAPE','acct:a1','pro',100.0,'PEN',1)").execute(&app.db).await.unwrap();
        // An English buyer: the checkout is in dollars, the credit grant lands in soles.
        open(&app, "YAYA-PRO-CARD", "a1", "pro", 1, plans::amount_minor_in("pro", 1, "USD"), "USD").await;

        assert!(settle(&app, "YAYA-PRO-CARD", "dodo", "dodo:pay_1", None, None).await.unwrap());
        let eff = plans::effective_for(&app, &subject).await.unwrap();
        assert_eq!(eff.plan, "pro");
        assert!(eff.expires_at.is_some());
        assert_eq!(balance(&app, "a1").await, 0); // D14: plans grant no credits
        let (st,): (String,) = sqlx::query_as("SELECT status FROM checkouts WHERE id = 'YAYA-PRO-CARD'").fetch_one(&app.db).await.unwrap();
        assert_eq!(st, "paid");
        let (yape,): (String,) = sqlx::query_as("SELECT status FROM plan_requests WHERE ref = 'YAYA-PRO-YAPE'").fetch_one(&app.db).await.unwrap();
        assert_eq!(yape, "cancelled");

        // Same event again: nothing moves.
        assert!(!settle(&app, "YAYA-PRO-CARD", "dodo", "dodo:pay_1", None, None).await.unwrap());
        assert_eq!(balance(&app, "a1").await, 0); // D14: plans grant no credits
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM credit_ledger WHERE account = 'a1'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0);

        // An unknown checkout is an error, and a short payment is refused.
        assert!(settle(&app, "YAYA-NOPE", "dodo", "dodo:pay_2", None, None).await.unwrap_err().to_string().starts_with("unknown checkout"));
        open(&app, "YAYA-MAX-CARD", "a1", "max", 1, plans::amount_minor("max", 1), "PEN").await;
        assert!(settle(&app, "YAYA-MAX-CARD", "izipay", "izi:1", Some(plans::amount_minor("max", 1) - 1), None).await.is_err());
    }

    /// A card recarga is a topup plus the launch bonus, and closes the
    /// matching open Yape recarga.
    #[tokio::test]
    async fn settle_credits_a_recarga_with_the_launch_bonus() {
        let app = crate::test_app().await;
        std::env::set_var("LAUNCH_BONUS_PERCENT", "50");
        std::env::set_var("LAUNCH_BONUS_UNTIL", "2099-01-01T00:00:00Z");
        sqlx::query("INSERT INTO plan_requests (ref, agent, plan, amount, currency, months, amount_minor) VALUES ('YAYA-RECARGA-AB12','acct:a2','credits',20.0,'PEN',2000,2000)").execute(&app.db).await.unwrap();
        open(&app, "YAYA-RECARGA-CARD", "a2", "credits", 2000, 2000, "USD").await;
        assert!(settle(&app, "YAYA-RECARGA-CARD", "dodo", "dodo:pay_9", None, None).await.unwrap());
        std::env::remove_var("LAUNCH_BONUS_PERCENT");
        std::env::remove_var("LAUNCH_BONUS_UNTIL");
        assert_eq!(balance(&app, "a2").await, 3000);
        let kinds: Vec<(String, i64)> = sqlx::query_as("SELECT kind, delta FROM credit_ledger WHERE account = 'a2' ORDER BY delta DESC").fetch_all(&app.db).await.unwrap();
        assert_eq!(kinds, vec![("topup".to_string(), 2000), ("bonus".to_string(), 1000)]);
        let (yape,): (String,) = sqlx::query_as("SELECT status FROM plan_requests WHERE ref = 'YAYA-RECARGA-AB12'").fetch_one(&app.db).await.unwrap();
        assert_eq!(yape, "cancelled");
        // Plan untouched by a recarga.
        assert_eq!(plans::effective_for(&app, &accounts::subject("a2")).await.unwrap().plan, "free");
    }

    /// A recarga is one-time money: a second, differently-numbered event for
    /// the same checkout (a provider retry, or both rails reporting it) must
    /// not credit it again.
    #[tokio::test]
    async fn a_recarga_is_credited_once_whatever_the_event_ids() {
        let app = crate::test_app().await;
        open(&app, "YAYA-RECARGA-2EV", "a5", "credits", 2000, 2000, "PEN").await;
        assert!(settle(&app, "YAYA-RECARGA-2EV", "dodo", "dodo:pay_a", Some(2000), Some("PEN")).await.unwrap());
        assert!(!settle(&app, "YAYA-RECARGA-2EV", "dodo", "dodo:pay_b", Some(2000), Some("PEN")).await.unwrap());
        assert!(!settle(&app, "YAYA-RECARGA-2EV", "izipay", "izipay:tx", Some(2000), Some("PEN")).await.unwrap());
        let topups: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credit_ledger WHERE account = 'a5' AND kind = 'topup'").fetch_one(&app.db).await.unwrap();
        assert_eq!(topups, 1);
    }

    /// The money that arrived must be the money asked for, in its currency.
    #[tokio::test]
    async fn short_or_foreign_payments_settle_nothing() {
        let app = crate::test_app().await;
        open(&app, "YAYA-PRO-PEN", "a6", "pro", 1, plans::amount_minor("pro", 1), "PEN").await;
        let amount = plans::amount_minor("pro", 1);
        assert!(settle(&app, "YAYA-PRO-PEN", "dodo", "dodo:pay_usd", Some(amount), Some("USD")).await.is_err(), "same number, other currency");
        assert!(settle(&app, "YAYA-PRO-PEN", "dodo", "dodo:pay_low", Some(amount - 1), Some("PEN")).await.is_err());
        assert_eq!(plans::effective_for(&app, &accounts::subject("a6")).await.unwrap().plan, "free");
        assert!(settle(&app, "YAYA-PRO-PEN", "dodo", "dodo:pay_ok", Some(amount), Some("pen")).await.unwrap());
    }

    /// Dodo reports what was paid; the webhook checks it like the IPN does.
    #[tokio::test]
    async fn dodo_webhook_checks_the_amount_it_reports() {
        let mut app = crate::test_app().await;
        app.billing.dodo = Some(dodo::Dodo::for_test("whsec_dGVzdHNlY3JldA=="));
        let app = Arc::new(app);
        let amount = plans::amount_minor_in("pro", 1, "USD");
        open(&app, "YAYA-PRO-AMT", "a7", "pro", 1, amount, "USD").await;
        let d = app.billing.dodo.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let low = serde_json::to_vec(&json!({"type": "payment.succeeded", "data": {"payment_id": "pay_low", "total_amount": amount - 100, "currency": "USD", "metadata": {"checkout": "YAYA-PRO-AMT"}}})).unwrap();
        assert!(dodo_webhook(State(app.clone()), signed_headers(d, "m1", now, &low), Bytes::from(low)).await.is_err());
        assert_eq!(plans::effective_for(&app, &accounts::subject("a7")).await.unwrap().plan, "free");
        let ok = serde_json::to_vec(&json!({"type": "payment.succeeded", "data": {"payment_id": "pay_ok", "total_amount": amount, "currency": "USD", "metadata": {"checkout": "YAYA-PRO-AMT"}}})).unwrap();
        assert_eq!(dodo_webhook(State(app.clone()), signed_headers(d, "m2", now, &ok), Bytes::from(ok)).await.unwrap().status(), StatusCode::OK);
        assert_eq!(plans::effective_for(&app, &accounts::subject("a7")).await.unwrap().plan, "pro");
    }

    fn signed_headers(d: &dodo::Dodo, id: &str, ts: i64, body: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("webhook-id", id.parse().unwrap());
        h.insert("webhook-timestamp", ts.to_string().parse().unwrap());
        h.insert("webhook-signature", d.sign_for_test(id, &ts.to_string(), body).parse().unwrap());
        h
    }

    /// The webhook: bad or missing signature → 401, stale timestamp → 401,
    /// unknown checkout → 404, good event → settled, replay → not settled
    /// again, unrelated event → acknowledged and ignored.
    #[tokio::test]
    async fn dodo_webhook_verifies_then_settles() {
        let mut app = crate::test_app().await;
        app.billing.dodo = Some(dodo::Dodo::for_test("whsec_dGVzdHNlY3JldA=="));
        let app = Arc::new(app);
        open(&app, "YAYA-PRO-WH", "a3", "pro", 1, plans::amount_minor("pro", 1), "USD").await;
        let d = app.billing.dodo.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let body = serde_json::to_vec(&json!({"type": "payment.succeeded", "data": {"payment_id": "pay_wh1", "metadata": {"checkout": "YAYA-PRO-WH"}}})).unwrap();

        // No headers at all.
        let r = dodo_webhook(State(app.clone()), HeaderMap::new(), Bytes::from(body.clone())).await.unwrap_err();
        assert_eq!(r.0, StatusCode::UNAUTHORIZED);
        // Wrong signature.
        let mut bad = signed_headers(d, "msg_1", now, &body);
        bad.insert("webhook-signature", "v1,AAAA".parse().unwrap());
        assert_eq!(dodo_webhook(State(app.clone()), bad, Bytes::from(body.clone())).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
        // Stale timestamp, correctly signed.
        let stale = signed_headers(d, "msg_1", now - 3600, &body);
        assert_eq!(dodo_webhook(State(app.clone()), stale, Bytes::from(body.clone())).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
        // Garbage body with a good signature.
        let junk = b"not json".to_vec();
        assert_eq!(dodo_webhook(State(app.clone()), signed_headers(d, "msg_2", now, &junk), Bytes::from(junk)).await.unwrap_err().0, StatusCode::BAD_REQUEST);
        // Unknown checkout.
        let nope = serde_json::to_vec(&json!({"type": "payment.succeeded", "data": {"payment_id": "pay_x", "metadata": {"checkout": "YAYA-NOPE"}}})).unwrap();
        assert_eq!(dodo_webhook(State(app.clone()), signed_headers(d, "msg_3", now, &nope), Bytes::from(nope)).await.unwrap_err().0, StatusCode::NOT_FOUND);

        // The real thing.
        let ok = dodo_webhook(State(app.clone()), signed_headers(d, "msg_4", now, &body), Bytes::from(body.clone())).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(plans::effective_for(&app, &accounts::subject("a3")).await.unwrap().plan, "pro");
        let before = balance(&app, "a3").await;
        // Dodo retries the same event: acknowledged, nothing granted twice.
        let again = dodo_webhook(State(app.clone()), signed_headers(d, "msg_5", now, &body), Bytes::from(body.clone())).await.unwrap();
        assert_eq!(again.status(), StatusCode::OK);
        assert_eq!(balance(&app, "a3").await, before);
        // An event we do not act on.
        let other = serde_json::to_vec(&json!({"type": "payment.failed", "data": {"payment_id": "pay_f", "metadata": {"checkout": "YAYA-PRO-WH"}}})).unwrap();
        assert_eq!(dodo_webhook(State(app.clone()), signed_headers(d, "msg_6", now, &other), Bytes::from(other)).await.unwrap().status(), StatusCode::OK);
    }

    #[test]
    fn methods_follow_the_keys() {
        let none = Billing { dodo: None, izipay: None, nubefact: None };
        assert!(!none.card_available("PE"));
        assert!(!none.card_available("US"));
        assert_eq!(none.default_method("US"), None);
        assert_eq!(none.methods("US").as_array().unwrap().len(), 0);
        assert_eq!(none.methods("PE").as_array().unwrap().len(), 1); // Yape/Plin, always offered in Perú
        let dodo = Billing { dodo: Some(dodo::Dodo::for_test("s")), izipay: None, nubefact: None };
        assert!(dodo.card_available("US"));
        assert_eq!(dodo.default_method("US"), Some("dodo"));
        assert_eq!(dodo.default_method("PE"), Some("dodo"));
    }

    mod checkout_tests {
        use super::*;
        use crate::testkit::{self, account_with_agent, as_session, call, session_for, Keypair, Mock};

        async fn shop() -> (Mock, Shared, String) {
            let m = Mock::start().await;
            m.on("/Charge/CreatePayment", json!({"status": "SUCCESS", "answer": {"formToken": "ft"}}));
            m.on("/checkouts", json!({"checkout_url": "https://pay.dodo/x"}));
            let base = m.base.clone();
            let app = testkit::app_with(move |a| crate::App { billing: Billing {
                dodo: Some(dodo::Dodo::at_for_test(&base, "whsec_c2VjcmV0")), izipay: Some(izipay::Izipay::at_for_test(&base, "pw")), nubefact: None }, ..a }).await;
            account_with_agent(&app, "a", "51900000001", &Keypair::generate()).await;
            let s = session_for(&app, "a").await;
            (m, app, s)
        }

        async fn buy(app: &Shared, s: &str, body: Value, lang: &str) -> (u16, Value) {
            let bytes = serde_json::to_vec(&body).unwrap();
            call(app, "POST", "/v1/account/billing/checkout", &[("authorization", format!("Bearer {s}")), ("content-type", "application/json".into()), ("accept-language", lang.into())], Some(bytes)).await
        }

        async fn stored(app: &Shared, id: &str) -> (i64, String) {
            sqlx::query_as("SELECT amount_minor, currency FROM checkouts WHERE id = $1").bind(id).fetch_one(&app.db).await.unwrap()
        }

        #[tokio::test]
        async fn a_card_in_soles_always_charges_the_soles_price() {
            let (m, app, s) = shop().await;
            // An English browser asking to pay Pro through Izipay (soles).
            let (st, v) = buy(&app, &s, json!({"plan": "pro", "method": "izipay"}), "en-US").await;
            assert_eq!(st, 200, "{v}");
            let sent = m.seen_path("/Charge/CreatePayment").pop().unwrap().body;
            assert_eq!((sent["amount"].clone(), sent["currency"].clone()), (json!(10_000), json!("PEN")), "S/ 100, not 29 soles");
            assert_eq!(stored(&app, v["id"].as_str().unwrap()).await, (10_000, "PEN".to_string()));
        }

        #[tokio::test]
        async fn a_dodo_card_for_a_spanish_buyer_is_recorded_in_dollars_and_activates() {
            let (_m, app, s) = shop().await;
            let (st, v) = buy(&app, &s, json!({"plan": "pro", "method": "dodo"}), "es-PE").await;
            assert_eq!(st, 200, "{v}");
            let id = v["id"].as_str().unwrap().to_string();
            assert_eq!(stored(&app, &id).await, (2_900, "USD".to_string()), "Dodo charges the USD price");
            assert!(settle(&app, &id, "dodo", "dodo:p1", Some(2_900), Some("USD")).await.unwrap(), "the real Dodo payment activates the plan");
            assert_eq!(crate::plans::effective_for(&app, "acct:a").await.unwrap().plan, "pro");
        }

        #[tokio::test]
        async fn a_dodo_recarga_is_credited_in_soles() {
            let (m, app, s) = shop().await;
            let (st, v) = buy(&app, &s, json!({"amountMinor": 2_000, "method": "dodo"}), "es-PE").await;
            assert_eq!(st, 200, "{v}");
            assert_eq!(m.seen_path("/checkouts").pop().unwrap().body["product_cart"][0]["quantity"], 20);
            let id = v["id"].as_str().unwrap().to_string();
            // Dodo reports its own USD total for 20 one-sol units.
            assert!(settle(&app, &id, "dodo", "dodo:p2", Some(540), Some("USD")).await.unwrap());
            assert_eq!(crate::credits::balance(&app, "a").await.unwrap(), 2_000);
        }

        #[tokio::test]
        async fn checkout_validates_what_is_bought_and_who_buys() {
            let (_m, app, s) = shop().await;
            for bad in [json!({"plan": "free"}), json!({}), json!({"amountMinor": 1234}), json!({"plan": "pro", "method": "cash"}), json!({"plan": "pro", "ruc": "123"})] {
                assert_eq!(buy(&app, &s, bad.clone(), "es").await.0, 400, "{bad}");
            }
            assert_eq!(crate::testkit::anon(&app, "POST", "/v1/account/billing/checkout", Some(json!({"plan": "pro"}))).await.0, 401);
            let (_, v) = buy(&app, &s, json!({"plan": "max", "months": 99, "ruc": "20123456789"}), "es").await;
            assert_eq!((v["provider"].clone(), v["months"].clone()), (json!("izipay"), json!(12)), "Peru defaults to Izipay; months clamp: {v}");
            let id = v["id"].as_str().unwrap().to_string();
            let (_, st) = as_session(&app, &s, "GET", &format!("/v1/account/billing/checkout/{id}"), None).await;
            assert_eq!((st["status"].clone(), st["paidAt"].clone(), st["amount"].clone()), (json!("open"), Value::Null, json!(2_000.0)), "{st}");
            let other = { account_with_agent(&app, "b", "51900000002", &Keypair::generate()).await; session_for(&app, "b").await };
            assert_ne!(as_session(&app, &other, "GET", &format!("/v1/account/billing/checkout/{id}"), None).await.0, 200, "another account cannot see it");
        }
    }

    /// Izipay end to end through the router: the plan's first payment
    /// registers the card and schedules the subscription, installments
    /// renew it, the owner can cancel it, and a new plan replaces the old.
    mod izipay_flow_tests {
        use super::*;
        use crate::testkit::{self, account_with_agent, as_session, call, session_for, Keypair, Mock};

        const PW: &str = "prodpassword_test";

        async fn shop() -> (Mock, Shared, String) {
            let m = Mock::start().await;
            m.on("/Charge/CreatePayment", json!({"status": "SUCCESS", "answer": {"formToken": "ft"}}));
            m.on("/Subscription/Cancel", json!({"status": "SUCCESS", "answer": {"responseCode": 0}}));
            let base = m.base.clone();
            let app = testkit::app_with(move |a| crate::App { billing: Billing { dodo: None, izipay: Some(izipay::Izipay::at_for_test(&base, PW)), nubefact: None }, ..a }).await;
            account_with_agent(&app, "a", "51900000001", &Keypair::generate()).await;
            let s = session_for(&app, "a").await;
            (m, app, s)
        }

        async fn buy(app: &Shared, s: &str, body: Value) -> (u16, Value) {
            as_session(app, s, "POST", "/v1/account/billing/checkout", Some(body)).await
        }

        fn enc(s: &str) -> String {
            s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
        }

        /// POSTs `answer` to the IPN the way Izipay does: a signed form.
        async fn ipn(app: &Shared, answer: &Value) -> (u16, Value) {
            let a = answer.to_string();
            let h = izipay::Izipay::for_test(PW).hash_for_test(&a);
            let body = format!("kr-hash={}&kr-hash-algorithm=sha256_hmac&kr-hash-key=password&kr-answer-type=V4%2FPayment&kr-answer={}", h, enc(&a));
            call(app, "POST", "/v1/billing/webhook/izipay", &[("content-type", "application/x-www-form-urlencoded".into())], Some(body.into_bytes())).await
        }

        fn paid(order: &str, amount: i64, tx: Value) -> Value {
            json!({"orderStatus": "PAID", "orderCycle": "CLOSED", "serverDate": "2026-09-25T18:00:00+00:00",
                   "orderDetails": {"orderId": order, "orderTotalAmount": amount, "orderCurrency": "PEN", "mode": "PRODUCTION"},
                   "transactions": [tx]})
        }

        async fn sub_row(app: &Shared, checkout: &str) -> (String, Option<String>, Option<String>, Option<String>) {
            sqlx::query_as("SELECT status, external_id, next_charge_on, last_error FROM card_subscriptions WHERE checkout = $1").bind(checkout).fetch_one(&app.db).await.unwrap()
        }

        async fn plan_of(app: &Shared, account: &str) -> (String, Option<String>) {
            sqlx::query_as("SELECT plan, expires_at FROM plans WHERE agent = $1").bind(accounts::subject(account)).fetch_one(&app.db).await.unwrap()
        }

        /// Opens a Pro checkout and pays its first month with card `tok1`.
        async fn subscribed(m: &Mock, app: &Shared, s: &str) -> String {
            m.on("/Charge/CreateSubscription", json!({"status": "SUCCESS", "answer": {"subscriptionId": "sub_1"}}));
            let (st, v) = buy(app, s, json!({"plan": "pro", "email": "dueno@negocio.pe"})).await;
            assert_eq!(st, 200, "{v}");
            assert_eq!((v["answer"]["recurring"].clone(), v["answer"]["interval"].clone()), (json!(true), json!("month")));
            let id = v["id"].as_str().unwrap().to_string();
            let sent = m.seen_path("/Charge/CreatePayment").pop().unwrap().body;
            assert_eq!((sent["formAction"].clone(), sent["amount"].clone(), sent["customer"]["email"].clone()), (json!("REGISTER_PAY"), json!(10_000), json!("dueno@negocio.pe")));
            assert_eq!(ipn(app, &paid(&id, 10_000, json!({"uuid": "t1", "paymentMethodToken": "tok1"}))).await.0, 200);
            id
        }

        #[tokio::test]
        async fn a_plan_subscribes_the_card_its_first_month_was_paid_with() {
            let (m, app, s) = shop().await;
            let id = subscribed(&m, &app, &s).await;
            let calls = m.seen_path("/Charge/CreateSubscription");
            assert_eq!(calls.len(), 1);
            let b = &calls[0].body;
            let first = chrono::NaiveDate::parse_from_str(&b["effectDate"].as_str().unwrap()[..10], "%Y-%m-%d").unwrap();
            assert_eq!(b["effectDate"].as_str().unwrap().len(), 25);
            assert!(first > chrono::Utc::now().date_naive() + chrono::Duration::days(26), "the paid month is not charged again: {b}");
            assert!(b["rrule"].as_str().unwrap().starts_with("RRULE:FREQ=MONTHLY;BYMONTHDAY="), "{b}");
            assert_eq!((b["paymentMethodToken"].clone(), b["amount"].clone(), b["currency"].clone(), b["orderId"].clone()), (json!("tok1"), json!(10_000), json!("PEN"), json!(id)));
            let (st, ext, next, _) = sub_row(&app, &id).await;
            assert_eq!((st.as_str(), ext.as_deref(), next.clone()), ("active", Some("sub_1"), Some(first.to_string())));
            // The plan holds until three days past that charge.
            let (plan, exp) = plan_of(&app, "a").await;
            assert_eq!(plan, "pro");
            assert!(exp.unwrap().starts_with(&(first + chrono::Duration::days(3)).to_string()));
            // Izipay delivers the same IPN again: nothing is scheduled twice.
            assert_eq!(ipn(&app, &paid(&id, 10_000, json!({"uuid": "t1", "paymentMethodToken": "tok1"}))).await.0, 200);
            assert_eq!(m.seen_path("/Charge/CreateSubscription").len(), 1);
            // The web app shows it.
            let (_, me) = as_session(&app, &s, "GET", "/v1/account", None).await;
            assert_eq!(me["billing"]["subscriptions"][0]["status"], "active", "{me}");
            // Buying the same subscription again is refused before any charge.
            assert_eq!(buy(&app, &s, json!({"plan": "pro"})).await.0, 409);
        }

        #[tokio::test]
        async fn installments_renew_the_plan_and_a_refused_one_is_recorded() {
            let (m, app, s) = shop().await;
            let id = subscribed(&m, &app, &s).await;
            let (_, _, first, _) = sub_row(&app, &id).await;
            // An installment names its subscription; its orderId is not ours to rely on.
            let inst = |uuid: &str, status: &str| {
                let mut a = paid("", 10_000, json!({"uuid": uuid, "paymentMethodToken": "tok1", "transactionDetails": {"subscriptionDetails": {"subscriptionId": "sub_1"}}}));
                a["orderStatus"] = json!(status);
                a
            };
            // Its charge day has come: the installment due today is paid.
            let today = chrono::Utc::now().date_naive();
            sqlx::query("UPDATE card_subscriptions SET next_charge_on = $2, rrule = $3 WHERE checkout = $1")
                .bind(&id).bind(today.to_string()).bind(format!("RRULE:FREQ=MONTHLY;BYMONTHDAY={}", chrono::Datelike::day(&today).min(28))).execute(&app.db).await.unwrap();
            assert_eq!(ipn(&app, &inst("t2", "PAID")).await.0, 200);
            let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM billing_events WHERE checkout = $1").bind(&id).fetch_one(&app.db).await.unwrap();
            assert_eq!(n, 2, "the first month and one installment");
            let (st, _, next, err) = sub_row(&app, &id).await;
            assert_eq!(st, "active");
            let next = chrono::NaiveDate::parse_from_str(&next.unwrap(), "%Y-%m-%d").unwrap();
            assert!(next > today && next <= today + chrono::Duration::days(31), "the next charge moved a period on: {next} (was {first:?})");
            assert!(plan_of(&app, "a").await.1.unwrap() >= (next + chrono::Duration::days(3)).to_string());
            assert!(err.is_none());
            assert_eq!(plan_of(&app, "a").await.0, "pro");
            // A refused installment settles nothing and is kept for the owner/admin to see.
            assert_eq!(ipn(&app, &inst("t3", "UNPAID")).await.0, 200);
            assert!(sub_row(&app, &id).await.3.unwrap().contains("UNPAID"));
            let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM billing_events WHERE checkout = $1").bind(&id).fetch_one(&app.db).await.unwrap();
            assert_eq!(n, 2);
        }

        #[tokio::test]
        async fn a_subscription_izipay_refused_is_retried_by_the_next_ipn() {
            let (m, app, s) = shop().await;
            m.on("/Charge/CreateSubscription", json!({"status": "ERROR", "answer": {"errorCode": "INT_905", "errorMessage": "down"}}));
            m.on("/Charge/CreateSubscription", json!({"status": "SUCCESS", "answer": {"subscriptionId": "sub_9"}}));
            let (_, v) = buy(&app, &s, json!({"plan": "max", "email": "x@negocio.pe"})).await;
            let id = v["id"].as_str().unwrap().to_string();
            let answer = paid(&id, 20_000, json!({"uuid": "t1", "paymentMethodToken": "tok9"}));
            assert_eq!(ipn(&app, &answer).await.0, 500, "not fully applied: Izipay should retry");
            assert_eq!(plan_of(&app, "a").await.0, "max", "the paid month is on regardless");
            assert_eq!(sub_row(&app, &id).await.0, "error");
            assert_eq!(ipn(&app, &answer).await.0, 200);
            assert_eq!(sub_row(&app, &id).await.1.as_deref(), Some("sub_9"));
            let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM billing_events").fetch_one(&app.db).await.unwrap();
            assert_eq!(n, 1, "the payment itself counted once");
        }

        #[tokio::test]
        async fn a_recarga_is_one_charge_and_never_a_subscription() {
            let (m, app, s) = shop().await;
            let (st, v) = buy(&app, &s, json!({"amountMinor": 2_500, "email": "x@negocio.pe"})).await;
            assert_eq!(st, 200, "{v}");
            assert_eq!(v["answer"]["recurring"], false);
            assert_eq!(m.seen_path("/Charge/CreatePayment").pop().unwrap().body["formAction"], "PAYMENT");
            let id = v["id"].as_str().unwrap().to_string();
            assert_eq!(ipn(&app, &paid(&id, 2_500, json!({"uuid": "r1"}))).await.0, 200);
            assert_eq!(crate::credits::balance(&app, "a").await.unwrap(), 2_500);
            assert!(m.seen_path("/Charge/CreateSubscription").is_empty());
            let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM card_subscriptions").fetch_one(&app.db).await.unwrap();
            assert_eq!(n, 0);
        }

        #[tokio::test]
        async fn the_browser_return_settles_with_the_hmac_key_only_for_its_owner() {
            let (m, app, s) = shop().await;
            m.on("/Charge/CreateSubscription", json!({"status": "SUCCESS", "answer": {"subscriptionId": "sub_1"}}));
            let (_, v) = buy(&app, &s, json!({"plan": "pro", "email": "x@negocio.pe"})).await;
            let id = v["id"].as_str().unwrap().to_string();
            let a = paid(&id, 10_000, json!({"uuid": "t1", "paymentMethodToken": "tok1"})).to_string();
            let z = izipay::Izipay::for_test(PW);
            let ret = |hash: String| json!({"krAnswer": a, "krHash": hash});
            // Signed with the IPN password instead of the HMAC key: refused.
            assert_eq!(as_session(&app, &s, "POST", "/v1/account/billing/izipay/return", Some(ret(z.hash_for_test(&a)))).await.0, 401);
            // Someone else's session cannot settle this checkout.
            account_with_agent(&app, "b", "51900000002", &Keypair::generate()).await;
            let other = session_for(&app, "b").await;
            assert_eq!(as_session(&app, &other, "POST", "/v1/account/billing/izipay/return", Some(ret(z.browser_hash_for_test(&a)))).await.0, 404);
            let (st, v) = as_session(&app, &s, "POST", "/v1/account/billing/izipay/return", Some(ret(z.browser_hash_for_test(&a)))).await;
            assert_eq!((st, v["status"].clone()), (200, json!("paid")), "{v}");
            assert_eq!(sub_row(&app, &id).await.0, "active");
            // The IPN arriving after it changes nothing.
            assert_eq!(ipn(&app, &paid(&id, 10_000, json!({"uuid": "t1", "paymentMethodToken": "tok1"}))).await.0, 200);
            assert_eq!(m.seen_path("/Charge/CreateSubscription").len(), 1);
        }

        #[tokio::test]
        async fn cancelling_stops_the_charges_and_keeps_the_paid_period() {
            let (m, app, s) = shop().await;
            let id = subscribed(&m, &app, &s).await;
            let (_, _, next, _) = sub_row(&app, &id).await;
            let (st, v) = as_session(&app, &s, "POST", "/v1/account/billing/subscription/cancel", None).await;
            assert_eq!(st, 200, "{v}");
            let c = m.seen_path("/Subscription/Cancel").pop().unwrap().body;
            assert_eq!((c["paymentMethodToken"].clone(), c["subscriptionId"].clone()), (json!("tok1"), json!("sub_1")));
            assert_eq!(sub_row(&app, &id).await.0, "cancelled");
            let end = chrono::NaiveDate::parse_from_str(&next.unwrap(), "%Y-%m-%d").unwrap() + chrono::Duration::days(1);
            let (plan, exp) = plan_of(&app, "a").await;
            assert_eq!(plan, "pro");
            assert!(exp.unwrap().starts_with(&end.to_string()), "on until the day the next charge would have been");
            assert_eq!(as_session(&app, &s, "POST", "/v1/account/billing/subscription/cancel", None).await.0, 404);
            // Cancelled: buying Pro again is allowed.
            assert_eq!(buy(&app, &s, json!({"plan": "pro"})).await.0, 200);
        }

        #[tokio::test]
        async fn a_new_plan_replaces_the_subscription_it_upgrades() {
            let (m, app, s) = shop().await;
            let pro = subscribed(&m, &app, &s).await;
            m.on("/Charge/CreateSubscription", json!({"status": "SUCCESS", "answer": {"subscriptionId": "sub_2"}}));
            let (_, v) = buy(&app, &s, json!({"plan": "max", "email": "x@negocio.pe"})).await;
            let max = v["id"].as_str().unwrap().to_string();
            assert_eq!(ipn(&app, &paid(&max, 20_000, json!({"uuid": "t9", "paymentMethodToken": "tok2"}))).await.0, 200);
            assert_eq!(sub_row(&app, &max).await.0, "active");
            assert_eq!(sub_row(&app, &pro).await.0, "cancelled");
            assert_eq!(m.seen_path("/Subscription/Cancel").pop().unwrap().body["subscriptionId"], "sub_1");
            assert_eq!(plan_of(&app, "a").await.0, "max");
        }

        #[tokio::test]
        async fn a_test_mode_payment_buys_nothing_with_production_keys() {
            let (_m, app, s) = shop().await;
            let (_, v) = buy(&app, &s, json!({"plan": "pro"})).await;
            let id = v["id"].as_str().unwrap().to_string();
            let mut a = paid(&id, 10_000, json!({"uuid": "t1", "paymentMethodToken": "tok1"}));
            a["orderDetails"]["mode"] = json!("TEST");
            let (st, body) = ipn(&app, &a).await;
            assert_eq!(st, 200, "acknowledged so it is not retried: {body}");
            let none: Option<(String,)> = sqlx::query_as("SELECT plan FROM plans WHERE agent = 'acct:a'").fetch_optional(&app.db).await.unwrap();
            assert!(none.is_none());
        }
    }
}

