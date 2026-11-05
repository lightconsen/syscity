//! What the phone says has happened: windows, content changes, notifications.

use super::*;

/// Wait for the phone to report a change, instead of polling the screen.
///
/// The push channel is the point of this tool: while the accessibility service
/// is on, the phone calls into Rust as each window, content change or
/// notification happens, so a wait is a `select!` between an incoming event and
/// the deadline — no Binder thread held, no tree re-read every 250 ms. When the
/// service is off, or the host is a desktop driving a phone over ADB, there is
/// no push channel; this falls back to reading the service's ring
/// (`accessibilityEvents`) on the same 250 ms cadence and says so in `source`.
///
/// What an event is *not*: a picture of the screen. It says "something changed
/// in this app at this time", and the screen still has to be read afterwards.
/// `condition: event` is therefore for "wait until something happens", and
/// `appear`/`disappear` remain the tools for "wait until this specific thing".
#[derive(Debug)]
pub struct AdbEventTool {
    device: Option<String>,
}

impl Default for AdbEventTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbEventTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How many events one call returns by default.
const EVENT_DEFAULT_LIMIT: usize = 20;
/// The most one call returns.
const EVENT_MAX_LIMIT: usize = 200;

/// Read the phone's event ring.
///
/// `since` is the cursor from a previous answer; `None` starts from the oldest
/// event still buffered. The bridge is looked up process-wide for the same
/// reason the tree read is: events belong to the device, and the tool set that
/// asks may be several layers below the one holding a bridge reference.
pub(crate) async fn read_events(
    since: Option<u64>,
    kind: Option<crate::device::EventKind>,
    limit: usize,
) -> Result<crate::device::EventPage, String> {
    let bridge = crate::device::bridge().cloned().ok_or_else(|| {
        "The device event stream only exists inside the Syscity app on the phone itself — this \
         host has no accessibility service. Wait on the screen instead (android_wait with \
         condition 'appear', 'disappear' or 'stable')."
            .to_string()
    })?;
    read_events_from(bridge, since, kind, limit).await
}

/// The payload for one `accessibilityEvents` call.
///
/// `since` and `kind` are omitted rather than sent as null: the Kotlin side
/// treats an absent field as "no filter", and a null is a value it has to
/// special-case.
fn event_query(
    since: Option<u64>,
    kind: Option<crate::device::EventKind>,
    limit: usize,
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "limit": limit });
    if let Some(since) = since {
        payload["since"] = serde_json::json!(since);
    }
    if let Some(kind) = kind {
        payload["kind"] = serde_json::json!(kind.label());
    }
    payload
}

/// [`read_events`] against a given bridge, so the call can be asserted without
/// installing the process-wide one.
pub(crate) async fn read_events_from(
    bridge: crate::device::DeviceBridgeRef,
    since: Option<u64>,
    kind: Option<crate::device::EventKind>,
    limit: usize,
) -> Result<crate::device::EventPage, String> {
    let answer = bridge
        .call(crate::device::CMD_A11Y_EVENTS, event_query(since, kind, limit))
        .await
        .map_err(|e| format!("The phone could not be asked for events ({e})"))?;
    crate::device::EventPage::parse(&answer).map_err(|detail| {
        format!(
            "This Syscity build on the phone does not report device events ({detail}); update \
             the app on the phone, or wait on the screen instead."
        )
    })
}

/// The service's own status, as `accessibilityStatus` reports it.
///
/// Separate from the event page because the two answer different questions:
/// the page says what happened, this says whether the stream can happen at
/// all. `handled_event_types` is what *this build* asks for; the page's
/// `subscribed_event_types` is what the running binding actually delivers, and
/// the difference between them is an app update that has not been followed by
/// a service toggle.
pub(crate) async fn read_a11y_status() -> Option<crate::device::A11yStatus> {
    let bridge = crate::device::bridge().cloned()?;
    let answer = bridge
        .call(crate::device::CMD_A11Y_STATUS, serde_json::json!({}))
        .await
        .ok()?;
    Some(crate::device::A11yStatus::parse(&answer))
}

