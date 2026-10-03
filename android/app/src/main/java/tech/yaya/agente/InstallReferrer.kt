package tech.yaya.agente

import android.content.Context
import android.net.Uri
import android.util.Log
import com.android.installreferrer.api.InstallReferrerClient
import com.android.installreferrer.api.InstallReferrerStateListener
import com.android.installreferrer.api.ReferrerDetails

/**
 * Captures WHERE an install came from — the Play Store "referrer" string
 * riding a link like
 * `market://details?id=yaya.tech.agento&referrer=utm_source%3Dfacebook%26utm_medium%3Dcpc%26utm_campaign%3Dlaunch`.
 * Play stores this on the device at install time; the client library reads it
 * back exactly once (Google's own contract — a second call on the same
 * install returns the same value, but there's no reason to ask twice).
 *
 * Fired from [AgenteApp.onCreate] so the value is in [Prefs] well before
 * RegistrationActivity's `doOnboard()` needs it — no UI waits on this.
 * Direct-channel installs (sideloaded, no Play Store) fail fast with
 * FEATURE_NOT_SUPPORTED; that's not an error, it just means "direct".
 */
object InstallReferrer {
    private const val TAG = "agente.referrer"

    fun fetch(ctx: Context) {
        val app = ctx.applicationContext
        if (Prefs.referrerFetched(app)) return
        val client = InstallReferrerClient.newBuilder(app).build()
        client.startConnection(object : InstallReferrerStateListener {
            override fun onInstallReferrerSetupFinished(responseCode: Int) {
                try {
                    if (responseCode == InstallReferrerClient.InstallReferrerResponse.OK) {
                        val details: ReferrerDetails = client.installReferrer
                        store(app, details.installReferrer)
                    } else {
                        // FEATURE_NOT_SUPPORTED (old Play Store / direct build),
                        // SERVICE_UNAVAILABLE, DEVELOPER_ERROR: nothing to parse.
                        Prefs.setReferrerFetched(app)
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "install referrer read failed", e)
                    Prefs.setReferrerFetched(app)
                } finally {
                    try { client.endConnection() } catch (_: Exception) {}
                }
            }

            override fun onInstallReferrerServiceDisconnected() {
                // Transient (Play services restarted mid-call). Not retried:
                // the next cold start calls fetch() again and Play still has
                // the same answer waiting.
            }
        })
    }

    /** "utm_source=facebook&utm_medium=cpc&utm_campaign=launch" -> parsed + raw, both saved. */
    internal fun store(ctx: Context, raw: String?) {
        val referrer = raw?.takeIf { it.isNotBlank() }
        val uri = referrer?.let { Uri.parse("https://agento.ceo/?$it") }
        Prefs.setInstallReferrer(
            ctx,
            raw = referrer,
            source = uri?.getQueryParameter("utm_source"),
            medium = uri?.getQueryParameter("utm_medium"),
            campaign = uri?.getQueryParameter("utm_campaign"),
        )
    }
}
