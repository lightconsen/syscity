package net.syscity.desktop

import android.accessibilityservice.AccessibilityService
import android.content.Intent
import android.os.Build
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.view.accessibility.AccessibilityWindowInfo

/**
 * The accessibility service the agent reads other apps through.
 *
 * The platform's accessibility API is the only way to see what is on screen
 * without a subprocess: `uiautomator dump` costs a shell round trip and an
 * `/sdcard` file, and cannot see anything but the default display. This service
 * hands the same data to Rust through [A11yBridge] on demand — it is not an
 * event-driven shadow of the screen, and it keeps no history, because a stale
 * tree is worse than no tree.
 *
 * The user enables it in Settings › Accessibility › Syscity; nothing here can
 * (or should) do that for them. `DevicePlugin.accessibilityStatus` reports
 * whether it is on, and `openAccessibilitySettings` takes the user there.
 */
class SyscityAccessibilityService : AccessibilityService() {
  private companion object {
    const val TAG = "SyscityA11y"
  }


  override fun onServiceConnected() {
    super.onServiceConnected()
    A11yBridge.attach(this)
  }

  override fun onAccessibilityEvent(event: AccessibilityEvent?) {
    // Nothing to do per event: a snapshot is taken when the agent asks for one.
    // Collecting events here would mean holding a tree that goes stale between
    // reads, which is exactly the failure the indexed-tap safety net exists to
    // catch.
  }

  override fun onInterrupt() {
    // The system revoked the service mid-way; no partial state is kept.
  }

  override fun onUnbind(intent: Intent?): Boolean {
    A11yBridge.detach(this)
    return super.onUnbind(intent)
  }

  /** The active window's root, or null when the screen is locked or empty. */
  fun rootOrNull(): AccessibilityNodeInfo? = try {
    rootInActiveWindow
  } catch (_: Exception) {
    // The window can disappear between the check and the call; a null root is
    // an ordinary answer here, and the caller falls back to ADB.
    null
  }

  /**
   * The focused root on another display — the agent's own screen.
   *
   * A virtual display's windows are not the "active window", so they have to be
   * picked out of the live window list by display id. `getDisplayId()` arrived
   * in API 30; below that a second display's tree is not reachable this way at
   * all, and the caller falls back to reading the frame.
   */
  fun rootForDisplay(displayId: Int): AccessibilityNodeInfo? = try {
    val candidates = windowsForDisplay(displayId)
    Log.d(TAG, "display $displayId: ${candidates.size} window(s) ${candidates.map { it.type }}")
    val chosen = candidates.firstOrNull { it.isFocused } ?: candidates.firstOrNull()
    val root = chosen?.root
    if (root == null && chosen != null) {
      Log.w(TAG, "display $displayId: window ${chosen.type} has no root")
    }
    root
  } catch (e: Exception) {
    Log.e(TAG, "display $displayId: window lookup failed", e)
    null
  }

  /**
   * Every window on `displayId`.
   *
   * `getWindows()` — the public one — covers only the default display.
   * `getWindowsOnAllDisplays()` is `@hide`, returns a `SparseArray` keyed by
   * display id, and is the only way an accessibility service sees the windows of
   * a second display at all. Reflected because the stubs do not carry it; a
   * phone whose build lacks it gets an empty list and the caller reports the
   * tree as unavailable, which is what it is.
   */
  @Suppress("PrivateApi", "DiscouragedPrivateApi")
  private fun windowsForDisplay(displayId: Int): List<AccessibilityWindowInfo> {
    val all = try {
      AccessibilityService::class.java
        .getMethod("getWindowsOnAllDisplays")
        .invoke(this) as? android.util.SparseArray<*>
    } catch (e: Exception) {
      Log.w(TAG, "getWindowsOnAllDisplays unavailable (${e.javaClass.simpleName}); using getWindows()")
      null
    }
    if (all == null) return windows.filter { it.displayId == displayId }
    val onDisplay = mutableListOf<AccessibilityWindowInfo>()
    for (index in 0 until all.size()) {
      (all.valueAt(index) as? AccessibilityWindowInfo)?.let { onDisplay.add(it) }
    }
    return onDisplay.filter { it.displayId == displayId }
  }

}
