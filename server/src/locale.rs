//! Country → locale environment. The owner picks a country once, at
//! registration; everything that used to be hardwired to Perú (language,
//! currency, timezone, the names of the local instant-payment rails, what
//! "a district" means) is derived from that pick here and nowhere else.
//!
//! Plugins never touch this table directly: `learning::compose` mounts the
//! profile into the business's `values` (`country`, `language`, `currency`,
//! `currencySymbol`, `paymentRails`, `timezone` default) so the owner can
//! override any of it by talking to the agent, like any other field.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountryProfile {
    /// ISO 3166-1 alpha-2.
    pub iso: &'static str,
    pub name: &'static str,
    /// BCP-47 primary language the agent speaks by default.
    pub language: &'static str,
    pub currency: &'static str,
    pub symbol: &'static str,
    /// Default IANA zone (capital / main market). Owners elsewhere in a wide
    /// country set `timezone` in the interview.
    pub timezone: &'static str,
    /// Local instant-payment rails customers actually use, for the prompts
    /// ("pay via …"). Free text, comma separated.
    pub rails: &'static str,
    /// What the delivery-zone unit is called locally.
    pub zone_word: &'static str,
}

macro_rules! c {
    ($iso:expr, $name:expr, $lang:expr, $cur:expr, $sym:expr, $tz:expr, $rails:expr, $zone:expr) => {
        CountryProfile { iso: $iso, name: $name, language: $lang, currency: $cur, symbol: $sym,
                         timezone: $tz, rails: $rails, zone_word: $zone }
    };
}

