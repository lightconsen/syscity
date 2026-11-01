package net.syscity.desktop.helper

import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Build
import android.os.Process
import android.view.Surface
import org.json.JSONObject
import java.io.BufferedReader
import java.io.ByteArrayOutputStream
import java.io.InputStreamReader
import java.io.OutputStream
import java.util.Base64

/**
 * The process that creates the agent's display.
 *
 * It is started by the app over loopback ADB:
 *
 * ```
 * adb shell CLASSPATH=<app.apk> app_process /system/bin net.syscity.desktop.helper.HelperMain
 * ```
 *
 * `app_process` runs a plain Java class with no Activity and no app sandbox, so
 * it inherits the identity of its parent — `adb shell`, i.e. uid 2000. That
 * identity is the whole point: `DisplayManager.createVirtualDisplay` requires
 * `CAPTURE_VIDEO_OUTPUT`, which an ordinary app can never hold.
 *
 * The display's pixels go to an `ImageReader` owned here, because a `Surface`
 * cannot be handed to another process over a `LocalSocket`. Frames therefore
 * leave as PNG, which is the right cost for an agent loop (one observation per
 * turn) and the wrong one for video.
 *
 * Protocol: one JSON object per connection, one JSON object back, then close.
 *
 * ```
 * -> {"op":"ping"}
 * <- {"ok":true,"uid":2000,"api":34,"shell":true}
 * -> {"op":"create","width":1080,"height":2400,"dpi":420}
 * <- {"ok":true,"display":3,"width":1080,"height":2400}
 * ```
 */
object HelperMain {

  /** Abstract socket name; the app connects to the same string. */
  private const val SOCKET_NAME = "syscity.vdisplay"

  /** uid 2000 is `shell`; anything else cannot create a display. */
  private const val SHELL_UID = 2000

  private val displays = HashMap<Int, AgentDisplay>()

  @JvmStatic
  fun main(args: Array<String>) {
    val uid = Process.myUid()
    if (uid != SHELL_UID && uid != 0) {
      // Starting it any other way (an app process, the wrong uid) produces a
      // "permission denied" deep inside the display manager, which reads like a
      // bug in the display code. Say it here instead.
      System.err.println(
        "syscity: the display helper must run as shell (uid $SHELL_UID = adb shell), not uid $uid",
      )
      System.exit(2)
    }

    val server = try {
      LocalServerSocket(SOCKET_NAME)
    } catch (e: Exception) {
      System.err.println("syscity: could not open $SOCKET_NAME: ${e.message}")
      System.exit(3)
      return
    }
    System.out.println("syscity: display helper ready (uid=$uid api=${Build.VERSION.SDK_INT})")
    System.out.flush()

    while (true) {
      val socket = try {
        server.accept()
      } catch (e: Exception) {
        System.err.println("syscity: accept failed: ${e.message}")
        continue
      }
      try {
        socket.use { handle(it) }
      } catch (e: Exception) {
        // One bad request must not take the display down with it.
        System.err.println("syscity: request failed: ${e.message}")
      }
    }
  }

  private fun handle(socket: LocalSocket) {
    val reader = BufferedReader(InputStreamReader(socket.inputStream))
    val line = reader.readLine() ?: return
    val request = try {
      JSONObject(line)
    } catch (e: Exception) {
      write(socket.outputStream, error("BAD_REQUEST", "not JSON: ${e.message}"))
      return
    }
    val answer = try {
      dispatch(request)
    } catch (e: Exception) {
      error("HELPER_ERROR", e.message ?: e::class.java.simpleName)
    }
    write(socket.outputStream, answer)
  }

  private fun write(out: OutputStream, answer: JSONObject) {
    out.write(answer.toString().toByteArray())
    out.write('\n'.code)
    out.flush()
  }

  private fun error(code: String, message: String): JSONObject =
    JSONObject().put("ok", false).put("code", code).put("error", message)

  private fun dispatch(request: JSONObject): JSONObject {
    when (request.optString("op")) {
      "ping" -> return JSONObject()
        .put("ok", true)
        .put("uid", Process.myUid())
        .put("api", Build.VERSION.SDK_INT)
        .put("displays", displays.size)

      "create" -> {
        val display = AgentDisplay.create(
          request.optInt("width", 0),
          request.optInt("height", 0),
          request.optInt("dpi", 0),
        )
        displays[display.id] = display
        return JSONObject()
          .put("ok", true)
          .put("display", display.id)
          .put("width", display.width)
          .put("height", display.height)
      }

      "destroy" -> {
        val id = request.optInt("display", -1)
        val display = displays.remove(id)
          ?: return error("NO_DISPLAY", "no display $id")
        display.release()
        return JSONObject().put("ok", true)
      }

      else -> {
        val id = request.optInt("display", -1)
        val display = displays[id]
          ?: return error("NO_DISPLAY", "no display $id (op=${request.optString("op")})")
        return when (request.optString("op")) {
          "launch" -> {
            val pkg = request.optString("package")
            display.launch(pkg)
            JSONObject().put("ok", true)
          }

          "screenshot" -> display.frame()

          "tap" -> {
            display.tap(request.optInt("x"), request.optInt("y"))
            JSONObject().put("ok", true)
          }

          "swipe" -> {
            display.swipe(
              request.optInt("x"),
              request.optInt("y"),
              request.optInt("x2"),
              request.optInt("y2"),
              request.optInt("duration", 300),
            )
            JSONObject().put("ok", true)
          }

          "text" -> {
            display.type(request.optString("text"))
            JSONObject().put("ok", true)
          }

          "key" -> {
            display.key(request.optString("keycode"))
            JSONObject().put("ok", true)
          }

          else -> error("UNKNOWN_OP", "unknown op ${request.optString("op")}")
        }
      }
    }
  }
}

