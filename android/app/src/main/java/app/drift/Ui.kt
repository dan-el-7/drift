package app.drift

import android.app.DownloadManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings as SysSettings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontVariation
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import androidx.compose.foundation.text.BasicText
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlin.math.PI

// ------------------------------------------------------------------ tokens (design/tokens.json)

private fun geist(w: Int) = Font(R.font.geist, FontWeight(w), variationSettings = FontVariation.Settings(FontVariation.weight(w)))
private fun mono(w: Int) = Font(R.font.geist_mono, FontWeight(w), variationSettings = FontVariation.Settings(FontVariation.weight(w)))
val Geist = FontFamily(geist(400), geist(500), geist(600))
val GeistMono = FontFamily(mono(400), mono(500))

data class Palette(val bg: Color, val fg: Color, val g1: Color, val g2: Color)
private val DarkPal = Palette(Color(0xFF000000), Color(0xFFFFFFFF), Color(0xFF8A8A8A), Color(0xFF2A2A2A))
private val LightPal = Palette(Color(0xFFFFFFFF), Color(0xFF000000), Color(0xFF757575), Color(0xFFD5D5D5))
val LocalPal = staticCompositionLocalOf { DarkPal }

/** design/motion.json springs: damping ratio + response (ms) → Compose stiffness. */
private fun <T> settle() = spring<T>(dampingRatio = 1f, stiffness = stiff(420))
private fun <T> snappy() = spring<T>(dampingRatio = 0.86f, stiffness = stiff(260))
private fun <T> drop() = spring<T>(dampingRatio = 0.62f, stiffness = stiff(520))
private fun <T> puff() = spring<T>(dampingRatio = 0.72f, stiffness = stiff(640))
private fun stiff(responseMs: Int): Float { val w = 2 * PI / (responseMs / 1000.0); return (w * w).toFloat() }

@Composable
private fun T(
    text: String, size: TextUnit = 13.sp, color: Color = LocalPal.current.fg, weight: Int = 400,
    mono: Boolean = false, tracking: TextUnit = 0.sp, modifier: Modifier = Modifier, maxLines: Int = Int.MAX_VALUE,
) = BasicText(
    text, modifier,
    style = TextStyle(color = color, fontSize = size, fontFamily = if (mono) GeistMono else Geist, fontWeight = FontWeight(weight), letterSpacing = tracking),
    maxLines = maxLines, overflow = TextOverflow.Ellipsis,
)

@Composable
private fun Pill(label: String, primary: Boolean = false, modifier: Modifier = Modifier, tight: Boolean = false, onClick: () -> Unit) {
    val p = LocalPal.current
    val src = remember { MutableInteractionSource() }
    val pressed by src.collectIsPressedAsState()
    val s by animateFloatAsState(if (pressed) 0.96f else 1f, snappy(), label = "press")
    Box(
        modifier
            .graphicsLayer { scaleX = s; scaleY = s }
            .clip(RoundedCornerShape(50))
            .then(if (primary) Modifier.background(p.fg) else Modifier.border(1.dp, p.g2, RoundedCornerShape(50)))
            .clickable(src, null, onClick = onClick)
            .padding(horizontal = if (tight) 8.dp else 18.dp, vertical = 11.dp),
        contentAlignment = Alignment.Center,
    ) { T(label, 14.sp, if (primary) p.bg else p.fg, 500, maxLines = 1) }
}

// ------------------------------------------------------------------ predictive back

/**
 * Predictive back for in-app screens: while the user swipes, the screen shrinks a little, leans toward
 * the edge they swipe from and fades (Android's own back preview), so they see where back goes before
 * letting go. Release runs [onBack]; cancel springs it back.
 */
private class BackPeek(val progress: Animatable<Float, *>, var edge: Int)

@Composable
private fun rememberBackPeek(enabled: Boolean, onBack: () -> Unit): BackPeek {
    val peek = remember { BackPeek(Animatable(0f), 0) }
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    // each time the screen comes back, start from rest
    LaunchedEffect(enabled) { if (enabled) peek.progress.snapTo(0f) }
    androidx.activity.compose.PredictiveBackHandler(enabled) { events ->
        try {
            events.collect { e -> peek.edge = e.swipeEdge; peek.progress.snapTo(e.progress) }
            onBack()
        } catch (c: kotlin.coroutines.cancellation.CancellationException) {
            scope.launch { peek.progress.animateTo(0f, settle()) }
            throw c
        }
    }
    return peek
}

