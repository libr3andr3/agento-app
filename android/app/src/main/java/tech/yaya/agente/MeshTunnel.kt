package tech.yaya.agente

import android.content.Context
import android.content.Intent
import android.util.Log
import com.wireguard.android.backend.GoBackend
import com.wireguard.android.backend.Tunnel
import com.wireguard.config.Config
import java.io.ByteArrayInputStream

/**
 * yaya mesh on the phone: the core mints the keys, does the ML-KEM-768
 * handshake over the relay and writes the wg-quick config; this object turns
 * that text into a VpnService tunnel (wireguard-go, userspace) and keeps it
 * in sync with the core's config version. Nothing here sees a peer's
 * secret: the PreSharedKey comes from the core already derived.
 */
object MeshTunnel {
    private const val TAG = "agente.mesh"
    private const val PREF = "mesh_wanted"
    @Volatile private var backend: GoBackend? = null
    @Volatile private var appliedVersion: Long = -1
    @Volatile private var syncing = false

    private val tunnel = object : Tunnel {
        override fun getName() = "yaya0"
        override fun onStateChange(newState: Tunnel.State) { Log.i(TAG, "tunnel ${newState.name}") }
    }

    fun wanted(ctx: Context): Boolean = ctx.getSharedPreferences("agento", Context.MODE_PRIVATE).getBoolean(PREF, false)

    /** The system's consent intent, or null when already granted. */
    fun prepare(ctx: Context): Intent? = GoBackend.VpnService.prepare(ctx)

    fun enable(ctx: Context, on: Boolean) {
        ctx.getSharedPreferences("agento", Context.MODE_PRIVATE).edit().putBoolean(PREF, on).apply()
        ServerClient.IO_EXECUTOR.execute {
            if (on) {
                if (ServerClient.meshStatus(ctx)?.optString("ip").isNullOrEmpty()) ServerClient.meshRegister(ctx)
                appliedVersion = -1
                sync(ctx)
            } else {
                try { backend?.setState(tunnel, Tunnel.State.DOWN, null) } catch (e: Exception) { Log.w(TAG, "down failed", e) }
                appliedVersion = -1
            }
        }
    }

    /**
     * Re-applies the config when the core's version moved (a new peer, a
     * new endpoint). Cheap to call often: the app calls it after every
     * dashboard refresh and the listener's connect.
     */
    fun sync(ctx: Context) {
        if (!wanted(ctx) || syncing) return
        syncing = true
        try {
            val st = ServerClient.meshStatus(ctx) ?: return
            val v = st.optLong("configVersion", 0)
            if (v == appliedVersion) return
            val text = ServerClient.meshConfig(ctx) ?: return
            if (!text.contains("Address")) { Log.i(TAG, "no mesh address yet"); return }
            val cfg = Config.parse(ByteArrayInputStream(text.toByteArray()))
            val b = backend ?: GoBackend(ctx.applicationContext).also { backend = it }
            b.setState(tunnel, Tunnel.State.UP, cfg)
            appliedVersion = v
            Log.i(TAG, "tunnel applied v$v (${cfg.peers.size} peers, ${st.optString("ip")})")
        } catch (e: Exception) {
            Log.w(TAG, "mesh sync failed", e)
        } finally {
            syncing = false
        }
    }
}
