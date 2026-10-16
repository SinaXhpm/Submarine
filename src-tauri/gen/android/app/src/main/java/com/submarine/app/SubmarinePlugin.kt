package com.submarine.app

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.Base64
import android.view.WindowManager
import androidx.activity.result.ActivityResult
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.IOException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
class SaveFileArgs {
  lateinit var path: String
  lateinit var name: String
}

@InvokeArg
class SecureScreenArgs {
  var enabled: Boolean = false
}

@InvokeArg
class QuickUnlockSealArgs {
  lateinit var alias: String
  lateinit var data: String
}

@InvokeArg
class QuickUnlockOpenArgs {
  lateinit var alias: String
  lateinit var sealed: String
  lateinit var iv: String
}

@InvokeArg
class QuickUnlockAliasArgs {
  lateinit var alias: String
}

// The Android pieces the Rust side calls. The page can't call them itself:
// the plugin has no permissions in the app's capabilities, and Tauri refuses
// plugin commands without one.
@TauriPlugin
class SubmarinePlugin(private val activity: Activity) : Plugin(activity) {
  // A file the Rust side copied into cache/export/ for an export, if it is
  // one: anything else is refused, so this can't hand out other app files.
  private fun stagedExport(path: String): File? {
    val file = File(path).canonicalFile
    val dir = File(activity.cacheDir, "export").canonicalFile
    return if (file.parentFile == dir && file.isFile) file else null
  }