/// What one event wait observed.
pub(crate) struct EventWait {
    pub(super) events: Vec<crate::device::DeviceEvent>,
    pub(super) cursor: u64,
    pub(super) source: &'static str,
    pub(super) waited_ms: u64,
}

/// Wait for the next event of `kind`, or the deadline.
///
/// Push first: subscribing *before* the baseline cursor is read is what keeps
/// the gap closed — an event that lands between the two is in the channel, and
/// the sequence filter discards only history that predates the wait.
pub(crate) async fn wait_for_event(
    since: u64,
    kind: Option<crate::device::EventKind>,
    budget: Duration,
) -> Result<EventWait, String> {
    let bridge = crate::device::bridge().cloned().ok_or_else(|| {
        "The device event stream only exists inside the Syscity app on the phone itself — this \
         host has no accessibility service. Wait on the screen instead (android_wait with \
         condition 'appear', 'disappear' or 'stable')."
            .to_string()
    })?;
    wait_for_event_on(bridge, since, kind, budget).await
}

/// [`wait_for_event`] against a given bridge.
///
/// Split out because the channel and the ring come from the same bridge, and a
/// test has to supply both — the process-wide one can be installed only once.
pub(crate) async fn wait_for_event_on(
    bridge: crate::device::DeviceBridgeRef,
    since: u64,
    kind: Option<crate::device::EventKind>,
    budget: Duration,
) -> Result<EventWait, String> {
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + budget;

    if let Some(mut receiver) = bridge.events() {
        // Subscribe first, then read the ring: an event posted between the two
        // is in the channel *and* in the ring, so nothing falls in the gap.
        // (The reverse order would miss exactly the event that triggered the
        // wait.)
        let page = read_events_from(bridge.clone(), Some(since), kind, EVENT_MAX_LIMIT).await?;
        if !page.events.is_empty() {
            let waited_ms = started.elapsed().as_millis() as u64;
            return Ok(EventWait {
                cursor: page.cursor.max(since),
                events: page.events,
                source: crate::device::EVENT_SOURCE_BUFFER,
                waited_ms,
            });
        }
        loop {
            tokio::select! {
                incoming = receiver.recv() => match incoming {
                    Ok(event) => {
                        if event.seq > since && event.matches(kind) {
                            let waited_ms = started.elapsed().as_millis() as u64;
                            return Ok(EventWait {
                                cursor: event.seq,
                                events: vec![event],
                                source: crate::device::EVENT_SOURCE_PUSH,
                                waited_ms,
                            });
                        }
                    }
                    // The subscriber fell behind. The events are not lost —
                    // the ring has them — so this is a reason to read the
                    // ring, not a failure.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let page = read_events_from(
                            bridge.clone(),
                            Some(since),
                            kind,
                            EVENT_MAX_LIMIT,
                        )
                        .await?;
                        let waited_ms = started.elapsed().as_millis() as u64;
                        return Ok(EventWait {
                            cursor: page.cursor.max(since),
                            events: page.events,
                            source: crate::device::EVENT_SOURCE_BUFFER,
                            waited_ms,
                        });
                    }
                    // The phone side went away (the channel was dropped).
                    // Fall through to the ring.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        let waited_ms = started.elapsed().as_millis() as u64;
        return Ok(EventWait {
            events: Vec::new(),
            cursor: since,
            source: crate::device::EVENT_SOURCE_PUSH,
            waited_ms,
        });
    }

    // No push channel: poll the ring. Same cadence as the screen poll, because
    // the reason for it is the same — a person could not see a change faster.
    let poll = Duration::from_millis(WAIT_POLL_MS);
    loop {
        let page = read_events_from(bridge.clone(), Some(since), kind, EVENT_MAX_LIMIT).await?;
        if !page.events.is_empty() {
            let waited_ms = started.elapsed().as_millis() as u64;
            let source = page.source();
            return Ok(EventWait {
                events: page.events,
                cursor: page.cursor,
                source,
                waited_ms,
            });
        }
        if started.elapsed() >= budget {
            let waited_ms = started.elapsed().as_millis() as u64;
            let source = page.source();
            return Ok(EventWait {
                events: Vec::new(),
                cursor: page.cursor.max(since),
                source,
                waited_ms,
            });
        }
        tokio::time::sleep(poll).await;
    }
}

#[async_trait]
impl Tool for AdbEventTool {
    fn name(&self) -> &str {
        "android_event"
    }

    fn description(&self) -> &str {
        "Read what the phone says has happened recently: windows opening, content changing, \
         notifications arriving. Each event is a fact — kind, app, time, and the notification's \
         text — not a picture of the screen, so read the screen separately when you need to act. \
         Pass the returned `cursor` back as `since` next time to get only what is new. `source` \
         says whether events are being pushed live or only read from the buffer."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read recent device events",
            serde_json::json!({
                "since": {
                    "type": "integer",
                    "description": "The `cursor` from a previous call: return only events after it."
                },
                "kind": {
                    "type": "string",
                    "enum": ["any", "window", "content", "notification"],
                    "description": "Only this kind of event (default any)."
                },
                "limit": {
                    "type": "integer",
                    "description": "How many events to return (default 20, max 200)."
                },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // `device` is validated for the same reason every other tool does it:
        // an empty serial means "ask the wrong phone", and the ring is read
        // from whichever bridge this process installed.
        if let Err(refusal) = resolve_device(&args, self.device.as_deref()) {
            return Ok(ToolExecutionResult::error(refusal));
        }
        let kind = match crate::device::EventKind::parse_filter(
            args.get("kind").and_then(Value::as_str).unwrap_or("any"),
        ) {
            Ok(kind) => kind,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let since = args.get("since").and_then(Value::as_u64);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(EVENT_DEFAULT_LIMIT)
            .clamp(1, EVENT_MAX_LIMIT);

        let page = match read_events(since, kind, limit).await {
            Ok(page) => page,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        if let Some(reason) = page.unavailable_reason() {
            // Say *which* half is missing. "The service is off" and "the
            // service is on but this build's subscription predates the event
            // types" are the same empty answer with different fixes, and the
            // status command is the only place that distinction exists.
            let status = read_a11y_status().await.unwrap_or_default();
            let mut text = reason;
            if let Some(detail) = status.subscription_gap() {
                text.push('\n');
                text.push_str(&detail);
            }
            return Ok(ToolExecutionResult::error(text).with_data(serde_json::json!({
                "code": "event_stream_unavailable",
                "events": [],
                "source": crate::device::EVENT_SOURCE_UNAVAILABLE,
                "enabled": status.enabled,
                "connected": status.connected,
                "subscribed_event_types": status.subscribed_event_types,
                "handled_event_types": status.handled_event_types,
            })));
        }

        // A `since` older than the buffer can answer is a real answer, but a
        // misleading one: say what was lost instead of showing a gap.
        let missed = since.filter(|since| page.dropped > 0 && *since < page.oldest.unwrap_or(0));
        let mut text = if page.events.is_empty() {
            "No new events.".to_string()
        } else {
            format!("{} event(s):", page.events.len())
        };
        for event in &page.events {
            text.push_str("\n- ");
            text.push_str(&event.describe());
        }
        if let Some(since) = missed {
            text.push_str(&format!(
                "\n\nEvents after cursor {since} have already rolled out of the phone's buffer, \
                 so this list is not the complete history. Read the screen for the current state."
            ));
        }
        if let Some(kind) = kind {
            if page.subscription_covers(kind) == Some(false) {
                text.push_str(&format!(
                    "\n\nThe service is bound without the `{}` event type: the app on the phone \
                     was updated with a new subscription but has not been re-enabled since. \
                     Toggle Syscity's accessibility service off and on in Settings to fix it.",
                    kind.label()
                ));
            }
        }
        text.push_str(&format!("\ncursor: {} (pass it as `since`)", page.cursor));

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "events": page.events,
            "cursor": page.cursor,
            "dropped": page.dropped,
            "oldest": page.oldest,
            "source": page.source(),
            "enabled": page.enabled,
            "connected": page.connected,
            "subscribed_event_types": page.subscribed_event_types,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::super::run::{EventWatch, WatchVerdict};
    use super::super::testkit::*;
    use super::*;

    #[tokio::test]
    async fn the_ring_is_read_through_the_bridge_command() {
        let bridge =
            std::sync::Arc::new(crate::device::tests::MockDeviceBridge::new(serde_json::json!({
                "events": [],
                "cursor": 4,
                "stream": "push",
                "enabled": true,
                "connected": true,
            })));
        let page = super::read_events_from(bridge.clone(), None, None, 10)
            .await
            .expect("a page");
        assert!(page.events.is_empty());
        assert_eq!(page.cursor, 4);
        assert_eq!(page.source(), crate::device::EVENT_SOURCE_PUSH);
        let calls = bridge.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, crate::device::CMD_A11Y_EVENTS);
        assert_eq!(calls[0].1["limit"], 10);
    }
    #[tokio::test]
    async fn a_window_event_from_another_app_is_what_a_run_calls_a_switch() {
        let sender = tokio::sync::broadcast::channel(8).0;
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let poster = sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = poster.send(window_event(1, "com.example.other"));
        });
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(1_000))
            .await;
        assert!(matches!(verdict, WatchVerdict::Moved(ref app) if app == "com.example.other"));
        assert!(watch.available());
    }
    #[tokio::test]
    async fn a_window_event_from_the_same_app_is_not_a_switch() {
        // A new activity inside the run's own app is still the run's screen;
        // the next step's read decides whether its targets survived.
        let sender = tokio::sync::broadcast::channel(8).0;
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let poster = sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = poster.send(window_event(1, "com.example.chat"));
        });
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(120))
            .await;
        assert!(matches!(verdict, WatchVerdict::Still));
    }
    #[tokio::test]
    async fn history_from_before_the_run_does_not_count_as_a_switch() {
        // The channel is process-wide; an event from the previous run must not
        // abort this one before its first step has even run.
        let sender = tokio::sync::broadcast::channel(8).0;
        let _ = sender.send(window_event(1, "com.example.other"));
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(30))
            .await;
        assert!(matches!(verdict, WatchVerdict::Still));
    }
    #[tokio::test]
    async fn a_watch_without_a_channel_says_so_rather_than_going_quiet() {
        let mut watch = EventWatch::with_channel(None);
        assert!(!watch.available());
        // The caller must read the tree: "no event" here means "nobody could
        // have sent one", which is not evidence that the app did not change.
        assert!(matches!(
            watch
                .moved(Some("com.example.chat"), Duration::from_millis(10))
                .await,
            WatchVerdict::Unavailable
        ));
    }
    #[tokio::test]
    async fn a_lagging_stream_stops_being_evidence() {
        // A missed event could have been the switch; the watch must say so and
        // let the run fall back to reading the tree.
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let _ = sender.send(window_event(1, "a"));
        let _ = sender.send(window_event(2, "b"));
        let _ = sender.send(window_event(3, "c"));
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(200))
            .await;
        assert!(matches!(verdict, WatchVerdict::Unavailable));
        assert!(!watch.available());
    }
    #[test]
    fn an_event_wait_payload_carries_the_cursor_and_the_kind() {
        // The Kotlin side filters by the same three fields; a typo in either
        // name silently returns everything.
        let payload = event_query(Some(7), Some(crate::device::EventKind::Notification), 5);
        assert_eq!(payload["since"], 7);
        assert_eq!(payload["kind"], "notification");
        assert_eq!(payload["limit"], 5);
        assert!(event_query(None, None, 5).get("since").is_none());
        assert!(event_query(None, None, 5).get("kind").is_none());
    }
}
