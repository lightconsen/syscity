package net.syscity.desktop.shell

import android.content.Context
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.os.Build
import android.os.ParcelFileDescriptor
import android.view.Surface
import java.io.File
import java.io.FileOutputStream

/**
 * `DisplayManager.VIRTUAL_DISPLAY_FLAG_*`, by value.
 *
 * Only the first five are public API. The rest are `@hide`: they exist in the
 * platform and it honours them, but they are absent from the SDK stubs, so the
 * names do not compile. This process runs under `app_process`, outside the
 * hidden-API check, which is exactly why it can use the values the app cannot.
 */
private object DisplayFlags {
    const val PUBLIC = 1 shl 0
    const val OWN_CONTENT_ONLY = 1 shl 3
    const val SUPPORTS_TOUCH = 1 shl 6
    const val ROTATES_WITH_CONTENT = 1 shl 7
    const val TRUSTED = 1 shl 10
    const val OWN_DISPLAY_GROUP = 1 shl 11
    const val ALWAYS_UNLOCKED = 1 shl 12
    const val OWN_FOCUS = 1 shl 13
    const val STEAL_TOP_FOCUS_DISABLED = 1 shl 14
}

/**
 * One virtual display, its surface, and the app currently on it.
 *
 * Everything here needs shell identity, which is why it lives in the helper and
 * not in the app: creating the display is `CAPTURE_VIDEO_OUTPUT`, starting an
 * app on a non-default display is `am --display`, and injecting input at that
 * display is `input -d`.
 */
