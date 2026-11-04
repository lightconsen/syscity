//! Device events: facts about what changed, delivered rather than polled.
//!
//! While Syscity's accessibility service is on, the platform tells it when a
//! window opens, when content on screen changes, and when a notification is
//! posted. Recording *that* — the kind, the app, the time, and the text the
//! notification carried — is what lets a waiting tool be woken by the change
//! instead of re-reading the tree every 250 ms. It is deliberately not a copy
//! of the screen: a tree kept between reads goes stale, and a stale tree is
//! worse than no tree. The screen is still read fresh, afterwards, by whoever
//! was woken.
//!
//! Two paths carry the facts, and the tool says which one answered:
//!
//! - **push** — the phone's Kotlin side calls into Rust through JNI as each
//!   event happens (`NativeEvents.onDeviceEvent` on Android). No waiter, no
//!   Binder thread held; the event wakes whoever is listening.
//! - **buffer** — the service keeps a bounded ring of recent facts, read on
//!   demand through the `accessibilityEvents` bridge command. This is the only
//!   path when the native hand-off is missing (library not packaged, another
//!   ABI) and the path that fills in history from before the wait began.
//!
//! Which one exists is a property of the device, not of this code: the bridge
//! reports it, and the types here parse that answer. Everything is `Clone +
//! Send + 'static` because events travel over a `tokio::sync::broadcast`
//! channel — see [`crate::device::DeviceBridge::events`].

use serde_json::Value;

/// The native hand-off is working: new events wake a waiter.
pub const EVENT_SOURCE_PUSH: &str = "push";
/// Events are only reaching the ring (read on demand).
pub const EVENT_SOURCE_BUFFER: &str = "buffer";
/// There is no event stream to read at all.
pub const EVENT_SOURCE_UNAVAILABLE: &str = "unavailable";

/// What kind of fact an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// A window opened, closed, or became the focused one.
    Window,
    /// The content inside the focused window changed.
    Content,
    /// A notification was posted.
    Notification,
}

impl EventKind {
    /// Every kind, for schemas and error messages.
    pub const ALL: [Self; 3] = [Self::Window, Self::Content, Self::Notification];

    /// The word used in results and accepted as a `kind` filter.
    pub fn label(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Content => "content",
            Self::Notification => "notification",
        }
    }

    /// Parse one kind name, exactly.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.label() == value.trim().to_ascii_lowercase())
    }

    /// Parse a `kind` argument: `any` (or an empty value) is no filter at all.
    pub fn parse_filter(value: &str) -> Result<Option<Self>, String> {
        let clean = value.trim().to_ascii_lowercase();
        if clean.is_empty() || clean == "any" || clean == "all" {
            return Ok(None);
        }
        Self::parse(&clean).map(Some).ok_or_else(|| {
            format!(
                "kind must be 'any', 'window', 'content' or 'notification', not {:?}",
                value.trim()
            )
        })
    }

    /// The `AccessibilityServiceInfo` event-type bit for this kind.
    ///
    /// The values are the platform's own (`TYPE_WINDOW_STATE_CHANGED` = 0x20,
    /// `TYPE_WINDOW_CONTENT_CHANGED` = 0x800,
    /// `TYPE_NOTIFICATION_STATE_CHANGED` = 0x10). `accessibilityEventTypes` is
    /// read once, when the system binds the service, so a build that starts
    /// handling a new kind only receives it after the user toggles the service.
    /// Comparing the bound mask against this is how that state becomes visible.
    pub fn subscription_bit(self) -> u32 {
        match self {
            Self::Window => 0x20,
            Self::Content => 0x800,
            Self::Notification => 0x10,
        }
    }
}

