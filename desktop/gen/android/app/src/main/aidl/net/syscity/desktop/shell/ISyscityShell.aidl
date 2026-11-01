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
}
