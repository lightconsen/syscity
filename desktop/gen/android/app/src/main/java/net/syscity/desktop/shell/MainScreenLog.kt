package net.syscity.desktop.shell

/**
 * What the user's own screen has shown, one arrival at a time.
 *
 * Facts only: whether an arrival was *caused* by the agent is decided by the
 * caller, which knows what it did and when. Keeping the judgement out of here
 * is what makes both halves testable without a phone.
 */
internal class MainScreenLog(private val keepMs: Long = KEEP_MS) {

    /** One task, as the system lists it. */
    data class Task(
        val taskId: Int,
        val displayId: Int,
        val packageName: String?,
        val topActivity: String?,
        /** A normal app page — not the launcher, recents, or the assistant. */
        val standard: Boolean = true,
    )

    /** One task rising to the top of the user's screen. */
    data class Arrival(
        val taskId: Int,
        val packageName: String?,
        val at: Long,
        /** Where it was last seen: [NEW] never seen before, [MAIN] already on the user's screen. */
        val from: Int,
        /** The same task, showing a different page. */
        val sameTask: Boolean,
        val standard: Boolean,
    )

    private var top: Task? = null
    private var displays: Map<Int, Int> = emptyMap()
    private val arrivals = ArrayDeque<Arrival>()

    /**
     * Feed one task list, most recently used first.
     *
     * `record = false` only takes a baseline: what changed before watching
     * started cannot be attributed to any action.
     */
    fun scan(tasks: List<Task>, now: Long, record: Boolean) {
        val mainTop = tasks.firstOrNull { it.displayId == MAIN }
        val last = top
        if (record && mainTop != null && last != null) {
            when {
                mainTop.taskId != last.taskId ->
                    add(mainTop, now, displays[mainTop.taskId] ?: NEW, sameTask = false)
                mainTop.topActivity != last.topActivity ->
                    add(mainTop, now, MAIN, sameTask = true)
            }
        }
        top = mainTop
        displays = tasks.associate { it.taskId to it.displayId }
        while (arrivals.isNotEmpty() && now - arrivals.first().at > keepMs) {
            arrivals.removeFirst()
        }
    }

    private fun add(task: Task, now: Long, from: Int, sameTask: Boolean) {
        arrivals.addLast(
            Arrival(task.taskId, task.packageName, now, from, sameTask, task.standard),
        )
    }

    /** The task on top of the user's screen right now, or -1. */
    val topTaskId: Int get() = top?.taskId ?: -1

    /** Arrivals at or after `since`, oldest first. */
    fun since(since: Long): List<Arrival> = arrivals.filter { it.at >= since }

    /** Where a task was last seen (-1 when it is gone). */
    fun displayOf(taskId: Int): Int = displays[taskId] ?: -1

    companion object {
        const val MAIN = 0
        const val NEW = -1
        const val KEEP_MS = 60_000L
    }
}
