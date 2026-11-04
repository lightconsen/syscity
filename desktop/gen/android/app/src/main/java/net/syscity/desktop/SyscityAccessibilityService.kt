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
 * hands the same data to Rust through [A11yBridge] on demand — reads are always
 * taken when the agent asks for one, and nothing here ever caches a tree.
 *
 * What it *does* keep is a bounded log of event **facts** — a window changed, a
 * notification was posted, at what time, in which app — so a waiting agent is
 * woken by the change instead of polling for it. That is not the stale-tree
 * design this service refused: an event records that something happened, never
 * what the screen looked like when it did. The tree is still read fresh,
 * afterwards, by whoever was woken.
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
    // The service's own configuration is only known here, and the event types
    // in it are what the system bound. Reporting them makes "code updated, old
    // subscription still in force" visible instead of a silently empty stream.
    runCatching {
      serviceInfo?.let { DeviceEvents.noteSubscription(it.eventTypes, it.notificationTimeout) }
    }.onFailure { Log.w(TAG, "could not read the service's event subscription", it) }
  }

  override fun onAccessibilityEvent(event: AccessibilityEvent?) {
    if (event == null) return
    // Classification is a pure function of the event; the log and the hand-off
    // to Rust happen in [DeviceEvents]. Nothing here touches the window tree:
    // an event is a fact about *when*, and the tree is read by whoever acts on
    // the wake-up.
    DeviceEvents.record(event)
  }

  override fun onInterrupt() {
    // The system revoked the service mid-way; no partial state is kept.
  }

  override fun onUnbind(intent: Intent?): Boolean {
    A11yBridge.detach(this)
    DeviceEvents.onServiceUnbound()
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
      Log.w(TAG, "getWindowsOnAllDisplays unavailable (${e.javaClass.simpleName})")
      null
    }
    if (all == null) return windows.filter { it.displayId == displayId }

    // The key of this SparseArray **is** the display id, and the value is a
    // `List<AccessibilityWindowInfo>` — not a window. Casting the value to a
    // window yields null for every entry, which reads as "this display has no
    // windows" and was the wrong conclusion for two rounds of debugging.
    val entry = all.get(displayId)
    val list = entry as? List<*>
    if (list == null) {
      Log.d(TAG, "display $displayId: entry is ${entry?.javaClass?.name}, keys=${all.size()}")
      return emptyList()
    }
    val onDisplay = list.filterIsInstance<AccessibilityWindowInfo>()
    Log.d(
      TAG,
      "display $displayId: ${onDisplay.size} window(s) of ${list.size} " +
        onDisplay.map { "${it.type} focused=${it.isFocused}" },
    )
    return onDisplay
  }

}
