package net.syscity.desktop

import android.view.accessibility.AccessibilityEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The rules behind the device event stream, without a device.
 *
 * These are the parts that decide what an event *means* and what happens when
 * more arrive than the buffer holds. The Android-bound half (`DeviceEvents`)
 * only moves bytes between them, and `NativeEvents` is a one-line JNI call —
 * neither can be checked here, which is why the rules were put where they can.
 */
class DeviceEventLogTest {

    // ── Classification ────────────────────────────────────────────────

    @Test
    fun aWindowChangeIsAWindowFact() {
        val event = classifyEvent(
            AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED,
            packageName = " com.example.mail ",
            title = null,
            text = null,
            fallbackText = "Inbox",
            atUptimeMs = 1_000,
        )!!
        assertEquals(EVENT_KIND_WINDOW, event.kind)
        assertEquals("com.example.mail", event.packageName)
        assertEquals("Inbox", event.text)
        assertEquals(1_000, event.atUptimeMs)
    }

    @Test
    fun aContentChangeIsAContentFact() {
        val event = classifyEvent(
            AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED,
            "com.example.mail", null, null, null, 5,
        )!!
        assertEquals(EVENT_KIND_CONTENT, event.kind)
    }

    @Test
    fun aTypeThisBuildDoesNotHandleIsNotAFact() {
        // `TYPE_VIEW_CLICKED` and friends are never subscribed; if a ROM sends
        // one anyway it must not become history the model can act on.
        assertNull(
            classifyEvent(
                AccessibilityEvent.TYPE_VIEW_CLICKED,
                "com.example", null, null, "x", 1,
            ),
        )
    }

    @Test
    fun aNotificationPrefersItsOwnPayloadOverTheEventText() {
        val event = classifyEvent(
            AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED,
            "com.example.chat",
            title = "Alice",
            text = "Are you free?",
            fallbackText = "Alice: Are you free?",
            atUptimeMs = 7,
        )!!
        assertEquals(EVENT_KIND_NOTIFICATION, event.kind)
        assertEquals("Alice", event.title)
        assertEquals("Are you free?", event.text)
    }

    @Test
    fun aRomThatWithholdsTheNotificationStillYieldsItsText() {
        // `parcelableData` is Policy-protected on some builds; only `event.text`
        // survives. The event is still worth having.
        val event = classifyEvent(
            AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED,
            "com.example.chat", null, null, "Alice: Are you free?", 7,
        )!!
        assertEquals(EVENT_KIND_NOTIFICATION, event.kind)
        assertEquals("Alice: Are you free?", event.text)
    }

    @Test
    fun aNotificationWithNoBodyAtAllIsStillAnEvent() {
        val event = classifyEvent(
            AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED,
            "com.example", null, null, null, 3,
        )!!
        assertEquals(EVENT_KIND_NOTIFICATION, event.kind)
        assertNull(event.text)
    }

    @Test
    fun longTextIsCutAndSaysSo() {
        val long = "x".repeat(EVENT_MAX_TEXT + 50)
        val event = classifyEvent(
            AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED,
            "com.example", null, long, null, 1,
        )!!
        assertEquals(EVENT_MAX_TEXT, event.text!!.length)
        assertTrue(event.truncated)
    }

    @Test
    fun shortTextIsNotMarkedTruncated() {
        val event = classifyEvent(
            AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED,
            "com.example", null, "hello", null, 1,
        )!!
        assertFalse(event.truncated)
    }

    @Test
    fun blankStringsAreAbsentNotEmpty() {
        val event = classifyEvent(
            AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED,
            null, "  ", " ", "", 1,
        )!!
        assertNull(event.title)
        assertNull(event.text)
        assertEquals("", event.packageName)
    }

    // ── The wire form ─────────────────────────────────────────────────

    @Test
    fun theJsonCarriesEveryField() {
        // Compared as a string on purpose: `org.json` is a stub in plain JVM
        // tests (every call throws "not mocked"), and the exact bytes are the
        // contract the Rust side parses anyway.
        val json = DeviceEvent(
            seq = 4, kind = EVENT_KIND_NOTIFICATION, packageName = "com.example",
            atUptimeMs = 99, title = "Alice", text = "Are you free?", truncated = true,
        ).toJson()
        assertEquals(
            "{\"seq\":4,\"kind\":\"notification\",\"package\":\"com.example\"," +
                "\"atUptimeMs\":99,\"title\":\"Alice\",\"text\":\"Are you free?\"," +
                "\"truncated\":true}",
            json,
        )
    }

