package net.syscity.desktop.shell;

import android.os.ParcelFileDescriptor;

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
     * The display's current frame as a PNG in a file.
     *
     * Returns a read-only fd the caller must close, or null when the display
     * produced nothing.
     */
    ParcelFileDescriptor frame(int displayId) = 9;

    /** The app's own debug surface: which overload created this display, for logs. */
    String describe(int displayId) = 10;
}
