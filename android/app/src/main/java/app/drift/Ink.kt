package app.drift

import android.graphics.BlendMode
import android.graphics.ColorMatrixColorFilter
import android.graphics.Paint
import android.graphics.RenderEffect
import android.graphics.RenderNode
import android.graphics.Shader
import androidx.compose.foundation.Canvas
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.asComposeRenderEffect
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.platform.LocalDensity
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.ln
import kotlin.math.min
import kotlin.math.pow
import kotlin.math.sin

/** Seconds since the composable appeared, ticking every frame. */
@Composable
fun rememberTime(): State<Float> = produceState(0f) {
    val start = androidx.compose.runtime.withFrameNanos { it }
    while (true) androidx.compose.runtime.withFrameNanos { value = (it - start) / 1e9f }
}

/**
 * Where things sit, in dp, shared by every look and the labels on top: the desktop's StageModel
 * (ui/windows/Stage/StageModel.cs; DIPs there, dp here), so the phone and the PC draw the same scene.
 */
object StageGeo {
    const val R = 84f         // the ring's outer edge at the start
    const val RB = 26f        // the ball it is swallowed into
    const val PEER = 146f     // the other device's drop, above the centre
    const val NODE = 112f     // nearby devices' orbit
    private val ANGLES = floatArrayOf(-90f, 25f, 155f, -25f, 205f)

    /** The self-eating ring: outer edge R -> RB (slow, then the swallow), inner edge -> 0. */
    fun ringGeo(p: Float, w: Float = 1.5f) = Pair(R + (RB - R) * p.pow(1.6f), (R - w) * (1 - p))

    /** Device i's resting place (dp offsets from the centre). */
    fun node(i: Int): Offset {
        val a = ANGLES[i % ANGLES.size] * PI.toFloat() / 180
        return Offset(cos(a) * NODE, sin(a) * NODE)
    }

    /** Real throughput (bytes/s) -> on-screen speed in dp/s (motion.json throughput_visual, log scale). */
    fun speed(bps: Float): Float = 24 + (640 - 24) * min(1f, ln(1 + bps / 1e6f) / ln(1f + 120))
}

/** What the stage shows; every field is animated by the caller (springs), the stage just draws. */
class StageModel(
    val core: Float,        // 0..1 size of the centre drop
    val sonar: Float,       // 0..1 presence ripples (visible to everyone)
    val nodes: List<Float>, // nearby devices, each 0..1 (pinching off the core)
    val peer: Float,        // 0..1 the other device's drop, above the core
    val ring: Float,        // 0..1 the self-eating ring's presence
    val progress: Float,    // 0..1 how much of the ring has been eaten
    val puff: Float,        // 0..1 done: the ball puffs out edge to edge
    val reduced: Boolean,
    val cy: Float = Float.NaN, // the stage's centre (px); NaN = the middle
    val rate: Float = 0f,      // bytes/s right now (drops and particles move at the real speed)
    val idle: Boolean = true,  // home screen: the satellites orbit wide
)

/** Ink's moving parts that live across frames. */
private class InkState {
    val ripples = mutableListOf<Float>()
    var lastRipple = -9f
    val drops = mutableListOf<FloatArray>() // y (dp from the centre), r (dp)
    var last = 0f
    val blobs = RenderNode("ink-blobs")
    val ring = RenderNode("ink-ring")
}

/**
 * Ink: everything is one drop of ink. Shapes are blurred, summed and thresholded (contrast ×12
 * around 50%), so nearby shapes fuse and thin ones evaporate; the ring is blurred less (4 vs 7) so
 * its stroke survives. Same recipe as the prototype and the Windows InkRenderer.
 */
