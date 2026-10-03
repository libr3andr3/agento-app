package tech.yaya.agente

import android.app.Notification
import android.content.Context
import android.service.notification.StatusBarNotification
import android.util.Log
import androidx.core.app.NotificationCompat
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

/**
 * The house phone's till: forwards every Yape notification to the gateway
 * (`POST /v1/collector/yape`), which matches its amount to the recarga or
 * plan it pays (unique céntimos, see gateway/src/yape.rs).
 *
 * Off on every phone but the one whose Yape receives agento's own sales:
 * it only runs once a collector key was set on it through
 * [CollectorActivity]. Only the Yape app's notifications are forwarded,
 * and they go to our gateway and nowhere else — the URL is compiled in.
 *
 * Nothing is lost when the network is: notifications wait in a small
 * durable queue and go out on the next notification or listener
 * reconnect. Sending one twice is harmless — the gateway stores each
 * notification once.
 */
object YapeCollector {
    const val YAPE = "com.bcp.innovacxion.yapeapp"
    private const val TAG = "YapeCollector"
    private const val FILE = "yape_collector"
    private const val KEY_KEY = "collector_key"
    private const val KEY_QUEUE = "queue"
    private const val MAX_QUEUED = 200

    /** Test seam; the app always uses the compiled-in gateway. */
    internal var baseUrl: String = BuildConfig.GATEWAY_URL

    private fun sp(ctx: Context) = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
    private val lock = Any()
    /** One flush at a time: two would both send the head and then each
     *  drop one item — the second of them unsent. */
    private val flushing = java.util.concurrent.locks.ReentrantLock()

    fun key(ctx: Context): String = sp(ctx).getString(KEY_KEY, null).orEmpty()
    fun isOn(ctx: Context) = key(ctx).isNotEmpty()
    fun setKey(ctx: Context, key: String?) {
        sp(ctx).edit().apply { if (key.isNullOrBlank()) remove(KEY_KEY) else putString(KEY_KEY, key.trim()) }.apply()
    }

    fun queued(ctx: Context): Int = synchronized(lock) { load(ctx).length() }

    /** What the gateway receives for one notification, or null when it is
     *  not a Yape notification worth sending. `postedAt` is the
     *  notification's own time, identical across reposts of it. */
    fun payload(pkg: String, whenMs: Long, postTime: Long, isSummary: Boolean,
                title: String?, text: String?, bigText: String?): JSONObject? {
        if (pkg != YAPE || isSummary) return null
        if (title.isNullOrBlank() && text.isNullOrBlank() && bigText.isNullOrBlank()) return null
        return JSONObject()
            .put("package", pkg)
            .put("postedAt", if (whenMs > 0) whenMs else postTime)
            .put("title", title.orEmpty())
            .put("text", text.orEmpty())
            .put("bigText", bigText.orEmpty())
    }

    /** Called for every posted notification; cheap no-op unless this is the
     *  house phone and the notification is Yape's. */
    fun onPosted(ctx: Context, sbn: StatusBarNotification) {
        if (sbn.packageName != YAPE || !isOn(ctx)) return
        val n = sbn.notification ?: return
        val x = n.extras
        val p = payload(
            sbn.packageName, n.`when`, sbn.postTime,
            n.flags and Notification.FLAG_GROUP_SUMMARY != 0,
            x?.getCharSequence(NotificationCompat.EXTRA_TITLE)?.toString(),
            x?.getCharSequence(NotificationCompat.EXTRA_TEXT)?.toString(),
            x?.getCharSequence(NotificationCompat.EXTRA_BIG_TEXT)?.toString(),
        ) ?: return
        enqueue(ctx, p)
        ServerClient.IO_EXECUTOR.execute { flush(ctx) }
    }

    /** After a reconnect: whatever Yape still shows may have arrived while
     *  we were unbound. Duplicates collapse at the gateway. */
    fun catchUp(ctx: Context, active: Array<StatusBarNotification>?) {
        if (!isOn(ctx)) return
        active?.filter { it.packageName == YAPE }?.forEach { onPosted(ctx, it) }
        ServerClient.IO_EXECUTOR.execute { flush(ctx) }
    }

    private fun load(ctx: Context): JSONArray =
        runCatching { JSONArray(sp(ctx).getString(KEY_QUEUE, "[]")) }.getOrDefault(JSONArray())

    private fun enqueue(ctx: Context, p: JSONObject) = synchronized(lock) {
        val q = load(ctx)
        q.put(p)
        // Oldest first out when full: a phone offline for days keeps the most recent.
        while (q.length() > MAX_QUEUED) q.remove(0)
        sp(ctx).edit().putString(KEY_QUEUE, q.toString()).apply()
    }

    /** Sends what is queued, in order; stops at the first network failure
     *  and keeps the rest for next time. Blocking: IO_EXECUTOR only. */
    fun flush(ctx: Context) {
        val key = key(ctx).takeIf { it.isNotEmpty() } ?: return
        flushing.lock()
        try { drain(ctx, key) } finally { flushing.unlock() }
    }

    private fun drain(ctx: Context, key: String) {
        while (true) {
            val head = synchronized(lock) { load(ctx).optJSONObject(0) } ?: return
            val code = runCatching { post(key, head) }.getOrElse {
                Log.w(TAG, "gateway unreachable, ${queued(ctx)} queued: ${it.javaClass.simpleName}")
                return
            }
            // Server trouble, or a key that is wrong until someone fixes it:
            // keep everything — these are payments.
            if (code in 500..599 || code == 429 || code == 401) {
                Log.w(TAG, "gateway $code, keeping the queue")
                return
            }
            // 2xx = stored; any other 4xx (not Yape, malformed) will never
            // succeed: drop it rather than block the queue behind it.
            if (code !in 200..299) Log.e(TAG, "gateway refused a notification: $code")
            synchronized(lock) {
                val q = load(ctx)
                if (q.length() > 0) q.remove(0)
                sp(ctx).edit().putString(KEY_QUEUE, q.toString()).apply()
            }
        }
    }

    private fun post(key: String, body: JSONObject): Int {
        val c = URL(baseUrl.trimEnd('/') + "/v1/collector/yape").openConnection() as HttpURLConnection
        return try {
            c.requestMethod = "POST"
            c.connectTimeout = 10_000
            c.readTimeout = 20_000
            c.doOutput = true
            c.setRequestProperty("content-type", "application/json")
            c.setRequestProperty("x-collector-key", key)
            c.outputStream.use { it.write(body.toString().toByteArray()) }
            c.responseCode
        } finally {
            c.disconnect()
        }
    }
}
