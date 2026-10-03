package tech.yaya.agente

import android.content.Context
import android.util.Log
import org.json.JSONObject
import java.io.File
import java.security.SecureRandom

/**
 * The business agent itself, running inside this process as `libagente_core.so`
 * (Rust: corazón kernel, plugins, SQLite, HTTP on the loopback). The phone IS
 * the server: every conversation, booking and payment lives in
 * `filesDir/agente.db` and never leaves the device unless the owner says so.
 *
 * The Kotlin side keeps talking plain HTTP to `http://127.0.0.1:<port>` —
 * same endpoints as the old hosted server — so the rest of the app did not
 * have to change. Only the LLM call leaves the phone, to the engine
 * configured in [Prefs] (yaya.tech by default, or the owner's own key).
 */
object AgenteCore {
    private const val TAG = "AgenteCore"

    init {
        System.loadLibrary("agente_core")
    }

    @JvmStatic private external fun start(configJson: String): Int
    @JvmStatic private external fun stop()
    @JvmStatic private external fun port(): Int
    @JvmStatic external fun version(): String

    /** Loopback port of the running core, booting it if needed. 0 = failed. */
    @Synchronized
    fun ensureStarted(ctx: Context): Int {
        val running = port()
        if (running > 0) return running
        val app = ctx.applicationContext
        return try {
            installSchemas(app)
            val code = start(config(app).toString())
            if (code <= 0) {
                Log.e(TAG, "core boot failed ($code)")
                0
            } else {
                Log.i(TAG, "core ${version()} listening on 127.0.0.1:$code")
                // Measured boot, off this thread: the core is up, so it can
                // answer /api/agent; the chain goes back to it when ready.
                Thread({ DeviceAttestation.ensure(app) }, "agente-attest").start()
                code
            }
        } catch (e: Throwable) {
            Log.e(TAG, "core boot threw", e)
            0
        }
    }

    fun baseUrl(ctx: Context): String = "http://127.0.0.1:${ensureStarted(ctx)}"

    fun shutdown() = stop()

    // ----------------------------------------------------------- config

    private fun config(app: Context): JSONObject {
        val files = app.filesDir
        val o = JSONObject()
        o.put("DATABASE_URL", "sqlite://" + File(files, "agente.db").absolutePath)
        o.put("SCHEMAS_DIR", File(files, "schemas").absolutePath)
        o.put("BIND_ADDR", "127.0.0.1:0")
        o.put("APP_KEY", appKey(app))
        o.put("ADMIN_KEY", secret(app, "core_admin_key"))
        // The agent's identity seed is sealed at rest under this key, which
        // itself lives Keystore-wrapped in SecureStore: a copy of agente.db
        // is not the business's identity.
        o.put("IDENTITY_KEK_HEX", hexSecret(app, "core_identity_kek"))
        // Shown on the account page's device list.
        o.put("DEVICE_LABEL", (android.os.Build.MANUFACTURER + " " + android.os.Build.MODEL).trim().take(60))
        // AI engine: blank values mean "yaya.tech gateway, authenticated by
        // this agent's own identity" — the core's defaults.
        Prefs.llmBaseUrl(app).takeIf { it.isNotBlank() }?.let { o.put("LLM_BASE_URL", it) }
        Prefs.llmApiKey(app).takeIf { it.isNotBlank() }?.let { o.put("LLM_API_KEY", it) }
        Prefs.llmModel(app).takeIf { it.isNotBlank() }?.let { o.put("LLM_MODEL", it) }
        // Speech and vision go through yaya.tech with the agent identity
        // (no provider keys on the phone).
        return o
    }

    /** Per-install shared secret between this shell and its core. Random,
     *  never leaves the device; anti-noise on the loopback. */
    fun appKey(ctx: Context): String = secret(ctx.applicationContext, "core_app_key")

    private fun secret(app: Context, key: String): String {
        val sp = Prefs.sp(app)
        SecureStore.getString(sp, key)?.takeIf { it.length >= 32 }?.let { return it }
        val alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
        val rnd = SecureRandom()
        val s = (1..40).map { alphabet[rnd.nextInt(alphabet.length)] }.joinToString("")
        SecureStore.putString(sp, key, s)
        return s
    }

    /** 32 random bytes as hex, minted once and kept Keystore-wrapped. */
    private fun hexSecret(app: Context, key: String): String {
        val sp = Prefs.sp(app)
        SecureStore.getString(sp, key)?.takeIf { it.length == 64 }?.let { return it }
        val bytes = ByteArray(32).also { SecureRandom().nextBytes(it) }
        val s = bytes.joinToString("") { "%02x".format(it) }
        SecureStore.putString(sp, key, s)
        return s
    }

    // ---------------------------------------------------------- schemas

    /** Copies bundled schemas (core.yml + vertical bundles) into filesDir once
     *  per app version, so the core reads plain files and a future bundle
     *  download can overwrite them. */
    private fun installSchemas(app: Context) {
        val dst = File(app.filesDir, "schemas")
        val stamp = File(dst, ".version")
        val want = BuildConfig.VERSION_CODE.toString()
        if (stamp.exists() && stamp.readText() == want) return
        copyAssetDir(app, "schemas", dst)
        stamp.writeText(want)
        Log.i(TAG, "installed schemas v$want into ${dst.absolutePath}")
    }

    private fun copyAssetDir(app: Context, src: String, dst: File) {
        val am = app.assets
        val entries = am.list(src) ?: return
        if (entries.isEmpty()) {
            // A file.
            dst.parentFile?.mkdirs()
            am.open(src).use { i -> dst.outputStream().use { o -> i.copyTo(o) } }
            return
        }
        dst.mkdirs()
        for (e in entries) copyAssetDir(app, "$src/$e", File(dst, e))
    }
}