  // Export through the system's "Save to" picker (Storage Access Framework):
  // the user picks the place, the app can write to that one document only,
  // and nothing is left in a shared folder unless the user put it there.
  @Command
  fun saveFile(invoke: Invoke) {
    val args = invoke.parseArgs(SaveFileArgs::class.java)
    if (stagedExport(args.path) == null) {
      invoke.reject("Not a staged export")
      return
    }
    val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
      addCategory(Intent.CATEGORY_OPENABLE)
      type = "application/octet-stream"
      putExtra(Intent.EXTRA_TITLE, args.name)
    }
    startActivityForResult(invoke, intent, "saveFileResult")
  }

  @ActivityCallback
  fun saveFileResult(invoke: Invoke, result: ActivityResult) {
    val uri = result.data?.data
    val staged = stagedExport(invoke.parseArgs(SaveFileArgs::class.java).path)
    if (result.resultCode != Activity.RESULT_OK || uri == null || staged == null) {
      invoke.resolve(JSObject().apply { put("saved", false) })
      return
    }
    // Off the main thread: a profile can be a few MB, and the place may be a
    // cloud provider.
    Thread {
      try {
        val out = activity.contentResolver.openOutputStream(uri, "w")
          ?: throw IOException("the chosen place can't be written to")
        out.use { o -> staged.inputStream().use { it.copyTo(o) } }
        invoke.resolve(JSObject().apply {
          put("saved", true)
          put("name", displayName(uri) ?: staged.name)
        })
      } catch (e: Exception) {
        // Don't leave a cut-off copy behind that looks like a good one.
        try { DocumentsContract.deleteDocument(activity.contentResolver, uri) } catch (_: Exception) {}
        invoke.reject("Couldn't write the export: ${e.message}")
      }
    }.start()
  }

  private fun displayName(uri: Uri): String? =
    try {
      activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
        if (c.moveToFirst()) c.getString(0) else null
      }
    } catch (_: Exception) {
      null
    }

  @Command
  fun getSecureScreen(invoke: Invoke) {
    invoke.resolve(JSObject().apply { put("enabled", isSecureScreen(activity)) })
  }

  // "Block screenshots": FLAG_SECURE keeps the window out of screenshots,
  // screen recording, casting and the Recents thumbnail. Kept natively so
  // MainActivity can apply it before the first frame on the next start.
  @Command
  fun setSecureScreen(invoke: Invoke) {
    val enabled = invoke.parseArgs(SecureScreenArgs::class.java).enabled
    prefs(activity).edit().putBoolean(KEY_SECURE_SCREEN, enabled).apply()
    activity.runOnUiThread {
      applySecureScreen(activity, enabled)
      invoke.resolve(JSObject().apply { put("enabled", enabled) })
    }
  }

  // Quick unlock (#166, quick_unlock.rs): the profile's vault key, sealed by
  // an AES key in the Android Keystore that works only right after a strong
  // biometric check, and is dropped when a fingerprint is added. The vault
  // key comes from and goes back to the Rust side only.
  @Command
  fun quickUnlockReady(invoke: Invoke) {
    val ready = BiometricManager.from(activity)
      .canAuthenticate(BiometricManager.Authenticators.BIOMETRIC_STRONG) == BiometricManager.BIOMETRIC_SUCCESS
    invoke.resolve(JSObject().apply { put("ready", ready) })
  }

  @Command
  fun quickUnlockSeal(invoke: Invoke) {
    val args = invoke.parseArgs(QuickUnlockSealArgs::class.java)
    if (!QUICK_UNLOCK_ALIAS.matches(args.alias)) {
      invoke.reject("BAD_ALIAS")
      return
    }
    val cipher = try {
      Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, newQuickUnlockKey(args.alias)) }
    } catch (e: Exception) {
      invoke.reject("KEYSTORE: ${e.message}")
      return
    }
    authenticate(invoke, cipher, "Turn on fingerprint unlock") { c ->
      val data = Base64.decode(args.data, Base64.NO_WRAP)
      try {
        val sealed = c.doFinal(data)
        JSObject().apply {
          put("sealed", Base64.encodeToString(sealed, Base64.NO_WRAP))
          put("iv", Base64.encodeToString(c.iv, Base64.NO_WRAP))
        }
      } finally {
        data.fill(0)
      }
    }
  }

  @Command
  fun quickUnlockOpen(invoke: Invoke) {
    val args = invoke.parseArgs(QuickUnlockOpenArgs::class.java)
    if (!QUICK_UNLOCK_ALIAS.matches(args.alias)) {
      invoke.reject("BAD_ALIAS")
      return
    }
    val key = try { keyStore().getKey(args.alias, null) as? SecretKey } catch (_: Exception) { null }
    if (key == null) {
      invoke.reject("KEY_GONE")
      return
    }
    val cipher = try {
      Cipher.getInstance("AES/GCM/NoPadding").apply {
        init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, Base64.decode(args.iv, Base64.NO_WRAP)))
      }
    } catch (_: KeyPermanentlyInvalidatedException) {
      // A fingerprint was added (or all removed): the key is gone for good.
      deleteQuickUnlockKey(args.alias)
      invoke.reject("KEY_GONE")
      return
    } catch (e: Exception) {
      invoke.reject("KEYSTORE: ${e.message}")
      return
    }
    authenticate(invoke, cipher, "Unlock the profile") { c ->
      val data = c.doFinal(Base64.decode(args.sealed, Base64.NO_WRAP))
      try {
        JSObject().apply { put("data", Base64.encodeToString(data, Base64.NO_WRAP)) }
      } finally {
        data.fill(0)
      }
    }
  }

  @Command
  fun quickUnlockForget(invoke: Invoke) {
    val args = invoke.parseArgs(QuickUnlockAliasArgs::class.java)
    if (QUICK_UNLOCK_ALIAS.matches(args.alias)) deleteQuickUnlockKey(args.alias)
    invoke.resolve(JSObject())
  }

  // BiometricPrompt with the cipher as its CryptoObject: the Keystore lets
  // the cipher work only once the finger was recognised. "Use password"
  // (or Back) is a cancel; the Rust side then asks for the password.
  private fun authenticate(invoke: Invoke, cipher: Cipher, title: String, onOk: (Cipher) -> JSObject) {
    val host = activity as? FragmentActivity
    if (host == null) {
      invoke.reject("NO_ACTIVITY")
      return
    }
    activity.runOnUiThread {
      val prompt = BiometricPrompt(host, ContextCompat.getMainExecutor(activity), object : BiometricPrompt.AuthenticationCallback() {
        override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
          val c = result.cryptoObject?.cipher
          if (c == null) {
            invoke.reject("NO_CIPHER")
            return
          }
          try {
            invoke.resolve(onOk(c))
          } catch (e: Exception) {
            invoke.reject("CRYPTO: ${e.message}")
          }
        }

        override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
          val cancelled = errorCode == BiometricPrompt.ERROR_USER_CANCELED ||
            errorCode == BiometricPrompt.ERROR_NEGATIVE_BUTTON ||
            errorCode == BiometricPrompt.ERROR_CANCELED
          invoke.reject(if (cancelled) "CANCELLED" else "BIOMETRIC: $errString")
        }
      })
      val info = BiometricPrompt.PromptInfo.Builder()
        .setTitle(title)
        .setSubtitle("Submarine")
        .setNegativeButtonText("Use password")
        .setAllowedAuthenticators(BiometricManager.Authenticators.BIOMETRIC_STRONG)
        .build()
      prompt.authenticate(info, BiometricPrompt.CryptoObject(cipher))
    }
  }

  private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

  private fun newQuickUnlockKey(alias: String): SecretKey {
    val spec = KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
      .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
      .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
      .setKeySize(256)
      .setUserAuthenticationRequired(true)
      .setInvalidatedByBiometricEnrollment(true)
      .apply {
        // Every use needs a strong biometric (no time window, no device PIN).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
          setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
        }
      }
      .build()
    val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    gen.init(spec)
    return gen.generateKey()
  }

  private fun deleteQuickUnlockKey(alias: String) {
    try { keyStore().deleteEntry(alias) } catch (_: Exception) {}
  }

  companion object {
    // The names quick_unlock.rs gives its keys; nothing else is touched.
    private val QUICK_UNLOCK_ALIAS = Regex("^submarine-quick-unlock-[0-9a-f]{32}$")
    private const val PREFS = "submarine"
    private const val KEY_SECURE_SCREEN = "secure_screen"

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun isSecureScreen(context: Context): Boolean = prefs(context).getBoolean(KEY_SECURE_SCREEN, false)

    fun applySecureScreen(activity: Activity, enabled: Boolean) {
      if (enabled) {
        activity.window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
      } else {
        activity.window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
      }
    }
  }
}
