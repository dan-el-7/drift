package app.drift

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiNetworkSpecifier
import android.util.Log
import java.util.concurrent.ConcurrentHashMap

/**
 * Joining the network a Drift PC hosts for a transfer (the core asks with `wifi_join`).
 * A local-only request: Android keeps the phone's normal Wi-Fi/data for everything else (on phones
 * with STA+STA concurrency both stay up), and only sockets the core binds to it use this network.
 */
object Wifi {
    private const val TAG = "Drift.Wifi"
    private val requests = ConcurrentHashMap<Long, ConnectivityManager.NetworkCallback>()
    private val networks = ConcurrentHashMap<Long, Network>()

    /** TCP over a joined network, through its own socket factory; the fd goes to the core. */
    fun connect(req: Long, ip: String, port: Int, done: (Int?, String?) -> Unit) {
        val net = networks[req] ?: return done(null, "not joined")
        Thread({
            runCatching {
                val s = net.socketFactory.createSocket()
                s.tcpNoDelay = true
                s.connect(java.net.InetSocketAddress(ip, port), 3000)
                val fd = android.os.ParcelFileDescriptor.fromSocket(s).detachFd() // a dup, now the core's
                s.close()
                fd
            }.onSuccess { done(it, null) }.onFailure { done(null, it.message ?: it.toString()) }
        }, "drift-wifi-connect").start()
    }

    fun join(ctx: Context, req: Long, ssid: String, password: String, done: (Long?, String?) -> Unit) {
        val cm = ctx.getSystemService(ConnectivityManager::class.java)
        val spec = WifiNetworkSpecifier.Builder().setSsid(ssid).setWpa2Passphrase(password).build()
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .setNetworkSpecifier(spec)
            .build()
        var answered = false
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(n: Network) {
                Log.i(TAG, "joined $ssid")
                networks[req] = n
                if (!answered) { answered = true; done(n.networkHandle, null) }
            }
            override fun onUnavailable() {
                Log.w(TAG, "couldn't join $ssid")
                if (!answered) { answered = true; done(null, "unavailable") }
            }
        }
        requests[req] = cb
        runCatching { cm.requestNetwork(request, cb, 55_000) }.onFailure {
            requests.remove(req)
            done(null, it.message ?: "request failed")
        }
    }

    /** Done with it: Android leaves the network once nothing else asks for it. */
    fun leave(ctx: Context, req: Long) {
        networks.remove(req)
        val cb = requests.remove(req) ?: return
        runCatching { ctx.getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(cb) }
        Log.i(TAG, "left ($req)")
    }
}