@Composable
fun InkStage(m: StageModel, dark: Boolean, modifier: Modifier = Modifier) {
    val time by rememberTime()
    val density = LocalDensity.current.density
    val threshold = remember(dark) {
        val k = 12f
        val (a, b) = if (dark) k to -(k - 1) / 2 * 255 else -k to (k + 1) / 2 * 255
        val matrix = floatArrayOf(
            a, 0f, 0f, 0f, b,
            0f, a, 0f, 0f, b,
            0f, 0f, a, 0f, b,
            0f, 0f, 0f, 0f, 255f,
        )
        RenderEffect.createColorFilterEffect(ColorMatrixColorFilter(matrix)).asComposeRenderEffect()
    }
    val st = remember { InkState() }
    // The approved look: Gaussian sigma 7 px at 125% (CSS blur()/Win2D) = 5.6 dp; the ring's 4 px = 3.2 dp.
    // Android takes a radius, not a deviation. Matched to Windows by eye and by the threshold maths on a
    // realme GT 7 Pro (Android 17, 2026-11-26): only the biggest satellite (6.5) may break off, as a tiny
    // dot, the others stay fused, and the presence ripples thin out as they spread. 1.73x (Skia's
    // radius-to-sigma formula) kept everything fused with no ripples; 1x let all three fly off.
    remember(density) {
        fun radius(sigmaDp: Float) = sigmaDp * density * 1.58f
        st.blobs.setRenderEffect(RenderEffect.createBlurEffect(radius(5.6f), radius(5.6f), Shader.TileMode.DECAL))
        st.ring.setRenderEffect(RenderEffect.createBlurEffect(radius(3.2f), radius(3.2f), Shader.TileMode.DECAL))
        // the two layers add up before the threshold, as Win2D's CanvasComposite.Add
        st.ring.setUseCompositingLayer(true, Paint().apply { blendMode = BlendMode.PLUS })
        density
    }
    Canvas(modifier.graphicsLayer { renderEffect = threshold; clip = true }) {
        val w = size.width.toInt()
        val h = size.height.toInt()
        if (w <= 0 || h <= 0) return@Canvas
        st.blobs.setPosition(0, 0, w, h)
        st.ring.setPosition(0, 0, w, h)
        val cx = size.width / 2
        val cy = if (m.cy.isNaN()) size.height / 2 else m.cy
        drawBlobs(st, m, time, density, cx, cy, w, h)
        drawRing(st, m, time, density, cx, cy)
        drawIntoCanvas { c ->
            val nc = c.nativeCanvas
            nc.drawColor(android.graphics.Color.BLACK)
            nc.drawRenderNode(st.blobs)
            if (m.ring > 0.02f) nc.drawRenderNode(st.ring)
        }
    }
}

private val white = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = android.graphics.Color.WHITE }
private val whiteStroke = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = android.graphics.Color.WHITE; style = Paint.Style.STROKE }

private fun android.graphics.Canvas.disc(x: Float, y: Float, r: Float) {
    if (r > 0.3f) drawCircle(x, y, r, white)
}

