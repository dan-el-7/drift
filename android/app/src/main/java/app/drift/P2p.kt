package app.drift

import android.annotation.SuppressLint
import android.content.Context
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pConfig
import android.net.wifi.p2p.WifiP2pDevice
import android.net.wifi.p2p.WifiP2pManager
import android.os.Handler
import android.os.Looper
import android.util.Log

/**
 * Companion path C1 (NOTES.md): the phone joins the PC's Wi-Fi Direct group itself, on p2p0, so it
 * keeps its own Wi-Fi. The phone *starts* the connection (Android only prompts for incoming ones);
 * the PC pairs and admits it in code. Probe for now: `am start ... --es p2p_join <PC name>`.
 */
@SuppressLint("MissingPermission") // NEARBY_WIFI_DEVICES, asked for in MainActivity
object P2p {
    private const val TAG = "Drift.P2p"
    private val main = Handler(Looper.getMainLooper())

    // ---------------------------------------------------------------- hosting (the real path)

    /**
     * The phone hosts a Wi-Fi Direct group for a transfer (NOTES.md, "phone hosts, PC joins"): on p2p0,
     * so the phone keeps its Wi-Fi; Android picks a non-DFS 5 GHz channel. It is a *saved* group with
     * a name and passphrase fixed per install: Android asks the user the first time a device joins,
     * and lets a device that joined before back in without asking, as long as the group stays the same.
     * Several transfers may share it; it ends when the last one lets go ([unserve]).
     */
    class Creds(val ssid: String, val password: String, val deviceName: String?, val frequency: Int)

    private var hostCh: WifiP2pManager.Channel? = null
    private val serving = mutableSetOf<Long>()
    private var listenWatch: Runnable? = null

    /** Name and passphrase for our saved group, made once per install. */
    private fun groupCreds(ctx: Context): Pair<String, String> {
        val prefs = ctx.getSharedPreferences("drift", Context.MODE_PRIVATE)
        val abc = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789"
        val rnd = java.security.SecureRandom()
        fun pick(n: Int) = (1..n).map { abc[rnd.nextInt(abc.length)] }.joinToString("")
        val name = prefs.getString("p2p_ssid", null) ?: "DIRECT-${pick(2)}-Drift".also { prefs.edit().putString("p2p_ssid", it).apply() }
        val pass = prefs.getString("p2p_pass", null) ?: pick(20).also { prefs.edit().putString("p2p_pass", it).apply() }
        return name to pass
    }

    fun serve(ctx: Context, req: Long, reply: (Creds?, String?) -> Unit) {
        val mgr = ctx.getSystemService(WifiP2pManager::class.java) ?: return reply(null, "no Wi-Fi Direct")
        val ch = hostCh ?: mgr.initialize(ctx, Looper.getMainLooper(), null).also { hostCh = it }
        val (ssid, pass) = groupCreds(ctx)
        val t0 = System.currentTimeMillis()
        fun log(m: String) = Log.i(TAG, "%5.1fs serve %d: %s".format((System.currentTimeMillis() - t0) / 1000f, req, m))
        serving += req
        closeLater?.let { main.removeCallbacks(it); closeLater = null } // the warm group is wanted again

        fun answer(g: android.net.wifi.p2p.WifiP2pGroup) {
            mgr.requestDeviceInfo(ch) { me ->
                log("group ${g.networkName} on ${g.frequency} MHz, device ${me?.deviceName}")
                reply(Creds(g.networkName, g.passphrase ?: pass, me?.deviceName, g.frequency), null)
                listenUntilJoined(ctx, mgr, ch)
                invite(ctx, mgr, ch, g, ::log)
            }
        }
        fun waitForGroup(tries: Int) {
            mgr.requestGroupInfo(ch) { g ->
                when {
                    g != null && g.isGroupOwner -> answer(g)
                    tries < 30 -> main.postDelayed({ waitForGroup(tries + 1) }, 250)
                    else -> { log("group never came up"); serving -= req; reply(null, "the group didn't come up") }
                }
            }
        }
        fun create() {
            val cfg = WifiP2pConfig.Builder()
                .setNetworkName(ssid)
                .setPassphrase(pass)
                .setGroupOperatingBand(WifiP2pConfig.GROUP_OWNER_BAND_5GHZ)
                .enablePersistentMode(true)
                .build()
            mgr.createGroup(ch, cfg, object : WifiP2pManager.ActionListener {
                override fun onSuccess() { log("createGroup ok"); waitForGroup(0) }
                override fun onFailure(r: Int) { log("createGroup failed $r"); serving -= req; reply(null, "couldn't start a Wi-Fi Direct group ($r)") }
            })
        }
        // ours already up (another transfer)? share it; anything else in the way goes first
        mgr.requestGroupInfo(ch) { g ->
            if (g != null && g.isGroupOwner && g.networkName == ssid) { log("sharing the running group"); answer(g) }
            else if (g != null) mgr.removeGroup(ch, object : WifiP2pManager.ActionListener {
                override fun onSuccess() { create() }
                override fun onFailure(r: Int) { create() }
            })
            else create()
        }
    }

