package app.drift

import androidx.compose.foundation.Canvas
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalDensity
import kotlin.math.PI
import kotlin.math.abs
import kotlin.math.cos
import kotlin.math.exp
import kotlin.math.hypot
import kotlin.math.ln
import kotlin.math.min
import kotlin.math.pow
import kotlin.math.roundToInt
import kotlin.math.sin
import kotlin.math.sqrt

/**
 * The optional looks, ported from the Windows renderers (ui/windows/Stage/Renderers.cs) onto the
 * phone's stage geometry (Ink's: ring 46 dp, peer 110 dp above the core, devices fanned at 96 dp),
 * so switching looks keeps everything in the same place. They draw in the *base* palette; the UI on
 * top inverts with the done flood as it does for Ink.
 */
enum class Look { Ink, Monolith, Field }

private fun dpPerSec(bps: Float) = StageGeo.speed(bps)
private const val R = StageGeo.R
private const val RB = StageGeo.RB
private fun ringGeo(p: Float, w: Float = 1.5f) = StageGeo.ringGeo(p, w)

private fun Color.a(a: Float) = copy(alpha = a.coerceIn(0f, 1f))

/** Positions shared by both looks, in px. */
private class Geo(val cx: Float, val cy: Float, val dp: Float, @Suppress("UNUSED_PARAMETER") n: Int, ks: List<Float>) {
    val peerY = cy - StageGeo.PEER * dp
    val nodes = ks.mapIndexed { i, k ->
        val o = StageGeo.node(i)
        Triple(cx + o.x * dp * k, cy + o.y * dp * k, k)
    }
}

// ------------------------------------------------------------------ Monolith

private class Particles { val ys = mutableListOf<Float>(); var last = 0f }

/** Monolith: precise and typographic; hairlines, a tick dial, particles on a line. */
@Composable
fun MonolithStage(m: StageModel, pal: Palette, modifier: Modifier = Modifier) {
    val time by rememberTime()
    val dp = LocalDensity.current.density
    val parts = remember { Particles() }
    Canvas(modifier) { drawMonolith(m, pal, time, dp, parts) }
}

private fun DrawScope.drawMonolith(m: StageModel, c: Palette, t: Float, dp: Float, parts: Particles) {
    drawRect(c.bg)
    val g = Geo(size.width / 2, if (m.cy.isNaN()) size.height / 2 else m.cy, dp, m.nodes.size, m.nodes)
    val cx = g.cx; val cy = g.cy
    val hair = 1f * dp
    if (m.sonar > 0.01f && !m.reduced) for (j in 0 until 3) {
        val u = (t / 2.8f + j / 3f) % 1f
        drawCircle(c.g1.a(0.5f * (1 - u).pow(1.6f) * m.sonar), (8 + u * 116) * dp, Offset(cx, cy), style = Stroke(hair))
    }
    for ((x0, y0, k) in g.nodes) {
        if (k < 0.01f) continue
        drawLine(c.g2.a(k), Offset(cx, cy), Offset(x0, y0), hair)
        drawCircle(c.fg.a(k), 4.5f * dp, Offset(x0, y0))
    }
    val dt = (t - parts.last).coerceIn(0f, 0.1f); parts.last = t
    if (m.peer > 0.01f) {
        val (ro, _) = ringGeo(m.progress)
        val ringOn = m.ring > 0.5f
        val end = if (ringOn) cy - (ro + 22 * m.ring) * dp else cy - 12 * dp
        val a = m.peer.coerceIn(0f, 1f)
        drawLine(c.g2.a(a), Offset(cx, g.peerY + 7 * dp), Offset(cx, maxOf(g.peerY + 7 * dp, end)), hair)
        drawCircle(c.fg.a(a), 4 * dp, Offset(cx, g.peerY))
        if (ringOn && !m.reduced) {
            val mbps = m.rate / 1e6f
            if (Math.random().toFloat() < dt * (if (mbps < 1) 2f else 22f)) parts.ys += g.peerY + 8 * dp
            val sp = dpPerSec(m.rate) * dp
            for (i in parts.ys.indices) parts.ys[i] += sp * dt
            parts.ys.removeAll { it >= end }
            for (y in parts.ys) drawRect(c.fg.a(a), Offset(cx - 0.75f * dp, y), Size(1.5f * dp, (if (mbps < 1) 3 else 7) * dp))
        } else parts.ys.clear()
    }
    if (m.core > 0.01f) {
        val b = if (m.reduced) 1f else 1 + 0.42f * (0.5f - 0.5f * cos(2 * PI.toFloat() * t / 2.4f))
        drawCircle(c.fg, 5 * dp * b * m.core, Offset(cx, cy))
    }
    if (m.ring > 0.01f) {
        // the tick dial fills as the file arrives
        for (i in 0 until 72) {
            val a = -PI.toFloat() / 2 + i / 72f * 2 * PI.toFloat()
            val on = i / 72f < m.progress
            val r0 = (R + 14) * dp
            val r1 = (R + if (i % 6 == 0) 21f else 18f) * dp
            drawLine((if (on) c.fg else c.g2).a(m.ring), Offset(cx + cos(a) * r0, cy + sin(a) * r0), Offset(cx + cos(a) * r1, cy + sin(a) * r1), hair)
        }
        val (ro, ri) = ringGeo(m.progress)
        val s = 0.6f + 0.4f * m.ring
        drawCircle(c.fg.a(m.ring), (ro + ri) / 2 * s * dp, Offset(cx, cy), style = Stroke(maxOf(1.5f, ro - ri) * s * dp))
    }
    if (m.puff > 0.01f) {
        val far = hypot(size.width, size.height) * 0.68f
        drawCircle(c.fg, RB * dp + (far - RB * dp) * m.puff, Offset(cx, cy))
    }
}

