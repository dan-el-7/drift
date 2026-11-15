package app.drift

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService

/**
 * Quick Settings tile: tap to be visible for 10 minutes without opening the app, tap again to hide.
 * Nothing runs the rest of the time; while visible, [VisibleService] keeps the process (and so the
 * core's listeners) alive and shows that it's on.
 */
class DriftTile : TileService() {
    override fun onStartListening() = update()

    override fun onClick() {
        // The tile's own state, not Drift's: Drift starts (visible) as soon as Android launches our
        // process for this tap, so "visible" alone would read as on and hide it again.
        Drift.init(applicationContext)
        val turnOn = !(VisibleService.running && Drift.visible)
        if (!turnOn) {
            Drift.show(Visibility.Hidden)
            stopService(Intent(this, VisibleService::class.java))
            android.widget.Toast.makeText(this, "Drift: hidden", android.widget.Toast.LENGTH_SHORT).show()
        } else {
            Drift.show(Visibility.TenMinutes)
            startForegroundService(Intent(this, VisibleService::class.java))
            android.widget.Toast.makeText(this, "Drift: visible to nearby devices for 10 minutes", android.widget.Toast.LENGTH_SHORT).show()
        }
        // the service starts a moment later: show what was just asked for, not what's running yet
        update(turnOn)
    }

    private fun update(forced: Boolean? = null) {
        val t = qsTile ?: return
        val on = forced ?: (VisibleService.running && Drift.visible)
        t.state = if (on) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        t.subtitle = if (on) {
            val left = ((Drift.visibleUntil - System.currentTimeMillis()) / 1000).coerceAtLeast(0)
            "Visible · %d:%02d".format(left / 60, left % 60)
        } else "Hidden"
        t.updateTile()
    }
}

/** Keeps Drift alive while visible from the tile; stops itself when visibility ends. */
class VisibleService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private val check = object : Runnable {
        override fun run() {
            if (!Drift.visible) stopSelf() else main.postDelayed(this, 5_000)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        running = true
        refreshTile()
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Visible to nearby devices", NotificationManager.IMPORTANCE_LOW))
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        val n = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle("Visible to nearby devices")
            .setContentText("For 10 minutes. Tap the Drift tile again to hide.")
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        startForeground(ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        main.removeCallbacks(check)
        main.postDelayed(check, 5_000)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        running = false
        refreshTile() // back to "off" when visibility ends (10 minutes, or hidden in the app)
        main.removeCallbacks(check)
        super.onDestroy()
    }

    private fun refreshTile() = TileService.requestListeningState(this, android.content.ComponentName(this, DriftTile::class.java))

    companion object {
        /** The tile turned visibility on and it's still on. */
        @Volatile var running = false
        private const val CHANNEL = "visible"
        private const val ID = 7
    }
}
