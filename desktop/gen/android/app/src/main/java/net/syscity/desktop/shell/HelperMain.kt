package net.syscity.desktop.shell

import android.content.ContentResolver
import android.net.Uri
import android.annotation.SuppressLint
import android.os.Binder
import android.os.Bundle
import android.os.IBinder
import android.os.Process
import android.os.RemoteException
import android.util.Log
import java.io.File

/**
 * The process that creates the agent's display.
 *
 * Started from the app over loopback ADB:
 *
 * ```
 * adb shell 'read n; (echo "$n" | CLASSPATH=<apk> setsid app_process /system/bin \
 *     --nice-name=net.syscity.desktop:shell HelperMain <pkg> <uid> <certPin> &); read _'
 * ```
 *
 * `app_process` runs a plain Java class with no Activity and no app sandbox, so
 * it inherits the identity of `adb shell` — uid 2000. That is the whole point:
 * `DisplayManager.createVirtualDisplay` needs `CAPTURE_VIDEO_OUTPUT`, which an
 * app can never hold.
 *
 * It cannot talk to the app over a socket: SELinux forbids `untrusted_app` →
 * `shell` unix sockets. It *can* hand a Binder to a ContentProvider the app
 * exports, which is the direction the policy allows, so that is the transport.
 * The handoff carries a nonce it received on stdin, which only the app knew.
 */
object HelperMain {

    private const val TAG = "SyscityShell"
    private const val SHELL_UID = 2000

    /** uid per Android user; a package's uid encodes its user in the high bits. */
    private const val PER_USER_RANGE = 100000

    /**
     * The shell Context this process built, kept for services that want one
     * (`isLowRamDevice` is a call on a Context, not a global).
     */
    private var shellContext: android.content.Context? = null

    @JvmStatic
    fun main(args: Array<String>) {
        val uid = Process.myUid()
        if (uid != SHELL_UID && uid != 0) {
            Log.e(TAG, "the shell helper must run as uid $SHELL_UID (adb shell), not $uid")
            System.exit(2)
        }
        // `ActivityThread.systemMain()`, the way to a Context with no Activity,
        // builds a Handler — which needs a main Looper, and `app_process`
        // starts without one.
        runCatching { android.os.Looper.prepareMainLooper() }

        // <packageName> <appUid>
        if (args.size < 2) {
            Log.e(TAG, "usage: HelperMain <package> <appUid>")
            System.exit(3)
        }
        val packageName = args[0]
        val appUid = args[1].toIntOrNull()
        if (appUid == null) {
            Log.e(TAG, "usage: HelperMain <package> <appUid> (appUid must be a number, got ${args[1]})")
            System.exit(3)
            return
        }

        // The nonce the app put in our environment when it launched us.
        //
        // Not on stdin: the launch string has to survive `adb shell sh -c "…"`,
        // and a `$var` inside it is the kind of thing that quietly expands to
        // nothing one quoting layer later. An environment prefix is passed to
        // the device shell verbatim, so there is nothing to escape.
        val nonce = System.getenv("SYSCITY_HANDOFF_NONCE")?.trim()
        if (nonce.isNullOrEmpty()) {
            Log.e(TAG, "no handoff nonce — this helper was not started by the app")
            System.exit(4)
            return
        }

        // `ShellContext` — not a bare `getSystemContext()`: it registers this
        // process as an app, without which ActivityManager refuses to start the
        // app's ContentProvider on its behalf ("Unable to find app for caller").
        val context = ShellContext.create()
        if (context == null) {
            Log.e(TAG, "no shell Context")
            System.exit(5)
            return
        }
        shellContext = context

        val stub = Stub()
        val token = handoff(context, packageName, appUid, nonce, stub)
        if (token == null) {
            Log.e(TAG, "handoff refused")
            System.exit(6)
            return
        }

        // Watch the app: when its token dies, the app is gone and so is any
        // reason to keep a display alive.
        token.linkToDeath({
            Log.i(TAG, "app died; releasing everything")
            stub.releaseAll()
            System.exit(0)
        }, 0)

        Log.i(TAG, "attached; serving")
        // Binder calls arrive on pool threads; the main thread only has to stay
        // alive for the process to.
        android.os.Looper.loop()
    }

