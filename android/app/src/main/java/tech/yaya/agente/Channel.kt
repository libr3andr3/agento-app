package tech.yaya.agente

/**
 * Where a message came from, independent of which app delivered it.
 *
 * The listener reads notifications from a dozen apps that agree on nothing:
 * WhatsApp names a conversation by the contact's phone number until it is
 * saved, Instagram only ever gives a handle, Messenger gives a display name,
 * Telegram gives either. Everything above the parser — the CRM, the agent's
 * memory of a customer, the owner's per-contact switches — needs one shape,
 * so that shape is defined here and nowhere else.
 *
 * Two apps can be one channel (`com.whatsapp` and `com.whatsapp.w4b` are both
 * `whatsapp`): the person who messages the owner's personal WhatsApp today and
 * the business one tomorrow is the same customer, and splitting them would
 * split their history.
 */
data class Channel(val id: String, val displayName: String) {
    companion object {
        /**
         * Package → channel. Kept deliberately explicit rather than derived by
         * substring: `com.facebook.katana` (Facebook) and `com.facebook.orca`
         * (Messenger) share a prefix and are different inboxes, and a substring
         * rule silently gets that wrong.
         *
         * The core has a mirror of this in `contacts.rs::source_of`, which is a
         * *fallback* for old clients — this build sends `channel` explicitly so
         * the two can never disagree about a customer's identity.
         */
        private val BY_PACKAGE = mapOf(
            "com.whatsapp" to Channel("whatsapp", "WhatsApp"),
            "com.whatsapp.w4b" to Channel("whatsapp", "WhatsApp Business"),
            "com.instagram.android" to Channel("instagram", "Instagram"),
            "com.facebook.orca" to Channel("messenger", "Facebook Messenger"),
            "com.facebook.mlite" to Channel("messenger", "Messenger Lite"),
            "com.facebook.katana" to Channel("facebook", "Facebook"),
            "org.telegram.messenger" to Channel("telegram", "Telegram"),
            "org.telegram.plus" to Channel("telegram", "Telegram"),
            "org.telegram.messenger.web" to Channel("telegram", "Telegram"),
            "org.thoughtcrime.securesms" to Channel("signal", "Signal"),
            "com.zhiliaoapp.musically" to Channel("tiktok", "TikTok"),
            "com.ss.android.ugc.trill" to Channel("tiktok", "TikTok"),
            "com.google.android.apps.messaging" to Channel("sms", "SMS"),
        )

        /**
         * An app this phone taught itself ([UnknownAppObserver]) has no entry
         * here and becomes its own channel, keyed by package. That is the
         * honest answer: we know its messages are conversations, we do not know
         * it shares an identity space with anything else.
         */
        fun of(packageName: String, fallbackLabel: String? = null): Channel =
            BY_PACKAGE[packageName]
                ?: Channel(packageName, fallbackLabel ?: packageName)
    }
}

/**
 * One human, as seen through one channel.
 *
 * [handle] is the stable key the CRM groups on. Where the app gives us a phone
 * number we use its digits, because that is the one identifier that survives a
 * rename and matches across apps; everywhere else we fall back to the
 * normalised display name, which is all Instagram and Messenger ever offer.
 *
 * **The honest limit:** a display-name handle is only as stable as the name.
 * If a customer renames themselves on Instagram we will read them as a new
 * person until something ties the two together — a phone number or an email
 * they give the agent. That is a real gap, not an oversight; the fix is the
 * core's merge-on-phone/email rule, not a cleverer string here.
 */
data class SenderRef(
    val channel: Channel,
    val handle: String,
    val displayName: String,
    /** True when [handle] is phone digits, i.e. safe to match across channels. */
    val isPhone: Boolean,
) {
    companion object {
        /** A name is a phone number if digits are essentially all it contains. */
        private val PHONEISH = Regex("^[+()\\-\\s\\d.]{7,25}$")

        fun from(channel: Channel, displayName: String): SenderRef {
            val shown = displayName.trim()
            val digits = shown.filter { it.isDigit() }
            val isPhone = PHONEISH.matches(shown) && digits.length >= 7
            val handle = if (isPhone) digits else normalise(shown)
            return SenderRef(channel, handle, shown, isPhone)
        }

        /**
         * Case and spacing are cosmetic on every platform here, so they must not
         * create a second customer. Everything else is left alone: emoji and
         * punctuation are part of how people actually name themselves, and
         * stripping them collides distinct handles.
         */
        private fun normalise(name: String): String =
            name.lowercase().replace(Regex("\\s+"), " ").trim()
    }

    /**
     * The conversation id the core has always used: `<package>:<shown name>`.
     *
     * Deliberately unchanged. Message history, the audit chain and D14's
     * `conversation_months` billing table are all keyed on this string, so
     * recomputing it would orphan every existing conversation and silently
     * re-bill customers the business has already paid for. The new identity
     * travels *beside* it, and the core decides which people are one person.
     */
    fun legacyPeer(appPackage: String): String = "$appPackage:$displayName"
}
