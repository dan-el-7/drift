package app.drift

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import android.os.SystemClock
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.text.format.Formatter
import kotlin.math.ceil

/**
 * Quick Settings tile: tap to be visible for 10 minutes without opening the app, tap again to hide.
 * [VisibleService] keeps the process alive while visibility is requested from the tile.
 */
class DriftTile : TileService() {
    override fun onStartListening() = update()

    override fun onClick() {
        // The tile's own state, not Drift's: Drift starts (visible) as soon as Android launches our
        // process for this tap, so "visible" alone would read as on and hide it again.
        Drift.init(applicationContext)
        val turnOn = !(VisibleService.tileActive && Drift.visible)
        if (!turnOn) {
            Drift.show(Visibility.Hidden)
            android.widget.Toast.makeText(this, "Drift: hidden", android.widget.Toast.LENGTH_SHORT).show()
        } else {
            if (!VisibleService.enableTile(this)) { update(); return }
            Drift.show(Visibility.TenMinutes)
            android.widget.Toast.makeText(this, "Drift: visible to nearby devices for 10 minutes", android.widget.Toast.LENGTH_SHORT).show()
        }
        // the service starts a moment later: show what was just asked for, not what's running yet
        update(turnOn)
    }

    private fun update(forced: Boolean? = null) {
        val t = qsTile ?: return
        val on = forced ?: (VisibleService.tileActive && Drift.visible)
        t.state = if (on) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        t.subtitle = if (on) {
            val left = ((Drift.visibleUntil - System.currentTimeMillis()) / 1000).coerceAtLeast(0)
            "Visible · %d:%02d".format(left / 60, left % 60)
        } else "Hidden"
        t.updateTile()
    }
}

/** Keeps the shared core alive for tile visibility and for in-flight transfers. */
class VisibleService : Service() {
    private data class Status(
        val title: String,
        val detail: String,
        val progress: Int? = null,
        val busy: Boolean = false,
        val expanded: String? = null,
    )
    private var shown: Status? = null
    private var lastProgressAt = -1_000L
    private val notifications get() = getSystemService(NotificationManager::class.java)

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        starting = false
        active = this
        notifications.createNotificationChannel(NotificationChannel(CHANNEL, "Nearby sharing", NotificationManager.IMPORTANCE_LOW))
        val current = status()
        val initial = current ?: Status("Drift", "Nearby sharing")
        startForeground(ID, notification(initial), ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        shown = initial
        refreshTile()
        if (current == null) update()
        return START_NOT_STICKY
    }

    private fun status(): Status? {
        val transfers = Drift.transfers.filter { it.state == TState.Waiting || it.state == TState.Xfer }
        val transfer = transfers.firstOrNull()
        if (transfer != null) {
            val detail = if (transfers.size > 1) "${transfers.size} active transfers" else transfer.title
            if (transfer.state == TState.Waiting) {
                return Status("Waiting for ${transfer.peer}", detail, busy = true)
            }
            val direction = if (transfer.incoming) "Receiving" else "Sending"
            val peer = if (transfer.incoming) "from ${transfer.peer}" else "to ${transfer.peer}"
            if (transfer.total <= 0) return Status("$direction $peer", detail, busy = true)

            val percent = (transfer.progress * 100).toInt()
            val remaining = (transfer.total - transfer.done).coerceAtLeast(0)
            val rate = transfer.bps.takeIf { it.isFinite() && it > 0 }
            val estimate = if (remaining == 0L) "Finishing…" else rate?.let { timeLeft(remaining, it) }
            val summary = "$percent%" + (estimate?.let { " · $it" } ?: "")
            val speed = rate?.let { "${size(it.toLong())}/s · " }.orEmpty()
            val sizes = "$speed${size(remaining)} left · ${size(transfer.total)} total"
            return Status("$direction ${transfer.title}", summary, percent, busy = true,
                expanded = "$summary\n$sizes\n$direction $peer" +
                    if (transfers.size > 1) " · $detail" else "")
        }
        if (Drift.preparingSend) return Status("Preparing to send", "Drift", busy = true)
        if (Drift.savingFiles > 0) return Status("Saving files", "Drift", busy = true)
        if (tileActive && Drift.visible) return Status("Visible to nearby devices", "Tap the Drift tile to hide.")
        return null
    }

    private fun size(bytes: Long): String = Formatter.formatShortFileSize(this, bytes)

    private fun timeLeft(bytes: Long, bytesPerSecond: Float): String {
        val seconds = ceil(bytes / bytesPerSecond.toDouble()).toLong()
        val duration = when {
            seconds < 60 -> "${seconds}s"
            seconds < 3_600 -> "${seconds / 60}m ${seconds % 60}s"
            else -> "${seconds / 3_600}h ${seconds / 60 % 60}m"
        }
        return "~$duration left"
    }

    private fun notification(s: Status): Notification {
        val intent = Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
        val open = PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE)
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle(s.title)
            .setContentText(s.detail)
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
            .apply { if (s.busy) setCategory(Notification.CATEGORY_PROGRESS).setProgress(100, s.progress ?: 0, s.progress == null) }
            .apply { s.expanded?.let { setStyle(Notification.BigTextStyle().bigText(it)) } }
            .build()
    }

    private fun update(progressOnly: Boolean = false) {
        val now = SystemClock.elapsedRealtime()
        if (progressOnly && now - lastProgressAt < 1_000) return
        val current = status()
        if (current == null) {
            active = null
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        } else if (current != shown) {
            notifications.notify(ID, notification(current))
            shown = current
            if (progressOnly) lastProgressAt = now
        }
    }

    override fun onDestroy() {
        if (active === this) {
            active = null
            tileActive = false
        }
        refreshTile()
        super.onDestroy()
    }

    private fun refreshTile() = TileService.requestListeningState(this, android.content.ComponentName(this, DriftTile::class.java))

    companion object {
        var tileActive = false
            private set
        private var active: VisibleService? = null
        private var starting = false
        val running get() = active != null || starting
        private const val CHANNEL = "visible"
        private const val ID = 7

        fun ensure(ctx: Context): Boolean {
            if (running) return true
            return try {
                ctx.startForegroundService(Intent(ctx, VisibleService::class.java))
                starting = true
                true
            } catch (e: IllegalStateException) {
                android.util.Log.i("Drift", "Foreground service unavailable", e)
                false
            } catch (e: SecurityException) {
                android.util.Log.w("Drift", "Foreground service permission unavailable", e)
                false
            }
        }

        fun enableTile(ctx: Context): Boolean {
            if (!ensure(ctx)) return false
            tileActive = true
            return true
        }

        fun visibilityChanged() {
            if (!Drift.visible) tileActive = false
            active?.let { it.update(); it.refreshTile() }
        }

        fun transferChanged(progressOnly: Boolean = false) { active?.update(progressOnly) }
    }
}