    /**
     * Hand the stub to the app's provider.
     *
     * Not through `contentResolver.call`: ActivityManager refuses that for a
     * process it has no app record for —
     * `SecurityException: Unable to find app for caller … when getting content
     * provider` — and `app_process` never registers one.
     *
     * `ActivityManager.getContentProviderExternal` is the route `adb shell
     * content call` itself takes, and it checks the *provider's* permissions
     * rather than the caller's app record. The provider is then invoked
     * directly, with the caller's `AttributionSource`, and released with
     * `removeContentProviderExternalAsUser` when done.
     *
     * Returns the app's token, or null on refusal.
     */
    @SuppressLint("PrivateApi", "DiscouragedPrivateApi")
    private fun handoff(
        context: android.content.Context,
        packageName: String,
        appUid: Int,
        nonce: String,
        stub: Stub,
    ): IBinder? {
        val authority = HelperHandoff.authority(packageName)
        val userId = appUid / PER_USER_RANGE
        val holderToken = Binder()

        val am = runCatching {
            Class.forName("android.app.ActivityManager").getMethod("getService").invoke(null)
        }.getOrNull() ?: run {
            Log.e(TAG, "no ActivityManager service")
            return null
        }

        val holder = runCatching {
            am.javaClass.getMethod(
                "getContentProviderExternal",
                String::class.java,
                Int::class.javaPrimitiveType,
                IBinder::class.java,
                String::class.java,
            ).invoke(am, authority, userId, holderToken, TAG)
        }.onFailure { Log.e(TAG, "getContentProviderExternal failed", it) }.getOrNull()
            ?: return null

        try {
            // An authority can only belong to one package, but check anyway:
            // handing the Binder to anything but this app would be a way in.
            val info = runCatching {
                holder.javaClass.getField("info").get(holder) as? android.content.pm.ProviderInfo
            }.getOrNull()
            if (info == null || info.packageName != packageName || info.applicationInfo?.uid != appUid) {
                Log.e(
                    TAG,
                    "provider $authority does not belong to $packageName/$appUid " +
                        "(got ${info?.packageName}/${info?.applicationInfo?.uid})",
                )
                return null
            }

            val provider = runCatching {
                holder.javaClass.getField("provider").get(holder)
            }.getOrNull() ?: run {
                Log.e(TAG, "provider holder carried no provider")
                return null
            }

            val extras = Bundle().apply {
                putString(HelperHandoff.KEY_NONCE, nonce)
                putBinder(HelperHandoff.KEY_BINDER, stub)
                putInt(HelperHandoff.KEY_UID, Process.myUid())
                putInt(HelperHandoff.KEY_PID, Process.myPid())
                // `java.class.path` is the CLASSPATH this process was started
                // with — the APK the app installed. `context.applicationInfo`
                // is not: under `getContentProviderExternal` the Context is the
                // system one and reports no source directory.
                putString(HelperHandoff.KEY_APK, System.getProperty("java.class.path"))
                putString(HelperHandoff.KEY_CERT_PIN, certPinOf(context))
            }
            // The five-argument `call(callingAttributionSource, authority,
            // method, arg, extras)` is what `ContentProvider` dispatches on;
            // the three-argument one is the public API and would skip the
            // attribution the provider checks.
            val call = provider.javaClass.methods.firstOrNull {
                it.name == "call" && it.parameterTypes.size == 5 &&
                    it.parameterTypes[0] == android.content.AttributionSource::class.java
            } ?: run {
                Log.e(TAG, "ContentProvider.call has no attribution form on this build")
                return null
            }
            val caller = android.content.AttributionSource.Builder(Process.myUid())
                .setPackageName(ShellContext.SHELL_PACKAGE)
                .build()
            val answer = runCatching {
                call.invoke(provider, caller, authority, HelperHandoff.METHOD_ATTACH, null, extras)
                    as? Bundle
            }.onFailure { Log.e(TAG, "provider call failed", it) }.getOrNull()
                ?: return null

            if (!answer.getBoolean(HelperHandoff.KEY_ACCEPTED)) {
                Log.e(TAG, "rejected: ${answer.getString(HelperHandoff.KEY_REASON)}")
                return null
            }
            return answer.getBinder(HelperHandoff.KEY_TOKEN)
        } finally {
            runCatching {
                am.javaClass.getMethod(
                    "removeContentProviderExternalAsUser",
                    String::class.java,
                    IBinder::class.java,
                    Int::class.javaPrimitiveType,
                ).invoke(am, authority, holderToken, userId)
            }.onFailure { Log.w(TAG, "could not release the provider holder: ${it.message}") }
        }
    }

