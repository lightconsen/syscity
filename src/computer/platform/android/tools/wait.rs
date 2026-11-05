//! Wait for a control to appear, go, or for the screen to settle.

use super::*;

/// Wait for a control to appear or go, or for the screen to settle.
#[derive(Debug)]
pub struct AdbWaitTool {
    device: Option<String>,
}

impl Default for AdbWaitTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbWaitTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How often the screen is re-read while waiting.
///
/// The tree is the only signal: polling faster than a person could see a change
/// just spends reads, and the whole point of the tool is to replace the model's
/// own "observe, then observe again" loop.
pub(crate) const WAIT_POLL_MS: u64 = 250;
/// Default budget for a wait.
const WAIT_DEFAULT_MS: u64 = 10_000;
/// Upper bound on a wait.
///
/// Deliberately under the 120 s tool budget an agent's context carries, so a
/// wait that runs out still returns an answer the model can act on.
const WAIT_MAX_MS: u64 = 30_000;

/// The `condition: event` half of `android_wait`.
///
/// Separate from the polling loop because it is answered by a different thing:
/// the phone's event stream rather than a reading of the tree. The baseline
/// cursor is taken first so "an event arrived" means "…after this call
/// started"; the screen is then read once, at the moment the wait returns, so
/// the answer still says where the phone got to.
async fn wait_on_events(
    device: &Option<String>,
    args: &Value,
    budget: Duration,
) -> crate::Result<ToolExecutionResult> {
    let kind = match crate::device::EventKind::parse_filter(
        args.get("kind").and_then(Value::as_str).unwrap_or("any"),
    ) {
        Ok(kind) => kind,
        Err(message) => return Ok(ToolExecutionResult::error(message)),
    };
    // A caller that already read `android_event` has a cursor; otherwise the
    // ring's current head is the baseline, so only later events count.
    let since = match args.get("since").and_then(Value::as_u64) {
        Some(since) => since,
        None => match read_events(None, None, 1).await {
            Ok(page) => page.cursor,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        },
    };
    let wait = match wait_for_event(since, kind, budget).await {
        Ok(wait) => wait,
        Err(message) => return Ok(ToolExecutionResult::error(message)),
    };

    let waited_ms = wait.waited_ms;
    let (met, mut text) = if wait.events.is_empty() {
        (
            false,
            format!(
                "No {} event within {waited_ms} ms. The phone may be idle, or the change you \
                 expect may not raise an accessibility event; read the screen with \
                 android_ui_tree to see where it got to.",
                kind.map_or("device", |kind| kind.label())
            ),
        )
    } else {
        let mut text = format!("{} event(s) after {waited_ms} ms:", wait.events.len());
        for event in &wait.events {
            text.push_str("\n- ");
            text.push_str(&event.describe());
        }
        (true, text)
    };
    if wait.source == crate::device::EVENT_SOURCE_BUFFER {
        text.push_str(
            "\n\nEvents are being read from the phone's buffer, not pushed: Syscity's \
             accessibility service is off or the native hand-off is unavailable. The wait still \
             works, at polling latency.",
        );
    }

    // The screen at the moment the wait ended — a wait whose answer is only
    // "something happened" leaves the caller needing to look anyway.
    let target = match Target::from_args(args) {
        Ok(target) => target,
        Err(message) => return Ok(ToolExecutionResult::error(message)),
    };
    let screen = dump_ui(device, target).await;
    let elements = match &screen {
        Ok(dump) => dump.elements.clone(),
        Err(_) => Vec::new(),
    };
    Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
        "met": met,
        "condition": condition_label(),
        "events": wait.events,
        "cursor": wait.cursor,
        "waited_ms": waited_ms,
        "source": wait.source,
        "elements": elements,
        "read_source": screen.as_ref().ok().map(|dump| dump.source),
        "device": resolved_serial(device).await,
    })))
}

/// The label `wait_on_events` reports; one place, so it cannot drift from
/// [`super::query::Condition::Event::label`].
fn condition_label() -> &'static str {
    super::query::Condition::Event.label()
}

#[async_trait]
impl Tool for AdbWaitTool {
    fn name(&self) -> &str {
        "android_wait"
    }