private fun Modifier.backPeek(p: BackPeek): Modifier = graphicsLayer {
    val k = p.progress.value
    if (k > 0f) {
        val s = 1f - 0.08f * k
        scaleX = s; scaleY = s
        // lean toward the swiped edge (EDGE_LEFT = 0 -> right, EDGE_RIGHT = 1 -> left)
        translationX = (if (p.edge == 0) 1 else -1) * 24.dp.toPx() * k
        alpha = 1f - 0.3f * k
        clip = true
        shape = RoundedCornerShape(28.dp * k)
    }
}

// ------------------------------------------------------------------ routes

private sealed interface Route {
    data object Home : Route
    data object Pick : Route
    data class Request(val t: Transfer) : Route
    data class Xfer(val t: Transfer) : Route
    data class Done(val t: Transfer) : Route
}

private val dismissed = mutableStateListOf<Long>()
/** Transfers sent to the background (Hide, or back): they keep running; home shows them as chips. */
private val hidden = mutableStateListOf<Long>()
private val finished = setOf(TState.Done, TState.Failed, TState.Declined, TState.Cancelled)

private fun route(): Route {
    Drift.transfers.firstOrNull { it.state == TState.Request }?.let { return Route.Request(it) }
    Drift.transfers.firstOrNull { (it.state == TState.Waiting || it.state == TState.Xfer) && it.share !in hidden }?.let { return Route.Xfer(it) }
    if (Drift.outbox != null) return Route.Pick
    Drift.transfers.firstOrNull()?.takeIf { it.state in finished && it.share !in dismissed }?.let { return Route.Done(it) }
    return Route.Home
}

// ------------------------------------------------------------------ the app

@Composable
fun DriftUi() {
    val dark = isSystemInDarkTheme()
    val base = if (dark) DarkPal else LightPal
    var settings by remember { mutableStateOf(false) }
    var trustedOpen by remember { mutableStateOf(false) }
    val r = route()
    // back from picking a device cancels it; back from a transfer hides it (it keeps going);
    // back from a finished transfer dismisses it
    val panelPeek = rememberBackPeek(!settings && (r == Route.Pick || r is Route.Done || r is Route.Xfer)) {
        when (r) {
            Route.Pick -> { Drift.outbox = null; Drift.scan(false) }
            is Route.Xfer -> hidden += r.t.share
            is Route.Done -> dismissed += r.t.share
            else -> {}
        }
    }
    // Done: the ink floods the screen and the UI on top inverts with it (as on the desktop)
    val flood by animateFloatAsState(if (r is Route.Done && r.t.state == TState.Done) 1f else 0f, settle(), label = "flood")
    val inv = if (dark) LightPal else DarkPal
    val p = Palette(
        lerp(base.bg, inv.bg, flood), lerp(base.fg, inv.fg, flood),
        lerp(base.g1, inv.g1, flood), lerp(base.g2, inv.g2, flood),
    )
    var stageY by remember { mutableFloatStateOf(Float.NaN) }
    // status/navigation bar icons follow the ink: dark icons whenever the screen is light
    val view = androidx.compose.ui.platform.LocalView.current
    val lightBars = if (dark) flood > 0.5f else flood < 0.5f
    androidx.compose.runtime.SideEffect {
        (view.context as? android.app.Activity)?.window?.let { w ->
            androidx.core.view.WindowCompat.getInsetsController(w, view).apply {
                isAppearanceLightStatusBars = lightBars
                isAppearanceLightNavigationBars = lightBars
            }
        }
    }
    CompositionLocalProvider(LocalPal provides p) {
        Box(Modifier.fillMaxSize().background(base.bg)) {
            // the stage is the whole screen, behind everything (the puff goes edge to edge)
            Stage(r, dark, base, stageY, Modifier.fillMaxSize())
            Column(Modifier.fillMaxSize().systemBarsPadding()) {
                Row(Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 14.dp), verticalAlignment = Alignment.CenterVertically) {
                    T("drift", 17.sp, weight = 600, tracking = (-0.02).em)
                    // Blitz unlocked for this transfer: a small bolt pops in beside the wordmark
                    if (r is Route.Xfer && r.t.share in Blitz.unlocked) {
                        Spacer(Modifier.width(6.dp))
                        BlitzMark(r.t.share, p.fg, Modifier.size(width = 10.dp, height = 14.dp))
                    }
                    Spacer(Modifier.weight(1f))
                    StatusLine(r)
                    T(if (settings) "Close" else "Settings", 13.sp, p.g1, modifier = Modifier.clip(RoundedCornerShape(50)).clickable { settings = !settings }.padding(8.dp))
                }
                Spacer(Modifier.fillMaxWidth().weight(1f).onGloballyPositioned { c ->
                    stageY = c.positionInRoot().y + c.size.height / 2f
                })
                AnimatedContent(
                    targetState = r,
                    contentKey = { it::class },
                    transitionSpec = {
                        (fadeIn(tween(250)) + slideInVertically(settle()) { 40 }) togetherWith (fadeOut(tween(200)) + slideOutVertically(tween(200)) { -20 })
                    },
                    label = "panel",
                ) { Box(Modifier.backPeek(panelPeek)) { Panel(it) } }
            }
            val sheetPeek = rememberBackPeek(settings && !trustedOpen) { settings = false }
            AnimatedVisibility(settings, enter = fadeIn(tween(250)) + slideInVertically(settle()) { 60 }, exit = fadeOut(tween(200))) {
                Box(Modifier.backPeek(sheetPeek)) { SettingsSheet({ settings = false }) { trustedOpen = it } }
            }
            NoticeCard()
        }
    }
}

