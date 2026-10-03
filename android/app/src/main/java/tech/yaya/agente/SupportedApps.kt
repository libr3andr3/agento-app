package tech.yaya.agente

/** Messaging apps agente knows how to parse, keyed by Android package name. */
data class SupportedApp(
    val packageName: String,
    val displayName: String
)

object SupportedApps {
    val ALL = listOf(
        SupportedApp("com.whatsapp", "WhatsApp"),
        SupportedApp("com.whatsapp.w4b", "WhatsApp Business"),
        SupportedApp("com.instagram.android", "Instagram"),
        SupportedApp("com.facebook.orca", "Facebook Messenger"),
        SupportedApp("com.facebook.mlite", "Messenger Lite"),
        SupportedApp("com.facebook.katana", "Facebook"),
        SupportedApp("org.telegram.messenger", "Telegram"),
        SupportedApp("org.thoughtcrime.securesms", "Signal"),
        SupportedApp("com.zhiliaoapp.musically", "TikTok"),
        SupportedApp("com.google.android.apps.messaging", "Google Messages (SMS)")
    )

    /**
     * Apps the agent answers on out of the box. WhatsApp Business only: it is
     * the one app on this list that is unambiguously a business inbox. A new
     * user's personal WhatsApp answering their friends with a receptionist
     * script is the fastest possible uninstall — everything else is opt-in
     * from Settings.
     */
    val DEFAULT_ENABLED = setOf("com.whatsapp.w4b")

    private val byPackage = ALL.associateBy { it.packageName }

    fun get(packageName: String): SupportedApp? = byPackage[packageName]
    fun isSupported(packageName: String): Boolean = byPackage.containsKey(packageName)
}

// Payment notifications: see PaymentDetector (any bank/wallet, any country).
