//! Test doubles and element builders shared by the tool tests.

use super::*;

/// One readable row of a wheel column at `y`, centred on `x`.
pub(super) fn wheel_row(index: usize, text: &str, x: i32, y: i32) -> UiElement {
    UiElement::new(
        index,
        Some(text.to_string()),
        None,
        None,
        "TextView".to_string(),
        false,
        true,
        (x - 30, y - 30, x + 30, y + 30),
    )
}

pub(super) fn scrollable(depth: u32, bounds: (i32, i32, i32, i32)) -> UiElement {
    let mut element =
        UiElement::new(1, None, None, None, "RecyclerView".to_string(), false, true, bounds);
    element.state.scrollable = true;
    element.state.scroll_forward = true;
    element.state.scroll_backward = true;
    element.state.depth = depth;
    element
}

pub(super) fn element(
    index: usize,
    text: &str,
    class: &str,
    clickable: bool,
    bounds: (i32, i32, i32, i32),
) -> UiElement {
    UiElement::new(
        index,
        Some(text.to_string()),
        None,
        None,
        class.to_string(),
        clickable,
        true,
        bounds,
    )
}

pub(super) fn dump(elements: Vec<UiElement>, truncated: bool) -> UiDump {
    UiDump {
        elements,
        screen: Some((1080, 2340)),
        source: "accessibility",
        package: Some("com.example".to_string()),
        snapshot: "s1-0".to_string(),
        truncated,
    }
}

/// A bridge with an event channel and a canned ring answer.
pub(super) struct EventBridge {
    pub(super) sender: tokio::sync::broadcast::Sender<crate::device::DeviceEvent>,
    page: Value,
}
impl EventBridge {
    pub(super) fn new(page: Value) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            sender: tokio::sync::broadcast::channel(16).0,
            page,
        })
    }
}
#[async_trait]
impl crate::device::DeviceBridge for EventBridge {
    async fn call(&self, _command: &str, _payload: Value) -> crate::Result<Value> {
        Ok(self.page.clone())
    }

    fn events(&self) -> Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>> {
        Some(self.sender.subscribe())
    }
}
pub(super) fn empty_page() -> Value {
    serde_json::json!({
        "events": [], "cursor": 3, "dropped": 0, "oldest": Value::Null,
        "stream": "push", "enabled": true, "connected": true,
    })
}
pub(super) fn window_event(seq: u64, package: &str) -> crate::device::DeviceEvent {
    crate::device::DeviceEvent {
        seq,
        kind: crate::device::EventKind::Window,
        package: package.to_string(),
        at_uptime_ms: seq,
        title: None,
        text: None,
        truncated: false,
    }
}