class AgentDisplay private constructor(
    private val display: VirtualDisplay,
    private val reader: ImageReader,
    val width: Int,
    val height: Int,
    val flags: Int,
    /** Which hidden `createVirtualDisplay` overload this build of Android took. */
    val signature: String,
    private val frameDir: File,
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
        // `am start --display` puts the activity on this display; without a
        // display id Android would put it on the user's. `-W` makes the call
        // wait for the activity to start, so the caller's next read does not
        // race the launch.
        val component = resolveLauncher(packageName)
        val output = exec(
            arrayOf(
                "am", "start", "-W",
                "--display", id.toString(),
                "-n", component,
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
        // `input text` cannot carry a space; the device shell's own convention
        // is `%s`, so the text is escaped rather than quoted.
        val escaped = text.replace("%", "%%").replace(" ", "%s")
        run("input", "-d", id.toString(), "text", escaped)
    }

    fun key(keyCode: Int) {
        run("input", "-d", id.toString(), "keyevent", keyCode.toString())
    }

    /**
     * The latest frame, as a file the app reads through a `ParcelFileDescriptor`.
     *
     * A file rather than a byte array because Binder caps a transaction at
     * 1 MB and a 1080x2400 PNG does not fit. The file is overwritten per call:
     * the app reads it before asking for the next frame, and one frame in
     * flight is all an agent loop ever has.
     */
    fun frameFile(): ParcelFileDescriptor? {
        val image = acquire(deadlineMs = 1200) ?: return null
        try {
            val bitmap = toBitmap(image)
            val file = File(frameDir, "frame-$id.png")
            FileOutputStream(file).use { out ->
                bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
            }
            bitmap.recycle()
            return ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
        } finally {
            image.close()
        }
    }

    /** A one-line description for logs: which overload created it and with what flags. */
    fun describe(): String =
        "display $id ${width}x$height flags=0x${Integer.toHexString(flags)} via $signature"

    private fun acquire(deadlineMs: Long): android.media.Image? {
        val until = System.currentTimeMillis() + deadlineMs
        while (System.currentTimeMillis() < until) {
            val latest = try {
                reader.acquireLatestImage()
            } catch (_: IllegalStateException) {
                // The reader is full because nobody closed an earlier image; the
                // next frame will do.
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
            arrayOf(
                "cmd", "package", "resolve-activity", "--brief", "-c",
                "android.intent.category.LAUNCHER", packageName,
            ),
        )
        return output.lineSequence()
            .map(String::trim)
            .lastOrNull { it.contains('/') }
            ?: throw IllegalStateException("$packageName has no launcher activity (${output.trim()})")
    }

    companion object {
        /**
         * Create a display sized like the user's, or at the caller's size.
         *
         * The flags are the difference between "a second screen" and "a second
         * screen that fights the first one": `OWN_FOCUS` (API 34+) is what lets
         * this display hold focus without taking it from the user's, and
         * `STEAL_TOP_FOCUS_DISABLED` stops it grabbing it anyway.
         */
        fun create(width: Int, height: Int, dpi: Int): AgentDisplay {
            val w = if (width > 0) width else 1080
            val h = if (height > 0) height else 2400
            val densityDpi = if (dpi > 0) dpi else 420

            val reader = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
            var flags = DisplayFlags.PUBLIC or DisplayFlags.OWN_CONTENT_ONLY or
                DisplayFlags.SUPPORTS_TOUCH or DisplayFlags.ROTATES_WITH_CONTENT
            if (Build.VERSION.SDK_INT >= 33) {
                flags = flags or DisplayFlags.TRUSTED or DisplayFlags.OWN_DISPLAY_GROUP or
                    DisplayFlags.ALWAYS_UNLOCKED
            }
            if (Build.VERSION.SDK_INT >= 34) {
                // Separate focus per display: the user keeps typing on their
                // screen while the agent types on this one.
                flags = flags or DisplayFlags.OWN_FOCUS or DisplayFlags.STEAL_TOP_FOCUS_DISABLED
            }

            val (display, signature) = createVirtualDisplay(w, h, densityDpi, reader.surface, flags)
            return AgentDisplay(display, reader, w, h, flags, signature, frameDir())
        }

        private fun frameDir(): File =
            File(System.getProperty("java.io.tmpdir") ?: "/data/local/tmp").also { it.mkdirs() }

        /**
         * Create the display, preferring the public `DisplayManager` API.
         *
         * The obvious route — `DisplayManagerGlobal.createVirtualDisplay` by
         * reflection — is the one that broke: on API 34+ that class keeps only
         * a `(Context, MediaProjection, VirtualDisplayConfig, Callback,
         * Executor)` overload, and the shape it used to have is gone.
         * `DisplayManager` still has both, and only ever needed the one thing a
         * bare `app_process` lacks: a `Context` that names a package whose uid
         * is ours.
         */
        private fun createVirtualDisplay(
            width: Int,
            height: Int,
            dpi: Int,
            surface: Surface,
            flags: Int,
        ): Pair<VirtualDisplay, String> {
            val attempts = mutableListOf<String>()
            val context = shellContext()

            if (context == null) {
                attempts.add("no shell Context")
            } else {
                val manager = context.getSystemService(Context.DISPLAY_SERVICE) as? DisplayManager
                if (manager == null) {
                    attempts.add("no DisplayManager")
                } else {
                    viaConfig(manager, width, height, dpi, surface, flags)
                        ?.let { return it }
                        ?: attempts.add("config overload not usable")
                }
            }

            val global = runCatching { createViaGlobal(width, height, dpi, surface, flags, attempts) }
                .getOrNull()
            global?.let { return it }

            throw IllegalStateException(
                "could not create a virtual display on API ${Build.VERSION.SDK_INT} " +
                    "(tried: ${attempts.joinToString("; ").ifEmpty { "nothing" }})",
            )
        }

        // The shared one, not a local copy: it reports the shell package through
        // `getPackageName`, `getOpPackageName` *and* `getAttributionSource`, and
        // the display service consults more than one of those.
        private fun shellContext(): Context? = ShellContext.create()

        /**
         * `DisplayManager.createVirtualDisplay(VirtualDisplayConfig, Callback,
         * Handler)`, with the config built by reflection because the class is
         * `@hide` in the SDK stubs.
         */
        private fun viaConfig(
            manager: DisplayManager,
            width: Int,
            height: Int,
            dpi: Int,
            surface: Surface,
            flags: Int,
        ): Pair<VirtualDisplay, String>? = runCatching {
            val configClass = Class.forName("android.hardware.display.VirtualDisplayConfig")
            val builderClass = Class.forName("android.hardware.display.VirtualDisplayConfig\$Builder")
            val builder = builderClass
                .getConstructor(
                    String::class.java,
                    Int::class.javaPrimitiveType,
                    Int::class.javaPrimitiveType,
                    Int::class.javaPrimitiveType,
                )
                .newInstance("syscity-agent", width, height, dpi)
            builderClass.getMethod("setSurface", Surface::class.java).invoke(builder, surface)
            builderClass.getMethod("setFlags", Int::class.javaPrimitiveType).invoke(builder, flags)
            val config = builderClass.getMethod("build").invoke(builder)

            val method = manager.javaClass.methods.firstOrNull { candidate ->
                candidate.name == "createVirtualDisplay" &&
                    candidate.parameterCount == 3 &&
                    candidate.parameterTypes[0] == configClass
            } ?: return null
            val created = method.invoke(manager, config, null, null)
            (created as? VirtualDisplay)?.let { it to "DisplayManager/VirtualDisplayConfig" }
        }.getOrNull()

        /**
         * `DisplayManagerGlobal.createVirtualDisplay(Context, MediaProjection,
         * VirtualDisplayConfig, Callback, Executor)` — the shape API 34+ kept,
         * and the one the emulator accepted. Kept as a fallback for builds whose
         * `DisplayManager` hides the config overload.
         */
        private fun createViaGlobal(
            width: Int,
            height: Int,
            dpi: Int,
            surface: Surface,
            flags: Int,
            attempts: MutableList<String>,
        ): Pair<VirtualDisplay, String>? {
            val global = Class.forName("android.hardware.display.DisplayManagerGlobal")
                .getMethod("getInstance").invoke(null) ?: return null
            val context = shellContext() ?: return null
            val configClass = Class.forName("android.hardware.display.VirtualDisplayConfig")
            val builderClass = Class.forName("android.hardware.display.VirtualDisplayConfig\$Builder")
            val builder = builderClass
                .getConstructor(
                    String::class.java,
                    Int::class.javaPrimitiveType,
                    Int::class.javaPrimitiveType,
                    Int::class.javaPrimitiveType,
                )
                .newInstance("syscity-agent", width, height, dpi)
            builderClass.getMethod("setSurface", Surface::class.java).invoke(builder, surface)
            builderClass.getMethod("setFlags", Int::class.javaPrimitiveType).invoke(builder, flags)
            val config = builderClass.getMethod("build").invoke(builder)

            val method = global.javaClass.methods.firstOrNull { candidate ->
                candidate.name == "createVirtualDisplay" &&
                    candidate.parameterCount == 5 &&
                    candidate.parameterTypes[2] == configClass
            } ?: run {
                attempts.add("no 5-arg createVirtualDisplay")
                return null
            }
            val outcome = runCatching { method.invoke(global, context, null, config, null, null) }
            outcome.onFailure { attempts.add("global/config: ${it.cause?.message ?: it.message}") }
            val created = outcome.getOrNull()
            if (created !is VirtualDisplay) {
                attempts.add("global/config returned ${created?.javaClass?.name}")
                return null
            }
            return created to "DisplayManagerGlobal/config"
        }
    }
}
