package net.syscity.desktop.shell

/**
 * The order of `MotionEvent`s that makes up one multi-finger gesture.
 *
 * The input dispatcher validates the pointer count against the action: an
 * `ACTION_DOWN` carrying two pointers is refused outright with
 * `invalid event: invalid pointer count 2`, which is how the first attempt at a
 * pinch failed. Fingers therefore have to arrive one at a time —
 * `ACTION_DOWN` for the first, `ACTION_POINTER_DOWN` for each one after it —
 * and leave the same way, so every event names exactly the pointers that are
 * down at that moment.
 *
 * Split out from the event building so the sequence can be checked on a JVM
 * without an Android runtime, which is the only place a mistake here is cheap.
 */
internal object GesturePlan {

    /** One step of a gesture: what to send, and how many pointers it names. */
    data class Step(val action: Int, val count: Int, val actionIndex: Int, val position: Int)

    // The action constants, repeated so this file has no Android dependency.
    const val ACTION_DOWN = 0
    const val ACTION_UP = 1
    const val ACTION_MOVE = 2
    const val ACTION_POINTER_DOWN = 5
    const val ACTION_POINTER_UP = 6

    /**
     * The events for [fingers] fingers moving through [steps] positions each.
     *
     * `position` is the index into each finger's own path; the last position is
     * held once a shorter path runs out, so a finger that stops moving stays
     * down where it stopped.
     */
    fun of(fingers: Int, steps: Int): List<Step> {
        if (fingers < 1 || steps < 1) return emptyList()
        val out = ArrayList<Step>(fingers * 2 + steps)
        out += Step(ACTION_DOWN, 1, -1, 0)
        for (finger in 1 until fingers) {
            out += Step(ACTION_POINTER_DOWN, finger + 1, finger, 0)
        }
        for (position in 1 until steps) {
            out += Step(ACTION_MOVE, fingers, -1, position)
        }
        // Lift in reverse: the last finger down is the first one up.
        for (finger in fingers - 1 downTo 1) {
            out += Step(ACTION_POINTER_UP, finger + 1, finger, steps - 1)
        }
        out += Step(ACTION_UP, 1, -1, steps - 1)
        return out
    }

    /**
     * Where finger [finger] is, as an index into its own path.
     *
     * A finger that has finished moving holds its last point; one whose path is
     * shorter than the gesture still has to report a position on every event.
     */
    fun positionFor(position: Int, pathLength: Int): Int =
        position.coerceIn(0, (pathLength - 1).coerceAtLeast(0))
}
