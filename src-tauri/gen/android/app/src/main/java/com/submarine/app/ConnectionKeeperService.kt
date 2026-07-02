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

// Foreground service whose only job is to hold the process alive while the
// user is switched to another app. All SSH/SFTP work lives in the Rust
// backend (russh + russh-sftp tasks spawned onto tokio); Android does not
// know or care about those tasks, so if it kills our process for memory
// pressure the sockets go with it and the terminal reconnects on next open.
// A foreground service with a visible notification is the ONE supported way
// to tell Android "please don't kill me, I have user-facing work to do".
//
// We deliberately do NOT observe SSH state from here — that would require an
// IPC bridge from Rust and buy us little (the notification is low priority
// and non-dismissible either way). Instead the lifecycle is:
//   MainActivity.onCreate  -> startForegroundService(this)  -> onStartCommand
//   MainActivity.onDestroy -> stopSelf()                    -> onDestroy
// so the service exists exactly while the Activity does. The user swiping the
// app away from Recents kills the process, which is the intended exit path.
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
            .setContentText("Keeping your SSH sessions alive")
            .setSmallIcon(R.mipmap.ic_launcher)
            .setOngoing(true)
            .setContentIntent(contentIntent)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .build()

        // Android 10+ requires the foregroundServiceType at runtime to match
        // the one declared in the manifest; older APIs accept the two-arg form.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            @Suppress("DEPRECATION")
            startForeground(NOTIFICATION_ID, notification)
        }

        // Do NOT restart if the system kills us — the Activity restart path
        // will re-arm the service. Auto-restart with no Activity would leave
        // a phantom notification with no way for the user to interact.
        return START_NOT_STICKY
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
                    "Keeps SSH and SFTP connections alive while the app is in the background."
                channel.setShowBadge(false)
                nm.createNotificationChannel(channel)
            }
        }
        return channelId
    }

    companion object {
        private const val NOTIFICATION_ID = 1042
    }
}