private fun drawBlobs(st: InkState, m: StageModel, t: Float, dp: Float, cx: Float, cy: Float, w: Int, h: Int) {
    val c = st.blobs.beginRecording()
    try {
        // breathing core with lava-lamp satellites (wide orbit at home, tighter elsewhere)
        val breath = if (m.reduced) 1f else 1f + 0.25f * (0.5f - 0.5f * cos(2 * PI.toFloat() * t / 2.4f))
        c.disc(cx, cy, 12 * dp * breath * m.core)
        if (!m.reduced && m.core > 0.02f) for (i in 0 until 3) {
            val a = t * (0.55f + i * 0.23f) + i * 2.1f
            val rr = (20 + 9 * sin(t * 0.8f + i)) * dp * m.core * (if (m.idle) 1f else 0.6f)
            c.disc(cx + cos(a) * rr, cy + sin(a) * rr, (4.5f + i) * dp * m.core)
        }
        // presence ripples: liquid rings that thin until they evaporate
        if (m.sonar > 0.05f && !m.reduced && t - st.lastRipple > 2.8f) { st.ripples += t; st.lastRipple = t }
        st.ripples.removeAll { t - it >= 2.6f || t < it }
        for (t0 in st.ripples) {
            val u = (t - t0) / 2.6f
            val lw = 9 * (1 - u).toDouble().pow(1.3).toFloat() * minOf(1f, m.sonar * 1.4f) * dp
            if (lw > 0.2f) { whiteStroke.strokeWidth = lw; c.drawCircle(cx, cy, (16 + u * 110) * dp, whiteStroke) }
        }
        // nearby devices pinch off the core; small drops have their own goofy life
        fun goofy(x: Float, y: Float, r: Float, ph: Float) {
            if (r < 0.3f) return
            if (m.reduced) { c.disc(x, y, r); return }
            val bob = 2.2f * dp * sin(t * 1.3f + ph)
            c.disc(x + 1.5f * dp * sin(t * 0.9f + ph * 2), y + bob, r * (1 + 0.12f * sin(t * 1.7f + ph)))
            val a = t * (1.1f + 0.15f * (ph % 3)) + ph
            val rr = (12 + 5 * sin(t * 0.9f + ph)) * dp * (r / (11 * dp))
            c.disc(x + cos(a) * rr, y + bob + sin(a) * rr, 4.6f * dp * (r / (11 * dp)))
        }
        m.nodes.forEachIndexed { i, k ->
            val o = StageGeo.node(i)
            goofy(cx + o.x * dp * k, cy + o.y * dp * k, 11 * dp * k, 1.7f + i * 2.3f)
        }
        // the other device
        goofy(cx, cy - StageGeo.PEER * dp, 10 * dp * m.peer, 0.4f)
        // droplets feeding the ring at the real speed
        val dt = (t - st.last).coerceIn(0f, 0.1f)
        st.last = t
        if (m.ring > 0.5f && !m.reduced) {
            val (ro, _) = StageGeo.ringGeo(m.progress.coerceIn(0f, 1f))
            val mbps = m.rate / 1e6f
            if (Math.random().toFloat() < dt * (if (mbps < 1) 1.2f else 9f)) st.drops += floatArrayOf(-StageGeo.PEER + 6, 3 + Math.random().toFloat() * 2)
            val v = StageGeo.speed(m.rate) * dt * 0.75f
            for (d in st.drops) d[0] += v
            st.drops.removeAll { it[0] >= -ro + 4 }
            for (d in st.drops) c.disc(cx + sin(d[0] * 0.05f) * 2 * dp, cy + d[0] * dp, d[1] * dp)
        } else st.drops.clear()
        // done: the ball puffs out edge to edge, with a living edge
        if (m.puff > 0.01f) {
            val far = kotlin.math.hypot(w.toFloat(), h.toFloat()) * 0.68f
            val pr = StageGeo.RB * dp + (far - StageGeo.RB * dp) * m.puff
            val path = android.graphics.Path()
            for (i in 0..120) {
                val a = i / 120f * 2 * PI.toFloat()
                val wob = if (m.reduced) 0f else 7 * dp * sin(6 * a + t * 3) * minOf(1f, 60 * dp / pr)
                val x = cx + cos(a) * (pr + wob)
                val y = cy + sin(a) * (pr + wob)
                if (i == 0) path.moveTo(x, y) else path.lineTo(x, y)
            }
            path.close()
            c.drawPath(path, white)
        }
    } finally {
        st.blobs.endRecording()
    }
}

/** The self-eating ring on its own, less blurred layer: shrinks inward as the file arrives. */
private fun drawRing(st: InkState, m: StageModel, t: Float, dp: Float, cx: Float, cy: Float) {
    val c = st.ring.beginRecording()
    try {
        if (m.ring <= 0.02f) return
        val p = m.progress.coerceIn(0f, 1f)
        val k = m.ring.coerceIn(0f, 1f)
        val (ro, ri) = StageGeo.ringGeo(p)
        val rcen = (ro + ri) / 2
        val s = 0.6f + 0.4f * k // grows in from 0.6, as on the desktop (RingS)
        val path = android.graphics.Path()
        for (i in 0..96) {
            val a = i / 96f * 2 * PI.toFloat()
            val wob = if (m.reduced) 0f else 1.8f * sin(5 * a + t * 2.2f) * (1 - p)
            val x = cx + cos(a) * (rcen + wob) * s * dp
            val y = cy + sin(a) * (rcen + wob) * s * dp
            if (i == 0) path.moveTo(x, y) else path.lineTo(x, y)
        }
        path.close()
        whiteStroke.strokeWidth = maxOf(12f, ro - ri) * k * dp
        c.drawPath(path, whiteStroke)
    } finally {
        st.ring.endRecording()
    }
}