    fn description(&self) -> &str {
        "Wait for the screen to reach a state, instead of reading it again a second later and \
         hoping. `condition: appear` waits for an element matching `text`/`resource_id`/`class` to \
         show up (a result, a button, a loaded list); `disappear` waits for it to go (a spinner, a \
         dialog); `stable` waits for the screen to stop changing; `event` waits for the phone to \
         report any change at all — a window opening, content changing, a notification arriving — \
         which is the cheapest wait when you do not know what the next screen looks like. Returns \
         whether it got there, how long it waited, and the screen as it was at that moment. When \
         it times out it says so and shows what is actually on screen, so a failure is information \
         rather than another blind retry."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Wait for a screen condition",
            serde_json::json!({
                "condition": {
                    "type": "string",
                    "description": "appear (default), disappear, stable, or event.",
                    "enum": ["appear", "disappear", "stable", "event"]
                },
                "kind": {
                    "type": "string",
                    "enum": ["any", "window", "content", "notification"],
                    "description": "For condition 'event': only this kind (default any)."
                },
                "since": {
                    "type": "integer",
                    "description": "For condition 'event': only count events after this cursor \
                                    (the `cursor` android_event returned). Default: now."
                },
                "text": {
                    "type": "string",
                    "description": "For appear/disappear: substring of the element's text or content description."
                },
                "resource_id": {
                    "type": "string",
                    "description": "For appear/disappear: substring of the element's resource id."
                },
                "class": {
                    "type": "string",
                    "description": "For appear/disappear: substring of the element's class."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long to wait (default 10000, max 30000)."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
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
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let condition = match super::query::Condition::parse(
            args.get("condition")
                .and_then(|v| v.as_str())
                .unwrap_or("appear"),
        ) {
            Ok(condition) => condition,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let query = super::query::Query::new(
            args.get("text").and_then(|v| v.as_str()),
            args.get("resource_id").and_then(|v| v.as_str()),
            args.get("class").and_then(|v| v.as_str()),
        );
        if condition.needs_query() && query.is_none() {
            return Ok(ToolExecutionResult::error(format!(
                "condition {:?} needs something to look for: set `text`, `resource_id` or \
                 `class`. Use condition 'stable' to wait for the screen to stop changing.",
                condition.label()
            )));
        }
        let budget = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(WAIT_DEFAULT_MS)
                .clamp(WAIT_POLL_MS, WAIT_MAX_MS),
        );
        if condition.reads_events() {
            return wait_on_events(&device, &args, budget).await;
        }
        let poll = Duration::from_millis(WAIT_POLL_MS);
        let started = std::time::Instant::now();
        let mut previous: Option<u64> = None;
        let mut reads = 0_u32;

        loop {
            let dump = match dump_ui(&device, target).await {
                Ok(dump) => dump,
                Err(failure) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "could not read the screen while waiting: {}",
                        failure.message
                    ))
                    .with_data(serde_json::json!({ "code": failure.code })))
                }
            };
            reads += 1;
            let matches = query.as_ref().map_or(0, |q| q.count(&dump.elements));
            let fingerprint = tree_fingerprint(&dump.elements);
            let seen_twice = previous == Some(fingerprint);
            previous = Some(fingerprint);

            if condition.met(matches, seen_twice) {
                let waited = started.elapsed().as_millis() as u64;
                let mut text = match condition {
                    super::query::Condition::Appear => {
                        format!("It appeared after {waited} ms: {matches} element(s) match.")
                    }
                    super::query::Condition::Disappear => {
                        format!("It is gone (after {waited} ms): nothing matches now.")
                    }
                    super::query::Condition::Stable => format!(
                        "The screen settled after {waited} ms ({} element(s)).",
                        dump.elements.len()
                    ),
                    // `wait_on_events` answers this one before the loop starts.
                    super::query::Condition::Event => unreachable!("event waits return early"),
                };
                if let Some(found) = query.as_ref().and_then(|q| q.find(&dump.elements, 1).pop()) {
                    text.push_str(&format!(
                        "\n[{}] {} {:?} center=({},{})",
                        found.index,
                        found.class,
                        element_label(found).unwrap_or("-"),
                        found.center.0,
                        found.center.1
                    ));
                }
                return Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "met": true,
                    "condition": condition.label(),
                    "matches": matches,
                    "waited_ms": waited,
                    "reads": reads,
                    "elements": dump.elements,
                    "device": resolved_serial(&device).await,
                })));
            }

            if started.elapsed() >= budget {
                let waited = started.elapsed().as_millis() as u64;
                let text = match condition {
                    super::query::Condition::Appear => format!(
                        "It did not appear within {waited} ms. The screen shows {} element(s); \
                         read it with android_ui_tree to see what is there.",
                        dump.elements.len()
                    ),
                    super::query::Condition::Disappear => format!(
                        "It is still there after {waited} ms ({matches} match(es)). The screen may \
                         be waiting on something else; read it with android_ui_tree."
                    ),
                    super::query::Condition::Stable => format!(
                        "The screen was still changing after {waited} ms; it may be animating or \
                         loading. Read it with android_ui_tree to see where it got to."
                    ),
                    // `wait_on_events` answers this one before the loop starts.
                    super::query::Condition::Event => unreachable!("event waits return early"),
                };
                return Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "met": false,
                    "condition": condition.label(),
                    "matches": matches,
                    "waited_ms": waited,
                    "reads": reads,
                    "elements": dump.elements,
                    "device": resolved_serial(&device).await,
                })));
            }
            tokio::time::sleep(poll).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::events::wait_for_event_on;
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn every_condition_the_wait_schema_offers_is_one_it_can_answer() {
        let schema = AdbWaitTool::new().parameters_schema();
        let offered: Vec<&str> = schema["properties"]["condition"]["enum"]
            .as_array()
            .expect("an enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        // If the schema and the parser disagree, the model is offered a value
        // that is refused — the exact failure this test exists to prevent.
        for value in &offered {
            assert!(
                super::super::query::Condition::parse(value).is_ok(),
                "{value} is in the schema but not in the parser"
            );
        }
        assert!(offered.contains(&"event"));
        // `wait_on_events` reads `since`; the schema has to offer it or a
        // model can never pass a cursor and gets "events from now" only.
        assert!(schema["properties"]["since"].is_object());
    }
    #[tokio::test]
    async fn an_event_wakes_the_wait_immediately() {
        let bridge = EventBridge::new(empty_page());
        let sender = bridge.sender.clone();
        tokio::spawn(async move {
            // Let the wait reach its select! before posting: the test is about
            // the push path, not about buffering before a subscriber exists.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = sender.send(crate::device::DeviceEvent {
                seq: 4,
                kind: crate::device::EventKind::Notification,
                package: "com.example.chat".to_string(),
                at_uptime_ms: 10,
                title: Some("Alice".to_string()),
                text: Some("Are you free?".to_string()),
                truncated: false,
            });
        });
        let wait = wait_for_event_on(bridge, 3, None, Duration::from_millis(2_000))
            .await
            .expect("a wait");
        assert_eq!(wait.events.len(), 1);
        assert_eq!(wait.events[0].seq, 4);
        assert_eq!(wait.source, crate::device::EVENT_SOURCE_PUSH);
        assert!(wait.waited_ms < 1_000, "waited {} ms", wait.waited_ms);
    }
    #[tokio::test]
    async fn an_event_of_another_kind_does_not_satisfy_the_wait() {
        let bridge = EventBridge::new(empty_page());
        let sender = bridge.sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send(crate::device::DeviceEvent {
                seq: 4,
                kind: crate::device::EventKind::Content,
                package: "com.example".to_string(),
                at_uptime_ms: 10,
                title: None,
                text: None,
                truncated: false,
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = sender.send(crate::device::DeviceEvent {
                seq: 5,
                kind: crate::device::EventKind::Notification,
                package: "com.example".to_string(),
                at_uptime_ms: 11,
                title: None,
                text: None,
                truncated: false,
            });
        });
        let wait = wait_for_event_on(
            bridge,
            3,
            Some(crate::device::EventKind::Notification),
            Duration::from_millis(2_000),
        )
        .await
        .expect("a wait");
        assert_eq!(wait.events[0].seq, 5);
    }
    #[tokio::test]
    async fn history_before_the_wait_is_taken_from_the_ring() {
        // The event that woke the model may have been posted before the tool
        // ran. It is in the ring, and the cursor filter keeps it.
        let bridge = EventBridge::new(serde_json::json!({
            "events": [
                { "seq": 4, "kind": "window", "package": "a", "atUptimeMs": 1 },
                { "seq": 5, "kind": "window", "package": "b", "atUptimeMs": 2 },
            ],
            "cursor": 5, "stream": "push", "enabled": true, "connected": true,
        }));
        let wait = wait_for_event_on(bridge, 3, None, Duration::from_millis(500))
            .await
            .expect("a wait");
        assert_eq!(wait.events.len(), 2);
        assert_eq!(wait.source, crate::device::EVENT_SOURCE_BUFFER);
        assert_eq!(wait.cursor, 5);
    }
    #[tokio::test]
    async fn a_silent_phone_times_out_without_failing() {
        let bridge = EventBridge::new(empty_page());
        let started = std::time::Instant::now();
        let wait = wait_for_event_on(bridge, 3, None, Duration::from_millis(120))
            .await
            .expect("a wait");
        assert!(wait.events.is_empty());
        assert_eq!(wait.cursor, 3);
        assert!(started.elapsed() >= Duration::from_millis(100));
    }
    #[tokio::test]
    async fn a_bridge_without_a_channel_polls_the_ring() {
        // A desktop driving a phone over ADB: no push, but the ring is still
        // there and the wait has to work through it.
        let bridge =
            std::sync::Arc::new(crate::device::tests::MockDeviceBridge::new(serde_json::json!({
                "events": [{ "seq": 9, "kind": "content", "package": "a", "atUptimeMs": 1 }],
                "cursor": 9, "stream": "broken", "enabled": true, "connected": true,
            })));
        let wait = wait_for_event_on(bridge, 8, None, Duration::from_millis(500))
            .await
            .expect("a wait");
        assert_eq!(wait.events.len(), 1);
        assert_eq!(wait.source, crate::device::EVENT_SOURCE_BUFFER);
    }
    #[test]
    fn a_wait_without_a_condition_does_not_wait_forever_open() {
        // The poll budget clamps to what the tool budget can afford: a caller
        // asking for an hour gets 30 s, and one asking for 0 ms still gets one
        // poll rather than a busy loop.
        assert_eq!(WAIT_MAX_MS, 30_000);
        assert!(WAIT_DEFAULT_MS <= WAIT_MAX_MS);
        assert!(WAIT_POLL_MS <= WAIT_DEFAULT_MS);
    }
}
