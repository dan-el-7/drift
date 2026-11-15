package app.drift

import android.content.Context
import android.content.Intent
import android.media.MediaScannerConnection
import android.net.Uri
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import android.provider.Settings
import android.system.Os
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

enum class TState { Request, Waiting, Xfer, Done, Failed, Declined, Cancelled }

data class FileItem(val name: String, val size: Long)
data class Device(val id: String, val name: String, val kind: String, val drift: Boolean = false)
data class TrustedDevice(val id: String, val name: String, val auto: Boolean)
data class Notice(val title: String, val detail: String?, val lasting: Boolean = false)

class Transfer(
    val share: Long,
    val incoming: Boolean,
    peer: String,
    val files: List<FileItem>,
    pin: String?,
    val drift: Boolean = false,
    trusted: Boolean = false,
    val texts: Int = 0,
    val textKind: String? = null,
) {
    var peer by mutableStateOf(peer)
    var pin by mutableStateOf(pin)
    var trusted by mutableStateOf(trusted)
    var state by mutableStateOf(TState.Request)
    var done by mutableLongStateOf(0L)
    var total by mutableLongStateOf(files.sumOf { it.size })
    var bps by mutableFloatStateOf(0f)
    var medium by mutableStateOf<String?>(null)
    var text by mutableStateOf<String?>(null)
    var error by mutableStateOf<String?>(null)
    val saved = mutableStateListOf<String>()
    val savedUris = mutableStateListOf<Uri>()
    val progress get() = if (total > 0) (done.toFloat() / total).coerceIn(0f, 1f) else 0f

    /** The title inside a sentence ("wants to share a link"); file names keep their case. */
    val phrase: String get() = if (files.size == 1) title else title.replaceFirstChar { it.lowercase() }

    val title: String
        get() = when {
            files.size == 1 -> files[0].name
            files.size > 1 -> "${files.size} files"
            textKind == "url" -> "A link"
            texts > 0 -> "Some text"
            else -> "Nothing"
        }
}

enum class Visibility { TenMinutes, Always, Hidden }

/** App state + the core. One per process; the UI observes it (Compose state, main thread only). */
object Drift {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var app: Context
    private var core: Core? = null
    private var multicast: WifiManager.MulticastLock? = null

    var name by mutableStateOf("")
    var started by mutableStateOf(false)
    var visibility by mutableStateOf(Visibility.TenMinutes)
        private set
    var visibleUntil by mutableLongStateOf(0L)
    var scanning by mutableStateOf(false)
    val devices = mutableStateListOf<Device>()
    val transfers = mutableStateListOf<Transfer>()
    var trusted by mutableStateOf(listOf<TrustedDevice>())
    var notice by mutableStateOf<Notice?>(null)
    /** Things waiting to be sent while the user picks a device (from Share or the picker). */
    var outbox by mutableStateOf<Outbox?>(null)

    /** A folder the user picked (Storage Access Framework tree); null = Download/Drift. */
    var saveTree by mutableStateOf<Uri?>(null)
        private set
    /** Show the link type (Bluetooth, Wi-Fi Direct...) in the status line and transfer screen. */
    var linkDetails by mutableStateOf(false)
        private set
    /** Ink (default), Monolith or Field, as on the desktop. */
    var look by mutableStateOf(Look.Ink)
        private set
    private val prefs get() = app.getSharedPreferences("drift", Context.MODE_PRIVATE)

