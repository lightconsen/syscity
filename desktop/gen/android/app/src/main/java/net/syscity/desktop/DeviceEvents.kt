package net.syscity.desktop

import android.app.Notification
import android.os.SystemClock
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import java.util.concurrent.Executors

/**
 * What changed on the phone, as facts rather than as a picture of the screen.
 *
 * The accessibility service is woken by the system when a window opens, when
 * content changes, and when a notification is posted. Recording *that* — the
 * kind, the app, the time, and whatever text the notification carried — lets a
 * waiting agent be woken by the change instead of re-reading the tree every
 * 250 ms. It deliberately does not record the tree: a tree kept between reads
 * goes stale, and a stale tree is worse than no tree. Reading the screen stays
 * on demand, always at the moment the agent acts.
 *
 * What accessibility gives and withholds (worth knowing before trusting this):
 *
 * - `TYPE_NOTIFICATION_STATE_CHANGED` arrives **once**, when the notification
 *   is posted. There is no event for it being dismissed, and no way to list the
 *   notifications that were already there when the service started.
 * - `parcelableData` carries a [Notification] on many ROMs, but some withhold
 *   it under a security policy; then only `event.text` is left, and this falls
 *   back to it.
 * - Reading the notification *list*, or cancelling one, is the job of a
 *   `NotificationListenerService` — a separate user grant this service does not
 *   ask for. The event stream only ever says "a notification arrived".
 *
 * This object is the Android-bound half: it turns an [AccessibilityEvent] into
 * a [DeviceEvent], keeps them in an [EventRing], and hands each finished string
 * to Rust. The classification and the ring are in `DeviceEventLog.kt` so they
 * can be tested on a JVM.
 */
object DeviceEvents {
  private const val TAG = "SyscityEvents"

  /** How many event facts are kept. Past this the oldest goes. */
  private const val CAPACITY = 512

  private val ring = EventRing(CAPACITY)

  /** Set once the JNI hand-off has failed; the push path is not tried again. */
  @Volatile private var streamBroken = false

  /** The event types the system actually bound the service to. */
  @Volatile private var subscriptionTypes: Int? = null

  /** The system's debounce window at bind time. */
  @Volatile private var subscriptionTimeout: Long? = null

  /**
   * The hand-off to Rust, on a thread of its own.
   *
   * `onAccessibilityEvent` runs on the main thread; calling into JNI there
   * would put the bridge's own work in front of every animation frame. One
   * serial thread is enough — order is preserved, and a slow consumer can only
   * make this queue grow, never the accessibility service's latency.
   */
  private val executor = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "syscity-device-events").apply { isDaemon = true }
  }

  /** Every event type this build handles. */
  internal val SUBSCRIBED_TYPES = intArrayOf(
    AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED,
    AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED,
    AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED,
  )

  /**
   * Called from `onServiceConnected`, with the configuration the system bound.
   *
   * `accessibilityEventTypes` is read once per enable. A build that starts
   * handling a new type does not get it until the user toggles the service off
   * and on — so a missing type is logged here, which is the only place the
   * mismatch can be seen.
   */
  fun noteSubscription(eventTypes: Int, notificationTimeout: Long) {
    subscriptionTypes = eventTypes
    subscriptionTimeout = notificationTimeout
    val missing = SUBSCRIBED_TYPES.count { eventTypes and it == 0 }
    if (missing == 0) {
      Log.i(TAG, "subscribed to event types 0x${eventTypes.toString(16)}")
    } else {
      Log.w(
        TAG,
        "service is bound without $missing event type(s) this build handles; " +
          "toggle the service off and on to pick up the new subscription",
      )
    }
  }

  fun onServiceUnbound() {
    // Bound-state only: the history is still true after the service goes away,
    // and a reader may still be catching up on it.
    subscriptionTypes = null
    subscriptionTimeout = null
  }

  /**
   * Record one event and hand it to Rust. Called on the main thread.
   *
   * Everything about the event is read here, while it is still valid, and the
   * JNI call receives only the finished string — the callback thread never
   * touches the service or the event object.
   */
  fun record(event: AccessibilityEvent) {
    val classified = try {
      classifyEvent(
        eventType = event.eventType,
        packageName = runCatching { event.packageName?.toString() }.getOrNull(),
        title = runCatching { notificationTitle(event) }.getOrNull(),
        text = runCatching { notificationText(event) }.getOrNull(),
        fallbackText = runCatching { event.text?.joinToString(" ") }.getOrNull(),
        atUptimeMs = SystemClock.uptimeMillis(),
      )
    } catch (t: Throwable) {
      // The event can be recycled underneath us by a misbehaving ROM; losing
      // one event is not worth taking the main thread down for.
      Log.w(TAG, "dropped an unreadable accessibility event", t)
      return
    } ?: return

    dispatch(ring.add(classified).toJson())
  }

  /**
   * Hand one serialized event to Rust.
   *
   * The class initialization that loads the native library happens on this
   * thread, on the first call. When the library is missing or built for another
   * ABI that is an [UnsatisfiedLinkError] — an `Error`, not an `Exception`, and
   * one that would kill this executor's thread outright. Catching `Throwable`
   * marks the stream unavailable once and leaves the ring working, so
   * `android_event` can still answer from the buffer and say the push path is
   * not there.
   */
  private fun dispatch(json: String) {
    if (streamBroken) return
    executor.execute {
      try {
        NativeEvents.onDeviceEvent(json)
      } catch (t: Throwable) {
        streamBroken = true
        Log.w(TAG, "device event stream unavailable; the buffer still answers", t)
      }
    }
  }

  /** Whether the JNI hand-off has been found broken on this device. */
  fun streamAvailable(): Boolean = !streamBroken

  /** The bound event types, or `null` while the service is not connected. */
  fun subscription(): Int? = subscriptionTypes

  /** The system's debounce window, or `null` while not connected. */
  fun notificationTimeout(): Long? = subscriptionTimeout

  /** One page of history, oldest first. */
  fun history(since: Long?, kind: String?, limit: Int): List<DeviceEvent> =
    ring.history(since, kind, limit)

  /** The highest sequence number recorded, or 0 when nothing has been. */
  fun cursor(): Long = ring.cursor()

  /** How many events have rolled out of the buffer. */
  fun dropped(): Long = ring.dropped()

  /** The oldest sequence number still buffered, or `null` when empty. */
  fun oldest(): Long? = ring.oldest()

  /** Clear everything. Tests only — the service never resets its history. */
  fun reset() {
    ring.clear()
    streamBroken = false
    subscriptionTypes = null
    subscriptionTimeout = null
  }

  private fun notificationTitle(event: AccessibilityEvent): String? {
    if (event.eventType != AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED) return null
    // `parcelableData` is typed `Parcelable?`; asking for a Notification is the
    // documented use, and some ROMs hand over a bare Text/Toast payload instead.
    val notification = event.parcelableData as? Notification ?: return null
    return notification.extras?.getCharSequence(Notification.EXTRA_TITLE)?.toString()
  }

  private fun notificationText(event: AccessibilityEvent): String? {
    if (event.eventType != AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED) return null
    val notification = event.parcelableData as? Notification ?: return null
    return notification.extras?.getCharSequence(Notification.EXTRA_TEXT)?.toString()
  }
}