@Composable
private fun Stage(r: Route, dark: Boolean, base: Palette, cy: Float, modifier: Modifier) {
    val reduced = remember { false }
    val t = when (r) { is Route.Request -> r.t; is Route.Xfer -> r.t; is Route.Done -> r.t; else -> null }
    // as on the desktop (StageModel.Pose): the centre drop only at home and while choosing a device
    val core by animateFloatAsState(if (r == Route.Home || r == Route.Pick) 1f else 0f, settle(), label = "core")
    // as on the desktop: full ripples while idle and visible, half while picking a device
    val sonar by animateFloatAsState(if (r == Route.Home && Drift.visible) 1f else if (r == Route.Pick) 0.5f else 0f, settle(), label = "sonar")
    val peer by animateFloatAsState(if (r is Route.Request || r is Route.Xfer) 1f else 0f, drop(), label = "peer")
    val ring by animateFloatAsState(if (r is Route.Xfer && r.t.state == TState.Xfer) 1f else 0f, settle(), label = "ring")
    val prog by animateFloatAsState(t?.progress ?: 0f, settle(), label = "progress")
    val puffK by animateFloatAsState(if (r is Route.Done && r.t.state == TState.Done) 1f else 0f, puff(), label = "puff")
    val nodes = mutableListOf<Float>()
    for (d in Drift.devices) key(d.id) {
        val a = remember { Animatable(0f) }
        LaunchedEffect(r == Route.Pick) { a.animateTo(if (r == Route.Pick) 1f else 0f, drop()) }
        nodes += a.value
    }
    val m = StageModel(core, sonar, nodes, peer, ring, prog, puffK, reduced, cy, t?.bps ?: 0f, idle = r == Route.Home)
    Box(modifier) {
        when (Drift.look) {
            Look.Ink -> InkStage(m, dark, Modifier.fillMaxSize())
            Look.Monolith -> MonolithStage(m, base, Modifier.fillMaxSize())
            Look.Field -> FieldStage(m, base, Modifier.fillMaxSize())
        }
        StageLabels(r, m, t?.peer, base)
    }
}

/**
 * What the desktop puts on its stage (MainWindow: Pct, DrawPeerName, the device buttons): the
 * percentage inside the ring, the other device's name beside its drop, and each nearby device's name
 * under its drop, which is what you tap to send.
 */