// ------------------------------------------------------------------ Field

private class Pulses { val ys = mutableListOf<Float>(); var last = 0f }

/** Field: the screen is a dot matrix and nothing else is drawn. */
@Composable
fun FieldStage(m: StageModel, pal: Palette, modifier: Modifier = Modifier) {
    val time by rememberTime()
    val dp = LocalDensity.current.density
    val pulses = remember { Pulses() }
    Canvas(modifier) { drawField(m, pal, time, dp, pulses) }
}

private fun DrawScope.drawField(m: StageModel, c: Palette, t: Float, dp: Float, pulses: Pulses) {
    drawRect(c.bg)
    val sp = 10 * dp
    val cols = (size.width / sp).toInt()
    val rows = (size.height / sp).toInt() + 1
    val ox = (size.width - (cols - 1) * sp) / 2
    val g = Geo(size.width / 2, if (m.cy.isNaN()) size.height / 2 else m.cy, dp, m.nodes.size, m.nodes)
    // snap the centre onto the grid so the core is one crisp dot
    val cx = ox + ((g.cx - ox) / sp).roundToInt() * sp
    val cy = (g.cy / sp).roundToInt() * sp
    val (ro0, ri0) = ringGeo(m.progress, 4f)
    val ro = ro0 * dp; val ri = ri0 * dp
    val br = if (m.reduced) 1f else 0.5f - 0.5f * cos(2 * PI.toFloat() * t / 2.4f)
    val dt = (t - pulses.last).coerceIn(0f, 0.1f); pulses.last = t
    val ringOn = m.ring > 0.5f
    if (ringOn && !m.reduced) {
        if (Math.random().toFloat() < dt * (if (m.rate < 1e6f) 1.5f else 14f)) pulses.ys += g.peerY + 10 * dp
        val v = dpPerSec(m.rate) * dp
        for (i in pulses.ys.indices) pulses.ys[i] += v * dt
        pulses.ys.removeAll { it >= cy - ro }
    } else pulses.ys.clear()
    val puffR = if (m.puff > 0.01f) RB * dp + (hypot(size.width, size.height) * 0.68f - RB * dp) * m.puff else 0f
    for (r in 0 until rows) for (col in 0 until cols) {
        val x = ox + col * sp
        val y = r * sp
        val dx = x - cx
        val d = sqrt(dx * dx + (y - cy) * (y - cy))
        var b = m.core * (if (d < 6 * dp) 1f else if (d < 16 * dp) 0.35f + 0.4f * br else 0f)
        if (m.sonar > 0.01f && !m.reduced) for (j in 0 until 3) {
            val u = (t / 2.8f + j / 3f) % 1f
            val rr = (10 + u * 150) * dp
            b = maxOf(b, m.sonar * 0.75f * (1 - u).pow(1.4f) * exp(-((d - rr) / (6 * dp)).pow(2)))
        }
        for ((nx, ny, k) in g.nodes) if (k > 0.01f && hypot(x - nx, y - ny) < 11 * dp) b = maxOf(b, k)
        if (m.peer > 0.01f) {
            if (hypot(dx, y - g.peerY) < 9 * dp) b = maxOf(b, m.peer)
            if (abs(dx) < 1 && y > g.peerY + 8 * dp && y < cy - (if (ringOn) ro else 10 * dp)) {
                var v = 0.28f
                for (py in pulses.ys) v = maxOf(v, exp(-((y - py) / (7 * dp)).pow(2)))
                b = maxOf(b, v * m.peer)
            }
        }
        if (m.ring > 0.01f) {
            val e = min(d - ri + 5 * dp, ro - d + 5 * dp) / (5 * dp)
            if (e > 0) b = maxOf(b, m.ring * min(1f, e))
        }
        var rad = 1.1f * dp
        if (puffR > 0 && d < puffR) { b = 1f; rad = (1.1f + (10 * 0.62f - 1.1f) * m.puff) * dp }
        val lit = b > 0.02f
        drawCircle(if (lit) c.fg.a(0.25f + 0.75f * min(1f, b)) else c.g2, if (lit) rad + 0.5f * dp * min(1f, b) else rad * 0.85f, Offset(x, y))
    }
}