    /** This APK's signing certificate digest, to prove it is the app's own copy. */
    private fun certPinOf(context: android.content.Context): String? = runCatching {
        // The app's own package, not the shell package the Context reports.
        val info = context.packageManager.getPackageInfo(
            System.getenv("SYSCITY_HANDOFF_PACKAGE") ?: context.packageName,
            android.content.pm.PackageManager.GET_SIGNING_CERTIFICATES,
        )
        HelperHandoff.certPin(info.signingInfo?.apkContentsSigners?.map { it.toByteArray() })
    }.getOrNull()

    /**
     * The Binder the app talks to.
     *
     * Everything here touches the display or the shell, so it is real Binder
     * work on pool threads. Frames go to a file and come back as an fd: a
     * Binder transaction is capped at 1 MB and a full-screen PNG is not.
     */
    private class Stub : ISyscityShell.Stub() {

        private val lock = Any()
        private val displays = HashMap<Int, AgentDisplay>()
        private val clipboard = ShellClipboard()
        private val mainWatch = MainScreenWatch { TaskQuery.current().orEmpty() }

        /** Set once a second display has failed to appear — it is not retried. */
        @Volatile
        private var secondDisplayFailed = false

        override fun identity(): IntArray = intArrayOf(Process.myUid(), Process.myPid())

        override fun createDisplay(width: Int, height: Int, dpi: Int): Int =
            synchronized(lock) {
                runCatching {
                    val display = AgentDisplay.create(width, height, dpi)
                    displays[display.id] = display
                    display.id
                }.getOrElse {
                    // A second display that will not start is not retried for the
                    // rest of the session: on a phone that refuses it, every
                    // retry costs a second and fails the same way.
                    if (displays.isNotEmpty()) secondDisplayFailed = true
                    Log.e(TAG, "createDisplay failed: ${it.message}")
                    -1
                }
            }

        override fun startApp(displayId: Int, packageName: String): Int =
            runCatching {
                display(displayId)?.launch(packageName)
                0
            }.getOrElse {
                Log.e(TAG, "startApp $packageName on $displayId: ${it.message}")
                1
            }

        override fun releaseDisplay(displayId: Int) {
            synchronized(lock) { displays.remove(displayId)?.release() }
        }

        override fun injectTap(displayId: Int, x: Int, y: Int): Boolean =
            runCatching { display(displayId)?.tap(x, y) != null }.getOrDefault(false)

        override fun injectSwipe(
            displayId: Int,
            x1: Int,
            y1: Int,
            x2: Int,
            y2: Int,
            durationMs: Int,
        ): Boolean = runCatching {
            display(displayId)?.swipe(x1, y1, x2, y2, durationMs) != null
        }.getOrDefault(false)

        override fun injectKey(displayId: Int, keyCode: Int): Boolean =
            runCatching { display(displayId)?.key(keyCode) != null }.getOrDefault(false)

        override fun injectText(displayId: Int, text: String): Boolean =
            runCatching { display(displayId)?.type(text) != null }.getOrDefault(false)

        override fun frame(displayId: Int): String? =
            runCatching { display(displayId)?.frameFile()?.absolutePath }.getOrNull()

        override fun describe(displayId: Int): String =
            display(displayId)?.describe() ?: "no display $displayId"

        override fun moveToUser(displayId: Int): Boolean =
            runCatching { display(displayId)?.moveTopTaskTo(0) == true }.getOrDefault(false)

        override fun moveToAgent(displayId: Int): Boolean =
            runCatching { display(displayId)?.moveTopTaskTo(displayId) == true }.getOrDefault(false)

