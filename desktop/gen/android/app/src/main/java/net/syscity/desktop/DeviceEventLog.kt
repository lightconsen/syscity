package net.syscity.desktop

import android.view.accessibility.AccessibilityEvent

/**
 * The pure half of the device event stream: what one event means, and the
 * bounded log that keeps the last few.
 *
 * Kept apart from [DeviceEvents] — which owns the accessibility service, the
 * executor and the hand-off to Rust — so the rules can be tested on a JVM. The
 * `TYPE_*` constants below are compile-time `static final int`s and are inlined
 * by the compiler, so referring to them loads no framework class.
 */

/** Longest text kept from one event. */
const val EVENT_MAX_TEXT = 2000

const val EVENT_KIND_WINDOW = "window"
const val EVENT_KIND_CONTENT = "content"
const val EVENT_KIND_NOTIFICATION = "notification"

/**
 * One fact about the phone, safe to hand to Rust.
 *
 * `seq` orders events inside the log and is the cursor a reader passes back as
 * `since`. `atUptimeMs` is the platform's monotonic clock — the only one that
 * keeps ordering across a sleep.
 */
data class DeviceEvent(
  val seq: Long,
  val kind: String,
  val packageName: String,
  val atUptimeMs: Long,
  val title: String? = null,
  val text: String? = null,
  val truncated: Boolean = false,
) {
  /**
   * The wire form, parsed by `DeviceEvent::from_payload` in `src/device/mod.rs`.
   *
   * Hand-written rather than `org.json.JSONObject`: this runs on the
   * accessibility service's main thread once per event, and a serializer that
   * only has to emit is a few lines.
   */
  fun toJson(): String = buildString(128) {
    append('{')
    append("\"seq\":").append(seq)
    append(",\"kind\":")
    appendJsonString(kind)
    append(",\"package\":")
    appendJsonString(packageName)
    append(",\"atUptimeMs\":").append(atUptimeMs)
    title?.let {
      append(",\"title\":")
      appendJsonString(it)
    }
    text?.let {
      append(",\"text\":")
      appendJsonString(it)
    }
    if (truncated) {
      append(",\"truncated\":true")
    }
    append('}')
  }
}

/**
 * Quote [value] as a JSON string.
 *
 * Control characters become `\u00XX` — legal JSON, and what a notification
 * body with a newline needs. Everything else, CJK included, passes through as
 * UTF-8.
 */
fun StringBuilder.appendJsonString(value: String) {
  append('"')
  for (char in value) {
    when (char) {
      '"' -> append("\\\"")
      '\\' -> append("\\\\")
      '\n' -> append("\\n")
      '\r' -> append("\\r")
      '\t' -> append("\\t")
      '\b' -> append("\\b")
      '\u000C' -> append("\\f")
      else -> if (char < ' ') {
        append("\\u")
        append(char.code.toString(16).padStart(4, '0'))
      } else {
        append(char)
      }
    }
  }
  append('"')
}

/**
 * Turn one accessibility event into a fact, or `null` for a type not handled.
 *
 * `title` and `text` are the notification's own payload; `fallbackText` is
 * `event.text`, which is all some ROMs leave when they withhold the
 * `Notification`. A notification with no body at all still becomes an event:
 * "a notification arrived" is the fact that wakes a waiter, and its title is
 * enough to say which.
 */
fun classifyEvent(
  eventType: Int,
  packageName: String?,
  title: String?,
  text: String?,
  fallbackText: String?,
  atUptimeMs: Long,
): DeviceEvent? {
  val kind = when (eventType) {
    AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED -> EVENT_KIND_WINDOW
    AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED -> EVENT_KIND_CONTENT
    AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED -> EVENT_KIND_NOTIFICATION
    else -> return null
  }
  var truncated = false
  fun limit(value: String?): String? {
    val clean = value?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    return if (clean.length > EVENT_MAX_TEXT) {
      truncated = true
      clean.take(EVENT_MAX_TEXT)
    } else {
      clean
    }
  }
  val body = if (kind == EVENT_KIND_NOTIFICATION) {
    limit(text) ?: limit(fallbackText)
  } else {
    limit(fallbackText)
  }
  return DeviceEvent(
    seq = 0,
    kind = kind,
    packageName = packageName?.trim().orEmpty(),
    atUptimeMs = atUptimeMs,
    title = limit(title),
    text = body,
    truncated = truncated,
  )
}

/**
 * The last [capacity] events, oldest first.
 *
 * Bounded on purpose: a phone left alone overnight posts thousands of content
 * events, and an unbounded list would be a slow memory leak in the one process
 * that is always running. Past the capacity the oldest goes, and [dropped]
 * counts them so a reader that asks for history it has already lost can be
 * told, rather than quietly shown a gap.
 */
class EventRing(private val capacity: Int) {
  private val items = ArrayDeque<DeviceEvent>()
  private var lastSeq = 0L
  private var droppedCount = 0L

  /** Append one, numbering it and dropping the oldest if full. */
  @Synchronized
  fun add(event: DeviceEvent): DeviceEvent {
    lastSeq += 1
    val numbered = event.copy(seq = lastSeq)
    items.addLast(numbered)
    while (items.size > capacity) {
      items.removeFirst()
      droppedCount += 1
    }
    return numbered
  }

  /** Events after `since` (inclusive of nothing), newest last, at most [limit]. */
  @Synchronized
  fun history(since: Long? = null, kind: String? = null, limit: Int = items.size): List<DeviceEvent> =
    items.asSequence()
      .filter { since == null || it.seq > since }
      .filter { kind == null || it.kind == kind }
      .take(limit.coerceAtLeast(0))
      .toList()

  /** The highest sequence number ever added. */
  @Synchronized
  fun cursor(): Long = lastSeq

  /** How many have rolled out since the log started. */
  @Synchronized
  fun dropped(): Long = droppedCount

  /** The oldest number still held, or `null` when empty. */
  @Synchronized
  fun oldest(): Long? = items.firstOrNull()?.seq

  @Synchronized
  fun size(): Int = items.size

  /** Clear everything. Tests only — the service never resets its history. */
  @Synchronized
  fun clear() {
    items.clear()
    lastSeq = 0
    droppedCount = 0
  }
}
