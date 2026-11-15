package app.drift

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.app.Notification

/** System notifications: an incoming share while Drift isn't in front, with Accept / Decline. */
object Notices {
    private const val CHANNEL = "incoming"

    private fun nm(ctx: Context): NotificationManager {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL) == null)
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "Incoming shares", NotificationManager.IMPORTANCE_HIGH))
        return nm
    }

    fun incoming(ctx: Context, t: Transfer) {
        if (DriftApp.inFront) return
        val open = PendingIntent.getActivity(
            ctx, t.share.toInt(), Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        fun act(accept: Boolean) = PendingIntent.getBroadcast(
            ctx, (t.share * 2 + if (accept) 1 else 0).toInt(),
            Intent(ctx, Respond::class.java).putExtra("share", t.share).putExtra("accept", accept),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = Notification.Builder(ctx, CHANNEL)
            .setSmallIcon(R.drawable.ic_drop)
            .setContentTitle("${t.peer} wants to share")
            .setContentText(t.title + (t.pin?.let { " · code $it" } ?: ""))
            .setContentIntent(open)
            .setCategory(Notification.CATEGORY_MESSAGE)
            .setAutoCancel(true)
            .addAction(Notification.Action.Builder(null, "Decline", act(false)).build())
            .addAction(Notification.Action.Builder(null, "Accept", act(true)).build())
            .build()
        runCatching { nm(ctx).notify(t.share.toInt(), n) } // no permission: the app shows it when opened
    }

    fun remove(ctx: Context, share: Long) = runCatching { nm(ctx).cancel(share.toInt()) }

    class Respond : BroadcastReceiver() {
        override fun onReceive(ctx: Context, i: Intent) {
            val share = i.getLongExtra("share", -1)
            val t = Drift.transfers.firstOrNull { it.share == share } ?: return
            if (t.state == TState.Request) Drift.respond(t, i.getBooleanExtra("accept", false))
            remove(ctx, share)
        }
    }
}