/**
 * One virtual display, its surface, and the app currently on it.
 *
 * Everything here needs shell identity, which is why it lives in the helper and
 * not in the app: creating the display is `CAPTURE_VIDEO_OUTPUT`, starting an
 * app on a non-default display is `am --display`, and injecting input at that
 * display is `input -d`.
 */
private class AgentDisplay private constructor(
  private val display: VirtualDisplay,
  private val reader: ImageReader,
  val width: Int,
  val height: Int,
) {
  val id: Int = display.display.displayId

  fun release() {
    try {
      display.release()
    } catch (_: Exception) {
      // Already gone; releasing again is not an error worth reporting.
    }
    reader.close()
  }

  /** Start `package`'s launcher activity on this display. */
  fun launch(packageName: String) {
    // `am start --display` puts the activity on this display; without a display
    // id Android would put it on the user's. `-W` makes the call wait for the
    // activity to start, so the caller's next read does not race the launch.
    val component = resolveLauncher(packageName)
    val output = exec(
      arrayOf(
        "am", "start", "-W",
        "--display", id.toString(),
        "-n", component,
        // The launched app must not take the user's screen; the flag keeps it
        // in the display it was started in.
        "--activity-single-top",
      ),
    )
    if (output.contains("Error")) {
      throw IllegalStateException("am start on display $id failed: ${output.trim()}")
    }
  }

  fun tap(x: Int, y: Int) {
    run("input", "-d", id.toString(), "tap", x.toString(), y.toString())
  }

  fun swipe(x1: Int, y1: Int, x2: Int, y2: Int, durationMs: Int) {
    run(
      "input", "-d", id.toString(), "swipe",
      x1.toString(), y1.toString(), x2.toString(), y2.toString(), durationMs.toString(),
    )
  }

  fun type(text: String) {
    // `input text` cannot carry a space; the device shell's own convention is
    // `%s`, so the text is escaped rather than quoted.
    val escaped = text.replace("%", "%%").replace(" ", "%s")
    run("input", "-d", id.toString(), "text", escaped)
  }

  fun key(keycode: String) {
    run("input", "-d", id.toString(), "keyevent", keycode)
  }

  /** The most recent frame, encoded as PNG. */
  fun frame(): JSONObject {
    val image = acquire(deadlineMs = 1200)
      ?: return JSONObject()
        .put("ok", false)
        .put("code", "NO_FRAME")
        .put("error", "the display produced no frame within 1.2 s")

    try {
      val bitmap = toBitmap(image)
      val out = ByteArrayOutputStream()
      bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
      bitmap.recycle()
      val encoded = Base64.getEncoder().encodeToString(out.toByteArray())
      return JSONObject()
        .put("ok", true)
        .put("png", encoded)
        .put("width", image.width)
        .put("height", image.height)
    } finally {
      image.close()
    }
  }

  private fun acquire(deadlineMs: Long): android.media.Image? {
    val until = System.currentTimeMillis() + deadlineMs
    while (System.currentTimeMillis() < until) {
      val latest = try {
        reader.acquireLatestImage()
      } catch (_: IllegalStateException) {
        // The reader is full because nobody closed an earlier image; the next
        // frame will do.
        null
      }
      if (latest != null) return latest
      Thread.sleep(40)
    }
    return null
  }

  /** RGBA_8888 rows may be padded; copy row by row rather than assuming they are not. */
  private fun toBitmap(image: android.media.Image): Bitmap {
    val plane = image.planes[0]
    val buffer = plane.buffer
    val rowStride = plane.rowStride
    val pixelStride = plane.pixelStride
    val rowBytes = image.width * pixelStride
    val bitmap = Bitmap.createBitmap(image.width, image.height, Bitmap.Config.ARGB_8888)
    val pixels = IntArray(image.width)
    for (row in 0 until image.height) {
      buffer.position(row * rowStride)
      buffer.limit(row * rowStride + rowBytes)
      for (column in 0 until image.width) {
        val offset = column * pixelStride
        val r = buffer.get(offset).toInt() and 0xFF
        val g = buffer.get(offset + 1).toInt() and 0xFF
        val b = buffer.get(offset + 2).toInt() and 0xFF
        val a = buffer.get(offset + 3).toInt() and 0xFF
        pixels[column] = (a shl 24) or (r shl 16) or (g shl 8) or b
      }
      bitmap.setPixels(pixels, 0, image.width, 0, row, image.width, 1)
    }
    buffer.position(0)
    return bitmap
  }

  private fun run(vararg command: String) {
    val output = exec(command)
    if (output.contains("Error") || output.contains("Exception")) {
      throw IllegalStateException("${command.first()} failed: ${output.trim()}")
    }
  }

  private fun exec(command: Array<out String>): String {
    val process = ProcessBuilder(*command).redirectErrorStream(true).start()
    val text = process.inputStream.bufferedReader().use { it.readText() }
    process.waitFor()
    return text
  }

  /** `pkg/activity` for the app's launcher activity, as `am -n` wants it. */
  private fun resolveLauncher(packageName: String): String {
    val output = exec(
      arrayOf("cmd", "package", "resolve-activity", "--brief", "-c",
        "android.intent.category.LAUNCHER", packageName),
    )
    val component = output.lineSequence()
      .map(String::trim)
      .lastOrNull { it.contains('/') }
      ?: throw IllegalStateException("$packageName has no launcher activity (${output.trim()})")
    return component
  }

  companion object {
    /**
     * Create a display sized like the user's, or at the caller's size.
     *
     * The flags are the difference between "a second screen" and "a second
     * screen that fights the first one": `OWN_FOCUS` (API 34+) is what lets this
     * display hold focus without taking it from the user's, and
     * `STEAL_TOP_FOCUS_DISABLED` stops it grabbing it anyway.
     */
    fun create(width: Int, height: Int, dpi: Int): AgentDisplay {
      val w = if (width > 0) width else 1080
      val h = if (height > 0) height else 2400
      val densityDpi = if (dpi > 0) dpi else 420

      val reader = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
      var flags = DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC or
        DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_CONTENT_ONLY or
        DisplayManager.VIRTUAL_DISPLAY_FLAG_SUPPORTS_TOUCH or
        DisplayManager.VIRTUAL_DISPLAY_FLAG_ROTATES_WITH_CONTENT
      if (Build.VERSION.SDK_INT >= 33) {
        flags = flags or DisplayManager.VIRTUAL_DISPLAY_FLAG_TRUSTED or
          DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_DISPLAY_GROUP or
          DisplayManager.VIRTUAL_DISPLAY_FLAG_ALWAYS_UNLOCKED
      }
      if (Build.VERSION.SDK_INT >= 34) {
        // Separate focus per display: the user keeps typing on their screen
        // while the agent types on this one.
        flags = flags or DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_FOCUS or
          DisplayManager.VIRTUAL_DISPLAY_FLAG_STEAL_TOP_FOCUS_DISABLED
      }

      val display = createVirtualDisplay(w, h, densityDpi, reader.surface, flags)
      return AgentDisplay(display, reader, w, h)
    }

    /**
     * `DisplayManagerGlobal.createVirtualDisplay`, by reflection.
     *
     * It is a hidden API and its signature has grown across releases (the
     * package name arrived in API 34), so both shapes are tried and the failure
     * reports which one was attempted. Running under `app_process` puts this
     * process outside the hidden-API blocklist, which is why reflection works
     * here and would not from the app.
     */
    private fun createVirtualDisplay(
      width: Int,
      height: Int,
      dpi: Int,
      surface: Surface,
      flags: Int,
    ): VirtualDisplay {
      val global = Class.forName("android.hardware.display.DisplayManagerGlobal")
        .getMethod("getInstance")
        .invoke(null)
        ?: throw IllegalStateException("DisplayManagerGlobal.getInstance() returned null")

      val attempts = mutableListOf<String>()
      // Newest first: API 34 added the calling package, API 30 added the
      // VirtualDisplayConfig argument.
      val argumentLists = listOf(
        arrayOf("syscity-agent", width, height, dpi, surface, flags, null, null, null,
          "net.syscity.desktop"),
        arrayOf("syscity-agent", width, height, dpi, surface, flags, null, null, null),
        arrayOf("syscity-agent", width, height, dpi, surface, flags),
      )
      for (arguments in argumentLists) {
        val types = arguments.map { argument ->
          when (argument) {
            is String -> String::class.java
            is Int -> Int::class.javaPrimitiveType
            is Surface -> Surface::class.java
            else -> Any::class.java
          }
        }
        try {
          val method = global.javaClass.methods.firstOrNull { candidate ->
            candidate.name == "createVirtualDisplay" &&
              candidate.parameterCount == arguments.size &&
              candidate.parameterTypes.indices.all { index ->
                types[index] == Any::class.java || candidate.parameterTypes[index] == types[index]
              }
          } ?: continue
          attempts.add("${method.parameterCount} args")
          val created = method.invoke(global, *arguments)
          if (created is VirtualDisplay) return created
        } catch (e: Exception) {
          attempts.add(
            "${arguments.size} args: ${e.cause?.message ?: e.message}",
          )
        }
      }
      throw IllegalStateException(
        "could not create a virtual display on API ${Build.VERSION.SDK_INT} " +
          "(tried: ${attempts.joinToString("; ").ifEmpty { "no matching method" }})",
      )
    }
  }
}
