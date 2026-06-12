package com.submarine.app

import android.os.Bundle

class MainActivity : TauriActivity() {
  // Tauri's scaffold previously called `enableEdgeToEdge()` here, which lets
  // the WebView draw under the system status bar / navigation bar. That made
  // the React title row render behind the clock + signal icons. Removing the
  // call lets Android lay out the WebView inside the system-bar insets the
  // normal way — content starts right below the status bar without any CSS
  // env(safe-area-inset-*) tricks, so xterm's container has identical
  // dimensions to before and FitAddon does not have to re-measure on launch.
  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
  }
}
