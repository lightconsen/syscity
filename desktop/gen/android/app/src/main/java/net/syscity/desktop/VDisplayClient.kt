package net.syscity.desktop

import android.net.LocalSocket
import android.net.LocalSocketAddress
import org.json.JSONObject
import java.io.IOException

/**
 * Client for the shell-side display helper ([net.syscity.desktop.helper.HelperMain]).
 *
 * One request per connection: write a JSON line, half-close, read one JSON line
 * back. That is enough for a control channel that carries a handful of calls
 * per agent turn, and it means a helper that dies mid-request cannot leave this
 * side waiting on a half-open stream that never ends.
 *
 * Throws [IOException] when the helper is not running — the Rust side maps that
 * to "unavailable, start it", which is a different thing from a helper that is
 * up and refusing.
 */
object VDisplayClient {

  private const val SOCKET_NAME = "syscity.vdisplay"

  /** Send one request. Blocks; callers run it off the main thread. */
  @Throws(IOException::class)
  fun request(payload: JSONObject): JSONObject {
    val socket = LocalSocket()
    try {
      socket.connect(
        LocalSocketAddress(SOCKET_NAME, LocalSocketAddress.Namespace.ABSTRACT),
      )
      socket.outputStream.apply {
        write((payload.toString() + "\n").toByteArray())
        flush()
      }
      // Half-close: the helper reads one line and answers; without this its
      // read would wait for more input that is never coming.
      socket.shutdownOutput()
      val line = socket.inputStream.bufferedReader().readLine()
        ?: throw IOException("the display helper closed the connection without answering")
      return JSONObject(line)
    } finally {
      try {
        socket.close()
      } catch (_: IOException) {
        // Closing a socket that already failed is not a second failure.
      }
    }
  }

  /** Whether a helper is listening, without creating anything. */
  fun isAvailable(): Boolean = try {
    request(JSONObject().put("op", "ping")).optBoolean("ok", false)
  } catch (_: Exception) {
    false
  }
}
