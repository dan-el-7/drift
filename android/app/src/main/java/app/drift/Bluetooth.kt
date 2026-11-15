package app.drift

import android.annotation.SuppressLint
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothSocket
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.os.ParcelUuid
import android.util.Log
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.Socket
import java.util.UUID

/**
 * Bluetooth for the core. Android only lets apps use Bluetooth through its Java API, so the app
 * does the radio work and the Rust core does the protocol:
 *  - while looking for devices, Quick Share adverts (0xFEF3 service data) go to the core, which
 *    parses them like the Windows scanner does (a PC's named advert carries its Bluetooth address);
 *  - when the core wants an RFCOMM link (`bt_connect`), we connect, dial its one-shot loopback
 *    port, write its token, and pump bytes both ways.
 */
@SuppressLint("MissingPermission") // asked for in MainActivity; every call is guarded
object Bluetooth {
    private const val TAG = "Drift.BT"
    private val COPRESENCE = ParcelUuid.fromString("0000fef3-0000-1000-8000-00805f9b34fb")
    private var callback: ScanCallback? = null

    private fun adapter(ctx: Context) = ctx.getSystemService(BluetoothManager::class.java)?.adapter

    fun startScan(ctx: Context, onAdvert: (ByteArray) -> Unit) {
        if (callback != null) return
        val scanner = runCatching { adapter(ctx)?.bluetoothLeScanner }.getOrNull() ?: return
        val cb = object : ScanCallback() {
            override fun onScanResult(type: Int, r: ScanResult) {
                r.scanRecord?.getServiceData(COPRESENCE)?.let(onAdvert)
            }
            override fun onBatchScanResults(rs: MutableList<ScanResult>) = rs.forEach { onScanResult(0, it) }
            override fun onScanFailed(code: Int) { Log.w(TAG, "scan failed $code") }
        }
        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .setLegacy(false) // PCs send the extended (named) advert: it carries their Bluetooth address
            .setPhy(ScanSettings.PHY_LE_ALL_SUPPORTED)
            .build()
        val filters = listOf(ScanFilter.Builder().setServiceData(COPRESENCE, ByteArray(0), ByteArray(0)).build())
        runCatching { scanner.startScan(filters, settings, cb); callback = cb }
            .onFailure { Log.w(TAG, "startScan: $it") }
    }

    fun stopScan(ctx: Context) {
        val cb = callback ?: return
        callback = null
        runCatching { adapter(ctx)?.bluetoothLeScanner?.stopScan(cb) }
    }

    /** Open RFCOMM to [mac] and bridge it to the core's loopback [port]. */
    fun connect(ctx: Context, mac: String, uuid: String, port: Int, token: ByteArray) {
        Thread({
            val bt: BluetoothSocket? = runCatching {
                val a = adapter(ctx) ?: error("no Bluetooth")
                runCatching { a.cancelDiscovery() }
                val s = a.getRemoteDevice(mac).createInsecureRfcommSocketToServiceRecord(UUID.fromString(uuid))
                s.connect()
                s
            }.onFailure { Log.w(TAG, "connect $mac: $it") }.getOrNull()
            // the core listens on 127.0.0.1 (getLoopbackAddress() may be ::1)
            val local = runCatching { Socket(InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1)), port).apply { tcpNoDelay = true } }
                .onFailure { Log.w(TAG, "loopback $port: $it") }.getOrNull()
            if (local == null) { runCatching { bt?.close() }; return@Thread }
            if (bt == null) {
                runCatching { local.getOutputStream().write(ByteArray(16)); local.close() } // tell the core it failed
                return@Thread
            }
            runCatching { local.getOutputStream().write(token); local.getOutputStream().flush() }
            Log.i(TAG, "bridged $mac")
            pump(bt.inputStream, local.getOutputStream()) { runCatching { bt.close() }; runCatching { local.close() } }
            pump(local.getInputStream(), bt.outputStream) { runCatching { bt.close() }; runCatching { local.close() } }
        }, "drift-bt-connect").start()
    }

    private fun pump(src: InputStream, dst: OutputStream, done: () -> Unit) {
        Thread({
            val buf = ByteArray(64 * 1024)
            try {
                while (true) {
                    val n = src.read(buf)
                    if (n < 0) break
                    dst.write(buf, 0, n)
                    dst.flush()
                }
            } catch (_: Exception) {
            } finally { done() }
        }, "drift-bt-pump").start()
    }

    // ------------------------------------------------------------ being found (receiving)

    private var advertising: AdvertiseCallback? = null

    /** Advertise the core's 0xFEF3 service data (null = stop). Non-connectable: PCs reach us over
     *  Bluetooth Classic (RFCOMM), the advert only says we're here. */
    fun advertise(ctx: Context, data: ByteArray?) {
        val adv = runCatching { adapter(ctx)?.bluetoothLeAdvertiser }.onFailure { Log.w(TAG, "advertiser: $it") }.getOrNull()
        Log.i(TAG, "advertise ${data?.size ?: "stop"} bytes, advertiser=${adv != null}")
        advertising?.let { cb -> runCatching { adv?.stopAdvertising(cb) } }
        advertising = null
        if (data == null || adv == null) return
        val cb = object : AdvertiseCallback() {
            override fun onStartFailure(code: Int) { Log.w(TAG, "advertise failed $code") }
            override fun onStartSuccess(s: AdvertiseSettings) { Log.i(TAG, "advertising") }
        }
        val settings = AdvertiseSettings.Builder()
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
            .setConnectable(false)
            .setTimeout(0)
            .build()
        val ad = AdvertiseData.Builder().setIncludeDeviceName(false).addServiceData(COPRESENCE, data).build()
        runCatching { adv.startAdvertising(settings, ad, cb); advertising = cb }.onFailure { Log.w(TAG, "startAdvertising: $it") }
    }

    private var server: BluetoothServerSocket? = null

    /** Accept RFCOMM links on Drift's service and bridge each into the core's loopback [port]:
     *  token, then the peer's address (6 bytes), then the stream. */
    fun listen(ctx: Context, uuid: String, port: Int, token: ByteArray) {
        runCatching { server?.close() }
        val ss = runCatching { adapter(ctx)?.listenUsingInsecureRfcommWithServiceRecord("Drift", UUID.fromString(uuid)) }
            .onFailure { Log.w(TAG, "listen: $it") }.getOrNull() ?: return
        server = ss
        Thread({
            while (true) {
                val bt = runCatching { ss.accept() }.getOrNull() ?: break
                Thread({
                    val local = runCatching { Socket(InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1)), port).apply { tcpNoDelay = true } }
                        .onFailure { Log.w(TAG, "loopback $port: $it") }.getOrNull()
                    if (local == null) { runCatching { bt.close() }; return@Thread }
                    val mac = hex(bt.remoteDevice.address.replace(":", ""))
                    runCatching { local.getOutputStream().apply { write(token); write(mac); flush() } }
                    Log.i(TAG, "link in from ${bt.remoteDevice.address}")
                    pump(bt.inputStream, local.getOutputStream()) { runCatching { bt.close() }; runCatching { local.close() } }
                    pump(local.getInputStream(), bt.outputStream) { runCatching { bt.close() }; runCatching { local.close() } }
                }, "drift-bt-in").start()
            }
        }, "drift-bt-listen").start()
    }

    fun hex(s: String): ByteArray = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
}
