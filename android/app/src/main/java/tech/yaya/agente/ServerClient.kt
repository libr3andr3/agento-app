package tech.yaya.agente

import android.content.Context
import android.net.Uri
import android.util.Log
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * Blocking HTTP client for the agent core on the loopback. Call only from [EXECUTOR] — a single
 * thread, so replies to the agent stay in conversation order.
 *
 * Threading contract (matches current call sites — keep it that way):
 *  - [executeAction], [onboardingMessage], [voiceMessage], [catalogPhoto],
 *    [verifyStart], [verifyCheck], [onboardBusiness] → [EXECUTOR] (the
 *    conversation lane; ordering matters).
 *  - [dashboard], [answerGap], [paymentEvent], the queue/media calls → [IO_EXECUTOR]
 *    (must not queue behind a 3-minute LLM turn).
 *  Nothing here may run on the main thread — every method blocks on network.
 */
object ServerClient {
    private const val TAG = "AgenteServer"

    /** Logcat-safe path: first two segments only — deeper ones can carry a
     *  customer's chat key (e.g. /api/conversations/<peer>). */
    private fun redact(path: String): String =
        path.substringBefore('?').split('/').take(3).joinToString("/")

    /** Conversation lane: replies must stay in order, so one thread. */
    val EXECUTOR: ExecutorService = Executors.newSingleThreadExecutor()

    /**
     * Fast lane for payment events, dashboard, and gap answers. Payment
     * notifications MUST NOT queue behind a slow LLM turn: a customer saying
     * "ya pagué" while their Yape waits in line looks like a missing payment.
     */
    val IO_EXECUTOR: ExecutorService = Executors.newCachedThreadPool()

    /** HTTP status + parsed body (null body = network failure or bad JSON). */
    data class Response(val code: Int, val json: JSONObject?, val text: String? = null)

    // ------------------------------------------------------------- taxonomy

    /**
     * What a [Response] means for the user, so callers stop switch-casing raw
     * status ints (and so new callers don't forget that code 0 exists).
     * Additive: every method still returns exactly what it returned before —
     * feed its Response (or bare code) through [classify] when you want words.
     */
    enum class Kind {
        /** 2xx — body (if any) is in [Response.json]. */
        OK,
        /** Code 0: never reached the server. Airplane mode, DNS, timeout. */
        OFFLINE,
        /** 401/403 — token rejected. Re-pair, don't retry. */
        AUTH,
        /** 429 — throttled. Back off, tell the user to wait. */
        RATE_LIMITED,
        /** 503 — feature intentionally off or server redeploying. The verify
         *  flow treats this as "skip the step", not as an error. */
        UNAVAILABLE,
        /** Other 5xx — the server is having a bad day; ours to fix, not the user's. */
        SERVER_DOWN,
        /** Remaining 4xx — the request itself was wrong (bad phone, bad code,
         *  unreadable photo). Retrying the same input won't help. */
        BAD,
    }

    fun classify(code: Int): Kind = when {
        code in 200..299 -> Kind.OK
        code == 0 -> Kind.OFFLINE
        code == 401 || code == 403 -> Kind.AUTH
        code == 429 -> Kind.RATE_LIMITED
        code == 503 -> Kind.UNAVAILABLE
        code in 500..599 -> Kind.SERVER_DOWN
        else -> Kind.BAD
    }

    fun classify(r: Response): Kind = classify(r.code)

    // ------------------------------------------------------------- transport

    /** When a failed exchange may be sent again. Retrying is opt-in per call
     *  because most of our endpoints have side effects (send a WhatsApp
     *  message, register a business, record a payment): the server dedupes
     *  some of those, but we do not rely on it. */
    private enum class Retry {
        /** Side-effectful POSTs: one shot, ever. */
        NEVER,
        /** Idempotent GET: one extra attempt on code 0 or any 5xx. */
        IDEMPOTENT,
        /** verifyStart/verifyCheck: one extra attempt ONLY on code 0 — the
         *  request died before the server saw it, so nothing was sent twice.
         *  A real status (429, 400…) is an answer, not a failure. */
        NETWORK_FAILURE_ONLY,
    }

    private const val RETRY_BACKOFF_MS = 500L

    /** Tests: talk to a fake core instead of booting the native one. */
    @Volatile internal var testBaseUrl: String? = null
    @Volatile internal var testAppKey: String? = null