@Composable
private fun StageLabels(r: Route, m: StageModel, peerName: String?, base: Palette) {
    val time by rememberTime()
    val density = LocalDensity.current
    androidx.compose.foundation.layout.BoxWithConstraints(Modifier.fillMaxSize()) {
        val cx = maxWidth / 2
        val cy = with(density) { (if (m.cy.isNaN()) constraints.maxHeight / 2f else m.cy).toDp() }
        fun Modifier.at(x: androidx.compose.ui.unit.Dp, y: androidx.compose.ui.unit.Dp) =
            offset { androidx.compose.ui.unit.IntOffset(x.roundToPx(), y.roundToPx()) }
        // Blitz: seven quick taps on the ring (no ripple, no hint: it's an easter egg)
        if (r is Route.Xfer && r.t.state == TState.Xfer) {
            val share = r.t.share
            Box(
                Modifier.at(cx - 90.dp, cy - 90.dp).size(180.dp)
                    .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) { Blitz.tap(share) },
            )
        }

        // percentage inside the ring, fading as the ring swallows it (not on Field)
        if (m.ring > 0.01f && Drift.look != Look.Field) {
            val p = m.progress.coerceIn(0f, 1f)
            val a = ((1 - (p - 0.3f) * 3.2f).coerceIn(0f, 1f)) * m.ring.coerceIn(0f, 1f)
            if (a > 0.01f) Box(Modifier.at(0.dp, cy - 10.dp).width(maxWidth).height(20.dp).graphicsLayer { alpha = a }, contentAlignment = Alignment.Center) {
                T("${kotlin.math.floor(p * 100).toInt()}%", 13.sp, base.fg, mono = true, tracking = 0.04.em)
            }
        }
        // the other device's name, right of its drop, riding Ink's bob
        if (m.peer > 0.01f && m.puff < 0.01f && !peerName.isNullOrEmpty()) {
            val bob = if (Drift.look == Look.Ink && !m.reduced) (2.2f * kotlin.math.sin(time * 1.3f + 0.4f)).dp else 0.dp
            Box(Modifier.at(cx + 22.dp, cy - StageGeo.PEER.dp - 10.dp + bob).height(20.dp).graphicsLayer { alpha = m.peer.coerceIn(0f, 1f) }, contentAlignment = Alignment.CenterStart) {
                T(peerName, 12.sp, base.g1, weight = 500, maxLines = 1)
            }
        }
        // nearby devices: the label arrives once the drop has pinched off and settled; tap to send
        if (r == Route.Pick) Drift.devices.forEachIndexed { i, d ->
            val k = m.nodes.getOrNull(i) ?: return@forEachIndexed
            val o = StageGeo.node(i)
            val a = ((k - 0.8f) / 0.2f).coerceIn(0f, 1f) * k.coerceIn(0f, 1f)
            key(d.id) {
                Column(
                    Modifier.at(cx + o.x.dp - 60.dp, cy + o.y.dp - 22.dp).width(120.dp).height(78.dp)
                        .graphicsLayer { alpha = a }
                        .clip(RoundedCornerShape(12.dp))
                        .clickable(enabled = a > 0.5f) { Drift.send(d); Drift.scan(false) }
                        .padding(start = 4.dp, end = 4.dp, bottom = 4.dp),
                    verticalArrangement = Arrangement.spacedBy(2.dp, Alignment.Bottom),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    T(d.name, 12.sp, base.fg, weight = 500, maxLines = 1)
                    // Link details adds whether it runs Drift, as on the desktop
                    val kind = d.kind + if (Drift.linkDetails && d.drift) " · drift" else ""
                    if (kind.isNotEmpty()) T(kind.uppercase(), 10.5.sp, base.g1, mono = true, tracking = 0.04.em, maxLines = 1)
                }
            }
        }
    }
}

@Composable
private fun Panel(r: Route) {
    Column(Modifier.fillMaxWidth().padding(start = 24.dp, end = 24.dp, bottom = 28.dp, top = 8.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        when (r) {
            Route.Home -> Home()
            Route.Pick -> Pick()
            is Route.Request -> Request(r.t)
            is Route.Xfer -> Xfer(r.t)
            is Route.Done -> Done(r.t)
        }
    }
}

@Composable
private fun Home() {
    val p = LocalPal.current
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isNotEmpty()) { Drift.outbox = Outbox(uris, null); Drift.scan(true) }
    }
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(Drift.visibility) { while (true) { now = System.currentTimeMillis(); delay(1000) } }
    Running()
    T(Drift.name, 28.sp, weight = 600, tracking = (-0.03).em, maxLines = 1)
    val line = when (Drift.visibility) {
        Visibility.Hidden -> "Hidden"
        Visibility.Always -> "Visible to everyone nearby"
        Visibility.TenMinutes -> {
            val left = ((Drift.visibleUntil - now) / 1000).coerceAtLeast(0)
            "Visible to everyone · %d:%02d".format(left / 60, left % 60)
        }
    }
    T(line, 13.sp, p.g1, mono = Drift.visibility == Visibility.TenMinutes)
    Spacer(Modifier.height(14.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        val next = when (Drift.visibility) { Visibility.TenMinutes -> Visibility.Always; Visibility.Always -> Visibility.Hidden; Visibility.Hidden -> Visibility.TenMinutes }
        Pill(when (Drift.visibility) { Visibility.TenMinutes -> "10 min"; Visibility.Always -> "Always"; Visibility.Hidden -> "Hidden" }) { Drift.show(next) }
        Pill("Send", primary = true) { picker.launch(arrayOf("*/*")) }
    }
}

/** Hidden transfers that are still going, as chips (the desktop's dock); tap one to open it. */
@Composable
private fun Running() {
    val running = Drift.transfers.filter { it.share in hidden && (it.state == TState.Waiting || it.state == TState.Xfer) }
    if (running.isEmpty()) return
    Row(Modifier.padding(bottom = 10.dp), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        for (t in running.take(2)) {
            val what = if (t.state == TState.Waiting) "waiting" else "${kotlin.math.floor(t.progress * 100).toInt()}%"
            Pill("${if (t.title.length > 18) t.title.take(17) + "…" else t.title} · $what", tight = true) { hidden -= t.share }
        }
        if (running.size > 2) Pill("+${running.size - 2}", tight = true) { hidden -= running[2].share }
    }
}

