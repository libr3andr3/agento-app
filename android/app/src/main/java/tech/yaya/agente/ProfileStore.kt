package tech.yaya.agente

import android.content.Context
import org.json.JSONObject

/**
 * Where learned chat apps live: one JSON object of [NotificationProfile]s
 * keyed by package, plus a sleep list for packages that failed the replay
 * test or were dropped. Persisted in SharedPreferences like [ReplyLog] —
 * survives process death, cleared with the app's data, never leaves the
 * phone unless the owner shares it.
 *
 * "Live" is never stored: it is derived every time from three facts —
 * the profile is eligible, it is not paused, and the owner flipped the
 * switch ([Prefs.isAppEnabled]). Consent stays exactly where it is for
 * built-in apps.
 */
object ProfileStore {
    private const val FILE = "agente_profiles"
    private const val KEY_PROFILES = "profiles"
    private const val KEY_SLEEP = "sleep"

    private fun sp(ctx: Context) = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    private fun loadAll(ctx: Context): JSONObject =
        runCatching { JSONObject(sp(ctx).getString(KEY_PROFILES, null) ?: "{}") }.getOrDefault(JSONObject())

    private fun saveAll(ctx: Context, all: JSONObject) =
        sp(ctx).edit().putString(KEY_PROFILES, all.toString()).apply()

    @Synchronized
    fun get(ctx: Context, pkg: String): NotificationProfile? =
        loadAll(ctx).optJSONObject(pkg)?.let { NotificationProfile.fromJson(it) }

    @Synchronized
    fun all(ctx: Context): List<NotificationProfile> {
        val all = loadAll(ctx)
        return all.keys().asSequence()
            .mapNotNull { all.optJSONObject(it)?.let(NotificationProfile::fromJson) }
            .sortedByDescending { it.builtAt }
            .toList()
    }

    @Synchronized
    fun mount(ctx: Context, profile: NotificationProfile) {
        val all = loadAll(ctx)
        all.put(profile.packageName, profile.toJson())
        saveAll(ctx, all)
    }

    fun isLive(ctx: Context, p: NotificationProfile): Boolean =
        !p.paused && p.eligible && Prefs.isAppEnabled(ctx, p.packageName)

    /**
     * One read of a notification from a learned app. Keeps a rolling window
     * (counts are halved past [NotificationProfile.UNWIND_WINDOW], so old
     * history fades), and pauses the profile when failures cross the line —
     * the owner's switch goes off with it. Returns the updated profile and
     * whether this read is the one that paused it.
     */
    @Synchronized
    fun record(ctx: Context, pkg: String, ok: Boolean): Pair<NotificationProfile, Boolean>? {
        val all = loadAll(ctx)
        var p = all.optJSONObject(pkg)?.let(NotificationProfile::fromJson) ?: return null
        var okN = p.parsedOk + if (ok) 1 else 0
        var failN = p.parseFail + if (ok) 0 else 1
        if (okN + failN > NotificationProfile.UNWIND_WINDOW) {
            okN /= 2
            failN /= 2
        }
        p = p.copy(parsedOk = okN, parseFail = failN)
        var pausedNow = false
        val judged = p.reads >= NotificationProfile.GRADUATE_OK
        if (!p.paused && judged && p.failRate > NotificationProfile.UNWIND_FAIL_RATE) {
            pausedNow = true
            p = p.copy(pausedAt = System.currentTimeMillis(), unwinds = p.unwinds + 1)
            Prefs.setAppEnabled(ctx, pkg, false)
        }
        if (p.unwinds > NotificationProfile.MAX_UNWINDS) {
            all.remove(pkg)
            saveAll(ctx, all)
            sleep(ctx, pkg, System.currentTimeMillis() + DROP_SLEEP_MS)
            return p to pausedNow
        }
        all.put(pkg, p.toJson())
        saveAll(ctx, all)
        return p to pausedNow
    }

    /** The owner re-enabled a paused app: give it a clean window. */
    @Synchronized
    fun resume(ctx: Context, pkg: String) {
        val all = loadAll(ctx)
        val p = all.optJSONObject(pkg)?.let(NotificationProfile::fromJson) ?: return
        all.put(pkg, p.copy(parsedOk = 0, parseFail = 0, pausedAt = 0L).toJson())
        saveAll(ctx, all)
    }

    // ------------------------------------------------------------------ sleep

    private fun sleepMap(ctx: Context): JSONObject =
        runCatching { JSONObject(sp(ctx).getString(KEY_SLEEP, null) ?: "{}") }.getOrDefault(JSONObject())

    @Synchronized
    fun isAsleep(ctx: Context, pkg: String): Boolean =
        sleepMap(ctx).optLong(pkg, 0L) > System.currentTimeMillis()

    @Synchronized
    fun sleep(ctx: Context, pkg: String, untilMs: Long) {
        val m = sleepMap(ctx)
        val now = System.currentTimeMillis()
        // Tidy expired entries while we are here.
        m.keys().asSequence().toList().forEach { k -> if (m.optLong(k) <= now) m.remove(k) }
        m.put(pkg, untilMs)
        sp(ctx).edit().putString(KEY_SLEEP, m.toString()).apply()
    }

    const val DROP_SLEEP_MS = 7L * 24 * 3600 * 1000
}
