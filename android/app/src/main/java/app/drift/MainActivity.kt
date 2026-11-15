package app.drift

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        Drift.init(this)
        requestPermissions(
            arrayOf(
                Manifest.permission.POST_NOTIFICATIONS,
                Manifest.permission.BLUETOOTH_SCAN,
                Manifest.permission.BLUETOOTH_CONNECT,
                Manifest.permission.BLUETOOTH_ADVERTISE,
                Manifest.permission.NEARBY_WIFI_DEVICES,
                "android.permission.ACCESS_LOCAL_NETWORK",
            ), 1,
        )
        takeShare(intent)
        setContent { DriftUi() }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        takeShare(intent)
    }

    /** "Share > Drift" from another app: hold the items and look for devices. */
    private fun takeShare(i: Intent?) {
        if (i == null) return
        if (i.hasExtra("listen_test")) { // debug: can the app itself take an inbound connection?
            val port = i.getIntExtra("listen_test", 47633)
            Thread {
                try {
                    val ss = java.net.ServerSocket(port)
                    android.util.Log.i("Drift.Listen", "listening on $port")
                    ss.soTimeout = 60_000
                    val c = ss.accept()
                    android.util.Log.i("Drift.Listen", "accepted ${c.remoteSocketAddress}")
                    c.getOutputStream().write("app-ok".toByteArray())
                    c.close(); ss.close()
                } catch (e: Exception) {
                    android.util.Log.i("Drift.Listen", "failed: $e")
                }
            }.start()
            return
        }
        if (i.hasExtra("p2p_host")) { P2p.host(this, i.getIntExtra("p2p_host", 5)); return } // debug probe
        i.getStringExtra("p2p_join")?.let { P2p.join(this, it, i.getBooleanExtra("low_latency", false)); return } // debug probe (C1)
        val uris = when (i.action) {
            Intent.ACTION_SEND -> listOfNotNull(i.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java))
            Intent.ACTION_SEND_MULTIPLE -> i.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            // "Open with" from apps that offer no Share: send the file instead of opening it
            Intent.ACTION_VIEW -> listOfNotNull(i.data?.takeIf { it.scheme == "content" || it.scheme == "file" })
            else -> return
        }
        val text = if (uris.isEmpty()) i.getStringExtra(Intent.EXTRA_TEXT) else null
        if (uris.isEmpty() && text == null) return
        Drift.outbox = Outbox(uris, text)
        Drift.scan(true)
    }
}
