package app.drift

import android.content.Context
import android.content.Intent
import android.provider.Settings
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathMeasure
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke

/**
 * Blitz (easter egg): seven quick taps on the centre during a transfer draw a lightning bolt once, it
 * flashes, and a Blitz button appears. Blitz frees the radio of whichever side is splitting its airtime
 * between two networks (measured: a phone on its own Wi-Fi drops its hotspot to 2.4 GHz; a PC on its Wi-Fi
 * plus a Wi-Fi Direct link time-shares two channels). Android won't let an app change Wi-Fi, so on the
 * phone Blitz opens Wi-Fi settings and says what to do: on its own hotspot, turn Wi-Fi off; on Wi-Fi
 * Direct, only leave the network (Wi-Fi off would end Wi-Fi Direct too).
 */
object Blitz {
    /** Shares with Blitz unlocked. */
    val unlocked = mutableStateListOf<Long>()
    /** Share whose bolt is playing (once). */
    var playing by mutableStateOf<Long?>(null)
    private val taps = mutableListOf<Long>()

    fun tap(share: Long) {
        val now = System.currentTimeMillis()
        taps.removeAll { now - it > 3_000 }
        taps += now
        if (taps.size >= 7 && share !in unlocked) {
            taps.clear()
            unlocked += share
            playing = share
        }
    }

    /** What Blitz does here, for this transfer's link. */
    fun run(ctx: Context, t: Transfer) {
        val direct = t.medium in setOf("same_hotspot", "direct", "phone_hosted", "hotspot")
        // On a Wi-Fi network as a client (WifiInfo's network id needs location permission; this doesn't).
        // Our hotspot and Wi-Fi Direct group aren't networks of ours, so they don't count.
        val cm = ctx.getSystemService(android.net.ConnectivityManager::class.java)
        @Suppress("DEPRECATION")
        val onWifi = cm?.allNetworks?.any { cm.getNetworkCapabilities(it)?.hasTransport(android.net.NetworkCapabilities.TRANSPORT_WIFI) == true } == true
        when {
            !direct -> Drift.notice = Notice("Blitz needs a direct link", "This transfer runs over ${t.medium ?: "Bluetooth"}; nothing to free up.")
            !onWifi -> Drift.notice = Notice("Already at full speed", "Your phone isn't splitting its radio with another network.")
            // our own hotspot doesn't need Wi-Fi: turning it off frees the radio for the hotspot
            t.medium in setOf("same_hotspot", "hotspot") -> {
                Drift.notice = Notice("Turn Wi-Fi off for this transfer", "Your hotspot keeps running and gets the whole radio (5 GHz). Turn Wi-Fi back on after.", lasting = true)
                runCatching { ctx.startActivity(Intent(Settings.Panel.ACTION_WIFI).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
            }
            // Wi-Fi Direct needs Wi-Fi on: only leave the network (turning Wi-Fi off ends this link)
            else -> {
                Drift.notice = Notice("Disconnect from your Wi-Fi network", "Keep Wi-Fi on: Wi-Fi Direct needs it. Just leave the network for this transfer, so the direct link gets the whole radio.", lasting = true)
                runCatching { ctx.startActivity(Intent(Settings.ACTION_WIFI_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
            }
        }
    }
}

/**
 * The mark: a small bolt right of the wordmark. Its outline draws on, then it fills and flashes, then
 * stays (Blitz is available for this transfer).
 */
@Composable
fun BlitzMark(share: Long, color: Color, modifier: Modifier = Modifier) {
    val draw = remember(share) { Animatable(if (Blitz.playing == share) 0f else 1f) }
    val fill = remember(share) { Animatable(if (Blitz.playing == share) 0f else 1f) }
    val flash = remember(share) { Animatable(0f) }
    LaunchedEffect(share) {
        if (Blitz.playing != share) return@LaunchedEffect
        draw.animateTo(1f, tween(420))
        fill.animateTo(1f, tween(140))
        flash.animateTo(1f, tween(110))
        flash.animateTo(0f, tween(420))
        Blitz.playing = null
    }
    Canvas(modifier) {
        val w = size.width
        val h = size.height
        // a bolt in a 10 x 14 box
        val pts = listOf(6.2f to 0f, 0.6f to 8f, 4.6f to 8f, 3.4f to 14f, 9.4f to 5.4f, 5.4f to 5.4f, 6.2f to 0f)
        val sx = w / 10f
        val sy = h / 14f
        val bolt = Path().apply {
            pts.forEachIndexed { i, (x, y) -> if (i == 0) moveTo(x * sx, y * sy) else lineTo(x * sx, y * sy) }
            close()
        }
        val pm = PathMeasure().apply { setPath(bolt, false) }
        val outline = Path()
        pm.getSegment(0f, pm.length * draw.value, outline, true)
        val k = 1f + 0.3f * flash.value // the flash: one quick swell, nothing else (black and white only)
        val pivot = Offset(w / 2, h / 2)
        drawContext.transform.scale(k, k, pivot)
        drawPath(outline, color, style = Stroke(width = 1.4f * sx, cap = StrokeCap.Round, join = StrokeJoin.Round))
        if (fill.value > 0f) drawPath(bolt, color.copy(alpha = fill.value))
        drawContext.transform.scale(1 / k, 1 / k, pivot)
    }
}
