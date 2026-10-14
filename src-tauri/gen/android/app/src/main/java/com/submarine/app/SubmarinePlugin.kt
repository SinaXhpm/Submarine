package com.submarine.app

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.view.WindowManager
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.IOException

@InvokeArg
class SaveFileArgs {
  lateinit var path: String
  lateinit var name: String
}

@InvokeArg
class SecureScreenArgs {
  var enabled: Boolean = false
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

  companion object {
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