    /**
     * The one place a socket is opened. Everything public funnels through
     * here so timeouts, headers, and logging cannot drift apart again (they
     * had, four ways).
     *
     * body == null → GET; otherwise POST with [contentType].
     * Note: Android's HttpURLConnection may itself silently re-send a POST on
     * a stale kept-alive connection (sun.net.http.retryPost) — one more reason
     * the server-side dedupe on onboard/payment endpoints must stay.
     */
    private fun exchange(
        ctx: Context,
        path: String,
        body: ByteArray?,
        contentType: String?,
        bearer: Boolean,
        readTimeoutMs: Int,
        retry: Retry,
    ): Response {
        val first = exchangeOnce(ctx, path, body, contentType, bearer, readTimeoutMs)
        val again = when (retry) {
            Retry.NEVER -> false
            Retry.IDEMPOTENT -> first.code == 0 || first.code in 500..599
            Retry.NETWORK_FAILURE_ONLY -> first.code == 0
        }
        if (!again) return first
        try {
            Thread.sleep(RETRY_BACKOFF_MS)
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
            return first
        }
        Log.i(TAG, "retrying ${redact(path)} after ${first.code}")
        return exchangeOnce(ctx, path, body, contentType, bearer, readTimeoutMs)
    }

    private fun exchangeOnce(
        ctx: Context,
        path: String,
        body: ByteArray?,
        contentType: String?,
        bearer: Boolean,
        readTimeoutMs: Int,
    ): Response {
        return try {
            val conn = URL((testBaseUrl ?: AgenteCore.baseUrl(ctx)) + path).openConnection() as HttpURLConnection
            try {
                conn.connectTimeout = 10_000
                conn.readTimeout = readTimeoutMs
                conn.setRequestProperty("X-App-Key", testAppKey ?: AgenteCore.appKey(ctx))
                if (bearer) conn.setRequestProperty(
                    "Authorization", "Bearer " + Prefs.deviceToken(ctx)
                )
                if (body != null) {
                    conn.requestMethod = "POST"
                    conn.doOutput = true
                    contentType?.let { conn.setRequestProperty("Content-Type", it) }
                    conn.outputStream.use { it.write(body) }
                }
                val code = conn.responseCode
                val text = (if (code in 200..299) conn.inputStream else conn.errorStream)
                    ?.bufferedReader()?.readText() ?: ""
                // Body stays out of logcat: error bodies can quote customer content.
                if (code !in 200..299) Log.w(TAG, "${redact(path)} -> $code")
                Response(code, runCatching { JSONObject(text) }.getOrNull(), text)
            } finally {
                conn.disconnect()
            }
        } catch (e: Exception) {
            Log.w(TAG, "${redact(path)} failed", e)
            Response(0, null)
        }
    }

    /** JSON POST, raw response back. LLM tool loops can take a while, hence
     *  the 3-minute read timeout on the conversation endpoints. */
    private fun postRaw(
        ctx: Context,
        path: String,
        body: JSONObject,
        bearer: Boolean,
        retry: Retry = Retry.NEVER,
    ): Response = exchange(
        ctx, path,
        body = body.toString().toByteArray(),
        contentType = "application/json",
        bearer = bearer,
        readTimeoutMs = 180_000,
        retry = retry,
    )

    private fun post(
        ctx: Context,
        path: String,
        body: JSONObject,
        bearer: Boolean,
        retry: Retry = Retry.NEVER,
    ): JSONObject? {
        val r = postRaw(ctx, path, body, bearer, retry)
        // Status only: the body of an error can quote what a customer wrote.
        if (r.code !in 200..299) Log.w(TAG, "post ${redact(path)} -> ${r.code}")
        return if (r.code in 200..299) r.json else null
    }

    // -------------------------------------------------------------- endpoints

    // Credits of the Yaya account — balance, prices, ledger (docs/CREDITS.md).
    // Bearer when a business is paired; the core talks to the gateway with
    // its own identity either way. [IO_EXECUTOR].