    @Test
    fun absentFieldsAreAbsentRatherThanNull() {
        val json = DeviceEvent(1, EVENT_KIND_CONTENT, "com.example", 2).toJson()
        assertEquals(
            "{\"seq\":1,\"kind\":\"content\",\"package\":\"com.example\",\"atUptimeMs\":2}",
            json,
        )
    }

    @Test
    fun awkwardTextIsEscapedSoTheJsonStaysParseable() {
        // Quotes, a backslash, a newline, a tab and a raw control character —
        // a notification body can carry all of them.
        val nasty = "line one\nline \"two\" \\ \u0001 tab\there 中文"
        val json = DeviceEvent(1, EVENT_KIND_CONTENT, "com.example", 2, text = nasty).toJson()
        assertTrue(
            json,
            json.contains(
                "\"text\":\"line one\\nline \\\"two\\\" \\\\ \\u0001 tab\\there 中文\""
            ),
        )
    }

    // ── The ring ──────────────────────────────────────────────────────

    private fun fact(seq: Long = 0, kind: String = EVENT_KIND_CONTENT, at: Long = seq) =
        DeviceEvent(seq, kind, "com.example", at)

    @Test
    fun eventsAreNumberedInArrivalOrder() {
        val ring = EventRing(4)
        assertEquals(1L, ring.add(fact()).seq)
        assertEquals(2L, ring.add(fact()).seq)
        assertEquals(2L, ring.cursor())
        assertEquals(1L, ring.oldest())
    }

    @Test
    fun theOldestGoesWhenTheRingIsFull() {
        val ring = EventRing(2)
        ring.add(fact(at = 1))
        ring.add(fact(at = 2))
        ring.add(fact(at = 3))
        val held = ring.history()
        assertEquals(2, held.size)
        assertEquals(2L, held.first().seq)
        assertEquals(3L, held.last().seq)
        assertEquals(1L, ring.dropped())
        assertEquals(2L, ring.oldest())
    }

    @Test
    fun sinceIsExclusiveSoACursorNeverRepeatsAnEvent() {
        val ring = EventRing(8)
        repeat(3) { ring.add(fact()) }
        val after = ring.history(since = 2)
        assertEquals(1, after.size)
        assertEquals(3L, after.first().seq)
        // A cursor past the end is legal: nothing has happened since.
        assertTrue(ring.history(since = 3).isEmpty())
    }

    @Test
    fun aKindFilterNarrowsWithoutChangingTheCursor() {
        val ring = EventRing(8)
        ring.add(fact(kind = EVENT_KIND_WINDOW))
        ring.add(fact(kind = EVENT_KIND_NOTIFICATION))
        ring.add(fact(kind = EVENT_KIND_CONTENT))
        val notes = ring.history(kind = EVENT_KIND_NOTIFICATION)
        assertEquals(1, notes.size)
        assertEquals(EVENT_KIND_NOTIFICATION, notes.first().kind)
        assertEquals(3L, ring.cursor())
    }

    @Test
    fun theLimitIsTakenFromTheOldestSide() {
        // `since` is how you move forward; `limit` only bounds one answer, and
        // dropping the newest would lose the event that just woke you.
        val ring = EventRing(8)
        repeat(5) { ring.add(fact()) }
        val page = ring.history(limit = 2)
        assertEquals(listOf(1L, 2L), page.map { it.seq })
    }

    @Test
    fun anEmptyRingAnswersHonestly() {
        val ring = EventRing(4)
        assertTrue(ring.history().isEmpty())
        assertEquals(0L, ring.cursor())
        assertNull(ring.oldest())
        assertEquals(0L, ring.dropped())
    }

    @Test
    fun aNegativeLimitIsNoItemsNotAnException() {
        val ring = EventRing(4)
        ring.add(fact())
        assertTrue(ring.history(limit = -1).isEmpty())
    }
}