    val visible get() = visibility != Visibility.Hidden
    val downloadDir: File
        get() = File(Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS), "Drift")

    fun init(ctx: Context) {
        if (core != null) return
        app = ctx.applicationContext
        name = prefs.getString("name", null)?.takeIf { it.isNotBlank() } ?: systemName()
        linkDetails = prefs.getBoolean("link_details", false)
        look = runCatching { Look.valueOf(prefs.getString("look", null) ?: "Ink") }.getOrDefault(Look.Ink)
        // keep the picked folder only while we still hold its permission
        saveTree = prefs.getString("save_tree", null)?.let(Uri::parse)
            ?.takeIf { u -> app.contentResolver.persistedUriPermissions.any { it.uri == u && it.isWritePermission } }
        val c = Core { e -> main.post { onEvent(e) } }
        core = c
        multicast = (app.getSystemService(Context.WIFI_SERVICE) as WifiManager)
            .createMulticastLock("drift-mdns").apply { setReferenceCounted(false); acquire() }
        downloadDir.mkdirs()
        val cfg = JSONObject()
            .put("device_name", name)
            .put("download_dir", downloadDir.absolutePath)
            .put("data_dir", app.filesDir.absolutePath)
        DeviceKey.secretHex(app)?.let { cfg.put("device_secret", it) }
        started = c.start(cfg)
        if (started) show(if (prefs.getBoolean("always_visible", false)) Visibility.Always else Visibility.TenMinutes)
        else notice = Notice("Drift couldn't start", "Restart the app.", lasting = true)
    }

    private fun systemName() = Settings.Global.getString(app.contentResolver, "device_name")?.takeIf { it.isNotBlank() } ?: Build.MODEL

    // ---------------------------------------------------------------- settings

    /** What phones and PCs see. Blank = the phone's own name. */
    fun rename(n: String) {
        val v = n.trim().take(40)
        prefs.edit().putString("name", v).apply()
        name = v.ifEmpty { systemName() }
        cmd("settings", "device_name" to name)
    }

    fun useLook(l: Look) {
        look = l
        prefs.edit().putString("look", l.name).apply()
    }

    fun showLinkDetails(on: Boolean) {
        linkDetails = on
        prefs.edit().putBoolean("link_details", on).apply()
    }

    /** A folder from the system picker, or null for Download/Drift. */
    fun saveInto(tree: Uri?) {
        val cr = app.contentResolver
        saveTree?.let { old -> if (old != tree) runCatching { cr.releasePersistableUriPermission(old, Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION) } }
        if (tree != null) cr.takePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        saveTree = tree
        prefs.edit().putString("save_tree", tree?.toString()).apply()
    }

    /** "Download/Drift", or the picked folder as the user knows it ("Documents/Work", "SD card/Drift"). */
    val saveLabel: String
        get() {
            val t = saveTree ?: return "Download/Drift"
            val id = runCatching { DocumentsContract.getTreeDocumentId(t) }.getOrNull() ?: return "Chosen folder"
            // Files' own storage: "primary:Documents/Work", or "<volume id>:<path>" on an SD card
            if (t.authority == "com.android.externalstorage.documents") {
                val (vol, path) = id.split(':', limit = 2).let { it[0] to it.getOrElse(1) { "" } }
                val root = if (vol == "primary") "" else "SD card"
                return listOf(root, path).filter { it.isNotEmpty() }.joinToString("/").ifEmpty { "Internal storage" }
            }
            // Other providers (the Downloads one gives ids like "msd:1484049"): ask for the folder's name
            val name = runCatching {
                app.contentResolver.query(DocumentsContract.buildDocumentUriUsingTree(t, id), arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME), null, null, null)
                    ?.use { c -> if (c.moveToFirst()) c.getString(0) else null }
            }.getOrNull() ?: return "Chosen folder"
            return if (t.authority == "com.android.providers.downloads.documents" && name != "Download") "Download/$name" else name
        }

    /**
     * The core writes into Download/Drift (it needs a real path); with a picked folder each finished file
     * then moves there through the storage framework. One local copy; a file handle from Kotlin would
     * avoid it (later).
     */
    private fun moveToTree(tree: Uri, path: String, done: (Uri?) -> Unit) = Thread {
        val r = runCatching {
            val f = File(path)
            val cr = app.contentResolver
            val parent = DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree))
            val mime = MimeTypeMap.getSingleton().getMimeTypeFromExtension(f.extension.lowercase()) ?: "application/octet-stream"
            val doc = DocumentsContract.createDocument(cr, parent, mime, f.name) ?: error("couldn't create ${f.name}")
            cr.openOutputStream(doc)!!.use { out -> f.inputStream().use { it.copyTo(out, 1 shl 20) } }
            f.delete()
            doc
        }
        r.exceptionOrNull()?.let { android.util.Log.w("Drift", "move to picked folder failed; kept in Download/Drift", it) }
        main.post { done(r.getOrNull()) }
    }.start()

    private fun cmd(c: String, vararg kv: Pair<String, Any?>) {
        val o = JSONObject().put("c", c)
        for ((k, v) in kv) o.put(k, v)
        core?.send(o)
    }

    // ---------------------------------------------------------------- visibility

    private val hide = Runnable { show(Visibility.Hidden) }

    fun show(v: Visibility) {
        visibility = v
        if (v != Visibility.Hidden) prefs.edit().putBoolean("always_visible", v == Visibility.Always).apply()
        main.removeCallbacks(hide)
        if (v == Visibility.TenMinutes) {
            visibleUntil = System.currentTimeMillis() + 10 * 60_000
            main.postDelayed(hide, 10 * 60_000)
        } else visibleUntil = 0
        cmd("visibility", "everyone" to (v != Visibility.Hidden))
    }

    fun scan(on: Boolean) {
        if (scanning == on) return
        scanning = on
        if (!on) devices.clear()
        cmd("scan", "on" to on)
        // Quick Share adverts heard over BLE go to the core (it parses them, like on Windows)
        if (on) Bluetooth.startScan(app) { data -> cmd("ble_seen", "data" to data.joinToString("") { "%02x".format(it) }) }
        else Bluetooth.stopScan(app)
    }

    // ---------------------------------------------------------------- shares

    fun respond(t: Transfer, accept: Boolean, always: Boolean = false) {
        cmd("respond", "share" to t.share, "accept" to accept)
        if (accept && always) { cmd("trust", "share" to t.share); t.trusted = true }
        t.state = if (accept) TState.Xfer else TState.Declined
    }

    fun cancel(t: Transfer) = cmd("cancel", "share" to t.share)
    fun untrust(id: String) = cmd("untrust", "id" to id)

    /** Send what's in the outbox to a device. Content URIs become paths the core can open:
     *  a symlink named like the file, pointing at our open descriptor. */
    fun send(device: Device) {
        val box = outbox ?: return
        outbox = null
        val dir = File(app.cacheDir, "send/${System.nanoTime()}").apply { mkdirs() }
        val paths = JSONArray()
        for (uri in box.uris) {
            val pfd = runCatching { app.contentResolver.openFileDescriptor(uri, "r") }.getOrNull() ?: continue
            openFds += pfd
            var name = displayName(uri) ?: "file"
            var link = File(dir, name)
            var n = 1
            while (link.exists()) link = File(dir, "${name.substringBeforeLast('.')} (${n++}).${name.substringAfterLast('.', "")}".trimEnd('.'))
            runCatching { Os.symlink("/proc/self/fd/${pfd.fd}", link.absolutePath) }.onSuccess { paths.put(link.absolutePath) }
        }
        val texts = JSONArray()
        box.text?.let { texts.put(JSONObject().put("kind", if (it.startsWith("http://") || it.startsWith("https://")) "url" else "text").put("body", it)) }
        pendingTexts = texts.length()
        pendingKind = box.text?.let { if (it.startsWith("http://") || it.startsWith("https://")) "url" else "text" }
        cmd("send", "device" to device.id, "paths" to paths, "texts" to texts)
    }

    private val openFds = mutableListOf<ParcelFileDescriptor>()
    private var pendingTexts = 0
    private var pendingKind: String? = null

    /** "Send <this>": the file's name, "3 files", "a link" or "some text" (as the desktop's heading). */
    fun outboxTitle(): String {
        val box = outbox ?: return ""
        return when {
            box.uris.size == 1 -> displayName(box.uris[0]) ?: "1 file"
            box.uris.size > 1 -> "${box.uris.size} files"
            box.text != null && (box.text.startsWith("http://") || box.text.startsWith("https://")) -> "a link"
            else -> "some text"
        }
    }

    private fun displayName(uri: Uri): String? =
        runCatching {
            app.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
                if (c.moveToFirst()) c.getString(0) else null
            }
        }.getOrNull() ?: uri.lastPathSegment?.substringAfterLast('/')

    // ---------------------------------------------------------------- events

    private fun find(share: Long) = transfers.firstOrNull { it.share == share }

    private fun onEvent(e: JSONObject) {
        val share = e.optLong("share", -1)
        when (e.optString("t")) {
            "ready" -> {}
            "wifi_join" -> {
                val req = e.getLong("req")
                Wifi.join(app, req, e.getString("ssid"), e.getString("password")) { net, err ->
                    cmd("wifi_joined", "req" to req, "net" to net, "error" to err)
                }
            }
            "wifi_leave" -> Wifi.leave(app, e.getLong("req"))
            "p2p_host" -> {
                val req = e.getLong("req")
                P2p.serve(app, req) { c, err ->
                    cmd("p2p_hosted", "req" to req, "ssid" to c?.ssid, "password" to c?.password,
                        "device_name" to c?.deviceName, "frequency" to c?.frequency, "error" to err)
                }
            }
            "p2p_unhost" -> P2p.unserve(app, e.getLong("req"))
            "wifi_connect" -> {
                val conn = e.getLong("conn")
                Wifi.connect(e.getLong("req"), e.getString("ip"), e.getInt("port")) { fd, err ->
                    cmd("wifi_connected", "conn" to conn, "fd" to fd, "error" to err)
                }
            }
            "bt_advertise" -> Bluetooth.advertise(app, e.optString("data").takeIf { !e.isNull("data") && it.isNotEmpty() }?.let { Bluetooth.hex(it) })
            "bt_listen" -> Bluetooth.listen(app, e.getString("uuid"), e.getInt("port"), Bluetooth.hex(e.getString("token")))
            "bt_connect" -> Bluetooth.connect(app, e.getString("mac"), e.getString("uuid"), e.getInt("port"), Bluetooth.hex(e.getString("token")))
            "warning" -> notice = Notice(e.optString("text"), e.optString("detail").ifEmpty { null }, lasting = true)
            "device_found" -> {
                val d = Device(e.getString("id"), e.optString("name"), e.optString("kind"), e.optBoolean("drift"))
                devices.removeAll { it.id == d.id }
                devices += d
            }
            "device_lost" -> devices.removeAll { it.id == e.optString("id") }
            "incoming" -> {
                val files = e.optJSONArray("files").items { FileItem(it.optString("name"), it.optLong("size")) }
                val t = Transfer(
                    share, true, e.optString("from", "A device"), files, e.optString("pin").ifEmpty { null },
                    drift = e.optBoolean("drift"), trusted = e.optBoolean("trusted"),
                    texts = e.optInt("texts"), textKind = e.optString("text_kind").ifEmpty { null },
                )
                if (e.optBoolean("auto")) t.state = TState.Xfer
                transfers.add(0, t)
                if (t.state == TState.Request) Notices.incoming(app, t)
            }
            "waiting" -> {
                val t = find(share) ?: Transfer(share, false, e.optString("name"), e.optJSONArray("files").items { FileItem(it.optString("name"), it.optLong("size")) }, null, texts = pendingTexts, textKind = pendingKind).also {
                    it.state = TState.Waiting
                    transfers.add(0, it)
                }
                e.optString("pin").ifEmpty { null }?.let { t.pin = it }
            }
            "accepted" -> find(share)?.state = TState.Xfer
            "medium" -> find(share)?.medium = e.optString("medium")
            "progress" -> find(share)?.let {
                it.done = e.optLong("done"); it.total = maxOf(it.total, e.optLong("total")); it.bps = e.optDouble("bps", 0.0).toFloat()
                if (it.state == TState.Waiting || it.state == TState.Request) it.state = TState.Xfer
            }
            "text" -> find(share)?.text = e.optString("text")
            "saved" -> {
                val path = e.optString("path")
                val t = find(share)
                t?.saved?.add(path)
                // show it in Files / Downloads, and get a content URI to open it with
                val tree = saveTree
                if (tree != null) moveToTree(tree, path) { doc ->
                    if (doc != null) t?.savedUris?.add(doc)
                    else MediaScannerConnection.scanFile(app, arrayOf(path), null) { _, uri -> if (uri != null && t != null) main.post { t.savedUris += uri } }
                }
                // show it in Files / Downloads, and get a content URI to open it with
                else MediaScannerConnection.scanFile(app, arrayOf(path), null) { _, uri -> if (uri != null && t != null) main.post { t.savedUris += uri } }
            }
            "done" -> find(share)?.let { t ->
                t.state = when (e.optString("outcome")) {
                    "completed" -> TState.Done
                    "declined" -> TState.Declined
                    "cancelled" -> TState.Cancelled
                    else -> TState.Failed
                }
                if (t.state == TState.Done) t.done = t.total
                t.error = e.optString("error").ifEmpty { null }
                Notices.remove(app, t.share)
                if (!t.incoming && transfers.none { !it.incoming && it.state in setOf(TState.Waiting, TState.Xfer) }) {
                    openFds.forEach { runCatching { it.close() } }
                    openFds.clear()
                }
            }
            "trusted" -> trusted = e.optJSONArray("devices").items { TrustedDevice(it.optString("id"), it.optString("name"), it.optBoolean("auto")) }
        }
    }

    private fun <T> JSONArray?.items(f: (JSONObject) -> T): List<T> =
        if (this == null) emptyList() else (0 until length()).mapNotNull { optJSONObject(it)?.let(f) }
}

/** Files and/or text to send, waiting for a device. */
data class Outbox(val uris: List<Uri>, val text: String?) {
    val label: String
        get() = when {
            uris.size == 1 -> "1 file"
            uris.size > 1 -> "${uris.size} files"
            text != null && (text.startsWith("http://") || text.startsWith("https://")) -> "A link"
            else -> "Some text"
        }
}