/// One fact about the device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceEvent {
    /// Position in the phone's ring, ascending. A reader passes the last one it
    /// saw back as `since`.
    pub seq: u64,
    pub kind: EventKind,
    /// The app the event came from. Empty when the platform withheld it.
    pub package: String,
    /// The platform's monotonic clock — the only one that keeps ordering
    /// across a sleep.
    pub at_uptime_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The phone cut this event's text at its own limit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl DeviceEvent {
    /// Parse the payload Kotlin sends (`DeviceEvent.toJson` in `DeviceEventLog.kt`).
    ///
    /// An event whose `kind` this build does not know is dropped: reporting a
    /// `window` that is really something else would be worse than reporting
    /// nothing.
    pub fn from_payload(value: &Value) -> Option<Self> {
        let kind = EventKind::parse(value.get("kind")?.as_str()?)?;
        Some(Self {
            seq: value.get("seq").and_then(Value::as_u64).unwrap_or(0),
            kind,
            package: value
                .get("package")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            at_uptime_ms: value.get("atUptimeMs").and_then(Value::as_u64).unwrap_or(0),
            title: string_field(value, "title"),
            text: string_field(value, "text"),
            truncated: value
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// [`from_payload`](Self::from_payload) from the JSON text the JNI callback
    /// carries.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        Self::from_payload(&value)
    }

    /// Whether this event satisfies a `kind` filter (`None` = any kind).
    pub fn matches(&self, kind: Option<EventKind>) -> bool {
        kind.is_none_or(|wanted| wanted == self.kind)
    }

    /// One line, for a tool's text answer.
    pub fn describe(&self) -> String {
        let app = if self.package.is_empty() {
            "an unknown app".to_string()
        } else {
            self.package.clone()
        };
        let mut line = format!("{} from {app}", self.kind.label());
        match (self.title.as_deref(), self.text.as_deref()) {
            (Some(title), Some(text)) if title != text => {
                line.push_str(&format!(": \"{title}\" — \"{text}\""));
            }
            (Some(value), _) | (None, Some(value)) => line.push_str(&format!(": \"{value}\"")),
            (None, None) => {}
        }
        if self.truncated {
            line.push_str(" (text cut)");
        }
        line
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The accessibility service's own state, from `accessibilityStatus`.
///
/// This is the answer to "can events happen at all", as opposed to the event
/// page's "what happened". The field that matters most is
/// `subscribed_event_types`: the platform reads the event subscription once,
/// when it binds the service, so after an app update the old subscription
/// stays in force until the user toggles the service — and every event of a
/// newly-added kind is silently never delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct A11yStatus {
    pub enabled: Option<bool>,
    pub connected: Option<bool>,
    /// The mask the running binding delivers.
    pub subscribed_event_types: Option<u32>,
    /// The mask this build asks for (`handledEventTypes`), when reported.
    pub handled_event_types: Option<Vec<u32>>,
}

impl A11yStatus {
    /// Parse the `accessibilityStatus` answer. Never fails: every field is
    /// optional, and an unparsable answer is "unknown", not an error.
    pub fn parse(value: &Value) -> Self {
        Self {
            enabled: value.get("enabled").and_then(Value::as_bool),
            connected: value.get("connected").and_then(Value::as_bool),
            subscribed_event_types: value
                .get("subscribedEventTypes")
                .and_then(Value::as_u64)
                .and_then(|mask| u32::try_from(mask).ok()),
            handled_event_types: value
                .get("handledEventTypes")
                .and_then(Value::as_array)
                .map(|bits| {
                    bits.iter()
                        .filter_map(Value::as_u64)
                        .filter_map(|bit| u32::try_from(bit).ok())
                        .collect()
                })
                .filter(|bits: &Vec<u32>| !bits.is_empty()),
        }
    }

    /// Why the subscription is narrower than this build expects, if it is.
    ///
    /// `None` means either "everything is subscribed" or "the phone did not
    /// say" — the two are deliberately not conflated into a false alarm.
    pub fn subscription_gap(&self) -> Option<String> {
        let bound = self.subscribed_event_types?;
        let handled = self.handled_event_types.as_ref()?;
        let missing: Vec<&str> = EventKind::ALL
            .iter()
            .filter(|kind| handled.contains(&kind.subscription_bit()))
            .filter(|kind| bound & kind.subscription_bit() == 0)
            .map(|kind| kind.label())
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "The service is bound without the {} event type(s), so these will never arrive. The \
             system reads the subscription once per enable; toggle Syscity's accessibility \
             service off and on in Settings › Accessibility to pick it up.",
            missing.join(", ")
        ))
    }
}

/// How new events reach Rust on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventStream {
    /// The native callback is working.
    Push,
    /// The callback failed; only the on-demand ring answers.
    Broken,
    /// The phone did not say.
    #[default]
    Unknown,
}

