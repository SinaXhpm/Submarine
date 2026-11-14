package com.submarine.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

// Foreground service whose only job is to hold the process alive while the
// user is switched to another app. All connection work lives in the Rust
// backend (russh, telnet, RDP, VNC and FTP tasks spawned onto tokio); Android
// does not know about those tasks, so if it kills our process for memory
// pressure the sockets go with it and the session reconnects on next open.
// A foreground service with a visible notification is the ONE supported way
// to tell Android "please don't kill me, I have user-facing work to do".
//
// It runs only while a connection is open, which the Rust side reports
// (android_bridge::watch_connections -> SubmarinePlugin.setKeepAlive):
//   a connection opens       -> setWanted(true)  -> startForegroundService
//   the last one closes      -> setWanted(false) -> stopService
//   MainActivity.onStart     -> resume()         -> start again if wanted
//   MainActivity.onDestroy   -> stopService
// Google Play allows a foreground service only for work the user started and
// still has going, so an app that is merely open shows no notification.
// The type is specialUse (see the manifest's subtype): an interactive
// connection held open fits none of the fixed types, and dataSync (transfers
// that finish) is limited to 6 hours a day on Android 15+. The user swiping
// the app away from Recents kills the process, which is the intended exit.
class ConnectionKeeperService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val channelId = ensureNotificationChannel()

        // Tap notification -> bring app to foreground. Uses launchMode
        // singleTask (see manifest) so tapping just resumes the existing
        // Activity instead of spawning a duplicate.
        val launchIntent = packageManager.getLaunchIntentForPackage(packageName)
        val contentIntent = if (launchIntent != null) {
            PendingIntent.getActivity(
                this,
                0,
                launchIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        } else null

        val notification: Notification = NotificationCompat.Builder(this, channelId)
            .setContentTitle("Submarine")
            .setContentText("Keeping your connections open")
            .setSmallIcon(R.mipmap.ic_launcher)
            .setOngoing(true)
            .setContentIntent(contentIntent)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .build()

        // Android 14+ wants the type at runtime, matching the manifest's.
        // Before 14 there is no specialUse constant: the two-argument form
        // takes the manifest's type.
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
                )
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: RuntimeException) {
            // Refused (the app went to the background in between): stop rather
            // than let Android end the app for a service that never came to
            // the foreground. resume() starts it when the app is back.
            stopSelf()
            return START_NOT_STICKY
        }

        // Do NOT restart if the system kills us: the Activity restart path
        // re-arms the service. Auto-restart with no Activity would leave
        // a phantom notification with no way for the user to interact.
        return START_NOT_STICKY
    }

    // Android calls this when a type's time limit is reached (dataSync has
    // one on Android 15+; specialUse has none today). A service still running
    // a few seconds later crashes the whole app, sessions and all, so stop.
    override fun onTimeout(startId: Int, fgsType: Int) {
        stopSelf()
    }

    private fun ensureNotificationChannel(): String {
        val channelId = "submarine_background"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            if (nm.getNotificationChannel(channelId) == null) {
                val channel = NotificationChannel(
                    channelId,
                    "Background sessions",
                    NotificationManager.IMPORTANCE_LOW,
                )
                channel.description =
                    "Keeps your connections open while the app is in the background."
                channel.setShowBadge(false)
                nm.createNotificationChannel(channel)
            }
        }
        return channelId
    }

    companion object {
        private const val NOTIFICATION_ID = 1042

        // Whether a connection is open, as the Rust side last said. Set on the
        // plugin's thread, read on the main thread.
        @Volatile
        private var wanted = false

        /** From SubmarinePlugin.setKeepAlive: a connection is open, or none is. */
        fun setWanted(context: Context, open: Boolean) {
            wanted = open
            if (open) {
                start(context)
            } else {
                context.stopService(Intent(context, ConnectionKeeperService::class.java))
            }
        }

        /** From MainActivity.onStart: start again if a connection is open and
         *  Android stopped the service, or refused to start it while the app
         *  was in the background. A start while it runs only repeats its
         *  notification. */
        fun resume(context: Context) {
            if (wanted) start(context)
        }

        private fun start(context: Context) {
            try {
                ContextCompat.startForegroundService(
                    context,
                    Intent(context, ConnectionKeeperService::class.java),
                )
            } catch (e: IllegalStateException) {
                // Android 12+ refuses a start while the app is in the
                // background (ForegroundServiceStartNotAllowedException).
                // resume() tries again when the app comes to the front.
            }
        }
    }
}
