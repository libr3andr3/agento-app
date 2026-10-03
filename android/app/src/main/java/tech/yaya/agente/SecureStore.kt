package tech.yaya.agente

import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Encrypts individual preference values under a key held in the Android
 * Keystore, so the key material never enters the app's process memory and never
 * lands in a backup or a file copy.
 *
 * This exists for the device bearer token: it authenticates every call the app
 * makes on the business's behalf, it does not expire, and it used to sit in
 * plaintext in SharedPreferences. `allowBackup="false"` closes the realistic
 * exfiltration route; this closes the rooted- or stolen-device one too.
 *
 * Hand-rolled AES/GCM rather than `androidx.security:security-crypto`, which is
 * deprecated and would add a dependency for one string. GCM is authenticated,
 * so tampering surfaces as a decryption failure rather than as corrupt data.
 */
object SecureStore {

    private const val TAG = "AgenteSecureStore"
    private const val KEY_ALIAS = "agente_prefs_key"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val IV_BYTES = 12
    private const val TAG_BITS = 128

    private fun secretKey(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        try {
            (ks.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        } catch (e: Exception) {
            // UnrecoverableKeyException et al: some OEM keystores corrupt or
            // invalidate entries (lock-screen change, OS update). If the entry
            // can't even be READ, it can never decrypt anything again — so
            // drop it and regenerate rather than throw forever, which would
            // also block STORING a new token and brick re-pairing. Old
            // ciphertext then decrypts to null → re-pair, the documented
            // failure mode.
            Log.w(TAG, "keystore entry unreadable, regenerating", e)
            runCatching { ks.deleteEntry(KEY_ALIAS) }
        }

        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                // No setUserAuthenticationRequired: the notification listener
                // runs in the background and must reach the token without the
                // owner unlocking the phone first.
                .build()
        )
        return gen.generateKey()
    }

    /** Stored as base64(iv):base64(ciphertext) — GCM needs a fresh IV per write. */
    private fun encrypt(plain: String): String {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, secretKey())
        val ct = cipher.doFinal(plain.toByteArray(Charsets.UTF_8))
        val iv = Base64.encodeToString(cipher.iv, Base64.NO_WRAP)
        return "$iv:${Base64.encodeToString(ct, Base64.NO_WRAP)}"
    }

    private fun decrypt(stored: String): String? {
        return try {
            val (ivB64, ctB64) = stored.split(":", limit = 2).let { it[0] to it[1] }
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(
                Cipher.DECRYPT_MODE,
                secretKey(),
                GCMParameterSpec(TAG_BITS, Base64.decode(ivB64, Base64.NO_WRAP))
            )
            String(cipher.doFinal(Base64.decode(ctB64, Base64.NO_WRAP)), Charsets.UTF_8)
        } catch (e: Exception) {
            // Keystore keys are dropped when the user changes their lock screen
            // in some OEM builds, and on restore-to-a-new-device. Losing the
            // token means re-pairing, which is the right failure: never fall
            // back to reading something unencrypted.
            Log.w(TAG, "could not decrypt stored value", e)
            null
        }
    }

    fun getString(sp: SharedPreferences, key: String): String? =
        sp.getString(key, null)?.let { decrypt(it) }

    fun putString(sp: SharedPreferences, key: String, value: String) {
        try {
            sp.edit().putString(key, encrypt(value)).apply()
        } catch (e: Exception) {
            Log.e(TAG, "could not encrypt value for $key", e)
        }
    }

    /**
     * Moves a value written by an older build out of plaintext. Runs once per
     * install: after this the legacy key is gone, so existing users keep their
     * pairing instead of being logged out by the upgrade.
     */
    fun migratePlaintext(sp: SharedPreferences, legacyKey: String, secureKey: String) {
        val legacy = sp.getString(legacyKey, null) ?: return
        if (legacy.isNotEmpty()) putString(sp, secureKey, legacy)
        sp.edit().remove(legacyKey).apply()
        Log.i(TAG, "migrated $legacyKey to encrypted storage")
    }
}
