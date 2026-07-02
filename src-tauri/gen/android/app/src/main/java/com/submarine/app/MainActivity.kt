package com.submarine.app

import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.view.View
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  // Android 15+ (API 35) forces edge-to-edge — `setDecorFitsSystemWindows`
  // and the deprecated translucent-status-bar flags are ignored when
  // targetSdk >= 35. Our compileSdk/targetSdk are both 36, so the WebView
  // is laid out behind the status bar and our React tab strip overlaps
  // the system clock.
  //
  // Fix: attach an OnApplyWindowInsetsListener to android.R.id.content and
  // pass system-bar + cutout + IME insets in as padding. The WebView then
  // mounts inside that padded region — tabs sit right below the status
  // bar, and when the soft keyboard opens the bottom padding grows by the
  // IME height so the terminal + MobileKeyBar stay visible above it.
  //
  // The IME inset is required because Android 15 edge-to-edge decouples
  // the WebView from the Activity's adjustResize behaviour: the manifest
  // still has adjustResize + stateHidden, but with
  // setDecorFitsSystemWindows(false) the WebView receives the IME as an
  // inset instead of a window resize, so we have to apply it manually or
  // the keyboard draws over the content.
  //
  // The status-bar + nav-bar colour setters are deprecated in API 35 but
  // still honoured at runtime on this device. They paint the bar
  // backgrounds with #0d0d10 so the OS chrome blends into our titlebar.
  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)

    // Keep the OS from killing our process while the user is in another
    // app. All SSH state lives in the Rust backend in this same process,
    // so a foreground service with a persistent notification is the only
    // sanctioned way to keep sockets open across a background trip.
    // ContextCompat handles the pre-O / O+ startForegroundService split.
    val svc = Intent(this, ConnectionKeeperService::class.java)
    ContextCompat.startForegroundService(this, svc)

    window.statusBarColor = Color.parseColor("#0d0d10")
    window.navigationBarColor = Color.parseColor("#0d0d10")
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { v, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars()
          or WindowInsetsCompat.Type.displayCutout()
      )
      val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
      v.setPadding(
        bars.left,
        bars.top,
        bars.right,
        maxOf(bars.bottom, ime.bottom)
      )
      WindowInsetsCompat.CONSUMED
    }
  }

  // Explicitly stop the keeper when the Activity finishes. Android will
  // also tear the service down when the process dies (e.g. user swipes
  // the app away), but stopping it on the ordinary destroy path removes
  // the persistent notification promptly instead of waiting for the next
  // system sweep.
  override fun onDestroy() {
    stopService(Intent(this, ConnectionKeeperService::class.java))
    super.onDestroy()
  }
}