impl EventStream {
    fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or("").trim() {
            "push" => Self::Push,
            "broken" => Self::Broken,
            _ => Self::Unknown,
        }
    }
}

/// One reading of the phone's event ring, as the bridge answers it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventPage {
    pub events: Vec<DeviceEvent>,
    /// The highest sequence number the phone has recorded — the cursor to pass
    /// back as `since` once these events have been seen.
    pub cursor: u64,
    /// How many events have rolled out of the ring, so a `since` that is too
    /// old can be explained rather than silently met with a short page.
    pub dropped: u64,
    /// The oldest sequence number still held.
    pub oldest: Option<u64>,
    pub stream: EventStream,
    /// Whether the accessibility service is switched on at all.
    pub enabled: Option<bool>,
    /// Whether the service is bound and holding a window.
    pub connected: Option<bool>,
    /// The event-type mask the service is *actually* bound with. Differs from
    /// what this build handles when the app was updated without the user
    /// toggling the service off and on.
    pub subscribed_event_types: Option<u32>,
}

impl EventPage {
    /// Parse the `accessibilityEvents` answer.
    ///
    /// A missing `events` array is a bad payload rather than an empty page: an
    /// older app build that does not have the command answers something else
    /// entirely, and silently reporting "nothing happened" would be wrong.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let events = value
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| "the phone answered without an event list".to_string())?;
        Ok(Self {
            events: events
                .iter()
                .filter_map(DeviceEvent::from_payload)
                .collect(),
            cursor: value.get("cursor").and_then(Value::as_u64).unwrap_or(0),
            dropped: value.get("dropped").and_then(Value::as_u64).unwrap_or(0),
            oldest: value.get("oldest").and_then(Value::as_u64),
            stream: EventStream::parse(value.get("stream").and_then(Value::as_str)),
            enabled: value.get("enabled").and_then(Value::as_bool),
            connected: value.get("connected").and_then(Value::as_bool),
            subscribed_event_types: value
                .get("subscribedEventTypes")
                .and_then(Value::as_u64)
                .and_then(|mask| u32::try_from(mask).ok()),
        })
    }

    /// The source to report: `push`, `buffer`, or `unavailable`.
    ///
    /// "Unavailable" is a statement about the device, not about this answer:
    /// the service is off (so nothing will ever arrive), or the phone does not
    /// run Syscity's own app and has no service at all.
    pub fn source(&self) -> &'static str {
        match (self.enabled, self.stream) {
            (Some(false), _) => EVENT_SOURCE_UNAVAILABLE,
            (_, EventStream::Push) => EVENT_SOURCE_PUSH,
            (_, EventStream::Broken) => EVENT_SOURCE_BUFFER,
            (_, EventStream::Unknown) => EVENT_SOURCE_BUFFER,
        }
    }

    /// Whether the bound subscription includes this kind, when the phone said.
    ///
    /// `None` means "no answer" (an older build), which callers treat as
    /// unknown rather than as "yes".
    pub fn subscription_covers(&self, kind: EventKind) -> Option<bool> {
        self.subscribed_event_types
            .map(|mask| mask & kind.subscription_bit() != 0)
    }

    /// The most useful sentence about why no event will arrive, if one won't.
    ///
    /// Written for a model: it says what to do, not just what is wrong.
    pub fn unavailable_reason(&self) -> Option<String> {
        if self.enabled == Some(false) {
            return Some(
                "Syscity's accessibility service is off, so the phone reports no events. Turn it \
                 on in Settings › Accessibility › Syscity; until then wait on the screen itself \
                 (condition 'appear', 'disappear' or 'stable')."
                    .to_string(),
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(kind: &str) -> Value {
        serde_json::json!({
            "seq": 7,
            "kind": kind,
            "package": " com.example.chat ",
            "atUptimeMs": 1_234,
            "title": "Alice",
            "text": "Are you free?",
        })
    }

    #[test]
    fn every_kind_parses_from_its_label() {
        for kind in EventKind::ALL {
            assert_eq!(EventKind::parse(kind.label()), Some(kind));
        }
        assert_eq!(EventKind::parse(" WINDOW "), Some(EventKind::Window));
        assert_eq!(EventKind::parse("toast"), None);
    }

    #[test]
    fn any_is_the_absence_of_a_filter() {
        assert_eq!(EventKind::parse_filter("any"), Ok(None));
        assert_eq!(EventKind::parse_filter(""), Ok(None));
        assert_eq!(EventKind::parse_filter("Notification"), Ok(Some(EventKind::Notification)));
        let refused = EventKind::parse_filter("toast").expect_err("unknown kind");
        assert!(refused.contains("notification"), "{refused}");
    }

    #[test]
    fn a_payload_becomes_an_event_with_its_fields_trimmed() {
        let event = DeviceEvent::from_payload(&payload("notification")).expect("an event");
        assert_eq!(event.seq, 7);
        assert_eq!(event.kind, EventKind::Notification);
        assert_eq!(event.package, "com.example.chat");
        assert_eq!(event.at_uptime_ms, 1_234);
        assert_eq!(event.title.as_deref(), Some("Alice"));
        assert_eq!(event.text.as_deref(), Some("Are you free?"));
        assert!(!event.truncated);
    }

    #[test]
    fn a_kind_this_build_does_not_know_is_dropped() {
        // Better to drop it: describing a `toast` as a window change would be
        // a false fact.
        assert_eq!(DeviceEvent::from_payload(&payload("toast")), None);
        assert_eq!(DeviceEvent::from_payload(&serde_json::json!({})), None);
        assert_eq!(DeviceEvent::from_payload(&serde_json::json!({ "kind": 3 })), None);
    }

    #[test]
    fn missing_optional_fields_do_not_lose_the_event() {
        // Some ROMs withhold the Notification and send no text at all; the
        // event still says "something happened in this app".
        let event = DeviceEvent::from_payload(&serde_json::json!({
            "kind": "window", "package": "com.example",
        }))
        .expect("an event");
        assert_eq!(event.seq, 0);
        assert_eq!(event.at_uptime_ms, 0);
        assert_eq!(event.title, None);
        assert_eq!(event.text, None);
    }

    #[test]
    fn a_blank_title_is_absence_not_an_empty_string() {
        let event = DeviceEvent::from_payload(&serde_json::json!({
            "kind": "notification", "package": "x", "title": "  ", "text": "",
        }))
        .expect("an event");
        assert_eq!(event.title, None);
        assert_eq!(event.text, None);
    }

    #[test]
    fn json_text_round_trips_through_the_callback_parser() {
        let original = DeviceEvent {
            seq: 3,
            kind: EventKind::Notification,
            package: "com.example".to_string(),
            at_uptime_ms: 9,
            title: Some("Alice".to_string()),
            text: Some("line\nquote \" backslash \\ end".to_string()),
            truncated: true,
        };
        // The Kotlin side writes this shape; re-encoding here proves the
        // parser accepts what the phone's hand-rolled emitter produces.
        let wire = serde_json::json!({
            "seq": original.seq,
            "kind": original.kind.label(),
            "package": original.package,
            "atUptimeMs": original.at_uptime_ms,
            "title": original.title,
            "text": original.text,
            "truncated": true,
        })
        .to_string();
        assert_eq!(DeviceEvent::from_json(&wire), Some(original));
        assert_eq!(DeviceEvent::from_json("not json"), None);
    }

    #[test]
    fn a_kind_filter_is_an_exact_match_or_any() {
        let event = DeviceEvent::from_payload(&payload("notification")).expect("an event");
        assert!(event.matches(None));
        assert!(event.matches(Some(EventKind::Notification)));
        assert!(!event.matches(Some(EventKind::Content)));
    }

    #[test]
    fn describe_names_the_kind_the_app_and_the_text() {
        let event = DeviceEvent::from_payload(&payload("notification")).expect("an event");
        let line = event.describe();
        assert!(line.starts_with("notification from com.example.chat"), "{line}");
        assert!(line.contains("Alice"), "{line}");
        assert!(line.contains("Are you free?"), "{line}");
    }

    #[test]
    fn describe_does_not_repeat_a_title_that_is_the_whole_text() {
        let event = DeviceEvent {
            seq: 1,
            kind: EventKind::Notification,
            package: "com.example".to_string(),
            at_uptime_ms: 0,
            title: Some("Charging".to_string()),
            text: Some("Charging".to_string()),
            truncated: false,
        };
        assert_eq!(event.describe(), "notification from com.example: \"Charging\"");
    }

    #[test]
    fn a_page_reads_the_cursor_and_the_subscription() {
        let page = EventPage::parse(&serde_json::json!({
            "events": [
                { "seq": 1, "kind": "window", "package": "a", "atUptimeMs": 1 },
                { "seq": 2, "kind": "notification", "package": "b", "atUptimeMs": 2 },
            ],
            "cursor": 2,
            "dropped": 0,
            "oldest": 1,
            "stream": "push",
            "enabled": true,
            "connected": true,
            "subscribedEventTypes": 0x830,
        }))
        .expect("a page");
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.cursor, 2);
        assert_eq!(page.oldest, Some(1));
        assert_eq!(page.stream, EventStream::Push);
        assert_eq!(page.source(), EVENT_SOURCE_PUSH);
        for kind in EventKind::ALL {
            assert_eq!(page.subscription_covers(kind), Some(true), "{kind:?}");
        }
    }

    #[test]
    fn an_old_subscription_is_visible_as_a_missing_kind() {
        // A build that predates notification support is bound with only the
        // two window types. That is silently empty rather than broken, so it
        // has to be reported.
        let page = EventPage::parse(&serde_json::json!({
            "events": [], "cursor": 4, "subscribedEventTypes": 0x820,
            "enabled": true, "connected": true, "stream": "push",
        }))
        .expect("a page");
        assert_eq!(page.subscription_covers(EventKind::Notification), Some(false));
        assert_eq!(page.subscription_covers(EventKind::Window), Some(true));
    }

    #[test]
    fn a_service_that_is_off_is_unavailable_whichever_stream_says_otherwise() {
        let page = EventPage::parse(&serde_json::json!({
            "events": [], "enabled": false, "connected": false, "stream": "push",
        }))
        .expect("a page");
        assert_eq!(page.source(), EVENT_SOURCE_UNAVAILABLE);
        assert!(page
            .unavailable_reason()
            .expect("a reason")
            .contains("Settings"));
    }

    #[test]
    fn a_broken_native_hand_off_still_has_a_buffer() {
        let page = EventPage::parse(&serde_json::json!({
            "events": [], "enabled": true, "connected": true, "stream": "broken",
        }))
        .expect("a page");
        assert_eq!(page.source(), EVENT_SOURCE_BUFFER);
        assert_eq!(page.unavailable_reason(), None);
    }

    #[test]
    fn an_answer_without_an_event_list_is_a_bad_payload() {
        // An older app build without the command answers something else; that
        // must not read as "nothing happened".
        assert!(EventPage::parse(&serde_json::json!({ "ok": true })).is_err());
    }

    #[test]
    fn a_binding_without_notifications_names_the_gap() {
        let status = A11yStatus::parse(&serde_json::json!({
            "enabled": true,
            "connected": true,
            "subscribedEventTypes": 0x820,
            "handledEventTypes": [0x10, 0x20, 0x800],
        }));
        let gap = status.subscription_gap().expect("a gap");
        assert!(gap.contains("notification"), "{gap}");
        assert!(gap.contains("toggle"), "{gap}");
    }

    #[test]
    fn a_complete_subscription_has_no_gap_to_report() {
        let status = A11yStatus::parse(&serde_json::json!({
            "subscribedEventTypes": 0x830,
            "handledEventTypes": [0x10, 0x20, 0x800],
        }));
        assert_eq!(status.subscription_gap(), None);
    }

    #[test]
    fn an_older_phone_that_does_not_report_the_mask_is_not_called_broken() {
        let status = A11yStatus::parse(&serde_json::json!({ "enabled": true }));
        assert_eq!(status.subscription_gap(), None);
        assert_eq!(status.subscribed_event_types, None);
    }

    #[test]
    fn the_status_never_fails_to_parse() {
        let status = A11yStatus::parse(&serde_json::json!("nonsense"));
        assert_eq!(status, A11yStatus::default());
    }

    #[test]
    fn the_subscription_bits_are_the_platforms_own() {
        assert_eq!(EventKind::Notification.subscription_bit(), 0x10);
        assert_eq!(EventKind::Window.subscription_bit(), 0x20);
        assert_eq!(EventKind::Content.subscription_bit(), 0x800);
    }
}
