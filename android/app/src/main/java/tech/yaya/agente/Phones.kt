package tech.yaya.agente

/**
 * What a person typed into a phone field, as E.164 ("+51987654321").
 *
 * People paste the number the way WhatsApp copies it — "+51 987 654 321" —
 * into a field that already shows +51, and "+51" + "51987654321" is a
 * number that does not exist: the verification code went nowhere. So a
 * typed international prefix (+ or 00) wins over the picker, and a dial
 * code repeated without one is dropped only when what remains is a valid
 * national length for the country and the whole is not — a Brazilian in
 * area code 55 dialing "55 99123 4567" keeps every digit.
 */
object Phones {
    /** National significant number lengths for the markets we serve most. */
    private val NATIONAL = mapOf(
        "PE" to setOf(9, 8), "MX" to setOf(10), "CO" to setOf(10), "CL" to setOf(9), "AR" to setOf(10, 11),
        "BR" to setOf(10, 11), "EC" to setOf(9), "BO" to setOf(8), "VE" to setOf(10), "UY" to setOf(8),
        "PY" to setOf(9), "US" to setOf(10), "CA" to setOf(10), "ES" to setOf(9), "PT" to setOf(9),
    )

    const val MIN_DIGITS = 8
    const val MAX_DIGITS = 15

    fun e164(country: Country, raw: String): String? {
        val s = raw.trim()
        val international = s.startsWith("+") || s.startsWith("00")
        val digits = s.filter(Char::isDigit).let { if (!s.startsWith("+") && it.startsWith("00")) it.drop(2) else it }
        val full = if (international) digits else {
            val local = digits.trimStart('0')
            val national = NATIONAL[country.iso]
            val repeated = national != null && local.startsWith(country.dial) &&
                (local.length - country.dial.length) in national && local.length !in national
            if (repeated) local else country.dial + local
        }
        val localPart = if (full.startsWith(country.dial)) full.length - country.dial.length else full.length
        return if (full.length in MIN_DIGITS..MAX_DIGITS && localPart >= 7) "+$full" else null
    }
}
