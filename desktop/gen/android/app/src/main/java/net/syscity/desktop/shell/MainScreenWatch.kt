package net.syscity.desktop.shell

import android.app.ActivityManager
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.Parcel
import android.os.SystemClock
import android.util.Log

/**
 * Watching the user's own screen for the moment something lands on it.
 *
 * An action on the agent's display can start an app that the system puts on the
 * *user's* screen — a share sheet, a chooser, an app that ignores `--display` —
 * and from the agent's side that is indistinguishable from nothing happening.
 * Looking once afterwards cannot tell it apart from an app the user opened
 * themselves; recording what rose to the top, and when, can.
 *
 * Two ways to notice a change: a task-stack listener (shell holds the
 * permission), and polling if the registration is refused. The listener is only
 * a signal — the task list is re-read every time, because the callback's
 * parameter order differs between releases.
 */
internal class MainScreenWatch(
    /** Every task on every display, most recently used first. */
    private val tasks: () -> List<MainScreenLog.Task>,
) {
    private val log = MainScreenLog()
    private val thread = HandlerThread("syscity-main-watch").apply { start() }
    private val handler = Handler(thread.looper)
    @Volatile private var watching = false
    @Volatile private var until = 0L
    private var polling = false
    private var registered: Boolean? = null

    /** Start watching from now; returns the task on top of the user's screen right now. */
    fun watch(windowMs: Int): Int = synchronized(log) {
        val now = SystemClock.uptimeMillis()
        scanLocked(now, record = false)
        until = maxOf(until, now + windowMs.coerceIn(0, MAX_WINDOW_MS))
        watching = true
        if (registered == null) registered = register()
        if (registered == false && !polling) {
            polling = true
            handler.post(poll)
        }
        Log.d(TAG, "watching from $now for $windowMs ms; top task is ${log.topTaskId}")
        log.topTaskId
    }

    /** What rose to the top since `since` (uptime), oldest first. */
    fun arrivals(since: Long): List<MainScreenLog.Arrival> = synchronized(log) {
        // One more look, so the end of the window is not lost to whichever poll
        // happened to fire before it.
        if (SystemClock.uptimeMillis() < until + GRACE_MS) {
            scanLocked(SystemClock.uptimeMillis(), record = true)
        }
        log.since(since).also { found ->
            for (arrival in found) {
                Log.d(
                    TAG,
                    "arrival task=${arrival.taskId} package=${arrival.packageName} " +
                        "at=${arrival.at} from=${arrival.from} sameTask=${arrival.sameTask} " +
                        "standard=${arrival.standard} now=${log.displayOf(arrival.taskId)}",
                )
            }
        }
    }

    /** Where a task is now (-1 when it is gone). */
    fun displayOf(taskId: Int): Int = synchronized(log) { log.displayOf(taskId) }

    fun close() {
        watching = false
        thread.quitSafely()
    }

    private fun scanLocked(now: Long, record: Boolean) {
        val list = try {
            tasks()
        } catch (t: Throwable) {
            Log.w(TAG, "cannot list tasks", t)
            return
        }
        log.scan(list, now, record)
    }

    private fun scan() {
        if (!watching || SystemClock.uptimeMillis() >= until) return
        synchronized(log) { scanLocked(SystemClock.uptimeMillis(), record = true) }
    }

    private val poll = object : Runnable {
        override fun run() {
            if (!watching || SystemClock.uptimeMillis() >= until) {
                polling = false
                return
            }
            scan()
            handler.postDelayed(this, POLL_MS)
        }
    }

    /** Register the task-stack listener, if this build will have it. */
    @Suppress("PrivateApi", "DiscouragedPrivateApi")
    private fun register(): Boolean = runCatching {
        val am = ActivityManager::class.java.getMethod("getService").invoke(null)
        val stub = Class.forName("android.app.ITaskStackListener\$Stub")
            .getMethod("asInterface", IBinder::class.java)
            .invoke(null, Signal())
        val register = am.javaClass.methods.firstOrNull {
            it.name == "registerTaskStackListener" && it.parameterTypes.size == 1
        } ?: return false
        register.invoke(am, stub)
        Log.i(TAG, "task stack listener registered")
        true
    }.getOrElse {
        Log.w(TAG, "task stack listener unavailable; polling instead", it)
        false
    }

    /**
     * The callback the system fires on a task change.
     *
     * Only its arrival matters: the parameters are not read, because their
     * order has changed between releases. The methods are one-way, so there is
     * never a reply to write.
     */
    private inner class Signal : android.os.Binder() {
        init {
            attachInterface(null, DESCRIPTOR)
        }

        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            if (code in FIRST_CALL_TRANSACTION..LAST_CALL_TRANSACTION) {
                handler.postDelayed({ scan() }, COALESCE_MS)
                return true
            }
            return super.onTransact(code, data, reply, flags)
        }
    }

    private companion object {
        const val TAG = "SyscityWatch"
        const val DESCRIPTOR = "android.app.ITaskStackListener"
        const val POLL_MS = 100L
        const val COALESCE_MS = 30L
        /** The window has ended, but the caller needs a moment to ask. */
        const val GRACE_MS = 2_000L
        const val MAX_WINDOW_MS = 30_000
    }
}