    /** Findable (listen state) only until a client is in: listening hops channels and costs speed. */
    /**
     * Invite the device that joined last (a PC that's been here before): Windows otherwise only finds us
     * once its own scan notices the phone (7–30 s). Our group is saved and that device is a member, so
     * an invitation lets it straight back in.
     */
    private fun invite(ctx: Context, mgr: WifiP2pManager, ch: WifiP2pManager.Channel, g: android.net.wifi.p2p.WifiP2pGroup, log: (String) -> Unit) {
        val addr = ctx.getSharedPreferences("drift", Context.MODE_PRIVATE).getString("p2p_last_client", null) ?: return
        if (g.clientList.any { it.deviceAddress.equals(addr, ignoreCase = true) }) return
        // An invitation needs the device in the P2P peer table, i.e. seen by a search: search, and invite
        // the moment it shows up (the PC is scanning for us at the same time, so it answers searches).
        val until = System.currentTimeMillis() + 15_000
        fun tryInvite() {
            mgr.requestPeers(ch) { peers ->
                val seen = peers?.deviceList?.any { it.deviceAddress.equals(addr, ignoreCase = true) } == true
                when {
                    seen -> {
                        val cfg = WifiP2pConfig().apply { deviceAddress = addr; wps.setup = WpsInfo.PBC }
                        mgr.connect(ch, cfg, object : WifiP2pManager.ActionListener {
                            override fun onSuccess() { log("invited $addr") }
                            override fun onFailure(r: Int) { log("invite $addr failed $r") }
                        })
                    }
                    System.currentTimeMillis() < until -> main.postDelayed({
                        mgr.requestGroupInfo(ch) { g2 -> if (g2 != null && g2.clientList.isEmpty()) tryInvite() } // stop once it joined
                    }, 300)
                    else -> log("never saw $addr to invite")
                }
            }
        }
        mgr.discoverPeers(ch, object : WifiP2pManager.ActionListener {
            override fun onSuccess() { log("searching for $addr"); tryInvite() }
            override fun onFailure(r: Int) { log("search failed $r") }
        })
    }

    private fun listenUntilJoined(ctx: Context, mgr: WifiP2pManager, ch: WifiP2pManager.Channel) {
        listenWatch?.let { main.removeCallbacks(it) }
        mgr.startListening(ch, null)
        val started = System.currentTimeMillis()
        val w = object : Runnable {
            override fun run() {
                mgr.requestGroupInfo(ch) { g ->
                    // remember who joined, to invite them next time
                    g?.clientList?.firstOrNull()?.deviceAddress?.let {
                        ctx.getSharedPreferences("drift", Context.MODE_PRIVATE).edit().putString("p2p_last_client", it).apply()
                    }
                    if (g == null || g.clientList.isNotEmpty() || System.currentTimeMillis() - started > 60_000) {
                        mgr.stopListening(ch, null); mgr.stopPeerDiscovery(ch, null)
                        listenWatch = null
                    } else main.postDelayed(this, 400)
                }
            }
        }
        listenWatch = w
        main.postDelayed(w, 400)
    }

    /**
     * Start over with Wi-Fi Direct: a new saved group (new name and passphrase) and no remembered PC.
     * Apps can't delete Android's saved groups, but a new group makes every PC pair again (one prompt).
     */
    fun resetPairing(ctx: Context) {
        ctx.getSharedPreferences("drift", Context.MODE_PRIVATE).edit()
            .remove("p2p_ssid").remove("p2p_pass").remove("p2p_last_client").apply()
        val mgr = ctx.getSystemService(WifiP2pManager::class.java) ?: return
        val ch = hostCh ?: mgr.initialize(ctx, Looper.getMainLooper(), null).also { hostCh = it }
        if (serving.isEmpty()) mgr.removeGroup(ch, null)
        Log.i(TAG, "Wi-Fi Direct pairing reset")
    }

