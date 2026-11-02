package net.syscity.desktop.shell

import android.app.ActivityManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build

/**
 * The task list, as `dumpsys activity activities` reports it.
 *
 * Parsed here rather than through `IActivityTaskManager.getTasks` because the
 * latter is a hidden Binder whose parcel layout changes between releases, while
 * the dump is the same text `adb shell dumpsys` prints — stable enough to parse
 * and easy to check against a real device.
 *
 * Only what the spill check needs is extracted: which display a task is on, and
 * what package is on top of it.
 */
internal object TaskQuery {

    /**
     * Every task, most recently used first, as the system orders them.
     *
     * A task whose top activity cannot be named still appears, with a null
     * package — "something is on the user's screen and we cannot tell what" is
     * a different fact from "the screen is empty", and the caller decides.
     */
    fun parse(dump: String): List<MainScreenLog.Task> = tasks(dump)

    /**
     * Parse one `dumpsys activity activities` output.
     *
     * The sections are `Display #<id> (activities from top to bottom):`, and
     * each task under one begins `* Task{<hex> #<id> type=… …}`. The package
     * comes from `topResumedActivity`, then from `A=<uid>:<pkg>` on the task
     * line, then from `I=<pkg>/<class>` — the last of which is how the share
     * sheet's resolver is named, and missing it hid a real spill.
     *
     * The per-display listing ends at the first column-0 line that is not a
     * `Display #` header — in practice `ActivityTaskSupervisor state:`, whose
     * `Application tokens in top down Z order` block re-lists *every* task on
     * the device. Reading past it attributed the user's task to the last
     * display in the listing (the agent's), which is exactly the answer that
     * makes a real spill look like nothing happened.
     */
    fun tasks(dump: String): List<MainScreenLog.Task> {
        val out = mutableListOf<MainScreenLog.Task>()
        var displayId = -1
        var pending: MainScreenLog.Task? = null
        // `A=<uid>:<affinity>` names the task's affinity, which is usually the
        // package but may be a class (`com.android.calendar.event.LaunchInfoActivity`
        // was reported as an app). It is only a last resort.
        var affinity: String? = null

        fun flush() {
            pending?.let { task ->
                out.add(task.copy(packageName = task.packageName ?: affinity))
            }
            pending = null
            affinity = null
        }

        var started = false
        for (raw in dump.lineSequence()) {
            if (raw.isNotEmpty() && !raw[0].isWhitespace()) {
                // A column-0 line: either the next display, or the end of the
                // per-display listing. Everything before the first `Display #`
                // is the dump's own preamble and is skipped.
                if (raw.startsWith("Display #")) {
                    flush()
                    started = true
                    displayId = raw.substringAfter("Display #").takeWhile { it.isDigit() }
                        .toIntOrNull() ?: -1
                } else if (started) {
                    flush()
                    break
                }
                continue
            }
            if (!started) continue
            val line = raw.trim()
            if (line.startsWith("* Task{")) {
                flush()
                val taskId = line.substringAfter('#', "").takeWhile { it.isDigit() }.toIntOrNull()
                    ?: continue
                affinity = affinityOf(line)
                pending = MainScreenLog.Task(
                    taskId = taskId,
                    displayId = displayId,
                    packageName = componentPackage(line),
                    topActivity = null,
                    standard = true,
                )
                continue
            }
            val task = pending ?: continue
            // `topResumedActivity` is the authoritative name of what is on
            // screen, and it arrives after the task header. It wins over the
            // affinity for both fields.
            val resumed = resumedActivity(line)
            if (resumed != null) {
                pending = task.copy(
                    packageName = resumed.substringBefore('/'),
                    topActivity = resumed,
                )
                continue
            }
        }
        flush()

        return out.map { task ->
            // A task whose package cannot be named is not an app page: there is
            // nothing to show the user and nothing to move back.
            val named = task.packageName != null
            task.copy(standard = task.standard && named && !isHomeLike(task.packageName))
        }
    }

    /** Launcher, recents and system UI are not "an app opened on the user". */
    private fun isHomeLike(packageName: String?): Boolean {
        val name = packageName ?: return false
        return name.contains("launcher") ||
            name == "com.android.systemui" ||
            name.endsWith(".recents")
    }

    /** The activity component from the resumed record: `pkg/class`. */
    private fun resumedActivity(line: String): String? =
        Regex("topResumedActivity=ActivityRecord\\{[^}]*?\\s([^\\s/]+/[^\\s}]+)").find(line)
            ?.groupValues?.get(1)

    /**
     * The task's affinity from `A=<uid>:<affinity>`, a last resort.
     *
     * The affinity is usually the package and may be a class — Calendar's is
     * `com.android.calendar.event.LaunchInfoActivity` — so it may only name a
     * task that the resumed line did not.
     */
    private fun affinityOf(line: String): String? =
        Regex("A=\\d+:([^\\s/]+)").find(line)?.groupValues?.get(1)

    /** `I=<pkg>/<class>` on the task line: the share sheet's resolver shape. */
    private fun componentPackage(line: String): String? =
        Regex("I=([^\\s/]+)/").find(line)?.groupValues?.get(1)

    /** The dump, or null when it could not be produced. */
    fun dump(): String? = runCatching {
        val process = ProcessBuilder("dumpsys", "activity", "activities")
            .redirectErrorStream(true)
            .start()
        val text = process.inputStream.bufferedReader().use { it.readText() }
        process.waitFor()
        text
    }.getOrNull()

    /** The task list right now, or null. */
    fun current(): List<MainScreenLog.Task>? = dump()?.let(::tasks)

    /**
     * Whether this phone is low on memory, by Android's own definition.
     *
     * `isLowRamDevice` is what the platform uses to decide it will keep fewer
     * apps alive; the same answer decides whether a second screen is worth it.
     */
    fun isLowRam(context: Context): Boolean = runCatching {
        (context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager).isLowRamDevice
    }.getOrDefault(false)

    /** The API level, so a caller need not care how it is being run. */
    fun sdk(): Int = Build.VERSION.SDK_INT

    /** The launcher package, for identifying the home task. */
    fun launcherPackage(context: Context): String? = runCatching {
        val intent = android.content.Intent(android.content.Intent.ACTION_MAIN)
            .addCategory(android.content.Intent.CATEGORY_HOME)
        context.packageManager
            .resolveActivity(intent, PackageManager.MATCH_DEFAULT_ONLY)
            ?.activityInfo?.packageName
    }.getOrNull()
}