/// Every country the APK's registration picker offers. Keep in sync with
/// android Countries.kt — an ISO missing here degrades to [`GENERIC`].
pub static PROFILES: &[CountryProfile] = &[
    // Latin America & Caribbean
    c!("PE", "Perú", "es", "PEN", "S/", "America/Lima", "Yape, Plin", "distrito"),
    c!("AR", "Argentina", "es", "ARS", "$", "America/Argentina/Buenos_Aires", "Mercado Pago, transferencia (CBU/alias)", "barrio"),
    c!("BZ", "Belize", "en", "BZD", "BZ$", "America/Belize", "bank transfer", "area"),
    c!("BO", "Bolivia", "es", "BOB", "Bs", "America/La_Paz", "QR Simple, Tigo Money", "zona"),
    c!("BR", "Brasil", "pt", "BRL", "R$", "America/Sao_Paulo", "Pix", "bairro"),
    c!("CL", "Chile", "es", "CLP", "$", "America/Santiago", "transferencia bancaria, MACH, Tenpo, Mercado Pago", "comuna"),
    c!("CO", "Colombia", "es", "COP", "$", "America/Bogota", "Nequi, Daviplata, Bre-B (llave), Bancolombia", "barrio"),
    c!("CR", "Costa Rica", "es", "CRC", "₡", "America/Costa_Rica", "SINPE Móvil", "cantón"),
    c!("CU", "Cuba", "es", "CUP", "$", "America/Havana", "Transfermóvil, EnZona", "municipio"),
    c!("EC", "Ecuador", "es", "USD", "$", "America/Guayaquil", "transferencia, Deuna, De Una", "sector"),
    c!("SV", "El Salvador", "es", "USD", "$", "America/El_Salvador", "transferencia, Chivo", "colonia"),
    c!("GT", "Guatemala", "es", "GTQ", "Q", "America/Guatemala", "transferencia, Tigo Money", "zona"),
    c!("GY", "Guyana", "en", "GYD", "G$", "America/Guyana", "MMG, bank transfer", "area"),
    c!("HT", "Haïti", "fr", "HTG", "G", "America/Port-au-Prince", "MonCash, NatCash", "quartier"),
    c!("HN", "Honduras", "es", "HNL", "L", "America/Tegucigalpa", "Tigo Money, transferencia", "colonia"),
    c!("JM", "Jamaica", "en", "JMD", "J$", "America/Jamaica", "Lynk, bank transfer", "area"),
    c!("MX", "México", "es", "MXN", "$", "America/Mexico_City", "transferencia SPEI, CoDi, Mercado Pago", "colonia"),
    c!("NI", "Nicaragua", "es", "NIO", "C$", "America/Managua", "transferencia, Billetera Móvil", "barrio"),
    c!("PA", "Panamá", "es", "USD", "$", "America/Panama", "Yappy (Banco General), Nequi", "corregimiento"),
    c!("PY", "Paraguay", "es", "PYG", "₲", "America/Asuncion", "transferencia SIPAP, Tigo Money, Zimple", "barrio"),
    c!("PR", "Puerto Rico", "es", "USD", "$", "America/Puerto_Rico", "ATH Móvil", "barrio"),
    c!("DO", "República Dominicana", "es", "DOP", "RD$", "America/Santo_Domingo", "transferencia, tPago", "sector"),
    c!("SR", "Suriname", "nl", "SRD", "Sr$", "America/Paramaribo", "Mopé, bank transfer", "area"),
    c!("TT", "Trinidad and Tobago", "en", "TTD", "TT$", "America/Port_of_Spain", "bank transfer", "area"),
    c!("UY", "Uruguay", "es", "UYU", "$", "America/Montevideo", "transferencia, Prex, Mercado Pago", "barrio"),
    c!("VE", "Venezuela", "es", "VES", "Bs", "America/Caracas", "Pago Móvil, Zelle", "parroquia"),
    // North America / Europe
    c!("US", "United States", "en", "USD", "$", "America/New_York", "Zelle, Venmo, Cash App", "neighborhood"),
    c!("CA", "Canada", "en", "CAD", "$", "America/Toronto", "Interac e-Transfer", "neighbourhood"),
    c!("ES", "España", "es", "EUR", "€", "Europe/Madrid", "Bizum, transferencia", "barrio"),
    c!("PT", "Portugal", "pt", "EUR", "€", "Europe/Lisbon", "MB WAY, transferência", "freguesia"),
    c!("GB", "United Kingdom", "en", "GBP", "£", "Europe/London", "bank transfer (Faster Payments)", "area"),
    c!("FR", "France", "fr", "EUR", "€", "Europe/Paris", "virement, Lydia, Paylib", "quartier"),
    c!("DE", "Deutschland", "de", "EUR", "€", "Europe/Berlin", "Überweisung, PayPal", "Stadtteil"),
    c!("IT", "Italia", "it", "EUR", "€", "Europe/Rome", "bonifico, Satispay, PayPal", "quartiere"),
    c!("NL", "Nederland", "nl", "EUR", "€", "Europe/Amsterdam", "Tikkie, iDEAL", "wijk"),
    c!("BE", "België", "nl", "EUR", "€", "Europe/Brussels", "Payconiq, overschrijving", "wijk"),
    c!("CH", "Schweiz", "de", "CHF", "CHF", "Europe/Zurich", "TWINT", "Quartier"),
    c!("AT", "Österreich", "de", "EUR", "€", "Europe/Vienna", "Überweisung, PayPal", "Bezirk"),
    c!("IE", "Ireland", "en", "EUR", "€", "Europe/Dublin", "Revolut, bank transfer", "area"),
    c!("SE", "Sverige", "sv", "SEK", "kr", "Europe/Stockholm", "Swish", "område"),
    c!("NO", "Norge", "no", "NOK", "kr", "Europe/Oslo", "Vipps", "område"),
    c!("DK", "Danmark", "da", "DKK", "kr", "Europe/Copenhagen", "MobilePay", "område"),
    c!("FI", "Suomi", "fi", "EUR", "€", "Europe/Helsinki", "MobilePay, tilisiirto", "alue"),
    c!("PL", "Polska", "pl", "PLN", "zł", "Europe/Warsaw", "BLIK, przelew", "dzielnica"),
    c!("CZ", "Česko", "cs", "CZK", "Kč", "Europe/Prague", "převod, QR platba", "čtvrť"),
    c!("HU", "Magyarország", "hu", "HUF", "Ft", "Europe/Budapest", "átutalás, Revolut", "kerület"),
    c!("RO", "România", "ro", "RON", "lei", "Europe/Bucharest", "transfer bancar, Revolut", "cartier"),
    c!("GR", "Ελλάδα", "el", "EUR", "€", "Europe/Athens", "IRIS, τραπεζική μεταφορά", "περιοχή"),
    c!("UA", "Україна", "uk", "UAH", "₴", "Europe/Kyiv", "переказ на картку, Monobank", "район"),
    c!("TR", "Türkiye", "tr", "TRY", "₺", "Europe/Istanbul", "havale/EFT, FAST", "mahalle"),
    // Middle East / Africa
    c!("IL", "ישראל", "he", "ILS", "₪", "Asia/Jerusalem", "Bit, PayBox", "שכונה"),
    c!("AE", "UAE", "en", "AED", "AED", "Asia/Dubai", "bank transfer, Aani", "area"),
    c!("SA", "السعودية", "ar", "SAR", "SAR", "Asia/Riyadh", "STC Pay, تحويل بنكي", "حي"),
    c!("QA", "قطر", "ar", "QAR", "QAR", "Asia/Qatar", "bank transfer, Fawran", "منطقة"),
    c!("EG", "مصر", "ar", "EGP", "E£", "Africa/Cairo", "InstaPay, Vodafone Cash", "حي"),
    c!("MA", "Maroc", "fr", "MAD", "DH", "Africa/Casablanca", "virement, CashPlus", "quartier"),
    c!("ZA", "South Africa", "en", "ZAR", "R", "Africa/Johannesburg", "EFT, SnapScan, PayShap", "suburb"),
    c!("NG", "Nigeria", "en", "NGN", "₦", "Africa/Lagos", "bank transfer, OPay, PalmPay", "area"),
    c!("KE", "Kenya", "en", "KES", "KSh", "Africa/Nairobi", "M-Pesa", "estate"),
    c!("GH", "Ghana", "en", "GHS", "GH₵", "Africa/Accra", "MTN MoMo, Telecel Cash", "area"),
    // Asia / Pacific
    c!("IN", "India", "hi", "INR", "₹", "Asia/Kolkata", "UPI (PhonePe, Google Pay, Paytm)", "locality"),
    c!("CN", "中国", "zh", "CNY", "¥", "Asia/Shanghai", "微信支付, 支付宝", "区"),
    c!("JP", "日本", "ja", "JPY", "¥", "Asia/Tokyo", "PayPay, 銀行振込", "区"),
    c!("KR", "대한민국", "ko", "KRW", "₩", "Asia/Seoul", "카카오페이, 토스, 계좌이체", "동"),
    c!("HK", "Hong Kong", "zh", "HKD", "HK$", "Asia/Hong_Kong", "FPS, PayMe, AlipayHK", "district"),
    c!("TW", "台灣", "zh", "TWD", "NT$", "Asia/Taipei", "LINE Pay, 轉帳", "區"),
    c!("SG", "Singapore", "en", "SGD", "S$", "Asia/Singapore", "PayNow, PayLah!", "area"),
    c!("MY", "Malaysia", "ms", "MYR", "RM", "Asia/Kuala_Lumpur", "DuitNow, Touch 'n Go", "kawasan"),
    c!("ID", "Indonesia", "id", "IDR", "Rp", "Asia/Jakarta", "QRIS, GoPay, OVO, transfer bank", "kecamatan"),
    c!("PH", "Philippines", "en", "PHP", "₱", "Asia/Manila", "GCash, Maya", "barangay"),
    c!("TH", "ไทย", "th", "THB", "฿", "Asia/Bangkok", "PromptPay", "เขต"),
    c!("VN", "Việt Nam", "vi", "VND", "₫", "Asia/Ho_Chi_Minh", "chuyển khoản, MoMo, ZaloPay", "quận"),
    c!("PK", "Pakistan", "ur", "PKR", "Rs", "Asia/Karachi", "JazzCash, Easypaisa, Raast", "area"),
    c!("BD", "Bangladesh", "bn", "BDT", "৳", "Asia/Dhaka", "bKash, Nagad", "area"),
    c!("AU", "Australia", "en", "AUD", "$", "Australia/Sydney", "PayID, bank transfer", "suburb"),
    c!("NZ", "New Zealand", "en", "NZD", "$", "Pacific/Auckland", "bank transfer", "suburb"),
];