@Composable
private fun Pick() {
    val p = LocalPal.current
    Drift.outbox ?: return
    Running()
    // as on the desktop: the devices are drops on the stage, named under each one; tap one to send
    T("Send ${Drift.outboxTitle()}", 22.sp, weight = 600, tracking = (-0.02).em, maxLines = 1)
    T(if (Drift.devices.isEmpty()) "Looking for nearby devices…" else "Choose a device", 13.sp, p.g1)
    // more files while choosing a device: they join the ones already chosen
    val more = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        val box = Drift.outbox
        if (uris.isNotEmpty() && box != null && box.text == null) Drift.outbox = box.copy(uris = (box.uris + uris).distinct())
    }
    Spacer(Modifier.height(10.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Pill("Cancel") { Drift.outbox = null; Drift.scan(false) }
        if (Drift.outbox?.text == null) Pill("Add files") { more.launch(arrayOf("*/*")) }
    }
}

/** The code: digits focus in one after another with a small rise, then keep floating. */
@Composable
private fun Code(pin: String) {
    val time by rememberTime()
    Row(horizontalArrangement = Arrangement.spacedBy(2.dp)) {
        pin.forEachIndexed { i, c ->
            val a = remember(pin) { Animatable(0f) }
            LaunchedEffect(pin) { delay(90L * i); a.animateTo(1f, drop()) }
            val k = a.value
            T(
                c.toString(), 56.sp, weight = 600, tracking = (-0.02).em,
                modifier = Modifier
                    .graphicsLayer {
                        alpha = k.coerceIn(0f, 1f)
                        translationY = (1 - k) * 18.dp.toPx() + kotlin.math.sin(time * 1.3f + i * 1.7f) * 1.6.dp.toPx() * k
                        rotationZ = kotlin.math.sin(time * 0.9f + i * 2.1f) * 1.2f * k
                    }
                    .blur(((1 - k).coerceIn(0f, 1f) * 10).dp),
            )
        }
    }
}

@Composable
private fun Request(t: Transfer) {
    val p = LocalPal.current
    T(if (t.drift) "${t.peer} · Drift" else t.peer, 13.sp, p.g1, maxLines = 1)
    t.pin?.let { Code(it) }
    T("wants to share ${t.phrase}", 17.sp, weight = 500, maxLines = 2)
    if (t.total > 0) T(size(t.total), 13.sp, p.g1, mono = true)
    Spacer(Modifier.height(14.dp))
    // three choices share the row evenly (a phone is too narrow for three natural-width pills)
    val three = t.drift && !t.trusted
    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        val w = if (three) Modifier.weight(1f) else Modifier
        Pill("Decline", modifier = w, tight = three) { Drift.respond(t, false) }
        if (three) Pill("Always accept", modifier = Modifier.weight(1.6f), tight = true) { Drift.respond(t, true, always = true) }
        Pill("Accept", primary = true, modifier = w, tight = three) { Drift.respond(t, true) }
    }
}

@Composable
private fun Xfer(t: Transfer) {
    val p = LocalPal.current
    if (t.state == TState.Waiting) {
        T("Waiting for ${t.peer}", 13.sp, p.g1, maxLines = 1)
        t.pin?.let { Code(it) }
        T("to accept ${t.phrase}", 17.sp, weight = 500, maxLines = 2)
    } else {
        T(t.title, 22.sp, weight = 600, tracking = (-0.02).em, maxLines = 1)
        // MainWindow.XferLine: "266 MB of 485 MB · 54%"
        T("${mb(t.done)} of ${mb(t.total)} · ${kotlin.math.floor(t.progress * 100).toInt()}%", 13.sp, p.g1)
    }
    Spacer(Modifier.height(14.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        // Hide: back to home while this keeps going (send more meanwhile, as on the desktop)
        Pill("Hide") { hidden += t.share }
        Pill("Cancel") { Drift.cancel(t) }
        if (t.share in Blitz.unlocked && t.state == TState.Xfer) {
            val ctx = LocalContext.current
            Pill("Blitz", primary = true) { Blitz.run(ctx, t) }
        }
    }
}

@Composable
private fun Done(t: Transfer) {
    val p = LocalPal.current
    val ctx = LocalContext.current
    val head = when (t.state) {
        TState.Done -> if (t.incoming) "Received" else "Sent"
        TState.Declined -> if (t.incoming) "Declined" else "${t.peer} declined"
        TState.Cancelled -> "Cancelled"
        else -> "Didn't go through"
    }
    T((if (t.incoming) "From " else "To ") + t.peer, 13.sp, p.g1, maxLines = 1)
    T(head, 28.sp, weight = 600, tracking = (-0.03).em)
    T(if (t.state == TState.Done && t.incoming && t.files.isNotEmpty()) "${t.title} · ${Drift.saveLabel}" else t.text ?: t.title, 13.sp, p.g1, maxLines = 3)
    Spacer(Modifier.height(14.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Pill("Done") { dismissed += t.share }
        // a send that failed: back to choosing a device, with the same files
        val again = t.sent
        if (t.state == TState.Failed && !t.incoming && again != null)
            Pill("Try again", primary = true) { dismissed += t.share; Drift.outbox = again; Drift.scan(true) }
        if (t.state == TState.Done && t.incoming) {
            val text = t.text
            when {
                text != null && t.textKind == "url" && (text.startsWith("https://") || text.startsWith("http://")) -> {
                    Pill("Copy") { copy(ctx, text) }
                    Pill("Open", primary = true) { runCatching { ctx.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(text))) } }
                }
                text != null -> Pill("Copy", primary = true) { copy(ctx, text) }
                t.savedUris.size == 1 -> Pill("Open", primary = true) { open(ctx, t.savedUris[0]) }
                else -> Pill("Show", primary = true) { runCatching { ctx.startActivity(Intent(DownloadManager.ACTION_VIEW_DOWNLOADS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) } }
            }
        }
    }
}

private fun copy(ctx: Context, s: String) =
    ctx.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("Drift", s))

