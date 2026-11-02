package net.syscity.desktop.shell

import android.content.Context
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.os.Build
import android.util.Log
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
    const val DESTROY_CONTENT_ON_REMOVAL = 1 shl 8
    const val TRUSTED = 1 shl 10
    const val OWN_DISPLAY_GROUP = 1 shl 11
    const val ALWAYS_UNLOCKED = 1 shl 12
    const val TOUCH_FEEDBACK_DISABLED = 1 shl 13
    // OWN_FOCUS is 1 shl 14, not 13 — the bit below it is
    // TOUCH_FEEDBACK_DISABLED. Getting this wrong silently sets the wrong
    // flag: the display still works, it just does not get its own focus.
    const val OWN_FOCUS = 1 shl 14
    const val STEAL_TOP_FOCUS_DISABLED = 1 shl 15
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

    /** The last frame written, and when — see [frameFile]. */
    @Volatile
    private var lastFrameFile: File? = null
    @Volatile
    private var lastFrameAt: Long = 0

    fun release() {
        try {
            display.release()
        } catch (_: Exception) {
            // Already gone; releasing again is not an error worth reporting.
        }
        reader.close()
    }

    /**
     * The task currently on top of this display, or -1.
     *
     * `dumpsys activity` is the only way to ask without the hidden
     * `IActivityTaskManager` binding; the move itself needs the task id.
     */
    fun topTaskId(): Int {
        val output = exec(arrayOf("dumpsys", "activity", "activities"))
        val header = "Display #$id ("
        var inDisplay = false
        for (line in output.lineSequence()) {
            val trimmed = line.trim()
            if (trimmed.startsWith(header)) {
                inDisplay = true
                continue
            }
            if (inDisplay && trimmed.startsWith("Display #")) return -1
            if (inDisplay && trimmed.startsWith("* Task{")) {
                // `* Task{df2887c #59 type=standard ...`
                val afterHash = trimmed.substringAfter('#', "")
                val taskId = afterHash.takeWhile { it.isDigit() }
                if (taskId.isNotEmpty()) return taskId.toInt()
            }
        }
        return -1
    }

    /**
     * Move this display's top task to `targetDisplay`, or to the user's screen
     * when `targetDisplay` is 0.
     *
     * `am display move-stack` is the shell-visible form of
     * `IActivityTaskManager.moveRootTaskToDisplay`; it is what `adb shell` uses
     * for the same operation.
     */
    fun moveTopTaskTo(targetDisplay: Int): Boolean {
        val taskId = topTaskId()
        if (taskId < 0) return false
        val output = exec(arrayOf("am", "display", "move-stack", taskId.toString(), targetDisplay.toString()))
        return !output.contains("Error") && !output.contains("Exception")
    }

    /** Move `taskId` onto this display. */
    fun moveTaskTo(taskId: Int): Boolean {
        val output = exec(arrayOf("am", "display", "move-stack", taskId.toString(), id.toString()))
        return !output.contains("Error") && !output.contains("Exception")
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
     * The latest frame, written to a file and returned by path.
     *
     * A file rather than bytes because Binder caps a transaction at 1 MB and a
     * 1080x2400 PNG does not fit. A *path* rather than a `ParcelFileDescriptor`
     * because the reader is the app's own Rust process, which can open the file
     * directly — sending an fd through the Tauri bridge only to read it back as
     * base64 costs two copies of every frame for nothing.
     *
     * The file is overwritten per call: the reader takes it before asking for
     * the next frame, and one frame in flight is all an agent loop ever has.
     */
    fun frameFile(): File? {
        // Keep the last encoded frame. A virtual display only produces an image
        // when its content changes, so consuming the one frame it sent and then
        // asking again on a static screen waits for something that will not
        // come. Caching means "the screen as of its last change", which is what
        // an agent observation wants anyway.
        // Short: a static screen has no frame coming, and the cache answers
        // immediately. A changing screen delivers within a frame or two.
        val fresh = acquire(deadlineMs = 60)
        if (fresh != null) {
            try {
                val file = File(frameDir, "frame-$id.png")
                val bitmap = toBitmap(fresh)
                try {
                    FileOutputStream(file).use { out ->
                        bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
                    }
                } finally {
                    bitmap.recycle()
                }
                lastFrameFile = file
                lastFrameAt = System.currentTimeMillis()
                return file
            } finally {
                fresh.close()
            }
        }
        // Nothing new: the previous frame is still the current screen. Report
        // how old it is so a caller can tell a stale view from a fresh one.
        val cached = lastFrameFile
        if (cached != null && cached.exists()) {
            Log.d(FRAME_TAG, "serving cached frame (age ${System.currentTimeMillis() - lastFrameAt}ms)")
            return cached
        }
        Log.w(FRAME_TAG, "no frame and nothing cached")
        return null
    }

    /** A one-line description for logs: which overload created it and with what flags. */
    fun describe(): String =
        "display $id ${width}x$height flags=0x${Integer.toHexString(flags)} via $signature"

    private fun acquire(deadlineMs: Long): android.media.Image? {
        val until = System.currentTimeMillis() + deadlineMs
        var attempts = 0
        var lastError: String? = null
        while (System.currentTimeMillis() < until) {
            attempts++
            try {
                // `acquireLatestImage` drops older queued frames itself; the
                // `IllegalStateException` it throws is "no buffer free", which
                // means a previous image was never closed.
                val latest = reader.acquireLatestImage()
                if (latest != null) {
                    if (attempts > 2) Log.d(FRAME_TAG, "frame after $attempts attempts")
                    return latest
                }
            } catch (e: IllegalStateException) {
                lastError = e.message
            }
            Thread.sleep(40)
        }
        Log.w(FRAME_TAG, "no frame in ${deadlineMs}ms after $attempts attempts: $lastError")
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
        private const val FRAME_TAG = "SyscityFrame"

        /**
         * The task on top of the user's own display (display 0).
         *
         * A static query, not a method on one display: the answer has nothing
         * to do with which agent display is asking, and an instance method
         * invited a caller to read it off whichever display came first.
         */
        @JvmStatic
        fun mainScreenTop(): Pair<Int, String?> {
            val output = try {
                val process = ProcessBuilder("dumpsys", "activity", "activities")
                    .redirectErrorStream(true)
                    .start()
                process.inputStream.bufferedReader().use { it.readText() }
            } catch (e: Exception) {
                Log.w(FRAME_TAG, "dumpsys activity failed: ${e.message}")
                return -1 to null
            }
            var inDisplay = false
            var taskId = -1
            val lines = output.lineSequence().iterator()
            while (lines.hasNext()) {
                val trimmed = lines.next().trim()
                if (trimmed.startsWith("Display #0 (")) {
                    inDisplay = true
                    continue
                }
                if (inDisplay && trimmed.startsWith("Display #")) return -1 to null
                if (inDisplay && trimmed.startsWith("* Task{")) {
                    taskId = trimmed.substringAfter('#', "").takeWhile { it.isDigit() }
                        .toIntOrNull() ?: -1
                    // The package, best source first: `topResumedActivity` names
                    // the activity record directly; the task line itself names a
                    // component only in some builds (`A=uid:pkg`) or an intent
                    // (`I=pkg/class` — the share sheet's resolver is this shape,
                    // and missing it means a real spill reads as "no spill").
                    var packageName: String? = null
                    while (lines.hasNext()) {
                        val inner = lines.next()
                        val t = inner.trim()
                        if (t.startsWith("* Task{") || t.startsWith("Display #")) {
                            packageName = packageName ?: packageFromTaskLine(trimmed)
                            break
                        }
                        if (packageName == null) {
                            packageName =
                                Regex("topResumedActivity=ActivityRecord\\{[^}]*?\\s(\\S+?)/")
                                    .find(t)?.groupValues?.get(1)
                                    ?: packageFromTaskLine(t)
                        }
                    }
                    return taskId to packageName
                }
            }
            return -1 to null
        }

        /** The package a task/intent line names, if it names one. */
        private fun packageFromTaskLine(line: String): String? =
            Regex("A=\\d+:([^\\s/]+)").find(line)?.groupValues?.get(1)
                ?: Regex("I=([^\\s/]+)/").find(line)?.groupValues?.get(1)

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
                DisplayFlags.SUPPORTS_TOUCH or DisplayFlags.ROTATES_WITH_CONTENT or
                DisplayFlags.DESTROY_CONTENT_ON_REMOVAL
            if (Build.VERSION.SDK_INT >= 33) {
                flags = flags or DisplayFlags.TRUSTED or DisplayFlags.OWN_DISPLAY_GROUP or
                    DisplayFlags.ALWAYS_UNLOCKED or DisplayFlags.TOUCH_FEEDBACK_DISABLED
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