    private var closeLater: Runnable? = null

    fun unserve(ctx: Context, req: Long) {
        if (!serving.remove(req) || serving.isNotEmpty()) return
        // Keep the group a minute: a PC that just used it reconnects at once for the next share. A new
        // serve() within that minute shares the running group and cancels this.
        closeLater?.let { main.removeCallbacks(it) }
        val r = Runnable { closeLater = null; if (serving.isEmpty()) closeNow(ctx, req) }
        closeLater = r
        main.postDelayed(r, 60_000)
    }

    private fun closeNow(ctx: Context, req: Long) {
        val mgr = ctx.getSystemService(WifiP2pManager::class.java) ?: return
        val ch = hostCh ?: return
        listenWatch?.let { main.removeCallbacks(it) }; listenWatch = null
        mgr.stopListening(ch, null)
        mgr.removeGroup(ch, null)
        Log.i(TAG, "serve $req: group closed")
    }

    // ---------------------------------------------------------------- probes

    /**
     * Probe: the phone hosts a saved (persistent) 5 GHz group for the PC to join as a P2P client.
     * Android asks the user for a fresh join; the question is whether the PC's later re-invocations of
     * this saved group get in without asking (wpa_supplicant persistent_reconnect).
     */
    fun host(ctx: Context, minutes: Int = 5) {
        val mgr = ctx.getSystemService(WifiP2pManager::class.java) ?: return
        val ch = mgr.initialize(ctx, Looper.getMainLooper(), null)
        val t0 = System.currentTimeMillis()
        fun log(m: String) = Log.i(TAG, "%5.1fs host: %s".format((System.currentTimeMillis() - t0) / 1000f, m))
        fun report(tries: Int) {
            mgr.requestGroupInfo(ch) { g ->
                if (g != null) {
                    log("group ${g.networkName} freq ${g.frequency} netId ${g.networkId} clients ${g.clientList.map { it.deviceName }}")
                    // a group owner is only findable while listening/discovering: do so until the PC is in
                    mgr.startListening(ch, object : WifiP2pManager.ActionListener {
                        override fun onSuccess() { log("listening") }
                        override fun onFailure(r: Int) {
                            log("startListening failed $r; discovering instead")
                            mgr.discoverPeers(ch, object : WifiP2pManager.ActionListener {
                                override fun onSuccess() { log("discovering") }
                                override fun onFailure(r: Int) { log("discoverPeers failed $r") }
                            })
                        }
                    })
                }
                if (g == null && tries < 20) main.postDelayed({ report(tries + 1) }, 500)
            }
        }
        val create = {
            // Default createGroup = persistent, Android picks the channel. Prefer 5 GHz via the builder's band,
            // but the builder needs a name/passphrase we keep stable so the saved group is the same each time.
            val (name, pass) = groupCreds(ctx) // the app's real saved group, so a paired PC gets in
            val cfg = WifiP2pConfig.Builder()
                .setNetworkName(name)
                .setPassphrase(pass)
                .setGroupOperatingBand(WifiP2pConfig.GROUP_OWNER_BAND_5GHZ)
                .enablePersistentMode(true)
                .build()
            mgr.createGroup(ch, cfg, object : WifiP2pManager.ActionListener {
                override fun onSuccess() {
                    log("createGroup ok"); report(0)
                }
                override fun onFailure(r: Int) { log("createGroup failed $r") }
            })
        }
        mgr.removeGroup(ch, object : WifiP2pManager.ActionListener {
            override fun onSuccess() { create() }
            override fun onFailure(r: Int) { create() }
        })
        // log joins as they happen, then leave
        val watch = object : Runnable {
            var last = ""
            override fun run() {
                mgr.requestGroupInfo(ch) { g ->
                    val now = g?.clientList?.joinToString { it.deviceName } ?: "(no group)"
                    if (now != last) {
                        last = now; log("clients: $now")
                        if (g?.clientList?.isNotEmpty() == true) { mgr.stopListening(ch, null); mgr.stopPeerDiscovery(ch, null) } // both hop channels
                    }
                }
                if (System.currentTimeMillis() - t0 < minutes * 60_000L) main.postDelayed(this, 500)
                else mgr.removeGroup(ch, null).also { log("removed") }
            }
        }
        hostWatch?.let { main.removeCallbacks(it) }
        hostWatch = watch
        main.postDelayed(watch, 1000)
    }
    private var hostWatch: Runnable? = null