private fun open(ctx: Context, uri: Uri) {
    val type = ctx.contentResolver.getType(uri)
    runCatching {
        ctx.startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(uri, type).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK))
    }
}

fun size(b: Long): String = when {
    b >= 1L shl 30 -> "%.1f GB".format(b / (1L shl 30).toDouble())
    b >= 1L shl 20 -> "%.1f MB".format(b / (1L shl 20).toDouble())
    b >= 1L shl 10 -> "%.0f KB".format(b / 1024.0)
    else -> "$b B"
}

private fun rate(bps: Float) = if (bps >= 1e6f) "%.0f MB/s".format(bps / 1e6f) else "%.0f KB/s".format(bps / 1e3f)

// ------------------------------------------------------------------ settings

@Composable
private fun SettingsSheet(close: () -> Unit, onPage: (Boolean) -> Unit) {
    val p = LocalPal.current
    val ctx = LocalContext.current
    var page by remember { mutableStateOf("main") }
    LaunchedEffect(page) { onPage(page != "main") }
    val pagePeek = rememberBackPeek(page != "main") { page = "main" }
    Column(
        Modifier.fillMaxSize().background(p.bg).backPeek(pagePeek).systemBarsPadding()
            .verticalScroll(androidx.compose.foundation.rememberScrollState()).padding(start = 20.dp, end = 20.dp),
        verticalArrangement = Arrangement.spacedBy(22.dp),
    ) {
        // its own bar, matching the main one, so Close stays where Settings was
        Row(Modifier.fillMaxWidth().padding(top = 14.dp), verticalAlignment = Alignment.CenterVertically) {
            T(if (page == "main") "Settings" else "Trusted devices", 17.sp, weight = 600, tracking = (-0.02).em,
                modifier = if (page == "main") Modifier else Modifier.clip(RoundedCornerShape(8.dp)).clickable { page = "main" })
            Spacer(Modifier.weight(1f))
            T(if (page == "main") "Close" else "Back", 13.sp, p.g1,
                modifier = Modifier.clip(RoundedCornerShape(50)).clickable { if (page == "main") close() else page = "main" }.padding(8.dp))
        }
        if (page == "main") {
            NameSetting()
            // the same three choices as the desktop, side by side
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                T("Visible to", 13.sp, p.g1)
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    for ((label, v) in listOf("10 min" to Visibility.TenMinutes, "Always" to Visibility.Always, "Hidden" to Visibility.Hidden))
                        Pill(label, primary = Drift.visibility == v, modifier = Modifier.weight(1f), tight = true) { Drift.show(v) }
                }
                T(when (Drift.visibility) {
                    Visibility.TenMinutes -> "Everyone nearby, for 10 minutes."
                    Visibility.Always -> "Everyone nearby, while Drift is open."
                    Visibility.Hidden -> "Nobody. You can still send."
                }, 12.sp, p.g1)
            }
            val pickFolder = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
                if (uri != null) runCatching { Drift.saveInto(uri) }
            }
            Setting("Saving to", Drift.saveLabel, null, chevron = true) { pickFolder.launch(Drift.saveTree) }
            if (Drift.saveTree != null)
                T("Use Download/Drift", 13.sp, p.g1, modifier = Modifier.clip(RoundedCornerShape(50)).clickable { Drift.saveInto(null) }.padding(vertical = 4.dp))
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                T("Look", 13.sp, p.g1)
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    for (l in Look.entries) Pill(l.name, primary = Drift.look == l, modifier = Modifier.weight(1f), tight = true) { Drift.useLook(l) }
                }
            }
            Setting("Link details", if (Drift.linkDetails) "On" else "Off", "Shows how each transfer travels: Bluetooth, Wi-Fi Direct or your network.") {
                Drift.showLinkDetails(!Drift.linkDetails)
            }
            Setting("Wi-Fi Direct", "Reset pairing", "If a PC's direct link stops connecting. Your phone asks once more next time.") {
                P2p.resetPairing(ctx)
                Drift.notice = Notice("Wi-Fi Direct reset", "The next PC that connects asks you once.")
            }
            Setting("Trusted devices", if (Drift.trusted.isEmpty()) "None yet" else Drift.trusted.joinToString(", ") { it.name }, null, chevron = true) { page = "trusted" }
            val version = remember { runCatching { ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName }.getOrNull() ?: "" }
            Row(Modifier.padding(top = 6.dp, bottom = 24.dp), verticalAlignment = Alignment.CenterVertically) {
                T("Drift $version", 12.sp, p.g1, mono = true, tracking = 0.04.em)
                T("  ·  ", 12.sp, p.g2)
                T("Share log", 12.sp, p.g1, modifier = Modifier.clip(RoundedCornerShape(50)).clickable { Drift.shareLog(ctx) }.padding(vertical = 6.dp))
            }
        } else {
            if (Drift.trusted.isEmpty()) T("None yet", 17.sp, weight = 500)
            for (d in Drift.trusted) key(d.id) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        T(d.name, 17.sp, weight = 500, maxLines = 1)
                        T(d.id.uppercase(), 11.sp, p.g1, mono = true, tracking = 0.06.em)
                    }
                    Pill("Remove") { Drift.untrust(d.id) }
                }
            }
        }
    }
}

