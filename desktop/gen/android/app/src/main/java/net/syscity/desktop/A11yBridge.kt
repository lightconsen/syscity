package net.syscity.desktop

import android.content.Context
import android.graphics.Rect
import android.hardware.display.DisplayManager
import android.provider.Settings
import android.view.accessibility.AccessibilityNodeInfo
import app.tauri.plugin.JSObject
import org.json.JSONArray
import org.json.JSONObject

/**
 * Snapshot of the focused window, for the Rust side.
 *
 * The payload is the contract with `src/computer/platform/mobile/a11y.rs`:
 *
 * ```
 * { "enabled": true,
 *   "package": "com.android.settings",
 *   "display": { "width": 1080, "height": 2400 },
 *   "truncated": false,
 *   "nodes": [ { "cls": "android.widget.Button", "text": "OK", "desc": "",
 *                "rid": "com.android.settings:id/ok", "clickable": true,
 *                "enabled": true, "depth": 3, "bounds": [x1, y1, x2, y2] } ] }
 * ```
 *
 * Nodes arrive in document order and unnarrowed: Rust decides what the model
 * can act on and assigns the indices, so the same filtering rules apply
 * whichever source produced the tree. `enabled: false` means the service is off
 * and is a fallback signal, not an error.
 */
object A11yBridge {

  /** Enough for any real screen; a bound because a runaway tree must not hang a turn. */
  private const val MAX_NODES = 1200

  /** Deep enough for any app, shallow enough that a pathological tree terminates. */
  private const val MAX_DEPTH = 40

  @Volatile
  private var service: SyscityAccessibilityService? = null

  fun attach(service: SyscityAccessibilityService) {
    this.service = service
  }

  fun detach(service: SyscityAccessibilityService) {
    if (this.service === service) {
      this.service = null
    }
  }

  /** Whether the user has switched the service on in system settings. */
  fun isEnabled(context: Context): Boolean {
    val enabled = Settings.Secure.getString(
      context.contentResolver,
      Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
    ) ?: return false
    val expected = "${context.packageName}/${SyscityAccessibilityService::class.java.name}"
    return enabled.split(':').any { it.equals(expected, ignoreCase = true) }
  }

  /** Whether the service is on *and* currently holding a window. */
  fun isConnected(): Boolean = service?.rootOrNull() != null

  /**
   * One read of the focused window.
   *
   * Null when the service is not running at all — the caller then uses ADB. A
   * running service with no root (lock screen, a mid-transition frame) answers
   * with an empty node list instead, which the Rust side treats the same way.
   */
  fun snapshot(displayId: Int? = null): JSObject? {
    val t0 = android.os.SystemClock.elapsedRealtime()
    val service = this.service ?: return null
    val out = JSObject()
    out.put("enabled", true)

    val root = if (displayId == null || displayId == 0) {
      service.rootOrNull()
    } else {
      service.rootForDisplay(displayId)
    }
    if (root == null) {
      out.put("nodes", JSONArray())
      out.put("truncated", false)
      return out
    }

    root.packageName?.toString()?.let { out.put("package", it) }
    val metrics = if (displayId == null || displayId == 0) {
      service.resources.displayMetrics
    } else {
      // The agent's display has its own size; borrowing the main screen's would
      // put every reported coordinate in the wrong space.
      val display = service.getSystemService(DisplayManager::class.java)?.getDisplay(displayId)
      val mode = display?.mode
      android.util.DisplayMetrics().apply {
        widthPixels = mode?.physicalWidth ?: 0
        heightPixels = mode?.physicalHeight ?: 0
      }
    }
    out.put(
      "display",
      JSONObject().put("width", metrics.widthPixels).put("height", metrics.heightPixels),
    )

    val nodes = JSONArray()
    var truncated = false
    val bounds = Rect()

    // Depth-first in document order. Children are pushed in reverse so the
    // stack pops them back in the order the tree lists them — the model's
    // element numbers have to match what a person reading the screen would
    // count down the page.
    val stack = ArrayList<Pair<AccessibilityNodeInfo, Int>>()
    stack.add(root to 0)

    while (stack.isNotEmpty()) {
      if (nodes.length() >= MAX_NODES) {
        truncated = true
        break
      }
      val (node, depth) = stack.removeAt(stack.size - 1)
      node.getBoundsInScreen(bounds)
      if (bounds.width() > 0 && bounds.height() > 0) {
        nodes.put(
          JSONObject()
            .put("cls", node.className?.toString() ?: "")
            .put("text", node.text?.toString() ?: "")
            .put("desc", node.contentDescription?.toString() ?: "")
            .put("rid", node.viewIdResourceName ?: "")
            .put("clickable", node.isClickable)
            .put("enabled", node.isEnabled)
            .put("depth", depth)
            .put(
              "bounds",
              JSONArray(listOf(bounds.left, bounds.top, bounds.right, bounds.bottom)),
            ),
        )
      }
      if (depth >= MAX_DEPTH) {
        truncated = true
        continue
      }
      for (index in node.childCount - 1 downTo 0) {
        val child = node.getChild(index) ?: continue
        stack.add(child to depth + 1)
      }
    }

    out.put("nodes", nodes)
    out.put("truncated", truncated)
    android.util.Log.d(
      "SyscityBridge",
      "display=$displayId nodes=${nodes.length()} walk=${android.os.SystemClock.elapsedRealtime() - t0}ms",
    )
    return out
  }
}
