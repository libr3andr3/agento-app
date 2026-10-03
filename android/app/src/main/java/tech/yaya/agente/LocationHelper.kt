package tech.yaya.agente

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.location.Geocoder
import android.location.Location
import android.location.LocationManager
import android.os.Build
import android.util.Log
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.util.Locale
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Where is this phone? Coarse location → reverse geocode → the agent core
 * (`POST /api/location`). For a business it becomes the public `offer.location`
 * + a 100 m-rounded geo so "near me" works on the network; for a person it
 * stays in the private profile and only rides along with find_businesses.
 *
 * No Play Services: plain LocationManager (network provider first — it is
 * enough for a district) and the platform Geocoder.
 */
object LocationHelper {
    private const val TAG = "LocationHelper"
    const val REQUEST_CODE = 4102
    private const val PREF_ASKED = "location_asked"
    private const val PREF_SENT_AT = "location_sent_at"

    fun granted(ctx: Context): Boolean =
        ContextCompat.checkSelfPermission(ctx, Manifest.permission.ACCESS_COARSE_LOCATION) ==
            PackageManager.PERMISSION_GRANTED

    fun asked(ctx: Context): Boolean = Prefs.sp(ctx).getBoolean(PREF_ASKED, false)

    /** Ask once (the system dialog); the caller's onRequestPermissionsResult → [onGranted]. */
    fun ask(activity: Activity) {
        Prefs.sp(activity).edit().putBoolean(PREF_ASKED, true).apply()
        ActivityCompat.requestPermissions(
            activity, arrayOf(Manifest.permission.ACCESS_COARSE_LOCATION), REQUEST_CODE
        )
    }

    /** Capture + send in the background. Re-sends at most once a day. */
    fun sync(ctx: Context, bearer: Boolean, force: Boolean = false) {
        val app = ctx.applicationContext
        if (!granted(app)) return
        val last = Prefs.sp(app).getLong(PREF_SENT_AT, 0L)
        if (!force && System.currentTimeMillis() - last < 24 * 3600 * 1000L) return
        ServerClient.IO_EXECUTOR.execute {
            try {
                val loc = locate(app) ?: run { Log.i(TAG, "no fix available"); return@execute }
                val body = JSONObject().put("lat", loc.latitude).put("lng", loc.longitude)
                geocode(app, loc)?.let { g ->
                    g.optString("city").takeIf { it.isNotBlank() }?.let { body.put("city", it) }
                    g.optString("district").takeIf { it.isNotBlank() }?.let { body.put("district", it) }
                    g.optString("region").takeIf { it.isNotBlank() }?.let { body.put("region", it) }
                    g.optString("countryCode").takeIf { it.isNotBlank() }?.let { body.put("countryCode", it) }
                }
                val ok = ServerClient.postLocation(app, body, bearer)
                if (ok) Prefs.sp(app).edit().putLong(PREF_SENT_AT, System.currentTimeMillis()).apply()
                // Outcome only: the district/city is a person's whereabouts.
                Log.i(TAG, "location sent=$ok")
            } catch (e: Throwable) {
                Log.w(TAG, "location sync failed: $e")
            }
        }
    }

    @Suppress("MissingPermission")
    private fun locate(ctx: Context): Location? {
        val lm = ctx.getSystemService(Context.LOCATION_SERVICE) as LocationManager
        val providers = listOf(LocationManager.NETWORK_PROVIDER, LocationManager.GPS_PROVIDER, LocationManager.PASSIVE_PROVIDER)
        // Fresh-enough last known fix first (under 30 min).
        val fresh = providers.mapNotNull { p -> runCatching { lm.getLastKnownLocation(p) }.getOrNull() }
            .filter { System.currentTimeMillis() - it.time < 30 * 60 * 1000L }
            .maxByOrNull { it.time }
        if (fresh != null) return fresh
        // One live fix, 20 s budget.
        val provider = providers.firstOrNull { runCatching { lm.isProviderEnabled(it) }.getOrDefault(false) }
            ?: return providers.mapNotNull { runCatching { lm.getLastKnownLocation(it) }.getOrNull() }.maxByOrNull { it.time }
        if (Build.VERSION.SDK_INT >= 30) {
            val latch = CountDownLatch(1)
            var got: Location? = null
            lm.getCurrentLocation(provider, null, ctx.mainExecutor) { got = it; latch.countDown() }
            latch.await(20, TimeUnit.SECONDS)
            return got ?: providers.mapNotNull { runCatching { lm.getLastKnownLocation(it) }.getOrNull() }.maxByOrNull { it.time }
        }
        return providers.mapNotNull { runCatching { lm.getLastKnownLocation(it) }.getOrNull() }.maxByOrNull { it.time }
    }

    private fun geocode(ctx: Context, loc: Location): JSONObject? {
        if (!Geocoder.isPresent()) return null
        val g = Geocoder(ctx, Locale.getDefault())
        val addr = try {
            @Suppress("DEPRECATION")
            g.getFromLocation(loc.latitude, loc.longitude, 1)?.firstOrNull()
        } catch (e: Exception) {
            Log.w(TAG, "geocoder: $e"); null
        } ?: return null
        return JSONObject()
            .put("city", addr.locality ?: addr.subAdminArea ?: "")
            .put("district", addr.subLocality ?: addr.locality ?: "")
            .put("region", addr.adminArea ?: "")
            .put("countryCode", addr.countryCode ?: "")
    }
}