/// Unknown or missing ISO: English, no currency assumption, UTC. The owner
/// sets what matters in the interview; nothing breaks, nothing lies.
pub static GENERIC: CountryProfile =
    c!("", "unknown", "en", "", "", "UTC", "bank transfer", "area");

pub fn profile(iso: &str) -> &'static CountryProfile {
    let up = iso.trim().to_ascii_uppercase();
    PROFILES.iter().find(|p| p.iso == up).unwrap_or(&GENERIC)
}

impl CountryProfile {
    /// The locale fields mounted under the business's values. Layered
    /// BETWEEN core defaults and the owner's patch: the owner's word wins.
    pub fn defaults(&self) -> Value {
        json!({
            "country": self.iso,
            "language": self.language,
            "currency": self.currency,
            "currencySymbol": self.symbol,
            "paymentRails": self.rails,
            "timezone": self.timezone,
        })
    }
}

/// Locale as the plugins see it: read back from the composed values (so
/// owner overrides apply), never from the static table.
#[derive(Debug, Clone)]
pub struct Locale {
    pub country: String,
    pub language: String,
    pub currency: String,
    pub symbol: String,
    pub rails: String,
    pub zone_word: String,
}

impl Locale {
    pub fn from_values(values: &Value) -> Locale {
        let iso = values["country"].as_str().unwrap_or("");
        let p = profile(iso);
        let s = |k: &str, d: &str| values[k].as_str().filter(|s| !s.is_empty()).unwrap_or(d).to_string();
        Locale {
            country: s("country", p.iso),
            language: s("language", p.language),
            currency: s("currency", p.currency),
            symbol: s("currencySymbol", p.symbol),
            rails: s("paymentRails", p.rails),
            zone_word: p.zone_word.to_string(),
        }
    }