    fun join(ctx: Context, name: String, lowLatency: Boolean = false, done: (String?, String?) -> Unit = { _, _ -> }) {
        val mgr = ctx.getSystemService(WifiP2pManager::class.java) ?: return done(null, "no Wi-Fi Direct")
        val ch = mgr.initialize(ctx, Looper.getMainLooper(), null)
        val t0 = System.currentTimeMillis()
        fun log(m: String) = Log.i(TAG, "%5.1fs %s".format((System.currentTimeMillis() - t0) / 1000f, m))
        var connecting = false
        var tries = 0 // discovery polls
        var waits = 0 // group polls, counted apart so a slow find doesn't shorten the join wait

        fun waitForGroup() {
            mgr.requestConnectionInfo(ch) { info ->
                if (info != null && info.groupFormed && !info.isGroupOwner) {
                    log("joined; group owner ${info.groupOwnerAddress?.hostAddress}")
                    mgr.stopPeerDiscovery(ch, null) // discovery hops channels: never keep it running on a live link
                    // Probe: Wi-Fi power save off while the link is used (public API; works while we're in front).
                    if (lowLatency) runCatching {
                        val wm = ctx.getSystemService(android.net.wifi.WifiManager::class.java)
                        val lock = wm.createWifiLock(android.net.wifi.WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "drift-p2p")
                        lock.acquire(); log("low-latency lock held")
                        main.postDelayed({ lock.release() }, 180_000)
                    }.onFailure { log("low-latency lock: $it") }
                    done(info.groupOwnerAddress?.hostAddress, null)
                } else if (++waits < 60) main.postDelayed({ waitForGroup() }, 500)
                else { log("never joined"); done(null, "never joined") }
            }
        }

        fun connect(d: WifiP2pDevice) {
            connecting = true
            log("found ${d.deviceName} ${d.deviceAddress} (status ${d.status}); connecting (push-button)")
            val cfg = WifiP2pConfig().apply {
                deviceAddress = d.deviceAddress
                wps.setup = WpsInfo.PBC
                groupOwnerIntent = 0 // the PC is the group owner
            }
            // A fresh, temporary group every time. By default Android re-invokes the group it saved
            // last time (an invitation), which the PC answers "information unavailable": the first
            // join works, every later one fails. netId -1 = WifiP2pGroup.NETWORK_ID_TEMPORARY.
            runCatching { WifiP2pConfig::class.java.getField("netId").setInt(cfg, -1) }
                .onFailure { log("couldn't ask for a temporary group: $it") }
            mgr.connect(ch, cfg, object : WifiP2pManager.ActionListener {
                override fun onSuccess() { log("connect started"); waitForGroup() }
                override fun onFailure(r: Int) { log("connect failed $r"); done(null, "connect failed $r") }
            })
        }

        fun look() {
            if (connecting) return
            mgr.requestPeers(ch) { peers ->
                val d = peers?.deviceList?.firstOrNull { it.deviceName.startsWith(name, ignoreCase = true) }
                when {
                    d != null -> connect(d)
                    ++tries < 40 -> main.postDelayed({ look() }, 500)
                    else -> { log("not found: ${peers?.deviceList?.map { it.deviceName }}"); done(null, "not found") }
                }
            }
        }

        val discover = {
            mgr.discoverPeers(ch, object : WifiP2pManager.ActionListener {
                override fun onSuccess() { log("discovering"); look() }
                override fun onFailure(r: Int) { log("discover failed $r"); done(null, "discover failed $r") }
            })
        }
        // a leftover group or connect attempt keeps the P2P stack busy: leave/clear it first
        val clear = object : WifiP2pManager.ActionListener {
            override fun onSuccess() { discover() }
            override fun onFailure(r: Int) { discover() }
        }
        mgr.removeGroup(ch, object : WifiP2pManager.ActionListener {
            override fun onSuccess() { mgr.cancelConnect(ch, clear) }
            override fun onFailure(r: Int) { mgr.cancelConnect(ch, clear) }
        })
    }
}
