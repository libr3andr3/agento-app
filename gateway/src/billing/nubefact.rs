//! NubeFact — the comprobante electrónico for every Peruvian charge, sent to
//! SUNAT and to the customer by NubeFact itself. Prices are IGV-inclusive:
//! the plan price is the total; the base and the 18% are derived.
//!
//! D18: which document depends on what the buyer gave us. A RUC is a
//! business asking to deduct the expense, and only a **factura** does that;
//! a DNI (or nothing) gets a **boleta**. They are different SUNAT documents
//! with different series, so each has its own correlative counter.

use serde_json::{json, Value};

/// The two comprobantes we issue. `tipo_de_comprobante` is SUNAT's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Doc {
    Boleta,
    Factura,
}

impl Doc {
    /// A RUC (11 digits) means a factura; anything else is a boleta.
    pub fn for_doc(number: Option<&str>, kind: Option<&str>) -> Self {
        let says_ruc = kind.map(|k| k.eq_ignore_ascii_case("ruc")).unwrap_or(false);
        let looks_like_ruc = number.map(|n| n.len() == 11 && n.chars().all(|c| c.is_ascii_digit())).unwrap_or(false);
        if says_ruc && looks_like_ruc { Doc::Factura } else { Doc::Boleta }
    }

    pub fn tipo(self) -> i64 {
        match self { Doc::Boleta => 2, Doc::Factura => 1 }
    }

    /// SUNAT's document-type code for the customer: 6 = RUC, 1 = DNI.
    fn cliente_tipo(self, has_doc: bool) -> &'static str {
        match (self, has_doc) { (Doc::Factura, _) => "6", (Doc::Boleta, true) => "1", (Doc::Boleta, false) => "-" }
    }

    pub fn as_str(self) -> &'static str {
        match self { Doc::Boleta => "boleta", Doc::Factura => "factura" }
    }
}

pub struct Nubefact {
    http: reqwest::Client,
    url: String,
    token: String,
    serie: String,
    serie_factura: String,
}

pub struct Issued {
    pub accepted: bool,
    pub pdf_url: Option<String>,
    pub xml_url: Option<String>,
    pub raw: Value,
}

impl Nubefact {
    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Option<Self>> {
        let url = match std::env::var("NUBEFACT_URL") { Ok(u) if !u.trim().is_empty() => u.trim().to_string(), _ => return Ok(None) };
        let token = std::env::var("NUBEFACT_TOKEN").map_err(|_| anyhow::anyhow!("NUBEFACT_TOKEN must be set with NUBEFACT_URL"))?;
        Ok(Some(Self {
            http,
            url,
            token,
            serie: crate::env_or("NUBEFACT_SERIE", "B001"),
            serie_factura: crate::env_or("NUBEFACT_SERIE_FACTURA", "F001"),
        }))
    }

    /// The boleta serie. Kept for callers that only ever issue boletas.
    pub fn serie(&self) -> &str {
        &self.serie
    }

    /// Boletas and facturas are numbered in separate series, each with its
    /// own correlative — SUNAT will not accept them sharing one.
    pub fn serie_of(&self, doc: Doc) -> &str {
        match doc { Doc::Boleta => &self.serie, Doc::Factura => &self.serie_factura }
    }

    /// One-line boleta. Kept as the narrow door for callers with no RUC.
    pub async fn boleta(&self, numero: i64, total_minor: i64, description: &str, dni: Option<&str>, name: Option<&str>, email: Option<&str>) -> anyhow::Result<Issued> {
        self.emit(Doc::Boleta, numero, total_minor, description, dni, name, None, email).await
    }

