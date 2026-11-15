package app.drift

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Drift's device key (trust between Drift devices; core/src/trust.rs). The P-256 secret is kept
 * in files/device.key, encrypted by an AES key that never leaves the Android Keystore, so the
 * secret is only readable by this app on this phone. If it can't be read it's replaced (trusted
 * PCs then ask once more).
 */
object DeviceKey {
    private const val ALIAS = "drift-device"

    private fun wrapKey(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    /** The secret as hex, for the core's `device_secret`; null if the Keystore is unusable. */
    fun secretHex(ctx: Context): String? = runCatching {
        val key = wrapKey()
        val file = File(ctx.filesDir, "device.key")
        if (file.exists()) {
            runCatching {
                val b = file.readBytes()
                val c = Cipher.getInstance("AES/GCM/NoPadding")
                c.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, b, 0, 12))
                return@runCatching hex(c.doFinal(b, 12, b.size - 12))
            }.getOrNull()?.let { return it }
        }
        val secret = ByteArray(32).also { SecureRandom().nextBytes(it) }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key)
        file.writeBytes(c.iv + c.doFinal(secret))
        hex(secret)
    }.getOrNull()

    private fun hex(b: ByteArray) = b.joinToString("") { "%02x".format(it) }
}
