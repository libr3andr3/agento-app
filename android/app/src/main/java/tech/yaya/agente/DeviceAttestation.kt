package tech.yaya.agente

import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.security.spec.ECGenParameterSpec

/**
 * Measured boot for the agent: a P-256 key generated inside the phone's
 * TEE/StrongBox with an attestation challenge bound to this agent's id. The
 * keystore answers with a certificate chain, signed by the chip and rooted
 * at Google, that states verified-boot status, bootloader lock, patch level
 * and which app asked. The core embeds the chain in the agent's card; the
 * registry (and anyone else) verifies it — no Google services involved.
 *
 * Runs once per agent id (the challenge is fixed at key generation), off
 * the main thread, best-effort: a phone without attestation simply stays
 * "unverified" on the network.
 */
object DeviceAttestation {
    private const val TAG = "DeviceAttestation"
    private const val ALIAS_PREFIX = "agente-device-"
    private const val PREF_DONE = "attestation_done_for"

    /** Call after the core is up; cheap no-op when already done for this id. */
    fun ensure(ctx: Context) {
        val app = ctx.applicationContext
        try {
            val agentId = fetchAgentId(app) ?: return
            if (Prefs.sp(app).getString(PREF_DONE, "") == agentId) return
            val alias = ALIAS_PREFIX + agentId.takeLast(16)
            val level = ensureKey(alias, challengeFor(agentId))
            val chain = chainFor(alias)
            if (chain.size < 2) {
                Log.w(TAG, "keystore returned ${chain.size} certificate(s); no attestation available")
                return
            }
            if (postChain(app, chain, level)) {
                Prefs.sp(app).edit().putString(PREF_DONE, agentId).apply()
                Log.i(TAG, "attestation chain (${chain.size} certs, $level) handed to core")
            }
        } catch (e: Throwable) {
            // Old/odd keystores throw here; that is an unverified phone, not a bug.
            Log.w(TAG, "attestation unavailable: $e")
        }
    }

    /** sha256("agente-attest:v1:" + agentId) — the same formula the registry recomputes. */
    private fun challengeFor(agentId: String): ByteArray =
        MessageDigest.getInstance("SHA-256").digest("agente-attest:v1:$agentId".toByteArray())

    /** Generates the attested key if absent. Returns strongbox | tee. */
    private fun ensureKey(alias: String, challenge: ByteArray): String {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        if (ks.containsAlias(alias)) return "existing"
        fun spec(strongBox: Boolean) = KeyGenParameterSpec.Builder(
            alias, KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY
        )
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256)
            .setAttestationChallenge(challenge)
            .apply { if (strongBox && Build.VERSION.SDK_INT >= 28) setIsStrongBoxBacked(true) }
            .build()
        val kpg = KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, "AndroidKeyStore")
        return try {
            kpg.initialize(spec(true)); kpg.generateKeyPair(); "strongbox"
        } catch (e: Exception) {
            // StrongBoxUnavailableException on most phones in our market; TEE is the bar.
            kpg.initialize(spec(false)); kpg.generateKeyPair(); "tee"
        }
    }

    private fun chainFor(alias: String): List<String> {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val chain = ks.getCertificateChain(alias) ?: return emptyList()
        return chain.map { Base64.encodeToString(it.encoded, Base64.NO_WRAP) }
    }

    // ------------------------------------------------------- core calls

    private fun fetchAgentId(app: Context): String? {
        val conn = URL(AgenteCore.baseUrl(app) + "/api/agent").openConnection() as HttpURLConnection
        return try {
            conn.connectTimeout = 5_000; conn.readTimeout = 10_000
            conn.setRequestProperty("X-App-Key", AgenteCore.appKey(app))
            if (conn.responseCode != 200) return null
            JSONObject(conn.inputStream.bufferedReader().readText()).optString("agent").takeIf { it.startsWith("agent:") }
        } finally { conn.disconnect() }
    }

    private fun postChain(app: Context, chain: List<String>, level: String): Boolean {
        val body = JSONObject().put("chain", JSONArray(chain)).put("level", level).toString()
        val conn = URL(AgenteCore.baseUrl(app) + "/api/agent/device").openConnection() as HttpURLConnection
        return try {
            conn.connectTimeout = 5_000; conn.readTimeout = 60_000
            conn.requestMethod = "POST"; conn.doOutput = true
            conn.setRequestProperty("X-App-Key", AgenteCore.appKey(app))
            conn.setRequestProperty("Content-Type", "application/json")
            conn.outputStream.use { it.write(body.toByteArray()) }
            val ok = conn.responseCode == 200
            if (!ok) Log.w(TAG, "core refused chain: ${conn.responseCode}")
            ok
        } finally { conn.disconnect() }
    }

}
