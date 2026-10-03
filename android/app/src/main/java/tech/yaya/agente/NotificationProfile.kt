package tech.yaya.agente

import org.json.JSONObject

/**
 * A chat app agente taught itself to read.
 *
 * Not code — a claim: "this package posts conversation notifications of this
 * shape, with an inline reply", plus the trial state that decides whether the
 * claim is trusted. The parser itself is the same one every built-in app uses
 * ([AgenteNotificationListener.parseMessage] handles MessagingStyle and plain
 * title/text); what a profile adds is *permission to try*, earned in stages:
 *
 *   observed (3 notifications, see [UnknownAppObserver])
 *   → shadow: parsed and logged, never answered
 *   → eligible: enough clean reads → the owner may switch replies on
 *   → live: owner enabled it in Ajustes → Apps conectadas
 *   → paused: reads started failing; the switch is turned off again
 *   → dropped after repeated pauses; the package sleeps a week.
 *
 * Every step is data in [ProfileStore]. Nothing else in the app changes, and
 * unmounting a profile leaves no residue — the same reversibility the
 * server's learning trials have.
 */
data class NotificationProfile(
    val packageName: String,
    val displayName: String,
    /** "messaging" (MessagingStyle) or "title_text" (plain title/text + reply action). */
    val style: String,
    val builtAt: Long,
    /** "device" when this phone built it; "network" when it came from the registry. */
    val builtBy: String,
    /** Notifications the profile was built from. */
    val samples: Int,
    val parsedOk: Int = 0,
    val parseFail: Int = 0,
    val unwinds: Int = 0,
    val pausedAt: Long = 0L,
) {
    val reads: Int get() = parsedOk + parseFail
    val failRate: Double get() = if (reads == 0) 0.0 else parseFail.toDouble() / reads
    val paused: Boolean get() = pausedAt > 0L

    /** Enough clean reads for the owner to be offered the switch. */
    val eligible: Boolean get() = parsedOk >= GRADUATE_OK && failRate < UNWIND_FAIL_RATE

    fun toJson(): JSONObject = JSONObject()
        .put("package", packageName)
        .put("label", displayName)
        .put("style", style)
        .put("builtAt", builtAt)
        .put("builtBy", builtBy)
        .put("samples", samples)
        .put("parsedOk", parsedOk)
        .put("parseFail", parseFail)
        .put("unwinds", unwinds)
        .put("pausedAt", pausedAt)

    companion object {
        /** Clean reads before the owner is offered the switch. */
        const val GRADUATE_OK = 10
        /** Reads kept in the rolling window that judges a live profile. */
        const val UNWIND_WINDOW = 50
        /** Failure share that pauses a profile (and blocks eligibility). */
        const val UNWIND_FAIL_RATE = 0.2
        /** Pauses tolerated before the profile is dropped. */
        const val MAX_UNWINDS = 2

        fun fromJson(o: JSONObject): NotificationProfile? {
            val pkg = o.optString("package").takeIf { it.isNotBlank() } ?: return null
            return NotificationProfile(
                packageName = pkg,
                displayName = o.optString("label", pkg),
                style = o.optString("style", "title_text"),
                builtAt = o.optLong("builtAt"),
                builtBy = o.optString("builtBy", "device"),
                samples = o.optInt("samples"),
                parsedOk = o.optInt("parsedOk"),
                parseFail = o.optInt("parseFail"),
                unwinds = o.optInt("unwinds"),
                pausedAt = o.optLong("pausedAt"),
            )
        }
    }
}