    pub fn language_name(&self) -> &'static str {
        language_name(&self.language)
    }

    /// "S/ 50" / "₹ 500" / "50 EUR" — whatever the business counts in.
    pub fn money(&self, amount: f64) -> String {
        let n = if amount == amount.trunc() { format!("{}", amount as i64) } else { format!("{amount:.2}") };
        if !self.symbol.is_empty() {
            format!("{} {}", self.symbol, n)
        } else if !self.currency.is_empty() {
            format!("{n} {}", self.currency)
        } else {
            n
        }
    }

    /// "the local currency" for prompts: "soles (PEN)", "rupees (INR)"…
    pub fn currency_phrase(&self) -> String {
        if self.currency.is_empty() {
            "the business's local currency (ask the owner which)".into()
        } else {
            format!("{} ({})", self.currency, self.symbol)
        }
    }

    /// The LOCALE section every agent prompt carries. Written in English
    /// (the instruction language the model follows best); the directive
    /// inside is what makes the agent SPEAK the country's language.
    pub fn prompt_section(&self) -> String {
        let country = profile(&self.country);
        let country_name = if country.iso.is_empty() { "an unspecified country".to_string() } else { country.name.to_string() };
        format!(
            "LOCALE: this business operates in {country_name}. Speak {lang} by default; if the \
             person writes in another language, mirror theirs. ALL money is in {cur}: write \
             amounts as '{example}' and never mention any other currency. Customers pay by \
             local instant transfer — here that means {rails}; call those by name instead of \
             any other country's apps. Delivery areas are local {zone}s. Dates, greetings and \
             tone follow local custom.",
            lang = self.language_name(),
            cur = self.currency_phrase(),
            example = self.money(50.0),
            rails = self.rails,
            zone = self.zone_word,
        )
    }
}

pub fn language_name(code: &str) -> &'static str {
    match code {
        "es" => "Spanish", "en" => "English", "pt" => "Portuguese", "fr" => "French",
        "de" => "German", "it" => "Italian", "nl" => "Dutch", "sv" => "Swedish",
        "no" => "Norwegian", "da" => "Danish", "fi" => "Finnish", "pl" => "Polish",
        "cs" => "Czech", "hu" => "Hungarian", "ro" => "Romanian", "el" => "Greek",
        "uk" => "Ukrainian", "tr" => "Turkish", "he" => "Hebrew", "ar" => "Arabic",
        "hi" => "Hindi (Hinglish is fine if the person writes that way)", "zh" => "Chinese",
        "ja" => "Japanese", "ko" => "Korean", "ms" => "Malay", "id" => "Indonesian",
        "th" => "Thai", "vi" => "Vietnamese", "ur" => "Urdu", "bn" => "Bengali",
        _ => "English",
    }
}