        override fun mainScreenTop(): String {
            // Not tied to a display: the user's screen is the user's screen
            // whoever asks, and asking the first-created display picked the
            // wrong one as soon as two were open.
            val (taskId, packageName) = AgentDisplay.mainScreenTop()
            return if (taskId < 0) "" else "$taskId|${packageName ?: ""}"
        }

        override fun moveTaskToMe(displayId: Int, taskId: Int): Boolean =
            runCatching { display(displayId)?.moveTaskTo(taskId) == true }.getOrDefault(false)

        override fun injectGesture(
            displayId: Int,
            x: IntArray,
            y: IntArray,
            path: IntArray,
            durationMs: Int,
        ): Boolean = runCatching {
            if (x.size != y.size || x.size != path.size || x.size < 2) return@runCatching false
            val points = x.indices.map { AgentDisplay.GesturePoint(path[it], x[it], y[it]) }
            display(displayId)?.gesture(points, durationMs) == true
        }.getOrDefault(false)

        override fun pasteText(displayId: Int, text: String): Int {
            val target = display(displayId) ?: return PASTE_FAILED
            if (!clipboard.put(text)) return PASTE_NO_CLIPBOARD
            return try {
                // Ctrl+A then Ctrl+V: select whatever is in the field first, so
                // the paste replaces it rather than appending to it.
                if (!target.ctrlKey(KEYCODE_A) || !target.ctrlKey(KEYCODE_V)) {
                    PASTE_FAILED
                } else {
                    // The app reads the clipboard when it handles the paste key.
                    // Give it a moment before taking the clipboard back.
                    Thread.sleep(PASTE_SETTLE_MS)
                    PASTE_OK
                }
            } finally {
                clipboard.restore()
            }
        }

        override fun watchMainScreen(windowMs: Int): Int =
            runCatching { mainWatch.watch(windowMs) }.getOrDefault(-1)

        override fun mainArrivals(sinceUptimeMs: Long): String = runCatching {
            val arrivals = mainWatch.arrivals(sinceUptimeMs)
            val out = org.json.JSONArray()
            for (arrival in arrivals) {
                out.put(
                    org.json.JSONObject()
                        .put("task", arrival.taskId)
                        .put("package", arrival.packageName ?: org.json.JSONObject.NULL)
                        .put("at", arrival.at)
                        .put("from", arrival.from)
                        .put("sameTask", arrival.sameTask)
                        .put("standard", arrival.standard)
                        .put("now", mainWatch.displayOf(arrival.taskId)),
                )
            }
            out.toString()
        }.getOrElse {
            Log.w(TAG, "mainArrivals failed: ${it.message}")
            null
        } ?: ""

        override fun frameSignature(displayId: Int): ByteArray? =
            runCatching { display(displayId)?.frameSignature() }.getOrNull()

        override fun parallelLimit(): String {
            val (limit, reason) = ParallelLimit.of(
                sdk = TaskQuery.sdk(),
                lowRam = TaskQuery.isLowRam(shellContext!!),
                secondDisplayFailed = secondDisplayFailed,
            )
            return org.json.JSONObject()
                .put("limit", limit)
                .put("open", synchronized(lock) { displays.size })
                .put("reason", reason ?: org.json.JSONObject.NULL)
                .toString()
        }

        override fun displayCount(): Int = synchronized(lock) { displays.size }

        /** Tear everything down; called when the app's token dies. */
        fun releaseAll() {
            synchronized(lock) {
                displays.values.forEach { runCatching { it.release() } }
                displays.clear()
            }
            runCatching { mainWatch.close() }
            runCatching { clipboard.restore() }
        }

        private fun display(id: Int): AgentDisplay? = synchronized(lock) { displays[id] }
    }

    private const val PASTE_OK = 0
    private const val PASTE_NO_CLIPBOARD = 1
    private const val PASTE_FAILED = 2
    private const val PASTE_SETTLE_MS = 250L
    private const val KEYCODE_A = android.view.KeyEvent.KEYCODE_A
    private const val KEYCODE_V = android.view.KeyEvent.KEYCODE_V
}
