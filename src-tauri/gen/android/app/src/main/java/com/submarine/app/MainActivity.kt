package com.submarine.app

import android.graphics.Color
import android.os.Bundle
import android.view.View
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  // Android 15+ (API 35) forces edge-to-edge — `setDecorFitsSystemWindows`
  // and the deprecated translucent-status-bar flags are ignored when
  // targetSdk >= 35. Our compileSdk/targetSdk are both 36, so the WebView
  // is laid out behind the status bar and our React tab strip overlaps
  // the system clock.
  //
  // The supported fix is to attach an OnApplyWindowInsetsListener to the
  // activity's android.R.id.content view and pass the system-bar / cutout
  // insets in as padding. The WebView then mounts inside that padded
  // region — tabs sit right below the status bar with no CSS env() tricks
  // and the terminal container keeps its v0.2.15 dimensions.
  //
  // The status-bar + nav-bar colour setters are deprecated in API 35 but
  // still honoured at runtime on this device. They paint the bar
  // backgrounds with #0d0d10 so the OS chrome blends into our titlebar.
  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    window.statusBarColor = Color.parseColor("#0d0d10")
    window.navigationBarColor = Color.parseColor("#0d0d10")
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { v, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars()
          or WindowInsetsCompat.Type.displayCutout()
      )
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }
}
