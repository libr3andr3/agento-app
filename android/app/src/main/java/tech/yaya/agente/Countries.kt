package tech.yaya.agente

import android.content.Context
import java.text.Collator
import java.text.Normalizer
import java.util.Locale

/**
 * One country the owner can register a phone from: ISO 3166-1 alpha-2 code,
 * a Spanish fallback name, and the E.164 dial code (digits only, no '+').
 * The flag is derived from the ISO code (regional indicator pair), so it can
 * never disagree with the country it labels. The display name comes from
 * the phone's own locale data ([name]), so a Portuguese phone reads
 * «Brasil» and an English one «Brazil» without a table per language.
 */
data class Country(val iso: String, val nameEs: String, val dial: String) {
    val flag: String = iso.uppercase(Locale.US).map {
        String(Character.toChars(0x1F1E6 + (it.code - 'A'.code)))
    }.joinToString("")

    /** The country's name in the app's current language. */
    fun name(ctx: Context): String = name(ctx.resources.configuration.locales[0])

    fun name(locale: Locale): String {
        val n = runCatching { Locale("", iso).getDisplayCountry(locale) }.getOrDefault("")
        return if (n.isBlank() || n.equals(iso, ignoreCase = true)) nameEs else n
    }
}

/**
 * Registration country list. All of Latin America plus the top world markets —
 * enough that a migrant owner or a traveling customer never hits a wall.
 * Sorted with the current language's collation so «España» files where a
 * Spanish speaker expects it and "Spain" where an English one does. Perú is
 * the default (our first market).
 */
object Countries {

    private val RAW = listOf(
        // Latin America & Caribbean — complete
        Country("AR", "Argentina", "54"),
        Country("BZ", "Belice", "501"),
        Country("BO", "Bolivia", "591"),
        Country("BR", "Brasil", "55"),
        Country("CL", "Chile", "56"),
        Country("CO", "Colombia", "57"),
        Country("CR", "Costa Rica", "506"),
        Country("CU", "Cuba", "53"),
        Country("EC", "Ecuador", "593"),
        Country("SV", "El Salvador", "503"),
        Country("GT", "Guatemala", "502"),
        Country("GY", "Guyana", "592"),
        Country("HT", "Haití", "509"),
        Country("HN", "Honduras", "504"),
        Country("JM", "Jamaica", "1"),
        Country("MX", "México", "52"),
        Country("NI", "Nicaragua", "505"),
        Country("PA", "Panamá", "507"),
        Country("PY", "Paraguay", "595"),
        Country("PE", "Perú", "51"),
        Country("PR", "Puerto Rico", "1"),
        Country("DO", "República Dominicana", "1"),
        Country("SR", "Surinam", "597"),
        Country("TT", "Trinidad y Tobago", "1"),
        Country("UY", "Uruguay", "598"),
        Country("VE", "Venezuela", "58"),
        // North America & Europe
        Country("US", "Estados Unidos", "1"),
        Country("CA", "Canadá", "1"),
        Country("ES", "España", "34"),
        Country("PT", "Portugal", "351"),
        Country("GB", "Reino Unido", "44"),
        Country("FR", "Francia", "33"),
        Country("DE", "Alemania", "49"),
        Country("IT", "Italia", "39"),
        Country("NL", "Países Bajos", "31"),
        Country("BE", "Bélgica", "32"),
        Country("CH", "Suiza", "41"),
        Country("AT", "Austria", "43"),
        Country("IE", "Irlanda", "353"),
        Country("SE", "Suecia", "46"),
        Country("NO", "Noruega", "47"),
        Country("DK", "Dinamarca", "45"),
        Country("FI", "Finlandia", "358"),
        Country("PL", "Polonia", "48"),
        Country("CZ", "Chequia", "420"),
        Country("HU", "Hungría", "36"),
        Country("RO", "Rumania", "40"),
        Country("GR", "Grecia", "30"),
        Country("UA", "Ucrania", "380"),
        Country("TR", "Turquía", "90"),
        // Middle East & Africa
        Country("IL", "Israel", "972"),
        Country("AE", "Emiratos Árabes Unidos", "971"),
        Country("SA", "Arabia Saudita", "966"),
        Country("QA", "Catar", "974"),
        Country("EG", "Egipto", "20"),
        Country("MA", "Marruecos", "212"),
        Country("ZA", "Sudáfrica", "27"),
        Country("NG", "Nigeria", "234"),
        Country("KE", "Kenia", "254"),
        Country("GH", "Ghana", "233"),
        // Asia & Pacific
        Country("IN", "India", "91"),
        Country("CN", "China", "86"),
        Country("JP", "Japón", "81"),
        Country("KR", "Corea del Sur", "82"),
        Country("HK", "Hong Kong", "852"),
        Country("TW", "Taiwán", "886"),
        Country("SG", "Singapur", "65"),
        Country("MY", "Malasia", "60"),
        Country("ID", "Indonesia", "62"),
        Country("PH", "Filipinas", "63"),
        Country("TH", "Tailandia", "66"),
        Country("VN", "Vietnam", "84"),
        Country("PK", "Pakistán", "92"),
        Country("BD", "Bangladés", "880"),
        Country("AU", "Australia", "61"),
        Country("NZ", "Nueva Zelanda", "64"),
    )

    /** Every country, in Spanish order (use [all] for the current language). */
    val ALL: List<Country> = RAW.sortedWith(
        compareBy(Collator.getInstance(Locale("es"))) { it.nameEs }
    )

    val DEFAULT: Country = RAW.first { it.iso == "PE" }

    fun byIso(iso: String?): Country = ALL.firstOrNull { it.iso == iso } ?: DEFAULT

    /**
     * Perú, always: it is the launch market, and a phone that happens to be
     * roaming or on a foreign SIM should still open on +51. The picker is one
     * tap away for everyone else.
     */
    fun defaultFor(@Suppress("UNUSED_PARAMETER") ctx: Context): Country = DEFAULT

    private fun locale(ctx: Context): Locale = ctx.resources.configuration.locales[0]

    /** Every country, sorted for the app's current language. */
    fun all(ctx: Context): List<Country> {
        val loc = locale(ctx)
        return RAW.sortedWith(compareBy(Collator.getInstance(loc)) { it.name(loc) })
    }

    /** Accent-insensitive lowercase for search ("peru" finds "Perú"). */
    private fun fold(s: String): String =
        Normalizer.normalize(s.lowercase(Locale.ROOT), Normalizer.Form.NFD)
            .replace(Regex("\\p{Mn}+"), "")

    /** Matches by name in the current language or Spanish (accent-insensitive),
     *  dial code (with or without '+'), or ISO. */
    fun search(ctx: Context, query: String): List<Country> {
        val q = fold(query.trim().removePrefix("+"))
        val list = all(ctx)
        if (q.isEmpty()) return list
        val loc = locale(ctx)
        return list.filter {
            fold(it.name(loc)).contains(q) ||
                fold(it.nameEs).contains(q) ||
                it.dial.startsWith(q) ||
                it.iso.lowercase(Locale.US).startsWith(q)
        }
    }
}
