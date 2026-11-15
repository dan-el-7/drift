package app.drift

import org.json.JSONObject

/**
 * The Rust core (drift_ffi), the same one the Windows app runs. JSON commands in, JSON events out
 * (ordered, on the core's event thread). Shapes: ui/windows/Core/Protocol.cs.
 */
class Core(private val sink: (JSONObject) -> Unit) {
    private var handle = 0L

    fun start(config: JSONObject): Boolean {
        if (handle == 0L) handle = start(config.toString())
        return handle != 0L
    }

    fun send(cmd: JSONObject): Int = if (handle != 0L) command(handle, cmd.toString()) else -2

    fun stop() {
        val h = handle
        handle = 0
        if (h != 0L) stop(h)
    }

    /** Called from Rust (JNI) for every event. */
    @Suppress("unused")
    fun onEvent(json: String) {
        runCatching { JSONObject(json) }.onSuccess(sink)
    }

    private external fun start(config: String): Long
    private external fun command(handle: Long, json: String): Int
    private external fun stop(handle: Long)

    companion object {
        init { System.loadLibrary("drift_ffi") }
    }
}