    /// One-line comprobante for `total_minor` (IGV included). On a boleta the
    /// document number is optional — without one SUNAT takes "sin documento";
    /// on a factura the RUC is what makes it a factura, so it is required.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit(&self, doc: Doc, numero: i64, total_minor: i64, description: &str, dni: Option<&str>, name: Option<&str>, address: Option<&str>, email: Option<&str>) -> anyhow::Result<Issued> {
        if doc == Doc::Factura {
            anyhow::ensure!(dni.is_some_and(|d| d.len() == 11), "a factura needs an 11-digit RUC");
        }
        let total = total_minor as f64 / 100.0;
        let base = (total / 1.18 * 100.0).round() / 100.0;
        let igv = ((total - base) * 100.0).round() / 100.0;
        let body = json!({
            "operacion": "generar_comprobante",
            "tipo_de_comprobante": doc.tipo(),
            "serie": self.serie_of(doc),
            "numero": numero,
            "sunat_transaction": 1,
            "cliente_tipo_de_documento": doc.cliente_tipo(dni.is_some()),
            "cliente_numero_de_documento": dni.unwrap_or("-"),
            "cliente_denominacion": name.unwrap_or("CLIENTE"),
            "cliente_direccion": address.unwrap_or(""),
            "cliente_email": email.unwrap_or(""),
            "fecha_de_emision": chrono::Utc::now().with_timezone(&chrono_tz::America::Lima).format("%d-%m-%Y").to_string(),
            "moneda": 1,
            "porcentaje_de_igv": 18.00,
            "total_gravada": base,
            "total_igv": igv,
            "total": total,
            "enviar_automaticamente_a_la_sunat": true,
            "enviar_automaticamente_al_cliente": email.is_some(),
            "items": [{
                "unidad_de_medida": "ZZ",
                "codigo": "AGENTE",
                "descripcion": description,
                "cantidad": 1,
                "valor_unitario": base,
                "precio_unitario": total,
                "subtotal": base,
                "tipo_de_igv": 1,
                "igv": igv,
                "total": total,
            }],
        });
        let r = self.http.post(&self.url).header("Authorization", format!("Token token=\"{}\"", self.token)).json(&body).send().await?;
        let v: Value = r.json().await?;
        if !v["errors"].is_null() && v["errors"] != "" {
            anyhow::bail!("nubefact: {}", v["errors"]);
        }
        Ok(Issued {
            accepted: v["aceptada_por_sunat"].as_bool().unwrap_or(false),
            pdf_url: v["enlace_del_pdf"].as_str().map(String::from),
            xml_url: v["enlace_del_xml"].as_str().map(String::from),
            raw: v,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Doc;

    /// D18: a RUC is a business that needs to deduct the expense, and only a
    /// factura lets it. Everything else — a DNI, a blank, a RUC-shaped
    /// number the buyer did not label as one — is a boleta. Getting this
    /// backwards issues the wrong SUNAT document, so it is worth pinning.
    #[test]
    fn a_ruc_buys_a_factura_and_nothing_else_does() {
        assert_eq!(Doc::for_doc(Some("20512345678"), Some("RUC")), Doc::Factura);
        assert_eq!(Doc::for_doc(Some("20512345678"), Some("ruc")), Doc::Factura);
        assert_eq!(Doc::for_doc(Some("12345678"), Some("DNI")), Doc::Boleta);
        assert_eq!(Doc::for_doc(None, None), Doc::Boleta);
        // Labelled a RUC but the wrong length, or the right length with no
        // label: a boleta, which is the document that can go without one.
        assert_eq!(Doc::for_doc(Some("2051234567"), Some("RUC")), Doc::Boleta);
        assert_eq!(Doc::for_doc(Some("20512345678"), None), Doc::Boleta);
    }

    /// SUNAT's codes, in one place: 1 = factura, 2 = boleta; the customer's
    /// document is 6 for a RUC, 1 for a DNI, "-" for none.
    #[test]
    fn sunat_codes() {
        assert_eq!(Doc::Factura.tipo(), 1);
        assert_eq!(Doc::Boleta.tipo(), 2);
        assert_eq!(Doc::Factura.cliente_tipo(true), "6");
        assert_eq!(Doc::Boleta.cliente_tipo(true), "1");
        assert_eq!(Doc::Boleta.cliente_tipo(false), "-");
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use crate::testkit::Mock;

    fn at(m: &Mock) -> Nubefact {
        Nubefact { http: reqwest::Client::new(), url: format!("{}/api/v1/xyz", m.base), token: "tk".into(), serie: "B001".into(), serie_factura: "F001".into() }
    }

    #[test]
    fn a_ruc_makes_a_factura_anything_else_a_boleta() {
        assert_eq!(Doc::for_doc(Some("20123456789"), Some("RUC")), Doc::Factura);
        assert_eq!(Doc::for_doc(Some("20123456789"), Some("dni")), Doc::Boleta, "the buyer said DNI");
        assert_eq!(Doc::for_doc(Some("2012345678X"), Some("ruc")), Doc::Boleta);
        assert_eq!(Doc::for_doc(None, None), Doc::Boleta);
        assert_eq!((Doc::Factura.tipo(), Doc::Boleta.tipo(), Doc::Factura.as_str()), (1, 2, "factura"));
        assert_eq!((Doc::Boleta.cliente_tipo(true), Doc::Boleta.cliente_tipo(false), Doc::Factura.cliente_tipo(true)), ("1", "-", "6"));
    }

    #[tokio::test]
    async fn comprobantes_split_igv_out_of_the_price_and_use_their_own_series() {
        let m = Mock::start().await;
        m.on("/api/v1/xyz", json!({"errors": null, "aceptada_por_sunat": true, "enlace_del_pdf": "https://n/pdf", "enlace_del_xml": "https://n/xml"}));
        let n = at(&m);
        assert_eq!((n.serie(), n.serie_of(Doc::Factura)), ("B001", "F001"));
        let got = n.boleta(7, 10_000, "Plan Pro", Some("12345678"), Some("Ana"), Some("a@b.pe")).await.unwrap();
        assert!(got.accepted && got.pdf_url.is_some() && got.xml_url.is_some() && got.raw.is_object());
        let seen = m.seen_path("/api/v1/xyz").pop().unwrap();
        assert_eq!(seen.headers["authorization"], "Token token=\"tk\"");
        let b = seen.body;
        assert_eq!((b["serie"].clone(), b["numero"].clone(), b["tipo_de_comprobante"].clone()), (json!("B001"), json!(7), json!(2)));
        assert_eq!((b["total"].clone(), b["total_gravada"].clone(), b["total_igv"].clone()), (json!(100.0), json!(84.75), json!(15.25)));
        assert_eq!(b["enviar_automaticamente_al_cliente"], true);
        assert!(n.emit(Doc::Factura, 1, 10_000, "Plan", Some("1234"), None, None, None).await.is_err(), "a factura needs a RUC");
        n.emit(Doc::Factura, 1, 20_000, "Plan Max", Some("20123456789"), Some("ACME SAC"), Some("Av. Lima 1"), None).await.unwrap();
        let b = m.seen_path("/api/v1/xyz").pop().unwrap().body;
        assert_eq!((b["serie"].clone(), b["cliente_tipo_de_documento"].clone(), b["enviar_automaticamente_al_cliente"].clone()), (json!("F001"), json!("6"), json!(false)));
        m.on("/api/v1/xyz", json!({"errors": "serie inválida"}));
        assert!(n.boleta(8, 100, "x", None, None, None).await.is_err());
    }
}
