package tech.yaya.agente

import org.json.JSONArray
import org.json.JSONObject

/**
 * The onboarding chat as it is kept across a restart.
 *
 * It used to be blocks joined by a blank line with "🧑 "/"🟢 " markers,
 * and read back by splitting on blank lines — so every reply with a
 * paragraph break came back as two bubbles, the second one a grey system
 * line. New writes are a JSON list of `{role, text}`; the old format still
 * loads exactly as it always did. An empty chat is "" so "no transcript"
 * stays blank for the launcher's routing.
 */
object OnboardingTranscript {
    const val OWNER = "owner"
    const val AGENT = "agent"
    const val SYSTEM = "system"
    private const val OWNER_PREFIX = "🧑 "
    private const val AGENT_PREFIX = "🟢 "

    fun encode(msgs: List<Pair<String, String>>): String =
        if (msgs.isEmpty()) "" else JSONArray().apply { msgs.forEach { (r, t) -> put(JSONObject().put("role", r).put("text", t)) } }.toString()

    fun decode(stored: String?, typingIndicator: String): List<Pair<String, String>> {
        val s = stored?.trim().orEmpty()
        if (s.isEmpty()) return emptyList()
        if (s.startsWith("[")) {
            // org.json is lenient ("[nota]" parses), so only a list of message
            // objects is the new format; anything else is a legacy line.
            val arr = runCatching { JSONArray(s) }.getOrNull()
                ?.takeIf { a -> a.length() > 0 && (0 until a.length()).all { a.opt(it) is JSONObject } }
            if (arr != null) return (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                val t = o.optString("text")
                val r = o.optString("role").takeIf { it == OWNER || it == AGENT } ?: SYSTEM
                if (t.isBlank() || t == typingIndicator) null else r to t
            }
        }
        return s.split("\n\n").mapNotNull { line ->
            val t = line.trim()
            when {
                t.isEmpty() || t == typingIndicator -> null
                t.startsWith(OWNER_PREFIX) -> OWNER to t.removePrefix(OWNER_PREFIX)
                t.startsWith(AGENT_PREFIX) -> AGENT to t.removePrefix(AGENT_PREFIX)
                else -> SYSTEM to t
            }
        }
    }
}
