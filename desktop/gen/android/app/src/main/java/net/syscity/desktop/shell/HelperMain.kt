package net.syscity.desktop.shell

import android.content.ContentResolver
import android.net.Uri
import android.annotation.SuppressLint
import android.os.Binder
import android.os.Bundle
import android.os.IBinder
import android.os.Process
import android.os.RemoteException
import android.os.SystemClock
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
     * Where the helper keeps state that must survive a reboot.
     *
     * `/data`, shell-owned, and not wiped: neither `java.io.tmpdir` nor the
     * app's own `cacheDir` is (see `stateDir`).
     */
    private const val STATE_DIR = "/data/local/tmp"

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

        val stub = Stub(userId = appUid / PER_USER_RANGE)
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
     * Where this process may keep small pieces of state across restarts.
     *
     * Not `java.io.tmpdir`: under `app_process` that is `/tmp`, a tmpfs that a
     * reboot wipes — and the audio-mute record has to survive exactly that. It
     * is where the *frames* live, and that is right for them: a frame that
     * outlived a reboot would be a picture of a phone that no longer exists.
     *
     * `/data/local/tmp` is on `/data`, is owned by the shell uid this process
     * runs as, and is where adb keeps things meant to last. The platform's temp
     * directory is the fallback: a record that does not survive a reboot still
     * beats no record at all.
     */
    private fun stateDir(): File {
        val preferred = File(STATE_DIR)
        if (preferred.isDirectory && preferred.canWrite()) return preferred
        val fallback = File(System.getProperty("java.io.tmpdir") ?: STATE_DIR)
        return fallback.also { it.mkdirs() }
    }

    /**
     * The Binder the app talks to.
     *
     * Everything here touches the display or the shell, so it is real Binder
     * work on pool threads. Frames go to a file and come back as an fd: a
     * Binder transaction is capped at 1 MB and a full-screen PNG is not.
     */
    private class Stub(private val userId: Int) : ISyscityShell.Stub() {

        private val lock = Any()
        private val displays = HashMap<Int, AgentDisplay>()
        private val clipboard = ShellClipboard()
        private val pasteLock = Any()
        private val mainWatch = MainScreenWatch { TaskQuery.current().orEmpty() }

        /**
         * What each display has open, and which package it silenced for it.
         *
         * Keyed by display: two displays may show the same app, and the sound
         * may only come back when the last of them closes.
         */
        private val audio = AudioControl(
            ledger = AudioMuteLedger(File(stateDir(), "audio_mute_$userId")),
            userId = userId,
        )
        private val muted = HashMap<Int, String>()

        /**
         * Displays a person is touching right now.
         *
         * The agent's display is handed over with `moveToUser`, and the person
         * then acts on their own screen. While that is happening nothing may
         * write to the display they are looking at — another session tapping
         * under their finger is the worst possible moment for it. Reads still
         * work: the waiting session has to be able to judge what they did.
         *
         * The flag is the guarantee, because it is checked on the device and
         * so covers every caller: the process-wide lock in Rust serializes
         * sessions on one host, and this covers the case where that lock was
         * never taken — a second session that arrived while the handoff was in
         * flight.
         */
        private val heldByUser = HashMap<Int, Long>()

        init {
            // An earlier helper may have been killed with apps still silenced.
            // The file is the only record of them, and nothing else will ever
            // restore those apps: the display that muted them is gone. But a
            // helper that is still alive owns its record — restoring it would
            // undo a mute whose display is still up, and the app would be
            // audible to a user who cannot see it.
            if (audio.ownedByAnother()) {
                Log.w(TAG, "another live helper owns the audio-mute record; leaving it alone")
            } else {
                runCatching { audio.restoreAll() }
                    .onFailure {
                        Log.w(TAG, "could not restore apps muted by an earlier session: ${it.message}")
                    }
            }
        }

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
                val target = writable(displayId) ?: return 1
                target.launch(packageName)
                // Only after the launch took: an app that never started is not
                // an app the user would hear, and silencing it would outlive
                // the failed call.
                muteFor(displayId, packageName)
                0
            }.getOrElse {
                Log.e(TAG, "startApp $packageName on $displayId: ${it.message}")
                1
            }

        override fun releaseDisplay(displayId: Int) {
            synchronized(lock) { displays.remove(displayId)?.release() }
            restoreFor(displayId)
            // A display that no longer exists cannot be held: leaving the flag
            // would make a new display that reuses the id unreachable.
            takeBack(displayId)
        }

        /**
         * Silence what this display shows, and say whether it worked.
         *
         * A display keeps only one package muted: whatever was open before is
         * replaced, so an app the agent navigated away from gets its sound back
         * rather than staying silent for the rest of the session.
         *
         * A mute for a display that does not exist is refused rather than
         * applied: the record would outlive the display that justified it and
         * the app would be silent for the rest of the session, with nothing on
         * screen to explain why.
         *
         * The answer is advisory. The display is already running; a refusal to
         * mute is worth reporting and not worth failing the launch over.
         */
        override fun setAudioMuted(displayId: Int, packageName: String, muted: Boolean): Boolean {
            if (muted) {
                // A person holding the display decides what it sounds like.
                writable(displayId) ?: return false
                return muteFor(displayId, packageName)
            }
            // Un-muting names a package, so an app this display never silenced
            // is left alone — even when the display is holding something else
            // down, which is not what the caller asked to release.
            val held = synchronized(this.muted) { this.muted[displayId] }
            if (held != packageName) return true
            return restoreFor(displayId)
        }

        /**
         * Drop a takeover whose deadline has passed.
         *
         * The interval is the backstop, not the mechanism: the caller takes the
         * display back when it is done. What this prevents is a caller that
         * died, was cancelled, or never came back leaving the display
         * unwritable for the rest of the session.
         */
        private fun expireHold(displayId: Int) {
            val deadline = synchronized(heldByUser) { heldByUser[displayId] } ?: return
            if (SystemClock.elapsedRealtime() < deadline) return
            synchronized(heldByUser) { heldByUser.remove(displayId) }
            Log.w(TAG, "the takeover of display $displayId timed out; the agent may write again")
        }

        /**
         * The display, unless a person is holding it.
         *
         * Every write goes through here; reads do not. Null means either no
         * such display or one the user has taken over, and both are answered
         * with false by the callers that write — which is the same answer they
         * already give for a display that is not there, so no caller needs a
         * new branch.
         */
        private fun writable(id: Int): AgentDisplay? {
            expireHold(id)
            return synchronized(heldByUser) { if (id in heldByUser) null else display(id) }
        }

        override fun injectTap(displayId: Int, x: Int, y: Int): Boolean =
            runCatching { writable(displayId)?.tap(x, y) != null }.getOrDefault(false)

        override fun injectSwipe(
            displayId: Int,
            x1: Int,
            y1: Int,
            x2: Int,
            y2: Int,
            durationMs: Int,
        ): Boolean = runCatching {
            writable(displayId)?.swipe(x1, y1, x2, y2, durationMs) != null
        }.getOrDefault(false)

        override fun injectKey(displayId: Int, keyCode: Int): Boolean =
            runCatching { writable(displayId)?.key(keyCode) != null }.getOrDefault(false)

        override fun injectText(displayId: Int, text: String): Boolean =
            runCatching { writable(displayId)?.type(text) != null }.getOrDefault(false)

        override fun frame(displayId: Int): String? =
            runCatching { display(displayId)?.frameFile()?.absolutePath }.getOrNull()

        override fun describe(displayId: Int): String =
            display(displayId)?.describe() ?: "no display $displayId"

        /**
         * Put the display's app on the user's screen.
         *
         * This one is allowed on a held display, because it is what the hold is
         * for: `giveToUser` marks it, and then this is the move the person is
         * about to act on. Every other write is refused until they hand it back.
         */
        override fun moveToUser(displayId: Int): Boolean {
            val moved =
                runCatching { display(displayId)?.moveTopTaskTo(0) == true }.getOrDefault(false)
            // The app is on the user's screen now: they can see it, so they
            // should hear it. The mute exists to keep sound attached to
            // something visible, and the user is looking at it.
            if (moved) restoreFor(displayId)
            return moved
        }

        override fun giveToUser(displayId: Int, holdMs: Long): Boolean {
            expireHold(displayId)
            if (display(displayId) == null) return false
            // The display is marked before the app moves: a session that got
            // in between the move and the flag would be writing to a screen the
            // user had just picked up.
            val deadline = SystemClock.elapsedRealtime() + holdMs.coerceAtLeast(0)
            return synchronized(heldByUser) { heldByUser.put(displayId, deadline) == null }
        }

        override fun isHeldByUser(displayId: Int): Boolean {
            expireHold(displayId)
            return synchronized(heldByUser) { displayId in heldByUser }
        }

        override fun takeBack(displayId: Int) {
            synchronized(heldByUser) { heldByUser.remove(displayId) }
        }

        /** Take the app back. Allowed on a held display: the hold is over. */
        override fun moveToAgent(displayId: Int): Boolean {
            val returned =
                runCatching { display(displayId)?.moveTopTaskTo(displayId) == true }
                    .getOrDefault(false)
            // Back where the user cannot see it, so back to silent. The app is
            // re-muted from the mode it was just restored to, which is the mode
            // it had before the agent silenced it in the first place.
            if (returned) {
                shownPackage(displayId)?.let { packageName -> muteFor(displayId, packageName) }
            }
            return returned
        }

        override fun mainScreenTop(): String {
            // Not tied to a display: the user's screen is the user's screen
            // whoever asks, and asking the first-created display picked the
            // wrong one as soon as two were open.
            val (taskId, packageName) = AgentDisplay.mainScreenTop()
            return if (taskId < 0) "" else "$taskId|${packageName ?: ""}"
        }

        override fun moveTaskToMe(displayId: Int, taskId: Int): Boolean =
            runCatching { writable(displayId)?.moveTaskTo(taskId) == true }.getOrDefault(false)

        override fun injectGesture(
            displayId: Int,
            x: IntArray,
            y: IntArray,
            path: IntArray,
            durationMs: Int,
        ): Boolean = runCatching {
            if (x.size != y.size || x.size != path.size || x.size < 2) return@runCatching false
            val points = x.indices.map { AgentDisplay.GesturePoint(path[it], x[it], y[it]) }
            writable(displayId)?.gesture(points, durationMs) == true
        }.getOrDefault(false)

        /**
         * The paste, whole: put, press, wait, restore.
         *
         * `ShellClipboard` holds one saved clipboard at a time, so two pastes
         * overlapping would have the first restore the second's text and hand
         * the user back something neither of them typed. The Rust tools take a
         * lock per phone, which serializes them today — but that lock is about
         * input and lives on the other side of the bridge. The atomicity belongs
         * where the state is.
         */
        override fun pasteText(displayId: Int, text: String): Int {
            synchronized(pasteLock) { return paste(displayId, text) }
        }

        private fun paste(displayId: Int, text: String): Int {
            val target = writable(displayId) ?: return PASTE_FAILED
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

        /**
         * Record that `displayId` now shows `packageName`, muting it.
         *
         * Returning the display to its previous package restores that one
         * first: the map holds one package per display, so replacing it is also
         * the release of the old one.
         */
        private fun muteFor(displayId: Int, packageName: String): Boolean {
            val previous = synchronized(muted) { muted.put(displayId, packageName) }
            if (previous == packageName) {
                // Launched again, still the same app on this display. Asking the
                // ledger to mute it a second time would count this display
                // twice, and closing the display once would then leave two
                // releases owed — an app silent for the rest of the session.
                return true
            }
            if (previous != null) {
                runCatching { audio.restore(previous) }
                    .onFailure { Log.w(TAG, "could not restore $previous: ${it.message}") }
            }
            return runCatching { audio.mute(packageName) }.getOrElse {
                Log.w(TAG, "could not silence $packageName: ${it.message}")
                false
            }
        }

        /** What is on top of `displayId`, as the task list names it. */
        private fun shownPackage(displayId: Int): String? =
            TaskQuery.current()
                ?.firstOrNull { it.displayId == displayId }
                ?.packageName

        /** The display is going away or changed app: give the sound back. */
        private fun restoreFor(displayId: Int): Boolean {
            val previous = synchronized(muted) { muted.remove(displayId) } ?: return true
            return runCatching { audio.restore(previous) }.getOrElse {
                Log.w(TAG, "could not restore $previous: ${it.message}")
                false
            }
        }

        /** Tear everything down; called when the app's token dies. */
        fun releaseAll() {
            synchronized(lock) {
                displays.values.forEach { runCatching { it.release() } }
                displays.clear()
            }
            synchronized(muted) { muted.clear() }
            synchronized(heldByUser) { heldByUser.clear() }
            // The ledger is the list of apps still silent, whoever muted them;
            // restoring it here is what keeps a killed session from leaving the
            // user with an app that has no sound.
            runCatching { audio.restoreAll() }
                .onFailure { Log.w(TAG, "could not restore every muted app: ${it.message}") }
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