/// User-facing canned strings the server emits WITHOUT the model (tool-loop
/// bail-out, reminder mail). Falls back to English for languages not yet
/// translated — add a row, nothing else to wire.
pub fn t(lang: &str, key: &str) -> &'static str {
    match (lang, key) {
        ("es", "fallback") => "Disculpa, no pude completar eso ahorita — el dueño te responde en un momento. 🙏",
        ("pt", "fallback") => "Desculpa, não consegui terminar isso agora — o dono te responde em instantes. 🙏",
        ("fr", "fallback") => "Désolé, je n'ai pas pu terminer — le propriétaire vous répond dans un instant. 🙏",
        ("de", "fallback") => "Entschuldigung, das hat gerade nicht geklappt — der Inhaber meldet sich gleich. 🙏",
        ("it", "fallback") => "Scusa, non sono riuscito a completare — il titolare ti risponde a breve. 🙏",
        ("hi", "fallback") => "माफ़ कीजिए, अभी यह पूरा नहीं हो पाया — मालिक आपसे जल्द संपर्क करेंगे। 🙏",
        ("id", "fallback") => "Maaf, belum bisa menyelesaikan itu sekarang — pemilik akan segera membalas. 🙏",
        ("ar", "fallback") => "عذراً، لم أتمكن من إكمال ذلك الآن — سيرد عليك صاحب المحل قريباً. 🙏",
        ("zh", "fallback") => "抱歉，这个暂时没能完成——店主稍后会回复您。🙏",
        (_, "fallback") => "Sorry, I couldn't finish that just now — the owner will get back to you shortly. 🙏",

        // First-contact AI disclosure (Peru Ley 31814 / DS 115-2025-PCM and
        // similar transparency rules): the disclosure plugin instructs the
        // agent to open its first-ever reply to a customer with this line.
        // `{business}` is substituted by the caller.
        ("es", "ai_disclosure") => "Hola, soy el asistente con IA de {business}. 🤖",
        ("pt", "ai_disclosure") => "Olá, sou o assistente de IA de {business}. 🤖",
        ("fr", "ai_disclosure") => "Bonjour, je suis l'assistant IA de {business}. 🤖",
        ("de", "ai_disclosure") => "Hallo, ich bin der KI-Assistent von {business}. 🤖",
        ("it", "ai_disclosure") => "Ciao, sono l'assistente IA di {business}. 🤖",
        ("hi", "ai_disclosure") => "नमस्ते, मैं {business} का AI असिस्टेंट हूँ। 🤖",
        ("id", "ai_disclosure") => "Halo, saya asisten AI dari {business}. 🤖",
        ("ar", "ai_disclosure") => "مرحباً، أنا مساعد الذكاء الاصطناعي لـ {business}. 🤖",
        ("zh", "ai_disclosure") => "您好，我是{business}的AI助手。🤖",
        (_, "ai_disclosure") => "Hi, I'm {business}'s AI assistant. 🤖",

        ("es", "limit_wait") => "¡Hola! Hoy estamos recibiendo muchos mensajes. Te respondemos personalmente en un momento. 🙏",
        ("pt", "limit_wait") => "Olá! Hoje estamos recebendo muitas mensagens. Respondemos pessoalmente em instantes. 🙏",
        ("fr", "limit_wait") => "Bonjour ! Nous recevons beaucoup de messages aujourd'hui. Nous vous répondons personnellement très vite. 🙏",
        ("de", "limit_wait") => "Hallo! Heute erreichen uns sehr viele Nachrichten. Wir melden uns gleich persönlich. 🙏",
        ("hi", "limit_wait") => "नमस्ते! आज हमें बहुत सारे संदेश मिल रहे हैं। हम जल्द ही व्यक्तिगत रूप से जवाब देंगे। 🙏",
        (_, "limit_wait") => "Hi! We're getting a lot of messages today. We'll reply to you personally in a moment. 🙏",

        ("es", "ics_confirmed") => "Reserva confirmada. Te esperamos.",
        ("pt", "ics_confirmed") => "Reserva confirmada. Esperamos por você.",
        ("fr", "ics_confirmed") => "Réservation confirmée. À bientôt.",
        ("de", "ics_confirmed") => "Reservierung bestätigt. Bis bald.",
        ("hi", "ics_confirmed") => "बुकिंग कन्फ़र्म। हम आपका इंतज़ार करेंगे।",
        (_, "ics_confirmed") => "Booking confirmed. See you then.",

        ("es", "ics_reminder") => "Recordatorio de tu cita",
        ("pt", "ics_reminder") => "Lembrete da sua consulta",
        ("fr", "ics_reminder") => "Rappel de votre rendez-vous",
        ("de", "ics_reminder") => "Erinnerung an Ihren Termin",
        ("hi", "ics_reminder") => "आपकी अपॉइंटमेंट का रिमाइंडर",
        (_, "ics_reminder") => "Reminder for your appointment",

        ("es", "mail_subject") => "Tu cita en {business} — {date}",
        ("pt", "mail_subject") => "Sua consulta em {business} — {date}",
        ("fr", "mail_subject") => "Votre rendez-vous chez {business} — {date}",
        ("de", "mail_subject") => "Ihr Termin bei {business} — {date}",
        ("hi", "mail_subject") => "{business} में आपकी अपॉइंटमेंट — {date}",
        (_, "mail_subject") => "Your appointment at {business} — {date}",

        ("es", "mail_body") => "Hola {customer},\n\nTu cita en {business} quedó confirmada para el {date} (hora local).\n\nAdjuntamos la invitación de calendario; tu teléfono te avisará {minutes} minutos antes.\n\n— agente",
        ("pt", "mail_body") => "Olá {customer},\n\nSua consulta em {business} está confirmada para {date} (hora local).\n\nSegue o convite de calendário; seu telefone vai avisar {minutes} minutos antes.\n\n— agente",
        ("fr", "mail_body") => "Bonjour {customer},\n\nVotre rendez-vous chez {business} est confirmé pour le {date} (heure locale).\n\nL'invitation de calendrier est jointe ; votre téléphone vous préviendra {minutes} minutes avant.\n\n— agente",
        ("de", "mail_body") => "Hallo {customer},\n\nIhr Termin bei {business} ist bestätigt für {date} (Ortszeit).\n\nDie Kalendereinladung ist angehängt; Ihr Telefon erinnert Sie {minutes} Minuten vorher.\n\n— agente",
        ("hi", "mail_body") => "नमस्ते {customer},\n\n{business} में आपकी अपॉइंटमेंट {date} (स्थानीय समय) के लिए कन्फ़र्म हो गई है।\n\nकैलेंडर इनवाइट संलग्न है; आपका फ़ोन {minutes} मिनट पहले याद दिलाएगा।\n\n— agente",
        (_, "mail_body") => "Hi {customer},\n\nYour appointment at {business} is confirmed for {date} (local time).\n\nThe calendar invite is attached; your phone will remind you {minutes} minutes before.\n\n— agente",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_real_timezone() {
        for p in PROFILES {
            assert!(p.timezone.parse::<chrono_tz::Tz>().is_ok(), "{} tz {}", p.iso, p.timezone);
            assert_eq!(p.iso.len(), 2);
        }
    }

    #[test]
    fn unknown_iso_is_generic_not_peru() {
        assert_eq!(profile("ZZ").iso, "");
        assert_eq!(profile("").language, "en");
        assert_eq!(profile("pe").currency, "PEN");
    }

    #[test]
    fn owner_overrides_win_over_table() {
        let l = Locale::from_values(&json!({"country": "IN", "currency": "USD", "currencySymbol": "$"}));
        assert_eq!(l.language, "hi");
        assert_eq!(l.money(50.0), "$ 50");
        let l = Locale::from_values(&json!({"country": "PE"}));
        assert_eq!(l.money(12.5), "S/ 12.50");
        assert!(l.prompt_section().contains("Yape"));
        let l = Locale::from_values(&json!({}));
        assert_eq!(l.money(3.0), "3");
    }

    /// The APK's registration picker and this table must list the same
    /// countries, or an owner can register somewhere the agent treats as
    /// "unknown" (English, UTC, no currency).
    #[test]
    fn matches_the_android_country_picker() {
        let kt = include_str!("../../android/app/src/main/java/tech/yaya/agente/Countries.kt");
        let re = regex::Regex::new(r#"Country\("([A-Z]{2})""#).unwrap();
        let mut apk: Vec<&str> = re.captures_iter(kt).map(|c| c.get(1).unwrap().as_str()).collect();
        apk.sort();
        apk.dedup();
        let mut here: Vec<&str> = PROFILES.iter().map(|p| p.iso).collect();
        here.sort();
        let missing: Vec<_> = apk.iter().filter(|i| !here.contains(i)).collect();
        let extra: Vec<_> = here.iter().filter(|i| !apk.contains(i)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "APK-only: {missing:?}; core-only: {extra:?}");
    }

    #[test]
    fn isos_are_unique_and_uppercase() {
        let mut seen = std::collections::HashSet::new();
        for p in PROFILES {
            assert!(seen.insert(p.iso), "duplicate {}", p.iso);
            assert_eq!(p.iso, p.iso.to_ascii_uppercase());
            assert!(!p.language.is_empty() && !p.currency.is_empty() && !p.symbol.is_empty() && !p.rails.is_empty() && !p.zone_word.is_empty(), "{}", p.iso);
        }
    }

    #[test]
    fn every_language_in_the_table_has_a_name() {
        for p in PROFILES {
            // "English" is also the fallback, so only en may map to it.
            assert!(p.language == "en" || language_name(p.language) != "English", "{} speaks {}", p.iso, p.language);
        }
        assert_eq!(language_name("xx"), "English");
    }

    #[test]
    fn profile_trims_and_ignores_case() {
        assert_eq!(profile("  br ").currency, "BRL");
        assert_eq!(*profile("??"), GENERIC);
    }

    #[test]
    fn defaults_mount_six_fields() {
        let d = profile("BR").defaults();
        assert_eq!(d, json!({"country": "BR", "language": "pt", "currency": "BRL", "currencySymbol": "R$", "paymentRails": "Pix", "timezone": "America/Sao_Paulo"}));
        assert_eq!(GENERIC.defaults()["timezone"], "UTC");
    }

    #[test]
    fn money_formats() {
        let l = |v: Value| Locale::from_values(&v);
        assert_eq!(l(json!({"country": "PE"})).money(0.0), "S/ 0");
        assert_eq!(l(json!({"country": "PE"})).money(1234.5), "S/ 1234.50");
        assert_eq!(l(json!({"country": "PE"})).money(0.005), "S/ 0.01");
        assert_eq!(l(json!({"currency": "EUR"})).money(9.0), "9 EUR");
        assert_eq!(l(json!({"country": "PE", "currencySymbol": ""})).money(5.0), "S/ 5", "an empty override does not erase the default");
    }

    #[test]
    fn currency_phrase_and_prompt() {
        let pe = Locale::from_values(&json!({"country": "PE"}));
        assert_eq!(pe.currency_phrase(), "PEN (S/)");
        assert_eq!(pe.language_name(), "Spanish");
        assert_eq!(pe.zone_word, "distrito");
        let p = pe.prompt_section();
        assert!(p.contains("Perú") && p.contains("Speak Spanish") && p.contains("'S/ 50'") && p.contains("distritos"));
        let g = Locale::from_values(&json!({}));
        assert!(g.currency_phrase().contains("ask the owner"));
        assert!(g.prompt_section().contains("an unspecified country"));
        // Owner rails override the table.
        let l = Locale::from_values(&json!({"country": "PE", "paymentRails": "Plin only"}));
        assert!(l.prompt_section().contains("Plin only") && !l.prompt_section().contains("Yape"));
    }

    #[test]
    fn canned_strings() {
        for key in ["fallback", "ai_disclosure", "limit_wait", "ics_confirmed", "ics_reminder", "mail_subject", "mail_body"] {
            assert!(!t("es", key).is_empty(), "{key}");
            assert!(!t("xx", key).is_empty(), "{key} has an English fallback");
        }
        assert!(t("es", "ai_disclosure").contains("{business}"));
        assert!(t("en", "mail_body").contains("{customer}") && t("en", "mail_body").contains("{minutes}"));
        assert_eq!(t("es", "nope"), "");
    }
}