    fun credits(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/credits", body = null, contentType = null,
            bearer = Prefs.serverConfigured(ctx), readTimeoutMs = 30_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    /** Opens a top-up: `{url}` for a card checkout (Dodo), or the Yape/Plin
     *  reference in Perú. Side-effectful (a checkout session): never retried. */
    fun topupSession(ctx: Context, amount: Int, method: String): Response =
        postRaw(ctx, "/api/topup/session", JSONObject().put("amount", amount).put("method", method), bearer = Prefs.serverConfigured(ctx))

    /** The money-app catalog the server pushes (`Wallets`). No bearer: fetched at launch. */
    fun wallets(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/wallets", body = null, contentType = null, bearer = false,
            readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    /** Business categories and the prohibited list (`Categories`). No bearer: needed before registration. */
    fun categories(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/categories", body = null, contentType = null, bearer = false,
            readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    // Yaya ID. No device token: the account comes first.
    // [IO_EXECUTOR].

    fun account(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/account", body = null, contentType = null, bearer = false,
            readTimeoutMs = 15_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    /** Sends a one-time code by WhatsApp and email. Retries once only on
     *  code 0 (never left the phone). */
    fun accountOtpStart(ctx: Context, email: String, phone: String, name: String?): Response =
        postRaw(ctx, "/api/account/otp/start",
            JSONObject().put("email", email).put("phone", phone).apply { name?.let { put("name", it) } },
            bearer = false, retry = Retry.NETWORK_FAILURE_ONLY)

    /** Proves the code; creates the account on first sign-in; links this phone. Never retried. */
    fun accountOtpCheck(ctx: Context, email: String, phone: String, code: String, name: String?): Response =
        postRaw(ctx, "/api/account/otp/check",
            JSONObject().put("email", email).put("phone", phone).put("code", code).apply { name?.let { put("name", it) } },
            bearer = false)


    fun accountShare(ctx: Context, share: Boolean): Boolean =
        postRaw(ctx, "/api/account/share", JSONObject().put("share", share), bearer = false).code in 200..299

    fun accountLogout(ctx: Context): Boolean =
        postRaw(ctx, "/api/account/logout", JSONObject(), bearer = false).code in 200..299

    /** Bring the account's latest snapshot onto this phone. Slow: it downloads
     *  and rewrites every table. Side-effectful: never retried. */
    fun restore(ctx: Context, force: Boolean): Response = exchange(
        ctx, "/api/restore",
        body = JSONObject().put("force", force).toString().toByteArray(),
        contentType = "application/json", bearer = false,
        readTimeoutMs = 180_000, retry = Retry.NEVER,
    )

    /** Coarse location → the core (public offer for a business, private
     *  profile for a person). Idempotent. [IO_EXECUTOR]. */
    fun postLocation(ctx: Context, body: JSONObject, bearer: Boolean): Boolean =
        postRaw(ctx, "/api/location", body, bearer = bearer, retry = Retry.IDEMPOTENT).code in 200..299

    /**
     * Customer message in, agent reply out. Null = server unreachable/error.
     * The conversation history lives server-side and is not sent from here —
     * a client-supplied history was a way to put words in the agent's mouth.
     * Side-effectful (sends a reply): never auto-retried. Runs on [EXECUTOR].
     */
    /**
     * A customer turn. [peer] stays the conversation key it has always been —
     * history, the audit chain and D14 billing are all keyed on it, so it is
     * never recomputed. [ref] travels beside it and says *who* this is in
     * channel/handle terms, which is what lets the core recognise the same
     * person across WhatsApp, Instagram and Messenger instead of filing them as
     * three strangers. Older cores ignore the extra fields.
     */
    fun executeAction(
        ctx: Context,
        peer: String,
        message: String,
        ref: SenderRef? = null,
    ): JSONObject? =
        post(
            ctx, "/api/execute_action",
            JSONObject()
                .put("phoneNumber", peer)
                .put("message", message)
                .apply {
                    if (ref != null) {
                        put("channel", ref.channel.id)
                        put("handle", ref.handle)
                        put("displayName", ref.displayName)
                        put("handleIsPhone", ref.isPhone)
                    }
                },
            bearer = true
        )

    /** Onboarding chat turn. Side-effectful: never auto-retried. [EXECUTOR]. */
    fun onboardingMessage(ctx: Context, message: String): JSONObject? =
        post(
            ctx, "/api/onboarding_message",
            JSONObject().put("message", message),
            bearer = true
        )

    /** Voice turn for onboarding: raw m4a bytes up, transcript+reply+wav down.
     *  Side-effectful (a turn in the interview): never auto-retried. [EXECUTOR]. */
    fun voiceMessage(ctx: Context, audio: ByteArray): JSONObject? {
        val r = exchange(
            ctx, "/api/voice_message",
            body = audio,
            contentType = "application/octet-stream",
            bearer = true,
            readTimeoutMs = 180_000,
            retry = Retry.NEVER,
        )
        return if (r.code in 200..299) r.json else null
    }

    /**
     * Catalog/menu photo up (JPEG bytes), extracted items+prices down. The
     * status code carries meaning (422 unreadable photo, 503 feature off,
     * 429 limited), so the raw [Response] is returned instead of a nullable
     * body — mirror of [voiceMessage] otherwise, same timeouts. Never
     * auto-retried (each attempt costs an LLM vision call). [EXECUTOR].
     */
    fun catalogPhoto(ctx: Context, image: ByteArray): Response = exchange(
        ctx, "/api/catalog_photo",
        body = image,
        contentType = "application/octet-stream",
        bearer = true,
        readTimeoutMs = 180_000,
        retry = Retry.NEVER,
    )

    /** Idempotent GET — the one endpoint that auto-retries freely (once,
     *  500ms) on network failure or 5xx. Callers cache the last good payload
     *  ([Prefs.dashboardCache]), so a final null must degrade, not blank the
     *  screen. Runs on [IO_EXECUTOR]. */
    fun dashboard(ctx: Context): JSONObject? {
        val r = exchange(
            ctx, "/api/dashboard",
            body = null,
            contentType = null,
            bearer = true,
            readTimeoutMs = 30_000,
            retry = Retry.IDEMPOTENT,
        )
        return if (r.code in 200..299) r.json else null
    }

    /** Fleet-learned money apps for this business's country. [IO_EXECUTOR]. */
    fun paymentSources(ctx: Context): JSONObject? {
        val r = exchange(
            ctx, "/api/payment_sources",
            body = null, contentType = null, bearer = true,
            readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT,
        )
        return if (r.code in 200..299) r.json else null
    }

    /**
     * Owner answers a question the agent couldn't. The server turns it into a
     * candidate the agent uses in the very next conversation. Side-effectful:
     * never auto-retried. Runs on [IO_EXECUTOR].
     */
    fun answerGap(ctx: Context, gapId: String, answer: String): JSONObject? =
        post(
            ctx, "/api/answer_gap",
            JSONObject().put("gapId", gapId).put("answer", answer),
            bearer = true
        )

    /** Forward a Yape/Plin payment notification for verification/matching.
     *  Side-effectful (marks money as seen): never auto-retried — a double
     *  send could double-confirm a payment. Runs on [IO_EXECUTOR]. */
    fun paymentEvent(ctx: Context, notification: JSONObject): JSONObject? =
        post(ctx, "/api/payment_event", notification, bearer = true)

    // yaya mesh (p2p post-quantum VPN): the core keeps the keys and the
    // peers; the app only drives the tunnel. App key, loopback. [IO_EXECUTOR].

    fun meshStatus(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/mesh", body = null, contentType = null, bearer = false, readTimeoutMs = 30_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun meshRegister(ctx: Context): JSONObject? = post(ctx, "/api/mesh/register", JSONObject(), bearer = false)

    fun meshLink(ctx: Context, peer: String): JSONObject? = post(ctx, "/api/mesh/link", JSONObject().put("peer", peer), bearer = false)

    fun meshAccept(ctx: Context, peer: String): JSONObject? = post(ctx, "/api/mesh/accept", JSONObject().put("peer", peer), bearer = false)

    /** The wg-quick text the core generated (private key inside; stays on the phone). */
    fun meshConfig(ctx: Context): String? {
        val r = exchange(ctx, "/api/mesh/config", body = null, contentType = null, bearer = false, readTimeoutMs = 30_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.text else null
    }

    // CRM. [IO_EXECUTOR].

    /** Upcoming appointments, every status (a bare array). Feeds the OS calendar mirror and .ics export. */
    fun appointments(ctx: Context): org.json.JSONArray? {
        val r = exchange(ctx, "/api/appointments", body = null, contentType = null, bearer = true, readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) runCatching { org.json.JSONArray(r.text ?: "") }.getOrNull() else null
    }

    fun conversations(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/conversations", body = null, contentType = null, bearer = true, readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun conversation(ctx: Context, peer: String): JSONObject? {
        val r = exchange(ctx, "/api/conversations/" + java.net.URLEncoder.encode(peer, "UTF-8"), body = null, contentType = null, bearer = true, readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun contacts(ctx: Context, q: String): JSONObject? {
        val r = exchange(ctx, "/api/contacts" + (if (q.isBlank()) "" else "?q=" + java.net.URLEncoder.encode(q, "UTF-8")), body = null, contentType = null, bearer = true, readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun updateContact(ctx: Context, id: String, patch: JSONObject): JSONObject? = post(ctx, "/api/contacts/$id", patch, bearer = true)

    // Cobros. [IO_EXECUTOR].

    /** Wallet names people use in this country, as suggestions. */
    fun rails(ctx: Context, country: String): JSONObject? {
        val r = exchange(ctx, "/api/rails?country=" + java.net.URLEncoder.encode(country, "UTF-8"),
            body = null, contentType = null, bearer = false, readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun payout(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/payout", body = null, contentType = null, bearer = true, readTimeoutMs = 15_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun setPayout(ctx: Context, body: JSONObject): Response = postRaw(ctx, "/api/payout", body, bearer = true)

    // ------------------------------------------ website + ops integration

    /** Domain, profile-link template, webhook and its secret (owner only). */
    fun ops(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/ops", body = null, contentType = null, bearer = true, readTimeoutMs = 15_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    /** 422 with `error` when the core refuses a field. */
    fun setOps(ctx: Context, body: JSONObject): Response = postRaw(ctx, "/api/ops", body, bearer = true)

    fun opsRotateSecret(ctx: Context): Response = postRaw(ctx, "/api/ops/secret", JSONObject(), bearer = true)

    /** A ping to the owner's webhook: `delivered`, or the `error` their server gave. */
    fun opsTest(ctx: Context): Response = postRaw(ctx, "/api/ops/test", JSONObject(), bearer = true)

    /**
     * Asks the server to WhatsApp a 6-digit code to the owner's phone.
     * The status code is the answer: 200 sent, 503 verification not deployed
     * (register without it), 429 throttled, 400 bad phone, 0 unreachable.
     * Auto-retries once ONLY on code 0 (request never left the device) —
     * a 429/400/503 is an answer and is returned as-is. [EXECUTOR].
     */
    fun verifyStart(ctx: Context, phone: String): Int =
        postRaw(
            ctx, "/api/verify/start", JSONObject().put("phone", phone),
            bearer = false, retry = Retry.NETWORK_FAILURE_ONLY
        ).code

    /** Null unless the code was right; the proof rides in "verificationToken".
     *  Auto-retries once ONLY on code 0 — a wrong code must not be re-tested
     *  (it burns the owner's limited attempts). [EXECUTOR]. */
    fun verifyCheck(ctx: Context, phone: String, code: String): JSONObject? =
        post(
            ctx, "/api/verify/check",
            JSONObject().put("phone", phone).put("code", code),
            bearer = false, retry = Retry.NETWORK_FAILURE_ONLY
        )

    // D15: the owner's work queue, the catalog's photos, the UI spec. [IO_EXECUTOR].

    /** done | undo | cancelled | paid. Side-effectful: never retried. */
    fun orderStatus(ctx: Context, id: String, status: String): JSONObject? =
        post(ctx, "/api/orders/" + Uri.encode(id), JSONObject().put("status", status), bearer = true)

    /** done | undo | cancelled | no_show | paid. Side-effectful: never retried. */
    fun appointmentStatus(ctx: Context, id: String, status: String): JSONObject? =
        post(ctx, "/api/appointments/" + Uri.encode(id), JSONObject().put("status", status), bearer = true)

    /** A resized JPEG up; `{id}` down. One shot (a retry would store it twice). */
    fun mediaUpload(ctx: Context, jpeg: ByteArray, product: String?, caption: String?): Response {
        val q = StringBuilder()
        product?.takeIf { it.isNotBlank() }?.let { q.append("product=").append(Uri.encode(it)) }
        caption?.takeIf { it.isNotBlank() }?.let { if (q.isNotEmpty()) q.append('&'); q.append("caption=").append(Uri.encode(it)) }
        val path = "/api/media" + (if (q.isEmpty()) "" else "?$q")
        return exchange(ctx, path, body = jpeg, contentType = "image/jpeg", bearer = true, readTimeoutMs = 60_000, retry = Retry.NEVER)
    }

    fun mediaUpdate(ctx: Context, id: String, product: String?, caption: String?): JSONObject? =
        post(ctx, "/api/media/" + Uri.encode(id), JSONObject().apply {
            product?.let { put("product", it) }; caption?.let { put("caption", it) }
        }, bearer = true)

    fun mediaDelete(ctx: Context, id: String): Boolean =
        postRaw(ctx, "/api/media/" + Uri.encode(id) + "/delete", JSONObject(), bearer = true).code in 200..299

    /** Mints the private link: `{url, expiresAt, photos}`; 404 = nothing to share, 503 = gateway unreachable. */
    fun mediaShare(ctx: Context, ids: List<String>, products: List<String>, note: String?): Response =
        postRaw(ctx, "/api/media/share", JSONObject()
            .put("ids", org.json.JSONArray(ids))
            .put("products", org.json.JSONArray(products))
            .apply { note?.let { put("note", it) } }, bearer = true)

    // Audit chain. [IO_EXECUTOR].

    fun audit(ctx: Context, before: Long?): JSONObject? {
        val r = exchange(ctx, "/api/audit?limit=100" + (before?.let { "&before=$it" } ?: ""), body = null, contentType = null, bearer = true, readTimeoutMs = 20_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    fun auditVerify(ctx: Context): JSONObject? {
        val r = exchange(ctx, "/api/audit/verify", body = null, contentType = null, bearer = true, readTimeoutMs = 60_000, retry = Retry.IDEMPOTENT)
        return if (r.code in 200..299) r.json else null
    }

    /** Ask the gateway to countersign the chain head now. One shot. */
    fun auditAnchor(ctx: Context): Response = postRaw(ctx, "/api/audit/anchor", JSONObject(), bearer = true)

    /**
     * Photo bytes. The one GET here that is not JSON — same socket rules as
     * [exchangeOnce] (app key, bearer, timeouts), separate because the body
     * is binary. Null on any failure. [IO_EXECUTOR].
     */
    fun mediaBytes(ctx: Context, id: String): ByteArray? {
        return try {
            val conn = URL(AgenteCore.baseUrl(ctx) + "/api/media/" + Uri.encode(id)).openConnection() as HttpURLConnection
            try {
                conn.connectTimeout = 10_000
                conn.readTimeout = 30_000
                conn.setRequestProperty("X-App-Key", AgenteCore.appKey(ctx))
                conn.setRequestProperty("Authorization", "Bearer " + Prefs.deviceToken(ctx))
                if (conn.responseCode !in 200..299) null else conn.inputStream.use { it.readBytes() }
            } finally {
                conn.disconnect()
            }
        } catch (e: Exception) {
            Log.w(TAG, "media $id failed", e)
            null
        }
    }

    /**
     * Registers the business and returns its device token. No admin key: the
     * ops key has no business inside a client binary, and the server no longer
     * accepts it in place of the app key. Side-effectful (creates the
     * business): NEVER auto-retried — the server dedupes by phone today, but
     * a client must not lean on that. [EXECUTOR].
     */
    fun onboardBusiness(
        ctx: Context, name: String, industry: String, ownerPhone: String,
        country: String, verificationToken: String? = null,
        category: String? = null, termsAcceptedAt: String? = null, network: Boolean = true,
        ruc: String? = null,
        // Attribution. The read side already exists (routes/admin.rs reads
        // businesses.referral_source for the dashboard); OnboardReq does not
        // parse these yet, so today they are tolerated and dropped serde-side.
        referralSource: String? = null, referralMedium: String? = null,
        referralCampaign: String? = null, installReferrer: String? = null,
    ): Response =
        postRaw(
            ctx, "/api/onboard_business",
            JSONObject()
                .put("businessName", name)
                .put("industry", industry)
                .put("ownerPhone", ownerPhone)
                .put("country", country)
                // "Participar del internet de los agentes" — the registration toggle
                // (default on). Only the explicit "no" travels: the core defaults
                // networkPublish to true and later changes go through the agent.
                .apply { if (!network) put("network", false) }
                .apply { ruc?.takeIf { it.isNotBlank() }?.let { put("ruc", it) } }
                .apply { verificationToken?.let { put("verificationToken", it) } }
                .apply { category?.let { put("category", it) } }
                // Closed-loop terms accepted on the registration screen (docs/CREDITS.md § 3).
                .apply { termsAcceptedAt?.let { put("termsVersion", Credits.TERMS_VERSION).put("termsAcceptedAt", it) } }
                .apply { referralSource?.let { put("referralSource", it) } }
                .apply { referralMedium?.let { put("referralMedium", it) } }
                .apply { referralCampaign?.let { put("referralCampaign", it) } }
                .apply { installReferrer?.let { put("installReferrer", it) } },
            bearer = false
        )
}
