package net.syscity.desktop

import android.accessibilityservice.AccessibilityService
import android.content.Intent
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

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
}
