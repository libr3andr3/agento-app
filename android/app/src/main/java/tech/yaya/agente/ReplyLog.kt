package tech.yaya.agente

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

/** One handled notification, kept for the on-screen activity feed (and later, server sync). */
data class ReplyEvent(
    val timestamp: Long,
    val appPackage: String,
    val appName: String,
    val sender: String,
    val incomingText: String,
    val replySent: Boolean,
    val detail: String
)

/** Rolling log of recent events, persisted as JSON in SharedPreferences. */
object ReplyLog {
    private const val FILE = "agente_log"
    private const val KEY = "events"
    private const val MAX_EVENTS = 100

    @Volatile
    var listener: (() -> Unit)? = null

    fun add(ctx: Context, event: ReplyEvent) {
        synchronized(this) {
            val events = load(ctx).toMutableList()
            events.add(0, event)
            while (events.size > MAX_EVENTS) events.removeAt(events.size - 1)
            save(ctx, events)
        }
        listener?.invoke()
    }

    fun load(ctx: Context): List<ReplyEvent> {
        val raw = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
            .getString(KEY, "[]") ?: "[]"
        // Malformed entries are skipped one by one, not wholesale: this log is
        // the owner's only proof of what the agent said on their behalf, so a
        // single bad record (interrupted write, schema drift from an old
        // build) must not blank the whole feed.
        val arr = try {
            JSONArray(raw)
        } catch (_: Exception) {
            return emptyList()
        }
        return (0 until arr.length()).mapNotNull { i ->
            try {
                val o = arr.getJSONObject(i)
                ReplyEvent(
                    timestamp = o.getLong("ts"),
                    appPackage = o.getString("pkg"),
                    appName = o.getString("app"),
                    sender = o.getString("sender"),
                    incomingText = o.getString("text"),
                    replySent = o.getBoolean("sent"),
                    detail = o.optString("detail", "")
                )
            } catch (_: Exception) {
                null
            }
        }
    }

    fun clear(ctx: Context) {
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).edit().remove(KEY).apply()
        listener?.invoke()
    }

    private fun save(ctx: Context, events: List<ReplyEvent>) {
        val arr = JSONArray()
        events.forEach { e ->
            arr.put(
                JSONObject()
                    .put("ts", e.timestamp)
                    .put("pkg", e.appPackage)
                    .put("app", e.appName)
                    .put("sender", e.sender)
                    .put("text", e.incomingText)
                    .put("sent", e.replySent)
                    .put("detail", e.detail)
            )
        }
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
            .edit().putString(KEY, arr.toString()).apply()
    }
}