/** Edited in place, like the desktop; blank goes back to the phone's own name. */
@Composable
private fun NameSetting() {
    val p = LocalPal.current
    val focus = LocalFocusManager.current
    var text by remember { mutableStateOf(Drift.name) }
    Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
        T("Device name", 13.sp, p.g1)
        BasicTextField(
            text, { text = it.take(40) },
            singleLine = true,
            textStyle = TextStyle(color = p.fg, fontSize = 17.sp, fontFamily = Geist, fontWeight = FontWeight(500)),
            cursorBrush = SolidColor(p.fg),
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
            keyboardActions = KeyboardActions(onDone = { Drift.rename(text); text = Drift.name; focus.clearFocus() }),
            modifier = Modifier.fillMaxWidth(),
        )
        T("Phones and PCs see “${Drift.name}”.", 12.sp, p.g1)
    }
}

/** Top right, as on the desktop: visibility, or the live link while something is moving. */
@Composable
private fun StatusLine(r: Route) {
    val p = LocalPal.current
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(Unit) { while (true) { now = System.currentTimeMillis(); delay(1000) } }
    // as on the desktop (MainWindow Meta): the link and its speed while moving (Link details swaps in
    // the precise name), the code while waiting, link details after, visibility otherwise
    val t = when (r) { is Route.Xfer -> r.t; is Route.Request -> r.t; is Route.Done -> r.t; else -> null }
    val text = when {
        t != null && t.state == TState.Xfer ->
            "${if (Drift.linkDetails) linkDetail(t.medium) else linkName(t.medium)} · ${statusRate(t.bps)}"
        t != null && t.state in setOf(TState.Waiting, TState.Request) && t.pin != null -> "code ${t.pin}"
        t != null && t.state in setOf(TState.Waiting, TState.Request) -> "waiting"
        r is Route.Done && t != null && t.state == TState.Done && Drift.linkDetails && (t.medium ?: "bluetooth") != "bluetooth" -> linkDetail(t.medium)
        r is Route.Done -> ""
        r == Route.Pick -> "looking"
        Drift.visibility == Visibility.Hidden -> "hidden"
        Drift.visibility == Visibility.Always -> "visible"
        else -> {
            val left = ((Drift.visibleUntil - now) / 1000).coerceAtLeast(0)
            "visible %d:%02d".format(left / 60, left % 60)
        }
    }
    T(text.uppercase(), 11.sp, p.g1, mono = true, tracking = 0.08.em, maxLines = 1, modifier = Modifier.padding(end = 10.dp))
}

/** The link in plain words (MainWindow.LinkName). */
private fun linkName(medium: String?) = when (medium ?: "bluetooth") {
    "bluetooth" -> "Bluetooth"
    "lan" -> "Wi-Fi"
    "hotspot", "direct", "phone_hosted" -> "Direct link"
    "same_hotspot" -> "Hotspot"
    else -> medium!!
}

