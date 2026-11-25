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

    // The core starts before these are answered (first launch): Bluetooth listening, advertising and
    // scanning failed then, so start them now.
    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        Drift.permissionsChanged()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        takeShare(intent)
    }

    /** "Share > Drift" from another app: hold the items and look for devices. */
    private fun takeShare(i: Intent?) {
        if (i == null) return
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
