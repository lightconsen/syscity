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

  /**
   * How many node handles are kept.
   *
   * A `ref` is a small integer that names one node of one read, so an action
   * can be addressed to the node itself rather than to a point on the screen —
   * `performAction` goes to the control, whereas a tap goes to whatever is at
   * those coordinates when the finger lands. The table is bounded because the
   * nodes hold a window alive: a handle from an old read is allowed to expire,
   * and the caller then gets `NODE_GONE` and re-reads.
   */
  private const val REF_CAPACITY = 2000

  /** `ref` → the node it names, oldest evicted first. */
  private val refs = object : LinkedHashMap<Int, AccessibilityNodeInfo>(256, 0.75f, true) {
    override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Int, AccessibilityNodeInfo>) =
      size > REF_CAPACITY
  }

  private var nextRef = 1

  @Volatile
  private var service: SyscityAccessibilityService? = null

  /** The handle for one node, reusing the one it had if it was already handed out. */
  private fun refFor(node: AccessibilityNodeInfo): Int {
    // A node that came back from this read is the same node: matching by
    // equality (the platform compares window id and source node id) keeps a
    // handle stable across reads, which is what makes it usable at all.
    for ((ref, known) in refs) {
      if (known == node) return ref
    }
    val ref = nextRef++
    refs[ref] = node
    return ref
  }

  /**
   * Run an accessibility action on the node `ref` names.
   *
   * The answer is `{ "ok": bool, "accepted": n, "reason": string? }`. Every
   * failure is an answer, not an exception: the caller falls back to the next
   * way of acting, and falls back knowing *why* this one did not work.
   *
   * `count` repeats a scroll action — a `NumberPicker` moves one row per
   * action, so a batch of five is five rows — and is reported back as
   * `accepted` so the caller knows how far the wheel actually moved.
   */
  fun action(ref: Int, action: String, text: String?, count: Int): JSObject {
    val out = JSObject()
    val node = refs[ref]
    if (node == null) {
      out.put("ok", false)
      out.put("accepted", 0)
      out.put("reason", "node_gone")
      return out
    }
    // A node from an older read can be detached from the window; refreshing
    // re-resolves it, and a refresh that finds nothing means the node is gone.
    if (!node.refresh()) {
      out.put("ok", false)
      out.put("accepted", 0)
      out.put("reason", "node_gone")
      return out
    }
    val times = if (action == "scrollForward" || action == "scrollBackward") {
      count.coerceIn(1, 64)
    } else {
      1
    }
    var accepted = 0
    for (i in 0 until times) {
      val ok = when (action) {
        "click" -> perform(node, AccessibilityNodeInfo.ACTION_CLICK)
        "longClick" -> perform(node, AccessibilityNodeInfo.ACTION_LONG_CLICK)
        "focus" -> perform(node, AccessibilityNodeInfo.ACTION_FOCUS)
        "scrollForward" -> node.performAction(AccessibilityNodeInfo.ACTION_SCROLL_FORWARD)
        "scrollBackward" -> node.performAction(AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD)
        "setText" -> {
          val value = text ?: ""
          val args = android.os.Bundle().apply {
            putCharSequence(
              AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
              value,
            )
          }
          // Focus first: a picker commits its edit when the row loses focus,
          // and `ACTION_SET_TEXT` on an unfocused row is ignored by some builds.
          node.performAction(AccessibilityNodeInfo.ACTION_FOCUS)
          node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
        }
        else -> {
          out.put("ok", false)
          out.put("accepted", 0)
          out.put("reason", "unknown_action")
          return out
        }
      }
      if (ok) accepted++ else break
    }
    out.put("ok", accepted == times)
    out.put("accepted", accepted)
    if (accepted == 0) {
      out.put("reason", "action_refused")
    } else if (accepted < times) {
      out.put("reason", "stopped_early")
    }
    return out
  }


  /**
   * Send one or two fingers as a real touch stream.
   *
   * This is the middle rung between a node action and shell injection. A view
   * that ignores injected `input tap` events — a canvas, a game surface, a
   * slider that tracks the finger — still sees these, because the platform
   * delivers them through the same pipeline as a person's finger. It needs
   * `canPerformGestures`, which the service's XML config already asks for.
   *
   * `onDone` is called back on the main thread with `{ok, reason?}`:
   * `no_service` (the rung does not exist here), `no_paths` (the caller sent
   * nothing usable), `not_dispatched` (the platform refused to start it),
   * `cancelled` (it started and something interrupted it).
   */
  fun gesture(paths: JSONArray?, durationMs: Int, onDone: (JSObject) -> Unit) {
    val reply = { ok: Boolean, reason: String? ->
      val out = JSObject().put("ok", ok)
      if (reason != null) out.put("reason", reason)
      mainHandler.post { onDone(out) }
    }
    val service = this.service
    if (service == null) {
      reply(false, "no_service")
      return
    }
    val durations = durationMs.coerceIn(1, MAX_GESTURE_MS)
    val strokes = ArrayList<android.accessibilityservice.GestureDescription.StrokeDescription>()
    for (index in 0 until (paths?.length() ?: 0)) {
      val points = paths?.optJSONArray(index) ?: continue
      if (points.length() < 2) continue
      val path = android.graphics.Path()
      for (point in 0 until points.length()) {
        val pair = points.optJSONArray(point) ?: continue
        val x = pair.optInt(0, 0).toFloat()
        val y = pair.optInt(1, 0).toFloat()
        if (point == 0) path.moveTo(x, y) else path.lineTo(x, y)
      }
      strokes.add(
        android.accessibilityservice.GestureDescription.StrokeDescription(path, 0, durations.toLong()),
      )
      if (strokes.size == MAX_GESTURE_FINGERS) break
    }
    if (strokes.isEmpty()) {
      reply(false, "no_paths")
      return
    }
    val description = android.accessibilityservice.GestureDescription.Builder()
      .apply { strokes.forEach { addStroke(it) } }
      .build()
    val callback = object : android.accessibilityservice.AccessibilityService.GestureResultCallback() {
      override fun onCompleted(gestureDescription: android.accessibilityservice.GestureDescription?) {
        reply(true, null)
      }

      override fun onCancelled(gestureDescription: android.accessibilityservice.GestureDescription?) {
        reply(false, "cancelled")
      }
    }
    val started = try {
      service.dispatchGesture(description, callback, null)
    } catch (e: Exception) {
      android.util.Log.w("SyscityA11y", "dispatchGesture failed", e)
      false
    }
    if (!started) reply(false, "not_dispatched")
  }

  /** Longest a touched stroke may last (`StrokeDescription` caps at 60 s). */
  private const val MAX_GESTURE_MS = 10_000

  /** How many fingers one gesture may carry. */
  private const val MAX_GESTURE_FINGERS = 2

  /** Where callbacks come back, so a reply never lands on a Binder thread. */
  private val mainHandler = android.os.Handler(android.os.Looper.getMainLooper())


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
   * Run one action, after checking the node offers it.
   *
   * `performAction` returns false for an action the node does not implement,
   * which is indistinguishable from an action that failed; the check makes
   * "this control has no click" a different answer from "the click did
   * nothing", and only the first is worth falling back for.
   */
  private fun perform(node: AccessibilityNodeInfo, actionId: Int): Boolean {
    if (node.actionList.none { it.id == actionId }) return false
    return node.performAction(actionId)
  }

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
            // What the node *is*, as opposed to what it says. Without these the
            // reader cannot tell a scrollable list from a static wrapper (so a
            // scroll has to be a guessed swipe down the middle of the screen),
            // or an editable field from a label (so a write cannot be read
            // back), or a password box from ordinary text (so its contents get
            // summarised to the model). `isPassword` is API 18; minSdk is 26.
            .put("scrollable", node.isScrollable)
            .put("editable", node.isEditable)
            .put("checkable", node.isCheckable)
            // A wheel picker: decided here because only this walk has the
            // children, and a wheel is recognised by its *contents* (a short,
            // centred, evenly spaced run of values) rather than by anything on
            // the node itself.
            .put("wheel", isWheel(node))
            // `getChecked()` (API 21) is the tri-state replacement for the
            // deprecated `isChecked()`. "Partially checked" is a real state for
            // a parent checkbox, but for the model it is still "on" — the point
            // of the flag is whether ticking this node changed anything.
            .put(
              "checked",
              node.checked != AccessibilityNodeInfo.CHECKED_STATE_FALSE,
            )
            .put("selected", node.isSelected)
            .put("focused", node.isFocused)
            .put("password", node.isPassword)
            .put("longClickable", node.isLongClickable)
            .put(
              "scrollActions",
              JSONArray(
                listOf(
                  node.actionList.any {
                    it.id == AccessibilityNodeInfo.ACTION_SCROLL_FORWARD
                  },
                  node.actionList.any {
                    it.id == AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD
                  },
                ),
              ),
            )
            // The handle an action can be addressed to. It names *this node*
            // in *this window*, not a point, so it stays valid while the
            // screen scrolls under it.
            .put("ref", refFor(node))
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

  /** The most characters one wheel item may have and still be a value. */
  private const val MAX_ITEM_CHARS = 8

  /**
   * Whether `node` reads as a wheel picker (a time, a date, a region).
   *
   * Two signs, and the first is exact: the platform's own `NumberPicker` is a
   * wheel by definition. Everything else is a judgement about the children —
   * a short, centred, evenly spaced run of distinct values — which is
   * deliberately conservative (see `WheelMath.looksLikeWheel`): a chat list and
   * a settings page also have evenly spaced short labels, and calling one of
   * those a wheel would make `android_set_wheel` drag it.
   *
   * The judgement is repeated in Rust (`wheel::looks_like_wheel`) because the
   * flat element list is what the second read has: this flag says the *node*
   * is a picker, and the Rust side re-derives the same answer from the items
   * when it needs to know how many of them there are.
   */
  private fun isWheel(node: AccessibilityNodeInfo): Boolean {
    if (node.className?.toString()?.endsWith("NumberPicker") == true) return true
    if (!node.isScrollable || node.childCount !in 3..12) return false
    val box = Rect().also(node::getBoundsInScreen)
    if (box.width() <= 0) return false
    val bounds = Rect()
    val items = ArrayList<Triple<String, Int, Int>>()
    for (index in 0 until node.childCount) {
      val child = node.getChild(index) ?: continue
      if (!child.isVisibleToUser) continue
      // A row's label is its own text, or its only child's.
      val label = ownLabel(child)
        ?: child.takeIf { it.childCount == 1 }?.getChild(0)?.let(::ownLabel)
        ?: return false
      if (label.length > MAX_ITEM_CHARS) return false
      child.getBoundsInScreen(bounds)
      items += Triple(label.trim(), bounds.centerX(), bounds.centerY())
    }
    return looksLikeWheel(items, box.left, box.right)
  }

  /** The node's own text or content description, trimmed; null when it has neither. */
  private fun ownLabel(node: AccessibilityNodeInfo): String? =
    (node.text?.toString()?.trim()?.takeIf { it.isNotEmpty() }
      ?: node.contentDescription?.toString()?.trim()?.takeIf { it.isNotEmpty() })

  /**
   * The geometry test for a run of wheel items: 3–9 of them, each non-empty,
   * short and distinct; every one near the column's vertical centre line; in
   * strictly descending screen order; with even gaps.
   */
  private fun looksLikeWheel(items: List<Triple<String, Int, Int>>, left: Int, right: Int): Boolean {
    if (items.size !in 3..9 || right <= left) return false
    val texts = items.map { normalizeLabel(it.first) }
    if (texts.any { it.isEmpty() || it.length > MAX_ITEM_CHARS }) return false
    if (texts.toSet().size != texts.size) return false
    val mid = (left + right) / 2.0
    if (items.any { kotlin.math.abs(it.second - mid) > (right - left) * 0.15 }) return false
    val sorted = items.sortedBy { it.third }
    val gaps = sorted.zipWithNext { above, below -> below.third - above.third }
    if (gaps.any { it <= 0 }) return false
    return gaps.isEmpty() || gaps.max() < gaps.min() * 1.5
  }

  /** Comparison form: case-folded, whitespace removed, full-width digits folded. */
  private fun normalizeLabel(text: String): String = buildString {
    for (c in text.trim()) {
      when {
        c.isWhitespace() -> Unit
        c in '０'..'９' -> append('0' + (c - '０'))
        else -> append(c.lowercaseChar())
      }
    }
  }
}