/** The precise link, for Link details (MainWindow.LinkDetail). On the phone, "direct" is our own group. */
private fun linkDetail(medium: String?) = when (medium ?: "bluetooth") {
    "bluetooth" -> "Bluetooth"
    "lan" -> "Wi-Fi LAN"
    "hotspot" -> "Hotspot"
    "direct" -> "Wi-Fi Direct"
    "phone_hosted" -> "Phone's Wi-Fi Direct"
    "same_hotspot" -> "Your hotspot"
    else -> medium!!
}

/** MainWindow.Mb: GB with two decimals, whole MB from 10 up, one decimal below. */
private fun mb(b: Long) = when {
    b >= 1_000_000_000 -> "%.2f GB".format(b / 1e9)
    b >= 10_000_000 -> "%.0f MB".format(b / 1e6)
    else -> "%.1f MB".format(maxOf(0.1, b / 1e6))
}

/** MainWindow.Rate: whole MB/s from 10 up, one decimal below. */
private fun statusRate(bps: Float) = if (bps >= 10e6f) "%.0f MB/s".format(bps / 1e6f) else "%.1f MB/s".format(bps / 1e6f)

@Composable
private fun Setting(label: String, value: String, note: String?, chevron: Boolean = false, onClick: (() -> Unit)?) {
    val p = LocalPal.current
    // the chevron is centred on the label + value block, not on the value line
    Row(
        // no rounded clip: its corner shaved the first letter of a note's last line ("once" -> "nce")
        Modifier.fillMaxWidth().then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            T(label, 13.sp, p.g1)
            T(value, 17.sp, weight = 500, maxLines = 1)
            note?.let { T(it, 12.sp, p.g1) }
        }
        if (chevron) T("›", 22.sp, p.g1)
    }
}

// ------------------------------------------------------------------ the notice card

/** Every pop-up is this card: an "i" whose ring draws on, the stem follows, the dot drops in. */
@Composable
private fun NoticeCard() {
    val p = LocalPal.current
    val n = Drift.notice
    AnimatedVisibility(n != null, enter = fadeIn(tween(250)) + slideInVertically(settle()) { -30 }, exit = fadeOut(tween(200))) {
        val shown = remember { mutableStateOf(n) }
        if (n != null) shown.value = n
        val cur = shown.value ?: return@AnimatedVisibility
        LaunchedEffect(cur) { if (!cur.lasting) { delay(5000); if (Drift.notice == cur) Drift.notice = null } }
        Row(
            Modifier.padding(top = 56.dp, start = 16.dp, end = 16.dp).fillMaxWidth()
                .clip(RoundedCornerShape(14.dp)).background(p.bg).border(1.dp, p.g2, RoundedCornerShape(14.dp))
                .padding(14.dp),
            verticalAlignment = Alignment.Top,
        ) {
            InfoMark(p.fg)
            Column(Modifier.weight(1f).padding(start = 12.dp, end = 8.dp), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                T(cur.title, 14.sp, weight = 500)
                cur.detail?.let { T(it, 12.5.sp, p.g1) }
                cur.action?.let { (label, run) -> Spacer(Modifier.height(6.dp)); Pill(label, primary = true, tight = true) { run() } }
            }
            T("×", 18.sp, p.g1, modifier = Modifier.clip(RoundedCornerShape(50)).clickable { Drift.notice = null }.padding(horizontal = 6.dp))
        }
    }
}

@Composable
private fun InfoMark(c: Color) {
    val ring = remember { Animatable(0f) }
    val stem = remember { Animatable(0f) }
    val dot = remember { Animatable(0f) }
    val time by rememberTime()
    LaunchedEffect(Unit) {
        ring.animateTo(1f, tween(520, easing = FastOutSlowInEasing))
        stem.animateTo(1f, tween(260, easing = FastOutSlowInEasing))
        dot.animateTo(1f, drop())
    }
    Canvas(Modifier.size(20.dp)) {
        val w = 1.5.dp.toPx()
        val r = size.minDimension / 2 - w
        drawArc(c, -90f, 360f * ring.value, false, topLeft = Offset(w, w), size = androidx.compose.ui.geometry.Size(r * 2, r * 2), style = Stroke(w, cap = StrokeCap.Round))
        val cx = size.width / 2
        val top = size.height * 0.45f
        val bottom = size.height * 0.72f
        if (stem.value > 0f) drawLine(c, Offset(cx, top), Offset(cx, top + (bottom - top) * stem.value), w, StrokeCap.Round)
        val breath = 1f + 0.12f * kotlin.math.sin(time * 2.2f) * dot.value
        if (dot.value > 0f) drawCircle(c, 1.2.dp.toPx() * dot.value * breath, Offset(cx, size.height * 0.30f - (1 - dot.value) * 6.dp.toPx()))
    }
}
