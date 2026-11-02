package net.syscity.desktop.shell

/**
 * Who may run how many displays at once.
 *
 * Each parallel task needs its own display, and a display needs its own focus to
 * be usable while another one is taking input — `FLAG_OWN_FOCUS`, API 34. Below
 * that a second display takes the keyboard away from the user mid-sentence, so
 * there is one. A low-memory phone gets one for a different reason, and a second
 * display that already failed to appear is not retried within the session.
 */
internal object ParallelLimit {
    const val MIN_SDK = 34
    const val MAX = 3

    /**
     * `(how many displays may exist, why not more or null)`.
     *
     * @param secondDisplayFailed whether creating a second display has already
     *   failed once in this session.
     */
    fun of(sdk: Int, lowRam: Boolean, secondDisplayFailed: Boolean): Pair<Int, String?> =
        when {
            sdk < MIN_SDK -> 1 to
                "Android $MIN_SDK or newer is needed to run a second screen in parallel: below " +
                "that, a second display takes the keyboard away from the user."
            lowRam -> 1 to "this phone reports low memory, so it runs one task at a time."
            secondDisplayFailed -> 1 to "a second display failed to start earlier in this session."
            else -> MAX to null
        }

    /** How many more may be opened, given how many are open and what the device allows. */
    fun remaining(open: Int, limit: Int): Int = (limit - open).coerceAtLeast(0)
}

/**
 * A coarse brightness grid of one frame.
 *
 * The accessibility tree says nothing about a game, a map, or a camera preview,
 * so "did the screen change" has to come from pixels. A 12x24 grid of average
 * luminance is enough to see a page transition and cheap enough to compare
 * without decoding anything.
 */
internal object FrameSignature {
    const val GRID_W = 12
    const val GRID_H = 24
    /** Cells differing by more than this count as changed. */
    const val CELL_DELTA = 16
    /** Fewer than this many changed cells is a blinking cursor, not a change. */
    const val MIN_CELLS = 3
    private const val CELL_SAMPLES = 4

    /** `columns`, `rows`, then one byte per cell in row-major order. */
    fun of(width: Int, height: Int, pixel: (Int, Int) -> Int): ByteArray {
        val cells = lumaGrid(width, height, pixel)
        return ByteArray(cells.size + 2).also { out ->
            out[0] = GRID_W.toByte()
            out[1] = GRID_H.toByte()
            for ((index, value) in cells.withIndex()) {
                out[index + 2] = value.coerceIn(0, 255).toByte()
            }
        }
    }

    /** Average luminance per cell, row-major. */
    fun lumaGrid(width: Int, height: Int, pixel: (Int, Int) -> Int): IntArray {
        val out = IntArray(GRID_W * GRID_H)
        val cellWidth = width / GRID_W
        val cellHeight = height / GRID_H
        if (cellWidth == 0 || cellHeight == 0) return out
        for (gy in 0 until GRID_H) {
            for (gx in 0 until GRID_W) {
                var sum = 0
                for (sy in 0 until CELL_SAMPLES) {
                    for (sx in 0 until CELL_SAMPLES) {
                        sum += luma(
                            pixel(
                                gx * cellWidth + sx * cellWidth / CELL_SAMPLES,
                                gy * cellHeight + sy * cellHeight / CELL_SAMPLES,
                            ),
                        )
                    }
                }
                out[gy * GRID_W + gx] = sum / (CELL_SAMPLES * CELL_SAMPLES)
            }
        }
        return out
    }

    private fun luma(argb: Int): Int {
        val red = argb shr 16 and 0xff
        val green = argb shr 8 and 0xff
        val blue = argb and 0xff
        return (red * 3 + green * 6 + blue) / 10
    }
}
