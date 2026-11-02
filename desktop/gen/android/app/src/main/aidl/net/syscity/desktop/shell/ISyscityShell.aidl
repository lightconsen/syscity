package net.syscity.desktop.shell;

/**
 * The display helper's Binder interface, as seen from the app.
 *
 * The helper runs in the `shell` SELinux domain (it is started through
 * `app_process` from `adb shell`, which is the only way to reach
 * `CAPTURE_VIDEO_OUTPUT`). An untagged app cannot open a unix socket there —
 * `avc: denied { connectto } untrusted_app → shell` — but it *can* receive a
 * Binder from it through a ContentProvider, which is a direction Android's
 * policy allows. So the helper hands this interface over once at start-up and
 * everything afterwards is ordinary Binder calls.
 *
 * Frames cross as a `ParcelFileDescriptor`-backed temp file rather than a byte
 * array: a Binder transaction is capped at 1 MB and a 1080x2400 PNG is not.
 */
interface ISyscityShell {
    /** `[uid, pid]` of the process this Binder lives in; checked before it is accepted. */
    int[] identity() = 1;

    /** Create a display and return its id, or -1. */
    int createDisplay(int width, int height, int dpi) = 2;

    /** Put `package` on the display. Returns 0 on success. */
    int startApp(int displayId, String packageName) = 3;

    /** Destroy a display and everything on it. */
    void releaseDisplay(int displayId) = 4;

    /** Tap at a point on the display. */
    boolean injectTap(int displayId, int x, int y) = 5;

    /** Swipe on the display. */
    boolean injectSwipe(int displayId, int x1, int y1, int x2, int y2, int durationMs) = 6;

    /** Press a key on the display. */
    boolean injectKey(int displayId, int keyCode) = 7;

    /** Type text into whatever has focus on the display. */
    boolean injectText(int displayId, String text) = 8;

    /**
     * The display's current frame, written to a PNG file.
     *
     * Returns the file's path — the reader is the app's own process and opens it
     * directly — or null when the display produced nothing.
     */
    String frame(int displayId) = 9;

    /** The app's own debug surface: which overload created this display, for logs. */
    String describe(int displayId) = 10;

    /**
     * Move the display's top task onto the user's own screen (display 0), so
     * they can act on it directly when the agent cannot.
     */
    boolean moveToUser(int displayId) = 11;

    /** Move it back to this display after the user is done. */
    boolean moveToAgent(int displayId) = 12;

    /**
     * What is on the user's own screen now: `"<taskId>|<package>"`, or `""`.
     *
     * An action that starts an app from a background service lands it on the
     * user's display, not this one, and nothing on this display changes. This
     * is how that becomes visible.
     */
    String mainScreenTop() = 13;

    /** Move an arbitrary task here (used to pull a spilled app back). */
    boolean moveTaskToMe(int displayId, int taskId) = 14;

    /**
     * One gesture of one or two fingers, as flat parallel arrays.
     *
     * `x`/`y`/`path` describe every point of every finger: `path[i]` is the
     * finger a point belongs to, and the points of one finger are in time
     * order. The fingers start together and run for `durationMs`. Coordinates
     * are the display's own pixels.
     *
     * `injectTap`/`injectSwipe` go through `input -d`, which only ever sends a
     * single straight stroke. A pinch is two fingers moving apart at the same
     * time, and that has no shell equivalent at all.
     */
    boolean injectGesture(int displayId, in int[] x, in int[] y, in int[] path, int durationMs) = 15;

    /**
     * Put `text` on the device clipboard and paste it with Ctrl+V on the
     * display, then put the user's own clipboard back.
     *
     * `input -d text` types through the key character map and mangles anything
     * outside ASCII; a clipboard paste carries any character the app can
     * render. Returns 0 when it pasted, 1 when the user's clipboard could not
     * be read (nothing was changed), 2 on failure.
     */
    int pasteText(int displayId, String text) = 16;

    /**
     * Start watching the user's screen for `windowMs`, and return the task on
     * top of it right now (or -1).
     *
     * An action can start an app that lands on the user's screen; a single
     * look afterwards cannot tell that apart from an app the user opened
     * themselves. Watching records what actually rose to the top, and when.
     */
    int watchMainScreen(int windowMs) = 17;

    /**
     * What rose to the top of the user's screen since `sinceUptimeMs`
     * (`SystemClock.uptimeMillis`), as JSON: an array of
     * `{task, package, at, from, sameTask, standard, now}`, oldest first.
     *
     * `from` is the display the task was last seen on (-1 when new), `now` is
     * where it is now (-1 when it is gone), `sameTask` marks the same task
     * showing a different page.
     */
    String mainArrivals(long sinceUptimeMs) = 18;

    /**
     * The display's latest frame as a coarse brightness grid: `columns`,
     * `rows`, then one byte per cell, row-major.
     *
     * A cheap "did the picture change" that works on screens the accessibility
     * tree describes nothing on — a game, a map, a camera preview.
     */
    byte[] frameSignature(int displayId) = 19;

    /**
     * The most displays this device will run at once, and why not more.
     *
     * Parallel tasks need per-display focus (API 34+); a low-memory phone gets
     * one either way, and once a second display has failed to appear in this
     * session nothing retries it.
     */
    String parallelLimit() = 20;

    /** How many displays this helper currently owns. */
    int displayCount() = 21;
}
