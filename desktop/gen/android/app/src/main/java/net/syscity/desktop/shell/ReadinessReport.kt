package net.syscity.desktop.shell

/**
 * What the phone is doing right now, in the terms a writer on the host can act
 * on.
 *
 * The host cannot see the lock screen and cannot see which task a display is
 * showing; this is the half of the readiness check that lives where those
 * facts are. It reports and does not judge — refusing to write is the caller's
 * decision — which is what lets the rule be tested on a JVM.
 */
internal object ReadinessReport {

    /** The task on top of one screen; `task < 0` means the screen is empty. */
    data class Top(val task: Int, val packageName: String?)

    data class State(
        val locked: Boolean,
        /** The user's own screen (display 0). */
        val main: Top,
        /** One entry per display this helper owns, keyed by display id. */
        val displays: Map<Int, Top>,
    ) {
        /**
         * What a display holds now: `null` when this helper does not own it,
         * a task of `-1` when it owns it and nothing is on it.
         *
         * The two are different facts and are kept apart here. The host
         * collapses both to "nothing to compare" — an empty display cannot
         * have moved — but a caller asking why a write was refused should not
         * be told a display is empty when it was never opened.
         */
        fun topOf(display: Int): Top? =
            if (display == 0) main else displays[display]
    }

    /**
     * Build the report.
     *
     * `tasks` is most-recently-used first, as the system lists them, so the
     * first task on a display is the one showing. A screen with no task is
     * reported as `-1` rather than omitted: "empty" and "not known" are the
     * same answer to the only question being asked — is there a screen here
     * that a coordinate could be wrong about.
     */
    fun of(tasks: List<MainScreenLog.Task>, open: List<Int>, locked: Boolean): State {
        fun topOf(display: Int): Top {
            val top = tasks.firstOrNull { it.displayId == display }
            return Top(task = top?.taskId ?: -1, packageName = top?.packageName)
        }
        return State(
            locked = locked,
            main = topOf(0),
            displays = open.associateWith { topOf(it) },
        )
    }

    /** The wire form the app parses. */
    fun toJson(state: State): String {
        fun json(top: Top) = org.json.JSONObject()
            .put("task", top.task)
            .put("package", top.packageName ?: org.json.JSONObject.NULL)

        val displays = org.json.JSONArray()
        for ((id, top) in state.displays) {
            displays.put(json(top).put("id", id))
        }
        return org.json.JSONObject()
            .put("locked", state.locked)
            .put("main", json(state.main))
            .put("displays", displays)
            .toString()
    }
}
