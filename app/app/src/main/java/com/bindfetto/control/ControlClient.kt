package com.bindfetto.control

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket

/**
 * Talks to the bindfetto runtime's control channel (the `--control` TCP server).
 *
 * The protocol is line-oriented text (see `runtime/bindfetto/src/main.rs`):
 *
 *  * `STATUS`      -> `key=value` lines, then `END`.
 *  * `START`/`STOP` -> toggle capture; reply `OK`.
 *  * `SINK <mode>` -> switch text sink; reply `OK`/`ERR`.
 *  * `DLT on|off`  -> toggle DLT streaming; reply `OK`.
 *  * `TRACK on|off`-> toggle interface discovery; reply `OK`.
 *  * `LIST`        -> every interface descriptor seen so far, one per line, then `END`.
 *  * `GET`         -> the interfaces in the active filter, one per line, then `END`.
 *  * `SET a,b,c`   -> replace the in-kernel filter; reply `OK <n>` or `ERR <msg>`.
 *  * `CLEAR`       -> disable filtering; reply `OK 0`.
 *
 * A fresh socket is opened per call: the command set is tiny and infrequent (a UI tap),
 * so a short-lived connection is simpler than holding one open and reconnecting.
 */
class ControlClient(private val host: String, private val port: Int) {

    /** Runtime status (`STATUS`) as key=value pairs. */
    suspend fun status(): Map<String, String> = withContext(Dispatchers.IO) {
        useSocket { reader, out ->
            out.write("STATUS\n".toByteArray()); out.flush()
            val map = LinkedHashMap<String, String>()
            while (true) {
                val line = reader.readLine() ?: break
                if (line == "END") break
                val eq = line.indexOf('=')
                if (eq > 0) map[line.substring(0, eq)] = line.substring(eq + 1)
            }
            map
        }
    }

    /** Toggle capture (`START`/`STOP`). */
    suspend fun setCapturing(on: Boolean): String = simple(if (on) "START" else "STOP")

    /** Switch the text sink (`SINK console|logcat|both|none`). */
    suspend fun setSink(mode: String): String = simple("SINK $mode")

    /** Toggle DLT streaming (`DLT on|off`). */
    suspend fun setDlt(on: Boolean): String = simple("DLT " + if (on) "on" else "off")

    /** Toggle interface discovery (`TRACK on|off`). */
    suspend fun setTracking(on: Boolean): String = simple("TRACK " + if (on) "on" else "off")

    /** Interfaces bindfetto has observed (`LIST`). */
    suspend fun list(): List<String> = readListCommand("LIST")

    /** Interfaces in the currently-active in-kernel filter (`GET`). */
    suspend fun activeFilter(): List<String> = readListCommand("GET")

    /**
     * Replace the in-kernel filter with [interfaces] (`SET`). An empty list clears the
     * filter. Returns the server's reply line (`OK <n>` / `ERR <msg>`).
     */
    suspend fun set(interfaces: List<String>): String = withContext(Dispatchers.IO) {
        useSocket { reader, out ->
            val line = if (interfaces.isEmpty()) "CLEAR" else "SET " + interfaces.joinToString(",")
            out.write((line + "\n").toByteArray())
            out.flush()
            reader.readLine()?.trim() ?: "ERR no response"
        }
    }

    /** Send a command that replies with a single line (`OK`/`ERR …`). */
    private suspend fun simple(command: String): String = withContext(Dispatchers.IO) {
        useSocket { reader, out ->
            out.write((command + "\n").toByteArray()); out.flush()
            reader.readLine()?.trim() ?: "ERR no response"
        }
    }

    private suspend fun readListCommand(command: String): List<String> =
        withContext(Dispatchers.IO) {
            useSocket { reader, out ->
                out.write((command + "\n").toByteArray())
                out.flush()
                val items = ArrayList<String>()
                while (true) {
                    val line = reader.readLine() ?: break
                    if (line == "END") break
                    if (line.isNotBlank()) items.add(line.trim())
                }
                items
            }
        }

    private inline fun <T> useSocket(block: (BufferedReader, OutputStream) -> T): T {
        Socket().use { socket ->
            socket.connect(InetSocketAddress(host, port), CONNECT_TIMEOUT_MS)
            socket.soTimeout = READ_TIMEOUT_MS
            val reader = BufferedReader(InputStreamReader(socket.getInputStream()))
            val out = socket.getOutputStream()
            return block(reader, out)
        }
    }

    companion object {
        private const val CONNECT_TIMEOUT_MS = 3000
        private const val READ_TIMEOUT_MS = 4000
    }
}
